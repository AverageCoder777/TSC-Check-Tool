#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();
use slint::Model;

use rayon::prelude::*;
use regex::Regex;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use tempfile::tempdir;
use zip::ZipArchive;

const DEFAULT_CONCURRENT_SERZ: usize = 8;

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
}

pub(crate) fn clear_route_icon_cache() {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
}

mod analysis_cache;
mod app;
mod assets;
mod auditor;
mod auditor_controller;
mod blueprint;
mod domain;
mod external_editor;
mod inventory_controller;
mod inventory_ui;
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
static MISSING_WEAK: OnceLock<slint::Weak<MissingAssetsWindow>> = OnceLock::new();
static INV_SLOT: OnceLock<Arc<Mutex<Option<slint::Weak<AssetInventoryWindow>>>>> = OnceLock::new();
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
    let inv_slot = INV_SLOT.get().cloned();
    if let Ok(mut app) = app_state().lock() {
        app.receive_background_results();
        if app.inventory_refresh_pending {
            let inv_weak = inv_slot
                .as_ref()
                .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()));
            if let Some(inv) = inv_weak.and_then(|w| w.upgrade()) {
                let show_missing = inv.get_show_missing_only();
                let (rows, safe) = if let Ok(state) = app.asset_inventory_state.lock() {
                    crate::inventory_ui::build_inventory_rows(&state, show_missing)
                } else {
                    (Vec::new(), Vec::new())
                };
                inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                app.inventory_refresh_pending = false;
            }
        }
        if !app.is_checking()
            && app.result.is_some()
            && app.missing_assets_popup.is_some()
            && !app.missing_assets_shown
        {
            let missing_assets = app
                .result
                .as_ref()
                .map(|r| r.missing_assets.clone())
                .unwrap_or_default();
            let paths: Vec<slint::SharedString> = missing_assets
                .iter()
                .map(|m| m.path.clone().into())
                .collect();
            let counts: Vec<i32> = missing_assets.iter().map(|m| m.uses.len() as i32).collect();
            let repairs: Vec<slint::SharedString> = missing_assets
                .iter()
                .map(|m| m.suggested_repair.clone().unwrap_or_default().into())
                .collect();
            let corrupted: Vec<bool> =
                missing_assets.iter().map(|m| m.is_corrupted_link).collect();
            let types: Vec<slint::SharedString> = missing_assets
                .iter()
                .map(|m| crate::assets::RouteAssetType::label(m.asset_type).into())
                .collect();
            let safe: Vec<bool> = missing_assets
                .iter()
                .map(|m| {
                    !crate::assets::is_protected_network_asset(&crate::assets::RouteAssetItem {
                        path: m.path.clone(),
                        uses: m.uses.clone(),
                    })
                })
                .collect();
            if let Some(mw) = MISSING_WEAK.get().and_then(|w| w.upgrade()) {
                mw.set_missing_assets(slint::ModelRc::new(slint::VecModel::from(paths)));
                mw.set_missing_counts(slint::ModelRc::new(slint::VecModel::from(counts)));
                mw.set_missing_suggested_repairs(slint::ModelRc::new(slint::VecModel::from(
                    repairs,
                )));
                mw.set_missing_corrupted(slint::ModelRc::new(slint::VecModel::from(corrupted)));
                mw.set_missing_asset_types(slint::ModelRc::new(slint::VecModel::from(types)));
                mw.set_missing_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                mw.set_active_tile_list(slint::ModelRc::new(slint::VecModel::<
                    slint::SharedString,
                >::default()));
                mw.set_active_tile_asset("".into());
                let _ = mw.show();
                app.missing_assets_shown = true;
            }
        }
        let q = ui.get_search_query().to_string();
        app.search_query = q.clone();
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
            .map(|s| s.clone().into())
            .collect();
        ui.set_ignored_routes(slint::ModelRc::new(slint::VecModel::from(ignored)));
        let incomplete: Vec<slint::SharedString> = app
            .incomplete_folders
            .iter()
            .map(|f| f.id.clone().into())
            .collect();
        ui.set_incomplete_folders(slint::ModelRc::new(slint::VecModel::from(incomplete)));
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
    let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().unwrap()));
    let missing_win = MissingAssetsWindow::new().unwrap();
    let missing_handle = missing_win.as_weak();
    let _ = MISSING_WEAK.set(missing_handle.clone());
    #[derive(Clone)]
    struct ApPickerState {
        archive_path: PathBuf,
        missing: domain::MissingAsset,
        rw_path: String,
        entries: Vec<String>,
    }
    let ap_picker_win = ApAssetPickerWindow::new().unwrap();
    let ap_picker_handle = ap_picker_win.as_weak();
    let ap_picker_state = Arc::new(Mutex::new(None::<ApPickerState>));
    missing_win.on_copy_to_clipboard({
        let clipboard = clipboard.clone();
        move |text| {
            if let Ok(mut ctx) = clipboard.lock()
                && let Err(e) = ctx.set_text(text.to_string())
            {
                eprintln!("Ошибка копирования в буфер: {:?}", e);
            }
        }
    });
    missing_win.on_copy_all_to_clipboard({
        let clipboard = clipboard;
        let missing_handle2 = missing_handle.clone();
        move || {
            if let Some(mw) = missing_handle2.upgrade() {
                let model = mw.get_missing_assets();
                let mut paths = Vec::new();
                for i in 0..model.row_count() {
                    if let Some(value) = model.row_data(i) {
                        paths.push(value.to_string());
                    }
                }
                if let Ok(mut ctx) = clipboard.lock() {
                    let _ = ctx.set_text(paths.join("\n"));
                }
            }
        }
    });
    missing_win.on_close_missing({
        let missing_handle3 = missing_handle.clone();
        move || {
            if let Some(mw) = missing_handle3.upgrade() {
                let _ = mw.hide();
            }
        }
    });
    missing_win.on_repair_corrupted({
        let app = app.clone();
        move |idx| {
            if let Ok(mut app) = app.lock() {
                let missing_assets = app
                    .result
                    .as_ref()
                    .map(|r| r.missing_assets.clone())
                    .unwrap_or_default();
                if let Some(missing) = missing_assets.get(idx as usize).cloned()
                    && let Some(repair) = missing.suggested_repair.clone()
                {
                    let assets_path = PathBuf::from(app.rw_path.trim()).join("Assets");
                    let replacement = assets_path.join(repair.replace('/', "\\"));
                    let replacement_relative = repair;
                    app.start_asset_replacement(
                        missing,
                        replacement,
                        false,
                        replacement_relative,
                        BTreeMap::new(),
                    );
                }
            }
        }
    });
    missing_win.on_repair_all_corrupted({
        let app = app.clone();
        move || {
            if let Ok(mut app) = app.lock() {
                let missing_assets = app
                    .result
                    .as_ref()
                    .map(|r| r.missing_assets.clone())
                    .unwrap_or_default();
                let repairable: Vec<_> = missing_assets
                    .into_iter()
                    .filter(|asset| {
                        asset
                            .suggested_repair
                            .as_ref()
                            .is_some_and(|repair| !repair.trim().is_empty())
                    })
                    .collect();
                let repairable_count = repairable.len();
                eprintln!("[Repair] Found {} repairable assets", repairable_count);
                if repairable_count == 0 {
                    eprintln!("[Repair] No candidates resolved; fuzzy directory matching failed.");
                    return;
                }
                let assets_path = PathBuf::from(app.rw_path.trim()).join("Assets");
                let queue: Vec<_> = repairable
                    .into_iter()
                    .filter_map(|missing| {
                        let repair = missing.suggested_repair.clone()?;
                        let replacement = assets_path.join(repair.replace('/', "\\"));
                        Some((missing, replacement, repair))
                    })
                    .collect();
                app.asset_replacement_queue = queue;
                app.asset_replacement = None;
                app.missing_assets_shown = false;
                app.start_next_asset_replacement();
            }
        }
    });

    missing_win.on_inspect_tiles({
        let app = app.clone();
        let handle = missing_handle.clone();
        move |idx| {
            let (asset, tiles) = if let Ok(app) = app.lock() {
                if let Some(result) = app.result.as_ref() {
                    if let Some(missing) = result.missing_assets.get(idx as usize) {
                        let mut counts: BTreeMap<String, (domain::TileKind, usize)> =
                            BTreeMap::new();
                        for usage in &missing.uses {
                            let entry = counts
                                .entry(usage.tile.clone())
                                .or_insert((usage.tile_kind, 0));
                            entry.1 += 1;
                        }
                        let tiles: Vec<slint::SharedString> = counts
                            .into_iter()
                            .map(|(tile, (kind, count))| {
                                let badge = match kind {
                                    domain::TileKind::Scenery => "[Сценери]",
                                    domain::TileKind::Loft => "[Лофт]",
                                    domain::TileKind::Road => "[Дорога]",
                                    domain::TileKind::Track => "[Пути]",
                                    domain::TileKind::TracksMaster
                                    | domain::TileKind::LoftsMaster
                                    | domain::TileKind::RoadsMaster => "[Сеть]",
                                    domain::TileKind::RouteProps => "[Свойства]",
                                    domain::TileKind::Other => "[Прочее]",
                                };
                                format!("{} {} ({})", badge, tile, count).into()
                            })
                            .collect();
                        (missing.path.clone(), tiles)
                    } else {
                        (String::new(), Vec::new())
                    }
                } else {
                    (String::new(), Vec::new())
                }
            } else {
                (String::new(), Vec::new())
            };
            if let Some(mw) = handle.upgrade() {
                mw.set_active_tile_asset(asset.into());
                mw.set_active_tile_list(slint::ModelRc::new(slint::VecModel::from(tiles)));
            }
        }
    });
    ap_picker_win.on_search_changed({
        let state = ap_picker_state.clone();
        let handle = ap_picker_handle.clone();
        move |query| {
            let entries = state
                .lock()
                .ok()
                .and_then(|s| s.as_ref().map(|s| s.entries.clone()))
                .unwrap_or_default();
            let q = query.to_lowercase();
            let filtered: Vec<slint::SharedString> = entries
                .iter()
                .filter(|e| q.is_empty() || e.to_lowercase().contains(&q))
                .map(|e| e.clone().into())
                .collect();
            if let Some(pw) = handle.upgrade() {
                pw.set_filtered_entries(slint::ModelRc::new(slint::VecModel::from(filtered)));
            }
        }
    });
    ap_picker_win.on_select_entry({
        let handle = ap_picker_handle.clone();
        move |idx| {
            if let Some(pw) = handle.upgrade() {
                pw.set_selected_index(idx);
            }
        }
    });
    ap_picker_win.on_confirm_selection({
        let state = ap_picker_state.clone();
        let handle = ap_picker_handle.clone();
        let app = app.clone();
        move || {
            let (archive_path, missing, rw_path, _selected_index, entry) = {
                let guard = match state.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let Some(s) = guard.as_ref() else { return };
                let pw = match handle.upgrade() {
                    Some(p) => p,
                    None => return,
                };
                let idx = pw.get_selected_index() as usize;
                let filtered = pw.get_filtered_entries();
                let selected_entry = filtered
                    .row_data(idx)
                    .map(|v| v.to_string())
                    .or_else(|| s.entries.get(idx).cloned());
                let Some(entry) = selected_entry else { return };
                (
                    s.archive_path.clone(),
                    s.missing.clone(),
                    s.rw_path.clone(),
                    idx,
                    entry,
                )
            };
            // derive provider/product from archive parent dirs
            let assets_path = PathBuf::from(rw_path.trim()).join("Assets");
            let archive_rel = archive_path.strip_prefix(&assets_path).ok();
            let mut parts: Vec<String> = archive_rel
                .map(|p| {
                    p.components()
                        .map(|c| c.as_os_str().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            // remove archive file name
            if !parts.is_empty() {
                parts.pop();
            }
            let provider = parts.first().cloned().unwrap_or_default();
            let product = parts.get(1).cloned().unwrap_or_default();
            // build a synthetic replacement relative path Provider/Product/...xml? Engine expects .bin file; extract entry to temp file.
            // For minimal integration, call start_asset_replacement with archive path? Instead, create temp file under assets.
            let temp_rel_dir = if provider.is_empty() || product.is_empty() {
                PathBuf::from("TempAp")
            } else {
                PathBuf::from(&provider).join(&product)
            };
            let temp_dir = assets_path.join(&temp_rel_dir);
            let _ = std::fs::create_dir_all(&temp_dir);
            let entry_file_name = Path::new(&entry)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "asset.bin".to_owned());
            let temp_file = temp_dir.join(&entry_file_name);
            // extract from archive
            if let Ok(file) = std::fs::File::open(&archive_path)
                && let Ok(mut zip) = zip::ZipArchive::new(file)
                && let Ok(mut member) = zip.by_name(&entry)
                && let Ok(mut out) = std::fs::File::create(&temp_file)
            {
                let _ = std::io::copy(&mut member, &mut out);
            }
            let replacement_relative = temp_file
                .strip_prefix(&assets_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| entry.clone());
            if let Ok(mut app) = app.lock() {
                app.start_asset_replacement(
                    missing,
                    temp_file,
                    false,
                    replacement_relative,
                    BTreeMap::new(),
                );
            }
            if let Some(pw) = handle.upgrade() {
                let _ = pw.hide();
            }
        }
    });
    ap_picker_win.on_cancel({
        let handle = ap_picker_handle.clone();
        move || {
            if let Some(pw) = handle.upgrade() {
                let _ = pw.hide();
            }
        }
    });

    let app = app_for_controllers.clone();
    missing_win.on_replace_missing(move |idx| {
        let (missing, rw_path) = {
            if let Ok(app) = app.lock() {
                if let Some(result) = app.result.as_ref() {
                    if let Some(missing) = result.missing_assets.get(idx as usize) {
                        (Some(missing.clone()), app.rw_path.clone())
                    } else {
                        (None, String::new())
                    }
                } else {
                    (None, String::new())
                }
            } else {
                (None, String::new())
            }
        };
        if let Some(missing) = missing {
            if crate::assets::is_protected_network_asset(&crate::assets::RouteAssetItem {
                path: missing.path.clone(),
                uses: missing.uses.clone(),
            }) {
                if let Ok(mut app) = app.lock() {
                    app.status = "Замена путевых объектов в Tracks.bin заблокирована для предотвращения повреждения маршрута.".to_owned();
                }
                return;
            }
            let assets_path = PathBuf::from(rw_path.trim()).join("Assets");
            let start_dir =
                inventory_ui::existing_directory_upwards(&inventory_ui::asset_directory(&assets_path, &missing.path))
                    .unwrap_or_else(|| assets_path.clone());
            let selected = rfd::FileDialog::new()
                .set_title("Выберите BIN/AP нового объекта")
                .add_filter("RailWorks Assets (*.bin, *.ap)", &["bin", "ap"])
                .add_filter("RailWorks Blueprint (*.bin)", &["bin"])
                .add_filter("AP Archive (*.ap)", &["ap"])
                .set_directory(&start_dir)
                .pick_file();
            if let Some(replacement) = selected {
                let ext = replacement
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext == "ap" {
                    let entries = crate::blueprint::ap_archive_entries(&replacement);
                    let archive_name = replacement
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Ok(mut state) = ap_picker_state.lock() {
                        *state = Some(ApPickerState {
                            archive_path: replacement,
                            missing,
                            rw_path,
                            entries: entries.clone(),
                        });
                    }
                    if let Some(pw) = ap_picker_handle.upgrade() {
                        pw.set_archive_name(archive_name.into());
                        let all: Vec<slint::SharedString> =
                            entries.iter().map(|e| e.clone().into()).collect();
                        pw.set_entries(slint::ModelRc::new(slint::VecModel::from(all.clone())));
                        pw.set_filtered_entries(slint::ModelRc::new(slint::VecModel::from(all)));
                        pw.set_search_query("".into());
                        pw.set_selected_index(-1);
                        let _ = pw.show();
                    }
                } else if let Ok(mut app) = app.lock() {
                        let replacement_relative = replacement
                            .strip_prefix(&assets_path)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| {
                                replacement
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            });
                        app.start_asset_replacement(
                            missing,
                            replacement,
                            false,
                            replacement_relative,
                            BTreeMap::new(),
                        );
                }
            }
        }
    });
    let inv_handle_timer_slot = Arc::new(Mutex::new(None::<slint::Weak<AssetInventoryWindow>>));
    let _ = INV_SLOT.set(inv_handle_timer_slot.clone());

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
    if let Ok(mut slot) = inv_handle_timer_slot.lock() {
        *slot = Some(inventory.window.as_weak());
    }
    inventory.bind();
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
    let inventory_weak_for_show = inventory.weak.clone();
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
