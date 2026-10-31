#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();
use rayon::prelude::*;
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tempfile::tempdir;
use zip::ZipArchive;

const DEFAULT_CONCURRENT_SERZ: usize = 8;

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
    static INVENTORY_CONTROLLER: RefCell<Option<inventory_controller::InventoryController>> = RefCell::new(None);
    static MISSING_ASSETS_CONTROLLER: RefCell<Option<missing_assets_controller::MissingAssetsController>> = RefCell::new(None);
    static AP_PICKER_CONTROLLER: RefCell<Option<ap_picker_controller::ApPickerController>> = RefCell::new(None);
}

pub(crate) fn clear_route_icon_cache() {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
}

mod analysis_cache;
mod app;
mod assets;
mod auditor;
mod ap_picker_controller;
mod auditor_controller;
mod blueprint;
mod domain;
mod external_editor;
mod inventory_controller;
mod inventory_ui;
mod missing_assets_controller;
mod reference;
mod routes;
mod scenario;
mod scenario_controller;
mod settings;
mod settings_controller;
mod texture;
mod translit;
mod windows;

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

pub(crate) fn map_broken_resources_to_ui(
    items: &[auditor::BrokenResourceRef],
) -> slint::ModelRc<slint::ModelRc<slint::StandardListViewItem>> {
    let rows: Vec<slint::ModelRc<slint::StandardListViewItem>> = items
        .iter()
        .map(|b| {
            let kind = b.kind.label();
            let mut item0 = slint::StandardListViewItem::default();
            item0.text = kind.into();
            let mut item1 = slint::StandardListViewItem::default();
            item1.text = b.expected_path.to_string_lossy().to_string().into();
            let mut item2 = slint::StandardListViewItem::default();
            item2.text = b.parent_file.to_string_lossy().to_string().into();
            slint::ModelRc::new(slint::VecModel::from(vec![item0, item1, item2]))
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}

static UI_WEAK: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static UI_SYNC_PENDING: AtomicBool = AtomicBool::new(false);

pub(crate) fn schedule_ui_sync() {
    if UI_SYNC_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(weak) = UI_WEAK.get().cloned() {
        let _ = slint::invoke_from_event_loop(move || {
            UI_SYNC_PENDING.store(false, Ordering::SeqCst);
            if let Some(ui) = weak.upgrade() {
                sync_ui(&ui);
            }
        });
    }
}

static APP: OnceLock<Arc<Mutex<app::CheckerApp>>> = OnceLock::new();

fn app_state() -> Arc<Mutex<app::CheckerApp>> {
    APP.get().cloned().expect("app state not initialized")
}

fn sync_ui(ui: &MainWindow) {
    if let Ok(mut app) = app_state().lock() {
        app.receive_background_results();
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
        let q = ui.get_search_query().to_string();
        let search_changed = app.search_query != q;
        app.search_query = q.clone();
        let routes_dirty = search_changed
            || app.synced_routes_revision != app.routes_revision
            || app.synced_ignored_count != app.ignored_route_ids.len()
            || app.synced_incomplete_count != app.incomplete_folders.len()
            || app.synced_thumbnails != app.route_thumbnails_enabled;
        if routes_dirty {
            if app.route_thumbnails_enabled {
                for route in &app.routes {
                    if let Some(path) = &route.icon_path {
                        ROUTE_ICON_CACHE.with(|cache| {
                            let mut cache = cache.borrow_mut();
                            if !cache.contains_key(path) {
                                cache.insert(path.clone(), load_route_icon_image(path));
                            }
                        });
                    }
                }
            }
            let query = q.to_lowercase();
            let items: Vec<RouteItem> = app
                .routes
                .iter()
                .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
                .map(|r| {
                    let icon = if app.route_thumbnails_enabled {
                        r.icon_path
                            .as_ref()
                            .and_then(|p| ROUTE_ICON_CACHE.with(|cache| cache.borrow().get(p).cloned()))
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
        ui.set_check_log(app.check_log.clone().into());
        ui.set_has_result(app.result.is_some());
        ui.set_missing_assets_count(
            app.result
                .as_ref()
                .map(|r| r.missing_assets.len() as i32)
                .unwrap_or(0),
        );
        ui.set_is_checking(app.is_checking());
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

fn handle_cli_args() {
    // Headless CLI / "Open With" support
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--register-shell") {
        if let Ok(exe) = std::env::current_exe() {
            println!("Windows registry entry (save as .reg and import):");
            println!("Windows Registry Editor Version 5.00");
            println!();
            println!("[HKEY_CLASSES_ROOT\\.bin\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.GeoPcDx\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.TgPcDx\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
        }
        std::process::exit(0);
    }
    if let Some(target) = cli_args.get(1) {
        let path = PathBuf::from(target);
        // view-texture CLI
        if target.eq_ignore_ascii_case("view-texture")
            && let Some(p) = cli_args.get(2)
        {
            let path = PathBuf::from(p);
            if path.is_file() {
                match std::fs::read(&path) {
                    Ok(data) => match crate::texture::decode_tgpcdx_to_rgba(&data) {
                        Ok((w, h, rgba)) => {
                            let viewer = TextureViewerWindow::new().unwrap();
                            viewer.set_texture_image(crate::texture::rgba_to_slint_image(
                                w, h, &rgba,
                            ));
                            viewer.set_texture_orig_width(w as i32);
                            viewer.set_texture_orig_height(h as i32);
                            viewer.set_zoom_factor(if w > 640 || h > 480 {
                                (640.0 / w as f32).min(480.0 / h as f32)
                            } else {
                                1.0
                            });
                            viewer.set_info_text(
                                format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display())
                                    .into(),
                            );
                            let _ = viewer.run();
                            std::process::exit(0);
                        }
                        Err(e) => {
                            crate::external_editor::show_native_error("Ошибка", &e);
                            std::process::exit(1);
                        }
                    },
                    Err(e) => {
                        crate::external_editor::show_native_error("Ошибка", &e.to_string());
                        std::process::exit(1);
                    }
                }
            }
        }
        // File association: open .TgPcDx directly in texture viewer
        if path.is_file()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
                .unwrap_or(false)
        {
            match std::fs::read(&path) {
                Ok(data) => match crate::texture::decode_tgpcdx_to_rgba(&data) {
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
                        viewer.set_info_text(
                            format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display()).into(),
                        );
                        let _ = viewer.run();
                        std::process::exit(0);
                    }
                    Err(e) => {
                        crate::external_editor::show_native_error("Ошибка", &e);
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    crate::external_editor::show_native_error("Ошибка", &e.to_string());
                    std::process::exit(1);
                }
            }
        }

        let is_asset = path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("geopcdx"))
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

pub(crate) struct AuditorState {
    all_items: Vec<auditor::BrokenResourceRef>,
    display_items: Vec<auditor::BrokenResourceRef>,
    sort_col: usize,
    sort_asc: bool,
}

impl Default for AuditorState {
    fn default() -> Self {
        Self {
            all_items: Vec::new(),
            display_items: Vec::new(),
            sort_col: 0,
            sort_asc: true,
        }
    }
}

fn main() {
    handle_cli_args();
    // Handle file association launch: TS-Checker opened with a .bin/.GeoPcDx file.
    let app = Arc::new(Mutex::new(app::CheckerApp::default()));
    let _ = APP.set(app.clone());
    let app_for_controllers = app.clone();
    let ui = MainWindow::new().unwrap();
    let _ = UI_WEAK.set(ui.as_weak());
    crate::settings::apply_saved_language(&crate::settings::load_language());
    let ui_handle = ui.as_weak();
    if let Ok(mut app) = app.lock() {
        app.start_route_scan();
        ui.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
    }
    let missing_controller = missing_assets_controller::MissingAssetsController::new(app_for_controllers.clone());
    MISSING_ASSETS_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(missing_controller);
    });
    MISSING_ASSETS_CONTROLLER.with(|cell| {
        if let Some(c) = cell.borrow().as_ref() {
            c.bind();
        }
    });
    let ap_picker_controller = ap_picker_controller::ApPickerController::new(app_for_controllers.clone());
    AP_PICKER_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(ap_picker_controller);
    });
    AP_PICKER_CONTROLLER.with(|cell| {
        if let Some(c) = cell.borrow().as_ref() {
            c.bind();
        }
    });

    let ui_handle2 = ui.as_weak();
    let app = app_for_controllers.clone();
    ui.on_route_selected(move |idx| {
        if let Some(ui) = ui_handle2.upgrade() {
            ui.set_selected_route(idx);
        }
        if let Ok(mut app) = app.lock() {
            let q = app.search_query.to_lowercase();
            let filtered: Vec<&domain::Route> = app
                .routes
                .iter()
                .filter(|r| q.is_empty() || r.name.to_lowercase().contains(&q))
                .collect();
            if idx >= 0 && ((idx as usize) < filtered.len()) {
                app.selected_route = Some(
                    app.routes
                        .iter()
                        .position(|r| {
                            r.name == filtered[idx as usize].name
                                && r.path == filtered[idx as usize].path
                        })
                        .unwrap_or(idx as usize),
                );
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
            && let Ok(app) = app.lock()
        {
            ui.set_check_log(app.check_log.clone().into());
        }
    });
    let inventory = inventory_controller::InventoryController::new(app_for_controllers.clone());
    INVENTORY_CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(inventory);
    });
    let inventory_weak_for_show =
        INVENTORY_CONTROLLER.with(|cell| cell.borrow().as_ref().unwrap().weak.clone());
    INVENTORY_CONTROLLER.with(|cell| {
        if let Some(inv) = cell.borrow().as_ref() {
            inv.bind();
        }
    });
    let settings = settings_controller::SettingsController::new(app_for_controllers.clone());
    settings.bind();

    let scenario = scenario_controller::ScenarioController::new(app_for_controllers.clone());
    scenario.bind(&ui);

    let app = app_for_controllers.clone();
    ui.on_open_route_folder(move || {
        if let Ok(app) = app.lock()
            && let Some(idx) = app.selected_route
            && let Some(route) = app.routes.get(idx)
        {
            let _ = windows::open_in_file_explorer(&route.path);
        }
    });
    let app = app_for_controllers.clone();
    ui.on_show_inventory(move || {
        if let Ok(mut app) = app.lock() {
            app.open_asset_inventory();
            if let Some(inv) = inventory_weak_for_show.upgrade() {
                if let Ok(state) = app.asset_inventory_state.lock() {
                    inv.set_show_missing_only(false);
                    inv.set_selected_asset(-1);
                    inv.set_search_query(state.search.clone().as_str().into());
                    let (rows, safe) = crate::inventory_ui::build_inventory_rows(&state, false);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    inv.set_search_query(state.search.clone().as_str().into());
                }
                inv.show().unwrap();
            }
        }
    });
    let auditor = auditor_controller::AuditorController::new();
    let auditor_weak_for_open_folder = auditor.weak.clone();
    ui.on_open_folder_auditor(move || {
        if let Some(w) = auditor_weak_for_open_folder.upgrade() {
            let _ = w.show();
        }
    });
    auditor.bind(ui.as_weak());

    // Texture viewer integration
    let auditor_state_for_preview = auditor.state.clone();
    let viewer_win = TextureViewerWindow::new().unwrap();
    let viewer_weak = viewer_win.as_weak();
    let tex_state: Arc<Mutex<Option<(u32, u32, Vec<u8>, PathBuf)>>> = Arc::new(Mutex::new(None));
    let tex_state_for_preview = tex_state.clone();
    let tex_state_for_toggle = tex_state.clone();
    let tex_state_for_export = tex_state.clone();
    let tex_state_for_replace = tex_state;
    let preview_generation = Arc::new(AtomicU64::new(0));
    auditor.window.on_preview_texture(move |row| {
        let generation_id = preview_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let preview_generation = preview_generation.clone();
        let items = auditor_state_for_preview
            .lock()
            .map(|g| g.display_items.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let item = &items[row as usize];
        let path = if item
            .expected_path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
            .unwrap_or(false)
        {
            item.expected_path.clone()
        } else if item
            .parent_file
            .extension()
            .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
            .unwrap_or(false)
        {
            item.parent_file.clone()
        } else {
            return;
        };
        if !path.is_file() {
            return;
        }
        let tex_state = tex_state_for_preview.clone();
        let viewer_weak = viewer_weak.clone();
        std::thread::spawn(move || {
            if preview_generation.load(Ordering::SeqCst) != generation_id {
                return;
            }
            if let Ok(data) = std::fs::read(&path)
                && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
            {
                if let Ok(mut st) = tex_state.lock() {
                    *st = Some((w, h, rgba.clone(), path.clone()));
                }
                let info = format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display());
                if preview_generation.load(Ordering::SeqCst) != generation_id {
                    return;
                }
                let vw = viewer_weak;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(v) = vw.upgrade() {
                        let img = crate::texture::rgba_to_slint_image(w, h, &rgba);
                        v.set_texture_image(img);
                        v.set_texture_orig_width(w as i32);
                        v.set_texture_orig_height(h as i32);
                        v.set_zoom_factor(if w > 640 || h > 480 {
                            (640.0 / w as f32).min(480.0 / h as f32)
                        } else {
                            1.0
                        });
                        v.set_info_text(info.into());
                        let _ = v.show();
                    }
                });
            }
        });
    });

    let viewer_weak_toggle = viewer_win.as_weak();
    viewer_win.on_toggle_alpha(move || {
        let st = tex_state_for_toggle
            .lock()
            .map(|g| g.clone())
            .unwrap_or(None);
        if let Some((w, h, rgba, _)) = st {
            let show = viewer_weak_toggle
                .upgrade()
                .map(|v| v.get_show_alpha())
                .unwrap_or(true);
            let mut data = rgba;
            if !show {
                for i in (3..data.len()).step_by(4) {
                    data[i] = 255;
                }
            }
            let img = crate::texture::rgba_to_slint_image(w, h, &data);
            if let Some(v) = viewer_weak_toggle.upgrade() {
                v.set_texture_image(img);
            }
        }
    });

    let viewer_weak_export = viewer_win.as_weak();
    viewer_win.on_export_dds(move || {
        let st = tex_state_for_export
            .lock()
            .map(|g| g.clone())
            .unwrap_or(None);
        if let Some((_, _, _, path)) = st {
            let out = rfd::FileDialog::new()
                .add_filter("DDS", &["dds"])
                .save_file();
            if let Some(out) = out {
                let _ = crate::texture::export_tgpcdx_to_dds(&path, &out);
            }
        }
        let _ = viewer_weak_export;
    });

    let viewer_weak_replace = viewer_win.as_weak();
    viewer_win.on_replace_from_dds(move || {
        let st = tex_state_for_replace
            .lock()
            .map(|g| g.clone())
            .unwrap_or(None);
        if let Some((_, _, _, path)) = st {
            let input = rfd::FileDialog::new()
                .add_filter("DDS", &["dds"])
                .pick_file();
            if let Some(input) = input {
                let _ = crate::texture::convert_dds_to_tgpcdx(&input, &path);
                if let Ok(data) = std::fs::read(&path)
                    && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                {
                    if let Ok(mut s) = tex_state_for_replace.lock() {
                        *s = Some((w, h, rgba.clone(), path.clone()));
                    }
                    let info = format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display());
                    let vw = viewer_weak_replace.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(v) = vw.upgrade() {
                            let img = crate::texture::rgba_to_slint_image(w, h, &rgba);
                            v.set_texture_image(img);
                            v.set_texture_orig_width(w as i32);
                            v.set_texture_orig_height(h as i32);
                            v.set_zoom_factor(if w > 640 || h > 480 {
                                (640.0 / w as f32).min(480.0 / h as f32)
                            } else {
                                1.0
                            });
                            v.set_info_text(info.into());
                        }
                    });
                }
            }
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
