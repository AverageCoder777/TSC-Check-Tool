#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod assets;
mod auditor;
mod common;
mod geo;
mod routes;
mod scenarios;
mod texture;

slint::include_modules!();

use std::cell::RefCell;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::assets::ap_picker_controller::ApPickerController;
use crate::assets::inventory_controller::InventoryController;
use crate::assets::missing_assets_controller::MissingAssetsController;

static UI_WEAK: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static UI_SYNC_PENDING: AtomicBool = AtomicBool::new(false);
static APP: OnceLock<Arc<Mutex<app::CheckerApp>>> = OnceLock::new();

thread_local! {
    static INVENTORY_CONTROLLER: RefCell<Option<InventoryController>> = const { RefCell::new(None) };
    static MISSING_ASSETS_CONTROLLER: RefCell<Option<MissingAssetsController>> = const { RefCell::new(None) };
    static AP_PICKER_CONTROLLER: RefCell<Option<ApPickerController>> = const { RefCell::new(None) };
    static INCOMPLETE_FOLDERS_CONTROLLER: RefCell<Option<routes::IncompleteFoldersController>> = const { RefCell::new(None) };
    static ROUTE_PROPERTIES_WARNING_WINDOW: RefCell<Option<RoutePropertiesWarningWindow>> = const { RefCell::new(None) };
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

pub(crate) fn log_event(ui: &MainWindow, message: &str, level: app::LogLevel) {
    if let Ok(mut app) = app_state().lock() {
        app.status = message.to_owned();
        app.log_event(message.to_owned(), level);
        let log_lines: Vec<LogLine> = app
            .check_log
            .iter()
            .map(|e| LogLine {
                text: e.text.clone().into(),
                color: match e.level {
                    app::LogLevel::Error => slint::Color::from_rgb_u8(239, 68, 68),
                    app::LogLevel::Warning => slint::Color::from_rgb_u8(245, 158, 11),
                    app::LogLevel::Success => slint::Color::from_rgb_u8(34, 197, 94),
                    _ => slint::Color::from_rgb_u8(204, 204, 204),
                },
            })
            .collect();
        ui.set_status(message.into());
        ui.set_log_lines(slint::ModelRc::new(slint::VecModel::from(log_lines)));
        ui.set_show_log(true);
    }
}

fn sync_ui(ui: &MainWindow) {
    if let Ok(mut app) = app_state().lock() {
        app.receive_background_results();
        let icons_updated = routes::drain_loaded_icons();
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

        if !app.is_checking()
            && app.result.is_some()
            && app
                .result
                .as_ref()
                .map(|r| !r.route_properties_warnings.is_empty())
                .unwrap_or(false)
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
                    if let Some(path) = &route.icon_path
                        && routes::try_get_route_icon(path).is_none()
                    {
                        routes::request_route_icon_load(path);
                    }
                }
            }
            let query = q.trim().to_lowercase();
            let filtered_routes: Vec<&common::domain::Route> = app
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
                            .and_then(|p| routes::try_get_route_icon(p))
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
                    app::app_controller::LogLevel::Error => slint::Color::from_rgb_u8(239, 68, 68),
                    app::app_controller::LogLevel::Warning => {
                        slint::Color::from_rgb_u8(245, 158, 11)
                    }
                    app::app_controller::LogLevel::Success => {
                        slint::Color::from_rgb_u8(34, 197, 94)
                    }
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
    let controller = texture::texture_controller::TextureController::new(Arc::new(Mutex::new(
        AuditorState::default(),
    )));
    controller.bind_buttons();
    if let Err(error) = controller.load_file_sync(path) {
        common::external_editor::show_native_error("Error", &error);
        std::process::exit(1);
    }
    let _ = controller.window.run();
    std::process::exit(0);
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
                .map(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("xsec"))
                .unwrap_or(false);
        if is_asset {
            println!("[ExternalEditor] Starting synchronization for: {:?}", path);
            match common::external_editor::run_headless(&path) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            }
        }
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("geopcdx"))
        {
            if let Err(error) = geo::viewer3d::show_geo_viewer(&path) {
                common::external_editor::show_native_error("GeoPcDx Viewer", &error);
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
}

#[macro_export]
macro_rules! gettext {
    ($s:expr) => {
        $crate::i18n::tr($s)
    };
    ($s:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::tr_fmt($s, &[$(&$arg as &dyn std::fmt::Display),+])
    };
}

#[macro_export]
macro_rules! ngettext {
    ($singular:expr, $plural:expr, $n:expr) => {
        $crate::i18n::ntr($singular, $plural, ($n) as u64)
    };
    ($singular:expr, $plural:expr, $n:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::ntr_fmt(
            $singular,
            $plural,
            ($n) as u64,
            &[$(&$arg as &dyn std::fmt::Display),+],
        )
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

fn main() {
    handle_cli_args();
    let (log_tx, log_rx) = mpsc::channel::<app::LogEntry>();
    let log_path = app::settings::log_file();
    thread::spawn(move || {
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("Failed to open log file ");
        let mut writer = BufWriter::new(file);
        for entry in log_rx {
            let _ = writeln!(
                writer,
                "[{}] {}",
                chrono::Local::now().format("%H:%M:%S"),
                entry.text
            );
            let _ = writer.flush();
        }
    });

    routes::init();

    let mut app = app::CheckerApp::default();
    app.log_tx = Some(log_tx);
    let app = Arc::new(Mutex::new(app));
    let _ = APP.set(app.clone());
    let app_for_controllers = app.clone();

    let ui = MainWindow::new().unwrap();
    let _ = UI_WEAK.set(ui.as_weak());
    app::settings::apply_saved_language(&app::settings::load_language());
    let ui_handle = ui.as_weak();
    if let Ok(mut app) = app.lock() {
        app.start_route_scan();
        ui.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
    }

    let incomplete_controller =
        routes::IncompleteFoldersController::new(app_for_controllers.clone());
    incomplete_controller.bind();
    INCOMPLETE_FOLDERS_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(incomplete_controller);
    });

    let missing_controller = MissingAssetsController::new(app_for_controllers.clone());
    missing_controller.bind();
    MISSING_ASSETS_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(missing_controller);
    });

    let ap_picker_controller = ApPickerController::new(app_for_controllers.clone());
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
            let filtered: Vec<&common::domain::Route> = app
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
            schedule_ui_sync();
            let hide = app.hide_quick_drive_free_roam;
            let scen: Vec<ScenarioItem> = app
                .scenarios
                .iter()
                .filter(|s| !hide || !scenarios::is_hidden_scenario_type(&s.scenario_type))
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
            schedule_ui_sync();
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

    let inventory =
        assets::inventory_controller::InventoryController::new(app_for_controllers.clone());
    inventory.bind();
    let inventory_weak_for_show = inventory.weak.clone();
    INVENTORY_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(inventory);
    });

    let settings = app::SettingsController::new(app_for_controllers.clone());
    settings.bind();

    let scenario = scenarios::ScenarioController::new(app_for_controllers.clone());
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
        #[cfg(windows)]
        {
            let Some(path) = path else {
                return;
            };
            common::windows::open_in_explorer(&path);
            if let Some(ui) = ui_weak_open.upgrade() {
                log_event(
                    &ui,
                    &format!("Folder is open in explorer: {}.", path.display()),
                    app::LogLevel::Info,
                );
            }
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
                        assets::inventory_ui::build_inventory_rows(&state, false);
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
            log_event(
                &ui,
                &format!("GUID copied into clipboard: {guid}"),
                app::LogLevel::Info,
            );
        }
    });

    let auditor = auditor::AuditorController::new(app_for_controllers.clone());
    let auditor_weak_for_open_folder = auditor.weak.clone();
    ui.on_open_folder_auditor(move || {
        if let Some(w) = auditor_weak_for_open_folder.upgrade() {
            let _ = w.show();
        }
    });
    auditor.bind(ui.as_weak());

    let texture_controller =
        texture::texture_controller::TextureController::new(auditor.state.clone());
    texture_controller.bind(&auditor);

    let app = app_for_controllers.clone();
    ui.on_dismiss_incomplete_warning_once(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            schedule_ui_sync();
        }
    });

    let app = app_for_controllers.clone();
    ui.on_dismiss_incomplete_warning_forever(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            app.ignore_incomplete_warning = true;
            let mut config = crate::app::settings::load_config();
            config.ignore_incomplete_warning = true;
            let _ = crate::app::settings::save_config(&config);
            schedule_ui_sync();
        }
    });

    let app = app_for_controllers.clone();
    ui.on_show_incomplete_folders_list(move || {
        if let Ok(mut app) = app.lock() {
            app.show_incomplete_warning = false;
            schedule_ui_sync();
            INCOMPLETE_FOLDERS_CONTROLLER.with(|cell| {
                if let Some(controller) = cell.borrow().as_ref() {
                    controller.show(&app);
                }
            });
        }
    });

    sync_ui(&ui);
    ui.run().unwrap();
}

#[cfg(test)]
pub(crate) use app::settings::*;
#[cfg(test)]
pub(crate) use assets::*;
#[cfg(test)]
pub(crate) use common::domain::*;
#[cfg(test)]
pub(crate) use common::translit::*;
#[cfg(test)]
pub(crate) use routes::*;

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
