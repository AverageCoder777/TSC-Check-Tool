#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(clippy::all)]
#![allow(clippy::type_complexity, reason = "required for syme essecntial types")]
slint::include_modules!();

use rayon::prelude::*;
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tempfile::tempdir;
use zip::ZipArchive;

const DEFAULT_CONCURRENT_SERZ: usize = 8;

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
    static INVENTORY_CONTROLLER: RefCell<Option<inventory_controller::InventoryController>> = const { RefCell::new(None) };
    static MISSING_ASSETS_CONTROLLER: RefCell<Option<missing_assets_controller::MissingAssetsController>> = const { RefCell::new(None) };
    static AP_PICKER_CONTROLLER: RefCell<Option<ap_picker_controller::ApPickerController>> = const { RefCell::new(None) };
}

pub(crate) fn clear_route_icon_cache() {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
}

mod analysis_cache;
mod ap_picker_controller;
mod archive;

mod app;
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
mod settings;
mod settings_controller;
mod texture;
mod texture_controller;
mod translit;
mod windows;
mod xml_scan;

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

#[allow(dead_code)]
pub(crate) fn map_broken_resources_to_ui(
    items: &[auditor::BrokenResourceRef],
) -> slint::ModelRc<IssueRowData> {
    let rows: Vec<IssueRowData> = items
        .iter()
        .map(|b| {
            let is_dead = b.kind == auditor::AssetResourceKind::Unused;
            let kind = if b.count > 1 {
                format!("{} (×{})", b.kind.label(), b.count)
            } else {
                b.kind.label().to_string()
            };
            IssueRowData {
                kind_text: kind.into(),
                missing_file: if is_dead {
                    "".into()
                } else {
                    crate::fs_utils::to_assets_relative_display(&b.expected_path).into()
                },
                source_file: crate::fs_utils::to_assets_relative_display(&b.parent_file).into(),
                is_dead_resource: is_dead,
            }
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}

static UI_WEAK: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static UI_SYNC_PENDING: AtomicBool = AtomicBool::new(false);

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
        let search_changed = app.synced_search_query != q;
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
                            .and_then(|p| {
                                ROUTE_ICON_CACHE.with(|cache| cache.borrow().get(p).cloned())
                            })
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
            println!("[HKEY_CLASSES_ROOT\\.bin\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.GeoPcDx\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.XSec\\shell\\Open with TS-Checker\\command]");
            println!("@=\"\\\"{}\\\" \\\"%1\\\"\"", exe.display());
            println!();
            println!("[HKEY_CLASSES_ROOT\\.TgPcDx\\shell\\Open with TS-Checker\\command]");
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

fn main() {
    handle_cli_args();
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
            && let Ok(app) = app.lock()
        {
            ui.set_check_log(app.check_log.clone().into());
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

    #[cfg(windows)]
    {
        let app = app_for_controllers.clone();
        ui.on_open_route_folder(move || {
            if let Ok(app) = app.lock()
                && let Some(idx) = app.selected_route
                && let Some(route) = app.routes.get(idx)
            {
                let _ = windows::open_in_file_explorer(&route.path);
            }
        });
    }

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
                }
                let _ = inv.show();
            }
        }
    });

    ui.on_open_settings(move || {
        settings.show();
    });

    let auditor = auditor_controller::AuditorController::new();
    let auditor_weak_for_open_folder = auditor.weak.clone();
    ui.on_open_folder_auditor(move || {
        if let Some(w) = auditor_weak_for_open_folder.upgrade() {
            let _ = w.show();
        }
    });
    auditor.bind(ui.as_weak());

    let texture_controller = texture_controller::TextureController::new(auditor.state.clone());
    texture_controller.bind(&auditor);

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
