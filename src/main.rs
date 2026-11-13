#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//#![deny(clippy::all)]
#![allow(clippy::type_complexity, reason = "required for some essential types")]
slint::include_modules!();

use rayon::prelude::*;
use regex::Regex;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tempfile::tempdir;
use zip::ZipArchive;

mod analysis_cache;
mod ap_picker_controller;
mod app;
mod archive;
mod assets;
mod auditor;
mod auditor_controller;
mod blueprint;
mod domain;
mod external_editor;
mod fs_utils;
mod graph_view;
mod inventory_controller;
mod inventory_ui;
mod missing_assets_controller;
mod reference;
mod routes;
mod scenario;
mod scenario_controller;
mod serz;
mod settings;
mod settings_controller;
mod texture;
mod texture_controller;
mod translit;
mod windows;
mod xml_scan;

static UI_WEAK: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static UI_SYNC_PENDING: AtomicBool = AtomicBool::new(false);
static APP: OnceLock<Arc<Mutex<app::CheckerApp>>> = OnceLock::new();

/// Канал запросов на асинхронную загрузку иконок (UI → фоновый поток).
static ICON_LOAD_TX: OnceLock<mpsc::Sender<PathBuf>> = OnceLock::new();
/// Канал результатов загрузки иконок (фоновый поток → UI): (путь, сырые байты файла).
/// Receiver обёрнут в Arc<Mutex> для безопасного хранения в OnceLock (статик).
static ICON_RESULT_RX: OnceLock<Arc<Mutex<mpsc::Receiver<(PathBuf, Vec<u8>)>>>> = OnceLock::new();

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<LruIconCache> = RefCell::new(LruIconCache::new(250));
    static IN_FLIGHT_ICONS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    static INVENTORY_CONTROLLER: RefCell<Option<inventory_controller::InventoryController>> = const { RefCell::new(None) };
    static MISSING_ASSETS_CONTROLLER: RefCell<Option<missing_assets_controller::MissingAssetsController>> = const { RefCell::new(None) };
    static AP_PICKER_CONTROLLER: RefCell<Option<ap_picker_controller::ApPickerController>> = const { RefCell::new(None) };
    static INCOMPLETE_FOLDERS_WINDOW: RefCell<Option<IncompleteFoldersWindow>> = const { RefCell::new(None) };
    static ROUTE_PROPERTIES_WARNING_WINDOW: RefCell<Option<RoutePropertiesWarningWindow>> = const { RefCell::new(None) };
}

/// Кэш иконок маршрутов с LRU-вытеснением по лимиту записей.
struct LruIconCache {
    cache: HashMap<PathBuf, slint::Image>,
    order: VecDeque<PathBuf>,
    limit: usize,
}

impl LruIconCache {
    fn new(limit: usize) -> Self {
        Self {
            cache: HashMap::with_capacity(limit),
            order: VecDeque::with_capacity(limit),
            limit,
        }
    }

    fn get(&mut self, path: &Path) -> Option<slint::Image> {
        if self.cache.contains_key(path) {
            // Перемещаем путь в конец порядка (недавно использованный).
            if let Some(pos) = self.order.iter().position(|p| p == path) {
                self.order.remove(pos);
            }
            self.order.push_back(path.to_path_buf());
            self.cache.get(path).cloned()
        } else {
            None
        }
    }

    fn insert(&mut self, path: PathBuf, image: slint::Image) {
        if self.cache.contains_key(&path) {
            // Обновляем существующую запись и перемещаем в конец.
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
            self.order.push_back(path.clone());
            self.cache.insert(path, image);
            return;
        }
        // Если кэш заполнен — вытесняем самую старую запись (FIFO/LRU).
        while self.cache.len() >= self.limit {
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
        self.order.push_back(path.clone());
        self.cache.insert(path, image);
    }

    fn contains(&self, path: &Path) -> bool {
        self.cache.contains_key(path)
    }

    fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.cache.len()
    }
}

pub(crate) fn clear_route_icon_cache() {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
    IN_FLIGHT_ICONS.with(|set| set.borrow_mut().clear());
}

/// Запускает фоновую загрузку иконки, если она ещё не в кэше и не загружается.
/// Возвращает `Some(image)` если иконка уже есть в кэше, иначе `None`.
pub(crate) fn try_get_route_icon(icon_path: &Path) -> Option<slint::Image> {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().get(icon_path))
}

/// Отправляет запрос на асинхронную загрузку иконки в фоновый поток,
/// если путь ещё не кэшируется и не находится в кэше.
pub(crate) fn request_route_icon_load(icon_path: &Path) {
    let in_flight = IN_FLIGHT_ICONS.with(|set| set.borrow().contains(icon_path));
    let cached = ROUTE_ICON_CACHE.with(|cache| cache.borrow().contains(icon_path));
    if in_flight || cached {
        return;
    }
    IN_FLIGHT_ICONS.with(|set| set.borrow_mut().insert(icon_path.to_path_buf()));
    if let Some(tx) = ICON_LOAD_TX.get() {
        let _ = tx.send(icon_path.to_path_buf());
    }
}

/// Декодирует сырые байты изображения в slint::Image (вызывается только в UI-потоке).
fn decode_icon_bytes(data: &[u8]) -> slint::Image {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        slint::Image::load_from_data(data, Some("png")).unwrap_or_default()
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        slint::Image::load_from_data(data, Some("jpeg")).unwrap_or_default()
    } else {
        slint::Image::default()
    }
}

/// Синхронная загрузка иконки (оставлена для совместимости, не вызывается из UI-потока).
#[allow(dead_code)]
fn load_route_icon_image(icon_path: &Path) -> slint::Image {
    if let Ok(data) = std::fs::read(icon_path) {
        if data.starts_with(&[0x89, 0x50, 0x4E, 0x47])
            && let Ok(image) = slint::Image::load_from_data(&data, Some("png"))
        {
            return image;
        } else if data.starts_with(&[0xFF, 0xD8, 0xFF])
            && let Ok(image) = slint::Image::load_from_data(&data, Some("jpeg"))
        {
            return image;
        }
    }
    slint::Image::load_from_path(icon_path).unwrap_or_default()
}

pub(crate) fn schedule_ui_sync() {
    let Some(weak) = UI_WEAK.get().cloned() else {
        return;
    };
    if UI_SYNC_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = slint::invoke_from_event_loop(move || {
        UI_SYNC_PENDING.store(false, Ordering::SeqCst);
        if let Some(ui) = weak.upgrade() {
            sync_ui(&ui);
        }
    });
}

fn app_state() -> Arc<Mutex<app::CheckerApp>> {
    APP.get().cloned().expect("app state not initialized")
}

pub(crate) fn log_event(ui: &MainWindow, message: &str, level: crate::app::LogLevel) {
    if let Ok(mut app) = app_state().lock() {
        app.status = message.to_owned();
        app.log_event(message.to_owned(), level);
        let log_lines: Vec<LogLine> = app
            .check_log
            .iter()
            .map(|e| LogLine {
                text: e.text.clone().into(),
                color: match e.level {
                    crate::app::LogLevel::Error => slint::Color::from_rgb_u8(239, 68, 68),
                    crate::app::LogLevel::Warning => slint::Color::from_rgb_u8(245, 158, 11),
                    crate::app::LogLevel::Success => slint::Color::from_rgb_u8(34, 197, 94),
                    _ => slint::Color::from_rgb_u8(204, 204, 204),
                },
            })
            .collect();
        ui.set_status(message.into());
        ui.set_log_lines(slint::ModelRc::new(slint::VecModel::from(log_lines)));
        ui.set_show_log(true);
    }
}

/// Проверяет, безопасно ли удалять директорию: путь должен находиться
/// внутри `<game_path>/Content/Routes` и не быть самой этой папкой.
fn validate_delete_path(path: &Path, app: &crate::app::CheckerApp) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Путь к удалению пустой.".to_owned());
    }
    let routes_dir = PathBuf::from(&app.rw_path).join("Content").join("Routes");
    let canon_routes = routes_dir
        .canonicalize()
        .map_err(|e| format!("Не удалось определить папку маршрутов: {e}"))?;
    let canon_target = path
        .canonicalize()
        .map_err(|e| format!("Не удалось определить целевой путь: {e}"))?;
    if canon_target == canon_routes {
        return Err("Удаление корневой папки маршрутов запрещено.".to_owned());
    }
    if !canon_target.starts_with(&canon_routes) {
        return Err(
            "Целевой путь находится вне папки маршрутов. Удаление запрещено.".to_owned(),
        );
    }
    Ok(())
}

fn sync_ui(ui: &MainWindow) {
    if let Ok(mut app) = app_state().lock() {
        app.receive_background_results();

        // === Асинхронная загрузка иконок: дренируем результаты из фонового потока ===
        let mut icons_updated = false;
        if let Some(rx_arc) = ICON_RESULT_RX.get()
            && let Ok(rx) = rx_arc.lock()
        {
            while let Ok((path, data)) = rx.try_recv() {
                let image = decode_icon_bytes(&data);
                ROUTE_ICON_CACHE.with(|cache| {
                    cache.borrow_mut().insert(path.clone(), image);
                });
                IN_FLIGHT_ICONS.with(|set| set.borrow_mut().remove(&path));
                icons_updated = true;
            }
        }

        if !app.is_checking()
            && app.result.is_some()
            && app.missing_assets_popup.is_some()
            && !app.missing_assets_shown
        {
            MISSING_ASSETS_CONTROLLER.with(|cell| {
                if let Some(c) = cell.borrow().as_ref() {
                    c.sync_missing_popup(&mut app);
                }
            });
        }

        // Окно предупреждений RouteProperties.xml (провайдеры/продукты не подключены в <RBlueprintSetPreLoad>).
        if !app.is_checking()
            && app.result.is_some()
            && app.result.as_ref().map(|r| !r.route_properties_warnings.is_empty()).unwrap_or(false)
            && !app.route_properties_warnings_shown
        {
            ROUTE_PROPERTIES_WARNING_WINDOW.with(|cell| {
                let mut cell = cell.borrow_mut();
                if cell.is_none() {
                    *cell = Some(RoutePropertiesWarningWindow::new().unwrap());
                }
                if let Some(window) = cell.as_mut() {
                    let warnings: Vec<slint::SharedString> = app
                        .result
                        .as_ref()
                        .unwrap()
                        .route_properties_warnings
                        .iter()
                        .map(|s| slint::SharedString::from(s.as_str()))
                        .collect();
                    window.set_warnings(slint::ModelRc::new(slint::VecModel::from(warnings)));
                    let weak = window.as_weak();
                    window.on_close_warning(move || {
                        if let Some(w) = weak.upgrade() {
                            let _ = w.hide();
                        }
                    });
                    window.show().unwrap();
                }
            });
            app.route_properties_warnings_shown = true;
        }
        let q = ui.get_search_query().to_string();
        let search_changed = app.synced_search_query != q;
        app.search_query = q.clone();
        let routes_dirty = search_changed
            || icons_updated
            || app.synced_routes_revision != app.routes_revision
            || app.synced_ignored_count != app.ignored_route_ids.len()
            || app.synced_incomplete_count != app.incomplete_folders.len()
            || app.synced_thumbnails != app.route_thumbnails_enabled;
        if routes_dirty {
            if app.route_thumbnails_enabled {
                for route in &app.routes {
                    if let Some(path) = &route.icon_path {
                        // Пытаемся получить иконку из кэша; если нет — запрашиваем фоновую загрузку.
                        if try_get_route_icon(path).is_none() {
                            request_route_icon_load(path);
                        }
                    }
                }
            }
            let query = q.trim().to_lowercase();
            let filtered_routes: Vec<&domain::Route> = app
                .routes
                .iter()
                .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
                .collect();
            let filtered_selected_idx = app
                .selected_route
                .and_then(|sel_idx| {
                    let sel_path = &app.routes.get(sel_idx)?.path;
                    filtered_routes.iter().position(|r| &r.path == sel_path)
                })
                .map(|i| i as i32)
                .unwrap_or(-1);
            let items: Vec<RouteItem> = filtered_routes
                .iter()
                .map(|r| {
                    let icon = if app.route_thumbnails_enabled {
                        r.icon_path
                            .as_ref()
                            .and_then(|p| try_get_route_icon(p))
                            .unwrap_or_default()
                    } else {
                        slint::Image::default()
                    };
                    RouteItem {
                        name: r.name.clone().into(),
                        icon_source: icon,
                    }
                })
                .collect();
            ui.set_route_items(slint::ModelRc::new(slint::VecModel::from(items)));
            ui.set_selected_route(filtered_selected_idx);
            let ignored: Vec<slint::SharedString> = app
                .ignored_route_ids
                .iter()
                .map(|id| id.clone().into())
                .collect();
            ui.set_ignored_routes(slint::ModelRc::new(slint::VecModel::from(ignored)));
            let incomplete: Vec<slint::SharedString> = app
                .incomplete_folders
                .iter()
                .map(|f| f.id.clone().into())
                .collect();
            ui.set_incomplete_folders(slint::ModelRc::new(slint::VecModel::from(incomplete)));
            app.synced_search_query = q.clone();
            app.synced_routes_revision = app.routes_revision;
            app.synced_ignored_count = app.ignored_route_ids.len();
            app.synced_incomplete_count = app.incomplete_folders.len();
            app.synced_thumbnails = app.route_thumbnails_enabled;
        }
        if let Some(route) = app.selected_route.and_then(|i| app.routes.get(i)) {
            ui.set_selected_route_name(route.name.clone().into());
            ui.set_selected_route_id(route.id.clone().into());
        } else {
            ui.set_selected_route_name("".into());
            ui.set_selected_route_id("".into());
        }
        ui.set_status(app.status.clone().into());
        let log_lines: Vec<LogLine> = app
            .check_log
            .iter()
            .map(|e| LogLine {
                text: e.text.clone().into(),
                color: match e.level {
                    crate::app::LogLevel::Error => slint::Color::from_rgb_u8(239, 68, 68),
                    crate::app::LogLevel::Warning => slint::Color::from_rgb_u8(245, 158, 11),
                    crate::app::LogLevel::Success => slint::Color::from_rgb_u8(34, 197, 94),
                    _ => slint::Color::from_rgb_u8(204, 204, 204),
                },
            })
            .collect();
        ui.set_log_lines(slint::ModelRc::new(slint::VecModel::from(log_lines)));
        ui.set_has_result(app.result.is_some());
        ui.set_missing_assets_count(
            app.result
                .as_ref()
                .map(|r| r.missing_assets.len() as i32)
                .unwrap_or(0),
        );
        ui.set_is_checking(app.is_checking());
        ui.set_show_incomplete_warning(app.show_incomplete_warning);
        if app.is_checking() {
            ui.set_show_log(true);
        }
        ui.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
    }
    INVENTORY_CONTROLLER.with(|cell| {
        if let Some(controller) = cell.borrow().as_ref() {
            controller.refresh();
        }
    });
}

fn open_texture_viewer(path: &Path) {
    if !path.is_file() {
        return;
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            crate::external_editor::show_native_error("Ошибка", &e.to_string());
            std::process::exit(1);
        }
    };
    match crate::texture::decode_tgpcdx_to_rgba(&data) {
        Ok((w, h, rgba)) => {
            let viewer = TextureViewerWindow::new().unwrap();
            viewer.set_texture_image(crate::texture::rgba_to_slint_image(w, h, &rgba));
            viewer.set_texture_orig_width(w as i32);
            viewer.set_texture_orig_height(h as i32);
            viewer.set_zoom_factor(if w > 640 || h > 480 {
                (640.0 / w as f32).min(480.0 / h as f32)
            } else {
                1.0
            });
            viewer.set_info_text(format!("{} x {} px  TGPCDX  {}", w, h, path.display()).into());
            let _ = viewer.run();
            std::process::exit(0);
        }
        Err(e) => {
            crate::external_editor::show_native_error("Ошибка", &e);
            std::process::exit(1);
        }
    }
}

fn handle_cli_args() {
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--register-shell") {
        if let Ok(exe) = std::env::current_exe() {
            println!("Windows registry entry (save as .reg and import):");
            println!("Windows Registry Editor Version 5.00");
            println!();
            println!("[HKEY_CLASSES_ROOT\\.bin\\shell\\Open with TSC Check Tool\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.GeoPcDx\\shell\\Open with TSC Check Tool\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.XSec\\shell\\Open with TSC Check Tool\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.TgPcDx\\shell\\Open with TSC Check Tool\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
        }
        std::process::exit(0);
    }
    if let Some(target) = cli_args.get(1) {
        if target.eq_ignore_ascii_case("view-texture")
            && let Some(p) = cli_args.get(2)
        {
            open_texture_viewer(Path::new(p));
        }
        let path = PathBuf::from(target);
        if path.is_file()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
                .unwrap_or(false)
        {
            open_texture_viewer(&path);
        }
        let is_asset = path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    e.eq_ignore_ascii_case("bin")
                        || e.eq_ignore_ascii_case("geopcdx")
                        || e.eq_ignore_ascii_case("xsec")
                })
                .unwrap_or(false);
        if is_asset {
            println!("[ExternalEditor] Запуск синхронизации для: {:?}", path);
            match crate::external_editor::run_headless(&path) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

#[macro_export]
macro_rules! gettext {
    ($s:expr) => {
        $s
    };
}

#[macro_export]
macro_rules! ngettext {
    ($singular:expr, $plural:expr, $n:expr) => {
        if $n == 1 { $singular } else { $plural }
    };
}

pub(crate) struct AuditorState {
    all_items: Vec<auditor::BrokenResourceRef>,
    display_items: Vec<auditor::BrokenResourceRef>,
    sort_col: usize,
    sort_asc: bool,
    dependency_graph: auditor::DependencyGraph,
    full_asset_graph: auditor::FullAssetGraph,
    is_scan_completed: bool,
}

impl Default for AuditorState {
    fn default() -> Self {
        Self {
            all_items: Vec::new(),
            display_items: Vec::new(),
            sort_col: 2,
            sort_asc: true,
            dependency_graph: auditor::DependencyGraph::default(),
            full_asset_graph: auditor::FullAssetGraph::default(),
            is_scan_completed: false,
        }
    }
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} МБ", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} КБ", bytes as f64 / 1024.0)
    } else {
        "< 1 КБ".to_owned()
    }
}

fn folder_count_word(count: usize) -> &'static str {
    let n = count % 100;
    if (5..=20).contains(&n) {
        "папок"
    } else {
        match n % 10 {
            1 => "папка",
            2..=4 => "папки",
            _ => "папок",
        }
    }
}

fn build_incomplete_folder_items(
    folders: &[crate::domain::IncompleteRouteFolder],
) -> Vec<crate::IncompleteFolderItem> {
    folders
        .iter()
        .map(|folder| crate::IncompleteFolderItem {
            folder_name: folder.id.clone().into(),
            full_path: folder.path.display().to_string().into(),
            missing_criteria: folder.problems.join(" • ").into(),
            file_count: folder.file_count as i32,
            size_mb: format_size_bytes(folder.total_size_bytes).into(),
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "--serz-diff" {
        serz::run_test(&args[2], &args[3]);
        return;
    }
    handle_cli_args();
    // Фоновый поток записи полного лога на диск (AppData\TSC Check Tool\app.log).
    let (log_tx, log_rx) = mpsc::channel::<crate::app::LogEntry>();
    let log_path = crate::settings::log_file();
    thread::spawn(move || {
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("Не удалось открыть файл лога");
        let mut writer = BufWriter::new(file);
        for entry in log_rx {
            let line = format!(
                "[{}] {}\n",
                chrono::Local::now().format("%H:%M:%S"),
                entry.text
            );
            let _ = writeln!(writer, "{}", line);
        }
    });

    // Фоновый поток асинхронной загрузки иконок маршрутов.
    let (icon_load_tx, icon_load_rx) = mpsc::channel::<PathBuf>();
    let (icon_result_tx, icon_result_rx) = mpsc::channel::<(PathBuf, Vec<u8>)>();
    ICON_LOAD_TX.set(icon_load_tx).ok();
    ICON_RESULT_RX.set(Arc::new(Mutex::new(icon_result_rx))).ok();
    thread::spawn(move || {
        while let Ok(path) = icon_load_rx.recv() {
            if let Ok(data) = std::fs::read(&path) {
                let _ = icon_result_tx.send((path, data));
            }
        }
    });

    let mut app = app::CheckerApp::default();
    app.log_tx = Some(log_tx);
    let app = Arc::new(Mutex::new(app));
    let _ = APP.set(app.clone());
    let app_for_controllers = app.clone();
    let ui = MainWindow::new().unwrap();
    let incomplete_folders_window = IncompleteFoldersWindow::new().unwrap();
    INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(incomplete_folders_window);
    });
    let _ = UI_WEAK.set(ui.as_weak());
    crate::settings::apply_saved_language(&crate::settings::load_language());
    let ui_handle = ui.as_weak();
    if let Ok(mut app) = app.lock() {
        app.start_route_scan();
        ui.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
    }
    let missing_controller =
        missing_assets_controller::MissingAssetsController::new(app_for_controllers.clone());
    missing_controller.bind();
    MISSING_ASSETS_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(missing_controller);
    });
    let ap_picker_controller =
        ap_picker_controller::ApPickerController::new(app_for_controllers.clone());
    ap_picker_controller.bind();
    AP_PICKER_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(ap_picker_controller);
    });
    let ui_handle2 = ui.as_weak();
    let app = app_for_controllers.clone();
    ui.on_route_selected(move |idx| {
        if let Some(ui) = ui_handle2.upgrade() {
            ui.set_selected_route(idx);
        }
        if let Ok(mut app) = app.lock() {
            let q = app.search_query.trim().to_lowercase();
            let filtered: Vec<&domain::Route> = app
                .routes
                .iter()
                .filter(|r| q.is_empty() || r.name.to_lowercase().contains(&q))
                .collect();
            if idx >= 0 && ((idx as usize) < filtered.len()) {
                let target_path = &filtered[idx as usize].path;
                app.selected_route = app.routes.iter().position(|r| &r.path == target_path);
            } else {
                app.selected_route = None;
            }
            app.refresh_scenarios();
            crate::schedule_ui_sync();
            let hide = app.hide_quick_drive_free_roam;
            let scen: Vec<ScenarioItem> = app
                .scenarios
                .iter()
                .filter(|s| !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type))
                .map(|s| ScenarioItem {
                    name: s.name.clone().into(),
                    scenario_type: s.scenario_type.clone().into(),
                })
                .collect();
            if let Some(ui) = ui_handle2.upgrade() {
                ui.set_scenario_items(slint::ModelRc::new(slint::VecModel::from(scen)));
                ui.set_selected_scenario(-1);
                if let Some(route) = app.selected_route.and_then(|i| app.routes.get(i)) {
                    ui.set_selected_route_name(route.name.clone().into());
                    ui.set_selected_route_id(route.id.clone().into());
                } else {
                    ui.set_selected_route_name("".into());
                    ui.set_selected_route_id("".into());
                }
            }
        }
    });
    let app = app_for_controllers.clone();
    ui.on_search_changed(move |query| {
        if let Ok(mut app) = app.lock() {
            app.search_query = query.to_string();
            crate::schedule_ui_sync();
        }
    });
    let ui_handle_start_check = ui_handle.clone();
    let app = app_for_controllers.clone();
    ui.on_start_check(move || {
        if let Some(ui) = ui_handle_start_check.upgrade() {
            ui.set_show_log(true);
        }
        if let Ok(mut app) = app.lock() {
            app.start_check();
        }
    });
    let ui_handle_reset = ui_handle;
    let app = app_for_controllers.clone();
    ui.on_reset_log(move || {
        if let Some(ui) = ui_handle_reset.upgrade()
            && let Ok(mut app) = app.lock()
        {
            app.check_log.clear();
            ui.set_log_lines(slint::ModelRc::new(slint::VecModel::from(
                Vec::<LogLine>::new(),
            )));
        }
    });
    let inventory = inventory_controller::InventoryController::new(app_for_controllers.clone());
    inventory.bind();
    let inventory_weak_for_show = inventory.weak.clone();
    INVENTORY_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(inventory);
    });
    let settings = settings_controller::SettingsController::new(app_for_controllers.clone());
    settings.bind();
    let scenario = scenario_controller::ScenarioController::new(app_for_controllers.clone());
    scenario.bind(&ui);
    let ui_weak_open = ui.as_weak();
    let app = app_for_controllers.clone();
    ui.on_open_route_folder(move || {
        let path = {
            let Ok(app) = app.lock() else {
                return;
            };
            app.selected_route
                .and_then(|idx| app.routes.get(idx))
                .map(|route| route.path.clone())
        };
        let Some(path) = path else {
            return;
        };
        windows::open_in_file_explorer(&path);
        if let Some(ui) = ui_weak_open.upgrade() {
            crate::log_event(
                &ui,
                &format!("Открыта папка в проводнике: {}.", path.display()),
                crate::app::LogLevel::Info,
            );
        }
    });
    let app = app_for_controllers.clone();
    ui.on_show_inventory(move || {
        if let Ok(mut app) = app.lock() {
            app.open_asset_inventory();
            if let Some(inv) = inventory_weak_for_show.upgrade() {
                if let Ok(mut state) = app.asset_inventory_state.lock() {
                    inv.set_show_missing_only(false);
                    inv.set_selected_asset(-1);
                    inv.set_search_query(state.search.clone().as_str().into());
                    let (rows, safe, delete_flags, filtered_indices) =
                        crate::inventory_ui::build_inventory_rows(&state, false);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    inv.set_inventory_can_delete(slint::ModelRc::new(slint::VecModel::from(
                        delete_flags,
                    )));
                    state.filtered_indices = filtered_indices;
                }
                let _ = inv.show();
            }
        }
    });
    ui.on_open_settings(move || {
        settings.show();
    });
    let ui_handle_copy_guid = ui.as_weak();
    ui.on_copy_guid(move |guid| {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(guid.to_string());
        }
        if let Some(ui) = ui_handle_copy_guid.upgrade() {
            crate::log_event(
                &ui,
                &format!("GUID скопирован в буфер обмена: {guid}"),
                crate::app::LogLevel::Info,
            );
        }
    });
    let auditor = auditor_controller::AuditorController::new(app_for_controllers.clone());
    let auditor_weak_for_open_folder = auditor.weak.clone();
    ui.on_open_folder_auditor(move || {
        if let Some(w) = auditor_weak_for_open_folder.upgrade() {
            let _ = w.show();
        }
    });
    auditor.bind(ui.as_weak());
    let texture_controller = texture_controller::TextureController::new(auditor.state.clone());
    texture_controller.bind(&auditor);
    let app = app_for_controllers.clone();
    ui.on_dismiss_incomplete_warning_once(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            crate::schedule_ui_sync();
        }
    });
    let app = app_for_controllers.clone();
    ui.on_dismiss_incomplete_warning_forever(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            app.ignore_incomplete_warning = true;
            let mut config = crate::settings::load_config();
            config.ignore_incomplete_warning = true;
            let _ = crate::settings::save_config(&config);
            crate::schedule_ui_sync();
        }
    });
    let app = app_for_controllers.clone();
    ui.on_show_incomplete_folders_list(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            let items = build_incomplete_folder_items(&app.incomplete_folders);
            let total_bytes: u64 = app
                .incomplete_folders
                .iter()
                .map(|f| f.total_size_bytes)
                .sum();
            let count = app.incomplete_folders.len();
            let summary = format!(
                "Всего: {} {} • общий вес: {}",
                count,
                folder_count_word(count),
                format_size_bytes(total_bytes)
            );
            crate::schedule_ui_sync();
            INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
                if let Some(window) = cell.borrow().as_ref() {
                    window.set_folders(slint::ModelRc::new(slint::VecModel::from(items)));
                    window.set_selected_index(-1);
                    window.set_summary(summary.clone().into());
                    let _ = window.show();
                }
            });
        }
    });
    // Привязка колбэков окна неполных папок.
    INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
        if let Some(window) = cell.borrow().as_ref() {
            window.on_open_in_explorer(move |path| {
                let _ = std::process::Command::new("explorer").arg(&path).spawn();
            });
        }
    });

    let app = app_for_controllers.clone();
    INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
        if let Some(window) = cell.borrow().as_ref() {
            window.on_delete_selected(move |index| {
                let confirmed = rfd::MessageDialog::new()
                    .set_title("Удаление папки")
                    .set_description("Удалить выбранную неполную папку навсегда?")
                    .set_buttons(rfd::MessageButtons::OkCancel)
                    .show();
                if confirmed != rfd::MessageDialogResult::Ok {
                    return;
                }
                if let Ok(mut app) = app.lock() {
                    let idx = index as usize;
                    if idx >= app.incomplete_folders.len() {
                        return;
                    }
                    let path = app.incomplete_folders[idx].path.clone();
                    match validate_delete_path(&path, &app) {
                        Ok(()) => {
                            match std::fs::remove_dir_all(&path) {
                                Ok(()) => {
                                    app.incomplete_folders.remove(idx);
                                    app.log_event(
                                        format!("Удалена неполная папка: {}.", path.display()),
                                        crate::app::LogLevel::Info,
                                    );
                                    let items = build_incomplete_folder_items(&app.incomplete_folders);
                                    let total: u64 = app
                                        .incomplete_folders
                                        .iter()
                                        .map(|f| f.total_size_bytes)
                                        .sum();
                                    let count = app.incomplete_folders.len();
                                    let summary = format!(
                                        "Всего: {} {} • общий вес: {}",
                                        count,
                                        folder_count_word(count),
                                        format_size_bytes(total)
                                    );
                                    INCOMPLETE_FOLDERS_WINDOW.with(|cell2| {
                                        if let Some(w) = cell2.borrow().as_ref() {
                                            w.set_folders(slint::ModelRc::new(slint::VecModel::from(
                                                items,
                                            )));
                                            w.set_selected_index(-1);
                                            w.set_summary(summary.into());
                                        }
                                    });
                                    crate::schedule_ui_sync();
                                }
                                Err(error) => {
                                    app.log_event(
                                        format!("Не удалось удалить {}: {error}.", path.display()),
                                        crate::app::LogLevel::Error,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            app.log_event(
                                format!(
                                    "Ошибка безопасности при удалении {}: {e}.",
                                    path.display()
                                ),
                                crate::app::LogLevel::Error,
                            );
                        }
                    }
                }
            });
        }
    });

    let app = app_for_controllers.clone();
    INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
        if let Some(window) = cell.borrow().as_ref() {
            window.on_delete_all(move || {
                let confirmed = rfd::MessageDialog::new()
                    .set_title("Удаление папок")
                    .set_description("Удалить все найденные неполные папки навсегда?")
                    .set_buttons(rfd::MessageButtons::OkCancel)
                    .show();
                if confirmed != rfd::MessageDialogResult::Ok {
                    return;
                }
                if let Ok(mut app) = app.lock() {
                    let mut remaining = Vec::new();
                    for folder in app.incomplete_folders.clone() {
                        match validate_delete_path(&folder.path, &app) {
                            Ok(()) => {
                                if std::fs::remove_dir_all(&folder.path).is_err() {
                                    remaining.push(folder);
                                }
                            }
                            Err(e) => {
                                app.log_event(
                                    format!(
                                        "Ошибка безопасности при удалении {}: {e}.",
                                        folder.path.display()
                                    ),
                                    crate::app::LogLevel::Error,
                                );
                                remaining.push(folder);
                            }
                        }
                    }
                    let removed = app.incomplete_folders.len() - remaining.len();
                    app.incomplete_folders = remaining;
                    app.log_event(
                        format!("Удалены все неполные папки ({removed} шт.)."),
                        crate::app::LogLevel::Info,
                    );
                    let items = build_incomplete_folder_items(&app.incomplete_folders);
                    let total: u64 = app
                        .incomplete_folders
                        .iter()
                        .map(|f| f.total_size_bytes)
                        .sum();
                    let count = app.incomplete_folders.len();
                    let summary = format!(
                        "Всего: {} {} • общий вес: {}",
                        count,
                        folder_count_word(count),
                        format_size_bytes(total)
                    );
                    INCOMPLETE_FOLDERS_WINDOW.with(|cell2| {
                        if let Some(w) = cell2.borrow().as_ref() {
                            w.set_folders(slint::ModelRc::new(slint::VecModel::from(items)));
                            w.set_selected_index(-1);
                            w.set_summary(summary.into());
                        }
                    });
                    crate::schedule_ui_sync();
                }
            });
        }
    });

    INCOMPLETE_FOLDERS_WINDOW.with(|cell| {
        if let Some(window) = cell.borrow().as_ref() {
            window.on_close_window(move || {
                INCOMPLETE_FOLDERS_WINDOW.with(|cell2| {
                    if let Some(w) = cell2.borrow().as_ref() {
                        let _ = w.hide();
                    }
                });
            });
        }
    });

    sync_ui(&ui);
    ui.run().unwrap();
}

#[cfg(test)]
pub(crate) use assets::*;
#[cfg(test)]
pub(crate) use domain::*;
#[cfg(test)]
pub(crate) use reference::*;
#[cfg(test)]
pub(crate) use routes::*;
#[cfg(test)]
pub(crate) use settings::*;
#[cfg(test)]
pub(crate) use translit::*;

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
