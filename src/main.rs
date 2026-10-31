#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();
use slint::Model;

use rayon::prelude::*;
use regex::Regex;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use zip::ZipArchive;

const DEFAULT_CONCURRENT_SERZ: usize = 8;

mod analysis_cache;
mod app;
mod assets;
mod auditor;
mod blueprint;
mod domain;
mod external_editor;
mod inventory_ui;
mod reference;
mod routes;
mod scenario;
mod settings;
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

fn load_vehicle_preview_image(
    rw_path: &Path,
    route_path: &Path,
    vehicle: &scenario::ScenarioVehicle,
    resolved: Option<&PathBuf>,
) -> slint::Image {
    let resolved = resolved.cloned().or_else(|| {
        crate::blueprint::resolve_blueprint_file_cached_free(
            rw_path,
            route_path,
            &vehicle.blueprint,
            &vehicle.provider,
            &vehicle.product,
        )
    });
    let Some(path) = resolved else {
        return slint::Image::default();
    };

    // Обычный файл на диске: картинка лежит рядом в LocoInformation/image.png.
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
    {
        let image_path = path
            .parent()
            .unwrap_or(path.as_path())
            .join("LocoInformation")
            .join("image.png");
        if image_path.is_file() {
            if let Ok(data) = std::fs::read(&image_path)
                && let Ok(image) = slint::Image::load_from_data(&data, Some("png"))
            {
                return image;
            }
            return slint::Image::load_from_path(&image_path).unwrap_or_default();
        }
        return slint::Image::default();
    }

    // Архив .ap: ищем запись блюпринта, затем соседнюю LocoInformation/image.png.
    let Ok(file) = std::fs::File::open(&path) else {
        return slint::Image::default();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return slint::Image::default();
    };

    let stem = Path::new(&vehicle.blueprint)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| vehicle.blueprint.clone());
    let mut base_dir = String::new();
    let mut found = false;
    for index in 0..zip.len() {
        let Ok(entry) = zip.by_index(index) else {
            continue;
        };
        let name = entry.name().unwrap_or_default().to_string();
        let entry_stem = Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if entry_stem.eq_ignore_ascii_case(&stem) {
            base_dir = name
                .rsplit_once('/')
                .or_else(|| name.rsplit_once('\\'))
                .map(|(dir, _)| dir.to_owned())
                .unwrap_or_default();
            found = true;
            break;
        }
    }
    if !found {
        return slint::Image::default();
    }

    let image_name = if base_dir.is_empty() {
        "LocoInformation/image.png".to_owned()
    } else {
        format!(
            "{}/LocoInformation/image.png",
            base_dir.trim_end_matches('/')
        )
    };

    for index in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(index) else {
            continue;
        };
        let entry_name = entry.name().unwrap_or_default().to_string();
        if !entry_name.eq_ignore_ascii_case(&image_name) {
            continue;
        }
        let mut data = Vec::new();
        if std::io::Read::read_to_end(&mut entry, &mut data).is_ok()
            && let Ok(image) = slint::Image::load_from_data(&data, Some("png"))
        {
            return image;
        }
    }
    slint::Image::default()
}

fn filter_items(items: &mut Vec<auditor::BrokenResourceRef>, filter: &str) {
    let f = filter.to_lowercase();
    let f = match f.as_str() {
        "geometry" => "геометрия".to_string(),
        "texture" => "текстура".to_string(),
        "script" => "скрипт".to_string(),
        "sound" | "audio" => "звук".to_string(),
        "child" | "children" => "дочер".to_string(),
        other => other.to_string(),
    };
    if f.is_empty() || f == "all" || f == "все" {
        return;
    }
    items.retain(|b| {
        let kind = match b.kind {
            auditor::AssetResourceKind::Geometry => "геометрия",
            auditor::AssetResourceKind::Texture => "текстура",
            auditor::AssetResourceKind::Animation => "анимация",
            auditor::AssetResourceKind::Script => "скрипт",
            auditor::AssetResourceKind::Sound => "звуки",
            auditor::AssetResourceKind::ChildBlueprint => "дочерние",
        };
        if f.contains("геометр") {
            return kind.contains("геометр");
        }
        if f.contains("текстур") {
            return kind.contains("текстур");
        }
        if f.contains("скрипт") {
            return kind.contains("скрипт");
        }
        if f.contains("звук") || f.contains("аудио") {
            return kind.contains("звук");
        }
        if f.contains("дочер") || f.contains("child") {
            return kind.contains("дочер");
        }
        kind.contains(&f)
    });
}

fn sort_items(items: &mut [auditor::BrokenResourceRef], col: usize, asc: bool) {
    items.sort_by(|a, b| {
        let va = match col {
            0 => match a.kind {
                auditor::AssetResourceKind::Geometry => "Геометрия",
                auditor::AssetResourceKind::Texture => "Текстура",
                auditor::AssetResourceKind::Animation => "Анимация",
                auditor::AssetResourceKind::Script => "Скрипт",
                auditor::AssetResourceKind::Sound => "Звуки",
                auditor::AssetResourceKind::ChildBlueprint => "Дочерние объекты",
            }
            .to_string(),
            1 => a.expected_path.to_string_lossy().to_string(),
            _ => a.parent_file.to_string_lossy().to_string(),
        };
        let vb = match col {
            0 => match b.kind {
                auditor::AssetResourceKind::Geometry => "Геометрия",
                auditor::AssetResourceKind::Texture => "Текстура",
                auditor::AssetResourceKind::Animation => "Анимация",
                auditor::AssetResourceKind::Script => "Скрипт",
                auditor::AssetResourceKind::Sound => "Звуки",
                auditor::AssetResourceKind::ChildBlueprint => "Дочерние объекты",
            }
            .to_string(),
            1 => b.expected_path.to_string_lossy().to_string(),
            _ => b.parent_file.to_string_lossy().to_string(),
        };
        if asc { va.cmp(&vb) } else { vb.cmp(&va) }
    });
}

fn build_inventory_rows(
    state: &app::AssetInventoryWindowState,
    show_missing: bool,
) -> (Vec<slint::ModelRc<slint::StandardListViewItem>>, Vec<bool>) {
    let mut assets: Vec<&domain::ReferencedAsset> = state.assets.iter().collect();
    match state.sort.column {
        inventory_ui::InventorySortColumn::Path => {
            if state.sort.descending {
                assets.sort_by(|a, b| b.path.cmp(&a.path));
            } else {
                assets.sort_by(|a, b| a.path.cmp(&b.path));
            }
        }
        inventory_ui::InventorySortColumn::Usage => {
            if state.sort.descending {
                assets.sort_by_key(|a| Reverse(a.uses.len()));
            } else {
                assets.sort_by_key(|a| a.uses.len());
            }
        }
    }
    let search = state.search.to_lowercase();
    let mut rows = Vec::new();
    let mut safe_flags = Vec::new();
    for a in assets {
        if show_missing && !a.missing {
            continue;
        }
        if !search.is_empty() {
            let matches = a.path.to_lowercase().contains(&search)
                || a.uses.iter().any(|u| {
                    u.tile.to_lowercase().contains(&search)
                        || u.source_file.display().to_lowercase().contains(&search)
                });
            if !matches {
                continue;
            }
        }
        let mut by_tile: BTreeMap<String, usize> = BTreeMap::new();
        for u in a.uses.iter() {
            *by_tile.entry(u.tile.clone()).or_insert(0) += 1;
        }
        let tiles: String = by_tile
            .iter()
            .map(|(tile, count)| format!("{} ({})", tile.trim_start_matches("тайл "), count))
            .collect::<Vec<_>>()
            .join(", ");
        let path_text = a.path.clone();

        let mut item0 = slint::StandardListViewItem::default();
        item0.text = path_text.as_str().into();
        let mut item1 = slint::StandardListViewItem::default();
        item1.text = a.uses.len().to_string().as_str().into();
        let mut item2 = slint::StandardListViewItem::default();
        item2.text = tiles.as_str().into();
        let row_items = vec![item0, item1, item2];
        let row: slint::ModelRc<slint::StandardListViewItem> =
            slint::ModelRc::new(slint::VecModel::from(row_items));
        rows.push(row);
        safe_flags.push(crate::assets::is_safe_for_global_replacement(
            &a.path, &a.uses,
        ));
    }
    (rows, safe_flags)
}

static APP_STATE: OnceLock<Arc<Mutex<app::CheckerApp>>> = OnceLock::new();

fn app_state() -> Arc<Mutex<app::CheckerApp>> {
    APP_STATE.get().cloned().expect("app state not initialized")
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

fn main() {
    handle_cli_args();
    // Handle file association launch: TS-Checker opened with a .bin/.GeoPcDx file.
    let app = Arc::new(Mutex::new(app::CheckerApp::default()));
    let _ = APP_STATE.set(app.clone());
    let ui = MainWindow::new().unwrap();
    crate::settings::apply_saved_language(&crate::settings::load_language());
    let ui_handle = ui.as_weak();
    if let Ok(mut app) = app.lock() {
        app.start_route_scan();
        ui.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
    }
    let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().unwrap()));
    let mut last_count: usize = 0;
    let mut last_query = String::new();
    let mut last_thumbnails_enabled: bool = false;
    let mut last_status = String::new();
    let mut last_check_log = String::new();
    let missing_win = MissingAssetsWindow::new().unwrap();
    let missing_handle = missing_win.as_weak();
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
        let app = app_state().clone();
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
        let app = app_state().clone();
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
        let app = app_state().clone();
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
        let app = app_state().clone();
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

    missing_win.on_replace_missing(move |idx| {
        let (missing, rw_path) = {
            if let Ok(app) = app_state().lock() {
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
                if let Ok(mut app) = app_state().lock() {
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
                } else if let Ok(mut app) = app_state().lock() {
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
    let timer = slint::Timer::default();
    let ui_handle_timer = ui_handle.clone();
    let inv_handle_timer_slot = Arc::new(Mutex::new(None::<slint::Weak<AssetInventoryWindow>>));
    let inv_handle_timer_slot_clone = inv_handle_timer_slot.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(50),
        move || {
            let ui = match ui_handle_timer.upgrade() {
                Some(ui) => ui,
                None => return,
            };
            if let Ok(mut app) = app_state().lock() {
                app.receive_background_results();
                if app.inventory_refresh_pending {
                    let inv_weak = inv_handle_timer_slot_clone
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    if let Some(inv) = inv_weak.and_then(|w| w.upgrade()) {
                        let show_missing = inv.get_show_missing_only();
                        let (rows, safe) = if let Ok(state) = app.asset_inventory_state.lock() {
                            build_inventory_rows(&state, show_missing)
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                        app.inventory_refresh_pending = false;
                    }
                }
                // Show missing-assets popup after route check finishes
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
                    let counts: Vec<i32> =
                        missing_assets.iter().map(|m| m.uses.len() as i32).collect();
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
                            !crate::assets::is_protected_network_asset(
                                &crate::assets::RouteAssetItem {
                                    path: m.path.clone(),
                                    uses: m.uses.clone(),
                                },
                            )
                        })
                        .collect();
                    if let Some(mw) = missing_handle.upgrade() {
                        mw.set_missing_assets(slint::ModelRc::new(slint::VecModel::from(paths)));
                        mw.set_missing_counts(slint::ModelRc::new(slint::VecModel::from(counts)));
                        mw.set_missing_suggested_repairs(slint::ModelRc::new(
                            slint::VecModel::from(repairs),
                        ));
                        mw.set_missing_corrupted(slint::ModelRc::new(slint::VecModel::from(
                            corrupted,
                        )));
                        mw.set_missing_asset_types(slint::ModelRc::new(slint::VecModel::from(
                            types,
                        )));
                        mw.set_missing_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                        mw.set_active_tile_list(slint::ModelRc::new(slint::VecModel::<
                            slint::SharedString,
                        >::default(
                        )));
                        mw.set_active_tile_asset("".into());
                        let _ = mw.show();
                        app.missing_assets_shown = true;
                    }
                }
                // Sync asset editor text when document loaded
                let q = ui.get_search_query().to_string();
                app.search_query = q.clone();
                if app.routes.len() == last_count
                    && q == last_query
                    && app.route_thumbnails_enabled == last_thumbnails_enabled
                {
                    if app.status != last_status {
                        last_status = app.status.clone();
                        ui.set_status(last_status.clone().into());
                    }
                    if app.check_log != last_check_log {
                        last_check_log = app.check_log.clone();
                        ui.set_check_log(last_check_log.clone().into());
                    }
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
                    ui.set_is_checking(app.is_checking());
                    if app.is_checking() {
                        ui.set_show_log(true);
                    }
                    return;
                }
                last_count = app.routes.len();
                last_query = q.clone();
                last_thumbnails_enabled = app.route_thumbnails_enabled;
                let query = q.to_lowercase();
                let items: Vec<RouteItem> = app
                    .routes
                    .iter()
                    .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
                    .map(|r| {
                        let icon = if app.route_thumbnails_enabled {
                            match r.icon_path.clone() {
                                Some(p) => load_route_icon_image(&p),
                                None => slint::Image::default(),
                            }
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
                if app.status != last_status {
                    last_status = app.status.clone();
                    ui.set_status(last_status.clone().into());
                }
                if app.check_log != last_check_log {
                    last_check_log = app.check_log.clone();
                    ui.set_check_log(last_check_log.clone().into());
                }
                ui.set_has_result(app.result.is_some());
                ui.set_missing_assets_count(
                    app.result
                        .as_ref()
                        .map(|r| r.missing_assets.len() as i32)
                        .unwrap_or(0),
                );
            }
        },
    );
    let ui_handle2 = ui.as_weak();
    ui.on_route_selected(move |idx| {
        if let Some(ui) = ui_handle2.upgrade() {
            ui.set_selected_route(idx);
        }
        if let Ok(mut app) = app_state().lock() {
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
    ui.on_start_check(move || {
        if let Some(ui) = ui_handle_start_check.upgrade() {
            ui.set_show_log(true);
        }
        if let Ok(mut app) = app_state().lock() {
            app.start_check();
        }
    });
    let ui_handle_reset = ui_handle;
    ui.on_reset_log(move || {
        if let Some(ui) = ui_handle_reset.upgrade()
            && let Ok(app) = app_state().lock()
        {
            ui.set_check_log(app.check_log.clone().into());
        }
    });
    let inventory_win = AssetInventoryWindow::new().unwrap();
    let inv_handle = inventory_win.as_weak();
    let inv_handle2 = inventory_win.as_weak();
    let inv_handle3 = inventory_win.as_weak();
    let inv_handle4 = inventory_win.as_weak();
    let inv_handle5 = inventory_win.as_weak();
    let inv_handle9 = inventory_win.as_weak();
    let inv_handle10 = inventory_win.as_weak();
    let inv_handle11 = inventory_win.as_weak();
    if let Ok(mut slot) = inv_handle_timer_slot.lock() {
        *slot = Some(inventory_win.as_weak());
    }
    let settings_win = SettingsWindow::new().unwrap();
    let settings_handle = settings_win.as_weak();
    let settings_handle2 = settings_win.as_weak();
    let settings_handle3 = settings_win.as_weak();
    let settings_handle4 = settings_win.as_weak();
    let settings_handle5 = settings_win.as_weak();
    if let Ok(app) = app_state().lock() {
        let available = settings::maximum_serz_processes();
        let all_presets: [usize; 6] = [1, 6, 8, 12, 16, 32];
        let presets: Vec<slint::SharedString> = all_presets
            .iter()
            .filter(|p| **p <= available)
            .map(|p| p.to_string().into())
            .collect();
        let mut current = app.serz_process_limit.clamp(1, available);
        if !presets.iter().any(|p| *p == current.to_string()) {
            current = presets
                .iter()
                .filter(|p| p.parse::<usize>().map(|v| v <= current).unwrap_or(false))
                .map(|p| p.parse::<usize>().unwrap_or(1))
                .max()
                .unwrap_or(1);
        }
        let index = presets
            .iter()
            .position(|p| *p == current.to_string())
            .unwrap_or(0);
        settings_win.set_serz_process_limit(current as i32);
        settings_win.set_serz_process_limit_max(available as i32);
        settings_win.set_serz_presets(slint::ModelRc::new(slint::VecModel::from(presets)));
        settings_win.set_serz_process_limit_index(index as i32);
        settings_win.set_locoswap_path(app.locoswap_path.clone().into());
        settings_win.set_route_thumbnails_enabled(app.route_thumbnails_enabled);
        settings_win.set_route_scan_on_hdd(app.route_scan_on_hdd);
        settings_win.set_hide_quick_drive_free_roam(app.hide_quick_drive_free_roam);
        settings_win.set_game_path(app.rw_path.clone().into());
        let langs = ["ru", "en", "de"];
        settings_win.set_language_options(slint::ModelRc::new(slint::VecModel::from(
            langs
                .iter()
                .map(|s| (*s).to_string().into())
                .collect::<Vec<slint::SharedString>>(),
        )));
        let lang_idx = langs.iter().position(|&l| l == app.language).unwrap_or(0);
        settings_win.set_language_index(lang_idx as i32);
    }
    settings_win.on_pick_locoswap(move || {
        if let Some(sw) = settings_handle.upgrade() {
            let current = sw.get_locoswap_path().to_string();
            let mut dialog = rfd::FileDialog::new().set_title("Выберите LocoSwap.exe");
            if !current.is_empty() {
                let p = PathBuf::from(&current);
                if let Some(parent) = p.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_file() {
                sw.set_locoswap_path(path.to_string_lossy().to_string().into());
            }
        }
    });
    settings_win.on_pick_game(move || {
        if let Some(sw) = settings_handle3.upgrade() {
            let current = sw.get_game_path().to_string();
            let mut dialog = rfd::FileDialog::new().set_title("Выберите папку с игрой RailWorks");
            if !current.is_empty() {
                let p = PathBuf::from(&current);
                if p.is_dir() {
                    dialog = dialog.set_directory(&p);
                } else if let Some(parent) = p.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_folder() {
                sw.set_game_path(path.to_string_lossy().to_string().into());
            }
        }
    });
    settings_win.on_language_selected(move |lang| {
        let lang = lang.to_string();
        if let Ok(mut app) = app_state().lock() {
            app.language = lang.clone();
        }
        crate::settings::apply_saved_language(&lang);
        let _ = crate::settings::save_language(&lang);
        if let Some(sw) = settings_handle5.upgrade() {
            let langs = ["ru", "en", "de"];
            let idx = langs.iter().position(|&l| l == lang).unwrap_or(0);
            sw.set_language_index(idx as i32);
        }
    });
    settings_win.on_serz_preset_selected(move |i| {
        if let Some(sw) = settings_handle4.upgrade() {
            let presets = sw.get_serz_presets();
            if let Some(item) = presets.row_data(i as usize)
                && let Ok(limit) = item.to_string().parse::<usize>()
            {
                sw.set_serz_process_limit(limit as i32);
                if let Ok(mut app) = app_state().lock() {
                    app.serz_process_limit = limit;
                }
            }
        }
    });
    settings_win.on_save_settings(move || {
        if let Some(sw) = settings_handle2.upgrade() {
            let limit = sw.get_serz_process_limit() as usize;
            let path = sw.get_locoswap_path().to_string();
            let game_path = sw.get_game_path().to_string();
            let thumbs = sw.get_route_thumbnails_enabled();
            let hdd = sw.get_route_scan_on_hdd();
            let hide_scenarios = sw.get_hide_quick_drive_free_roam();
            let lang_idx = sw.get_language_index() as usize;
            let langs = ["ru", "en", "de"];
            let language = langs.get(lang_idx).copied().unwrap_or("ru").to_owned();
            if let Ok(mut app) = app_state().lock() {
                app.serz_process_limit = limit;
                app.locoswap_path = path.clone();
                app.rw_path = game_path.clone();
                app.route_thumbnails_enabled = thumbs;
                app.route_scan_on_hdd = hdd;
                app.hide_quick_drive_free_roam = hide_scenarios;
                app.language = language.clone();
                let _ = crate::settings::save_serz_process_limit(limit);
                let _ = crate::settings::save_locoswap_path(&path);
                let _ = crate::settings::save_last_game_path(&game_path);
                let _ = crate::settings::save_route_thumbnail_setting(thumbs);
                let _ = crate::settings::save_route_scan_on_hdd(hdd);
                let _ = crate::settings::save_hide_quick_drive_free_roam(hide_scenarios);
                let _ = crate::settings::save_language(&language);
                crate::settings::apply_saved_language(&language);
            }
        }
    });
    let scenario_win = ScenarioWindow::new().unwrap();
    let scenario_handle = scenario_win.as_weak();
    let scenario_handle2 = scenario_win.as_weak();
    let ui_handle3 = ui.as_weak();
    ui.on_scenario_selected(move |idx| {
        let app = app_state().clone();
        let ui_handle = ui_handle3.clone();
        let scenario_handle = scenario_handle.clone();
        std::thread::spawn(move || {
            // 1. Быстро забираем данные под замком, затем сразу отпускаем его.
            let (scen_idx, scen_items, scenario, rw_path, route_path, consist_names, consist_player, need_info) = {
                let mut app = match app.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                app.refresh_scenarios();
                let hide = app.hide_quick_drive_free_roam;
                let filtered_indices: Vec<usize> = app
                    .scenarios
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type)
                    })
                    .map(|(i, _)| i)
                    .collect();
                let ui_idx = idx as usize;
                let Some(&scen_idx) = filtered_indices.get(ui_idx) else {
                    return;
                };
                if scen_idx >= app.scenarios.len() {
                    return;
                }
                crate::scenario::load_scenario_metadata(&mut app.scenarios[scen_idx]);

                let scen_items: Vec<ScenarioItem> = app
                    .scenarios
                    .iter()
                    .filter(|s| !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type))
                    .map(|s| ScenarioItem {
                        name: s.name.clone().into(),
                        scenario_type: s.scenario_type.clone().into(),
                    })
                    .collect();

                let needs_load = app.scenarios[scen_idx].consists.is_none();
                if needs_load {
                    let scenario_path = app.scenarios[scen_idx].path.clone();
                    let serz_path =
                        crate::assets::preferred_serz_path(std::path::Path::new(app.rw_path.trim()));
                    let consists = crate::scenario::load_consists(&scenario_path, &serz_path);
                    app.scenarios[scen_idx].consists = Some(consists);
                }

                let scenario = app.scenarios[scen_idx].clone();
                let rw_path = app.rw_path.clone();
                let route_path = app
                    .selected_route
                    .and_then(|i| app.routes.get(i))
                    .map(|r| r.path.clone())
                    .unwrap_or_default();

                let consist_names: Vec<slint::SharedString> = scenario
                    .consists
                    .as_ref()
                    .map(|c| c.iter().map(|x| x.name.clone().into()).collect())
                    .unwrap_or_default();
                let consist_player: Vec<bool> = scenario
                    .consists
                    .as_ref()
                    .map(|c| c.iter().map(|x| x.is_player).collect())
                    .unwrap_or_default();

                let need_info = app.scenario_consist_info.is_none()
                    || app
                        .scenario_consist_info
                        .as_ref()
                        .map(|v| v.len())
                        .unwrap_or(0)
                        != consist_names.len();

                (scen_idx, scen_items, scenario, rw_path, route_path, consist_names, consist_player, need_info)
            };

            // 2. Тяжелая работа БЕЗ удержания глобального мьютекса.
            let (computed_info, computed_vf) = if need_info {
                let mut info = Vec::new();
                let mut vf = Vec::new();
                if let Some(consists) = scenario.consists.as_ref() {
                    let (i, v) =
                        crate::scenario::analyze_consists_parallel(consists, &rw_path);
                    info = i;
                    vf = v;
                }
                (Some(info), Some(vf))
            } else {
                (None, None)
            };

            let serz_path = crate::assets::preferred_serz_path(std::path::Path::new(rw_path.trim()));
            let weather_name = crate::scenario::weather_display_name(
                std::path::Path::new(rw_path.trim()),
                std::path::Path::new(&route_path),
                &serz_path,
                &scenario.weather,
            );

            // 3. Быстро сохраняем результат под замком.
            let (vehicle_files, consist_missing, consist_complete) = {
                let mut app = match app.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if need_info {
                    if let (Some(info), Some(vf)) = (computed_info, computed_vf) {
                        app.scenario_consist_info = Some(info);
                        app.scenario_vehicle_files = Some(vf);
                    }
                }
                let vehicle_files = app.scenario_vehicle_files.clone().unwrap_or_default();
                app.selected_scenario = Some(scen_idx);
                app.selected_consist = Some(0);
                app.scenario_window_open = true;

                let consist_missing: Vec<bool> = app
                    .scenario_consist_info
                    .as_ref()
                    .map(|info| info.iter().map(|(_, ok)| !*ok).collect())
                    .unwrap_or_default();
                let consist_complete: Vec<bool> = app
                    .scenario_consist_info
                    .as_ref()
                    .map(|info| info.iter().map(|(_, ok)| *ok).collect())
                    .unwrap_or_default();

                (vehicle_files, consist_missing, consist_complete)
            };

            let vehicles: Vec<slint::SharedString> = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .map(|cc| {
                    cc.vehicles
                        .iter()
                        .map(|v| v.name.to_string().into())
                        .collect()
                })
                .unwrap_or_default();
            let missing: Vec<bool> = vehicle_files.first().cloned().unwrap_or_default();
            let paths: Vec<slint::SharedString> = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .map(|cc| {
                    cc.vehicles
                        .iter()
                        .map(|v| v.blueprint.clone().into())
                        .collect()
                })
                .unwrap_or_default();

            let selected_vehicle_info = if let Some(v) = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .and_then(|cc| cc.vehicles.first())
            {
                format!(
                    "Поставщик: {}\nДополнение: {}\nФайл: {}\nНомер: {}",
                    v.provider,
                    v.product,
                    v.blueprint,
                    v.number.clone().unwrap_or_else(|| "—".to_owned())
                )
            } else {
                "Выберите единицу техники".to_owned()
            };

            let scenario_name = scenario.name.clone();
            let scenario_title = format!(
                "Сценарий: {}",
                if scenario.name.trim().is_empty() {
                    "Без названия"
                } else {
                    scenario.name.as_str()
                }
            );
            let fallback = |value: &str| -> String {
                if value.trim().is_empty() {
                    "Не указано".to_owned()
                } else {
                    value.to_owned()
                }
            };
            let scenario_author = fallback(&scenario.author);
            let scenario_season = fallback(&scenario.season);
            let scenario_start_time = fallback(&scenario.start_time);
            let scenario_weather = fallback(&weather_name);

            let ui_selected_scenario = idx as i32;

            let result = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_handle.upgrade() else {
                    eprintln!("scenario UI handle dropped before update");
                    return;
                };
                ui.set_scenario_items(slint::ModelRc::from(scen_items.as_slice()));
                ui.set_selected_scenario(ui_selected_scenario);

                let Some(sw) = scenario_handle.upgrade() else {
                    eprintln!("scenario window handle dropped before update");
                    return;
                };
                sw.set_scenario_name(scenario_name.into());
                sw.set_scenario_title(scenario_title.into());
                sw.set_scenario_author(scenario_author.into());
                sw.set_scenario_season(scenario_season.into());
                sw.set_scenario_start_time(scenario_start_time.into());
                sw.set_scenario_weather(scenario_weather.into());
                sw.set_consist_names(slint::ModelRc::from(consist_names.as_slice()));
                sw.set_consist_player(slint::ModelRc::from(consist_player.as_slice()));
                sw.set_consist_missing(slint::ModelRc::from(consist_missing.as_slice()));
                sw.set_consist_complete(slint::ModelRc::from(consist_complete.as_slice()));
                sw.set_vehicle_names(slint::ModelRc::from(vehicles.as_slice()));
                sw.set_vehicle_missing(slint::ModelRc::from(missing.as_slice()));
                sw.set_vehicle_paths(slint::ModelRc::from(paths.as_slice()));
                sw.set_selected_vehicle_info(selected_vehicle_info.into());
                sw.set_selected_consist(0);
                if let Err(err) = sw.show() {
                    eprintln!("failed to show scenario window: {err}");
                }
            });
            if let Err(err) = result {
                eprintln!("failed to invoke scenario UI update: {err}");
            }
        });
    });

    scenario_win.on_consist_selected(move |idx| {
        if let Ok(mut app) = app_state().lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let i = idx as usize;
            if i >= consists.len() {
                return;
            }
            let c = &consists[i];
            let vehicles: Vec<slint::SharedString> = c
                .vehicles
                .iter()
                .map(|v| format!("{} - {}", v.provider, v.name).into())
                .collect();
            let missing: Vec<bool> = app
                .scenario_vehicle_files
                .as_ref()
                .and_then(|vf| vf.get(i))
                .cloned()
                .unwrap_or_default();
            let paths: Vec<slint::SharedString> = c
                .vehicles
                .iter()
                .map(|v| v.blueprint.clone().into())
                .collect();
            app.selected_consist = Some(i);
            if let Some(sw) = scenario_handle2.upgrade() {
                sw.set_vehicle_names(slint::ModelRc::new(slint::VecModel::from(vehicles)));
                sw.set_vehicle_missing(slint::ModelRc::new(slint::VecModel::from(missing)));
                sw.set_vehicle_paths(slint::ModelRc::new(slint::VecModel::from(paths)));
                sw.set_selected_consist(idx);
            }
        }
    });
    let ui_handle_close_scenario = ui.as_weak();
    scenario_win.on_open_scenario_folder(move || {
        if let Ok(app) = app_state().lock()
            && let Some(idx) = app.selected_scenario
            && let Some(scenario) = app.scenarios.get(idx)
        {
            let _ = windows::open_in_file_explorer(&scenario.path);
        }
    });
    let scenario_handle_close = scenario_win.as_weak();
    scenario_win.on_close_scenario(move || {
        if let Some(sw) = scenario_handle_close.upgrade() {
            let _ = sw.hide();
        }
        let ui_handle = ui_handle_close_scenario.clone();
        if let Ok(mut app) = app_state().lock() {
            app.scenario_window_open = false;
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
            let selected_ui = app.selected_scenario.and_then(|si| {
                app.scenarios
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type)
                    })
                    .position(|(i, _)| i == si)
            });
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_scenario_items(slint::ModelRc::new(slint::VecModel::from(scen)));
                ui.set_selected_scenario(selected_ui.map(|i| i as i32).unwrap_or(-1));
            }
        }
    });
    scenario_win.on_open_locoswap(move || {
        if let Ok(app) = app_state().lock() {
            let path = app.locoswap_path.clone();
            if !path.is_empty() && Path::new(&path).is_file() {
                let mut cmd = std::process::Command::new(&path);
                if let Some(parent) = Path::new(&path).parent() {
                    cmd.current_dir(parent);
                }
                if let Some(idx) = app.selected_scenario
                    && let Some(scenario) = app.scenarios.get(idx)
                {
                    cmd.arg(&scenario.path);
                }
                let _ = cmd.spawn();
            }
        }
    });
    scenario_win.on_open_locoswap_consist(move |idx| {
        if let Ok(app) = app_state().lock() {
            let locoswap_path = app.locoswap_path.clone();
            if locoswap_path.is_empty() || !Path::new(&locoswap_path).is_file() {
                return;
            }
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consist_index = scenario
                .consists
                .as_ref()
                .and_then(|consists| consists.get(idx as usize))
                .map(|consist| consist.index)
                .unwrap_or(idx as usize);
            let mut cmd = std::process::Command::new(&locoswap_path);
            if let Some(parent) = Path::new(&locoswap_path).parent() {
                cmd.current_dir(parent);
            }
            let _ = cmd
                .arg(&scenario.path)
                .arg("--consist")
                .arg(consist_index.to_string())
                .arg("--vehicle")
                .arg("-1")
                .spawn();
        }
    });
    let scenario_handle_preview = scenario_win.as_weak();
    scenario_win.on_vehicle_selected(move |idx| {
        if let Ok(mut app) = app_state().lock() {
            app.selected_vehicle = Some(idx as usize);
            let rw_path = PathBuf::from(app.rw_path.trim());
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consists
                .get(consist_idx)
                .and_then(|c| c.vehicles.get(idx as usize))
            {
                Some(v) => v.clone(),
                None => return,
            };
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                &rw_path,
                &route_path,
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            let exists = crate::blueprint::vehicle_blueprint_exists(
                &rw_path,
                &vehicle.provider,
                &vehicle.product,
                &vehicle.blueprint,
            );
            let path_text = if exists {
                if vehicle.provider.trim().is_empty() || vehicle.product.trim().is_empty() {
                    vehicle.blueprint.clone()
                } else {
                    format!(
                        "Assets\\{}\\{}\\{}",
                        vehicle.provider.trim(),
                        vehicle.product.trim(),
                        vehicle.blueprint.trim().replace('/', "\\")
                    )
                }
            } else {
                String::new()
            };
            let image =
                load_vehicle_preview_image(&rw_path, &route_path, &vehicle, resolved.as_ref());
            if let Some(sw) = scenario_handle_preview.upgrade() {
                if let Some(idx) = path_text.find("Assets") {
                    sw.set_selected_vehicle_path(path_text[idx..].into());
                } else {
                    sw.set_selected_vehicle_path(path_text.into());
                }
                sw.set_selected_vehicle_image(image);
                let details = format!(
                    "Поставщик: {}\nДополнение: {}\nФайл: {}\nНомер: {}",
                    vehicle.provider,
                    vehicle.product,
                    vehicle.blueprint,
                    vehicle.number.clone().unwrap_or_else(|| "—".to_owned())
                );
                sw.set_selected_vehicle_info(details.into());
            }
        }
    });
    scenario_win.on_open_selected_vehicle_path(move || {
        if let Ok(app) = app_state().lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let vehicle_idx = match app.selected_vehicle {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let consist = match consists.get(consist_idx) {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consist.vehicles.get(vehicle_idx) {
                Some(v) => v.clone(),
                None => return,
            };
            let rw_path = app.rw_path.clone();
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                Path::new(rw_path.trim()),
                Path::new(&route_path),
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            if let Some(path) = resolved {
                if path.is_file() {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg("/select,")
                        .arg(path.as_os_str())
                        .spawn();
                } else {
                    let mut cur = path.as_path();
                    while !cur.exists() {
                        match cur.parent() {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                    let _ = crate::windows::open_in_file_explorer(cur);
                }
            }
        }
    });
    scenario_win.on_open_vehicle_path(move |idx| {
        if let Ok(app) = app_state().lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s,
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let consist = match consists.get(consist_idx) {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consist.vehicles.get(idx as usize) {
                Some(v) => v,
                None => return,
            };
            let rw_path = app.rw_path.clone();
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                Path::new(rw_path.trim()),
                Path::new(&route_path),
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            if let Some(path) = resolved {
                if path.is_file() {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg("/select,")
                        .arg(path.as_os_str())
                        .spawn();
                } else {
                    let mut cur = path.as_path();
                    while !cur.exists() {
                        match cur.parent() {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                    let _ = crate::windows::open_in_file_explorer(cur);
                }
            } else {
                // Если блюпринт не найден, ничего не открываем.
            }
        }
    });
    ui.on_open_route_folder(move || {
        if let Ok(app) = app_state().lock()
            && let Some(idx) = app.selected_route
            && let Some(route) = app.routes.get(idx)
        {
            let _ = windows::open_in_file_explorer(&route.path);
        }
    });
    inventory_win.on_toggle_missing_only(move || {
        if let Some(inv) = inv_handle2.upgrade() {
            let cur = inv.get_show_missing_only();
            inv.set_show_missing_only(!cur);
            let show_missing = !cur;
            if let Ok(app) = app_state().lock()
                && let Ok(state) = app.asset_inventory_state.lock()
            {
                let (rows, safe) = build_inventory_rows(&state, show_missing);
                inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
            }
        }
    });
    inventory_win.on_sort_ascending(move |column| {
        if let Some(inv) = inv_handle3.upgrade() {
            let show_missing = inv.get_show_missing_only();
            if let Ok(app) = app_state().lock() {
                if let Ok(mut state) = app.asset_inventory_state.lock() {
                    match column {
                        0 => {
                            state.sort.column = inventory_ui::InventorySortColumn::Path;
                            state.sort.descending = false;
                        }
                        1 => {
                            state.sort.column = inventory_ui::InventorySortColumn::Usage;
                            state.sort.descending = false;
                        }
                        _ => {}
                    }
                }
                if let Ok(state) = app.asset_inventory_state.lock() {
                    let (rows, safe) = build_inventory_rows(&state, show_missing);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                }
            }
        }
    });
    inventory_win.on_sort_descending(move |column| {
        if let Some(inv) = inv_handle4.upgrade() {
            let show_missing = inv.get_show_missing_only();
            if let Ok(app) = app_state().lock() {
                if let Ok(mut state) = app.asset_inventory_state.lock() {
                    match column {
                        0 => {
                            state.sort.column = inventory_ui::InventorySortColumn::Path;
                            state.sort.descending = true;
                        }
                        1 => {
                            state.sort.column = inventory_ui::InventorySortColumn::Usage;
                            state.sort.descending = true;
                        }
                        _ => {}
                    }
                }
                if let Ok(state) = app.asset_inventory_state.lock() {
                    let (rows, safe) = build_inventory_rows(&state, show_missing);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                }
            }
        }
    });
    inventory_win.on_search_changed(move |text| {
        if let Some(inv) = inv_handle5.upgrade() {
            let show_missing = inv.get_show_missing_only();
            if let Ok(app) = app_state().lock() {
                if let Ok(mut state) = app.asset_inventory_state.lock() {
                    state.search = text.to_string();
                }
                if let Ok(state) = app.asset_inventory_state.lock() {
                    let (rows, safe) = build_inventory_rows(&state, show_missing);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                }
            }
        }
    });
    let mut inv_last_row: i32 = -1;
    let mut inv_last_click = Instant::now();
    const INV_DOUBLE_CLICK_MS: u64 = 400;
    inventory_win.on_row_selected(move |row| {
        if let Some(inv) = inv_handle9.upgrade() {
            inv.set_selected_asset(row);
        }
        let now = Instant::now();
        let is_double = row == inv_last_row
            && now.saturating_duration_since(inv_last_click)
                <= Duration::from_millis(INV_DOUBLE_CLICK_MS);
        inv_last_row = row;
        inv_last_click = now;
        if !is_double || row < 0 {
            return;
        }
        if let Some(inv) = inv_handle9.upgrade() {
            let rows = inv.get_inventory_rows();
            if (row as usize) >= rows.row_count() {
                return;
            }
            let first_cell = rows.row_data(row as usize).and_then(|rm| rm.row_data(0));
            let path_text = first_cell.map(|c| c.text.to_string()).unwrap_or_default();
            if path_text.is_empty() {
                return;
            }
            let rw_path = if let Ok(app) = app_state().lock() {
                app.rw_path.clone()
            } else {
                String::new()
            };
            if rw_path.is_empty() {
                return;
            }
            let full = PathBuf::from(rw_path.trim())
                .join("Assets")
                .join(path_text.replace('/', "\\"));
            // Open nearest existing folder; select file if present.
            let target = if full.exists() {
                // open containing folder with the file selected
                let _ = std::process::Command::new("explorer.exe")
                    .arg("/select,")
                    .arg(full.as_os_str())
                    .spawn();
                return;
            } else {
                // walk up to nearest existing parent
                let mut cur = full.as_path();
                while !cur.exists() {
                    match cur.parent() {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                cur.to_path_buf()
            };
            let _ = windows::open_in_file_explorer(&target);
        }
    });
    inventory_win.on_open_in_editor(move || {
        if let Some(inv) = inv_handle10.upgrade() {
            let row = inv.get_current_row();
            if row < 0 {
                return;
            }
            let rows = inv.get_inventory_rows();
            if (row as usize) >= rows.row_count() {
                return;
            }
            let first_cell = rows.row_data(row as usize).and_then(|rm| rm.row_data(0));
            let path_text = first_cell.map(|c| c.text.to_string()).unwrap_or_default();
            if path_text.is_empty() {
                return;
            }
            let rw_path = if let Ok(app) = app_state().lock() {
                app.rw_path.clone()
            } else {
                String::new()
            };
            if rw_path.is_empty() {
                return;
            }
            let full = PathBuf::from(rw_path.trim())
                .join("Assets")
                .join(path_text.replace('/', "\\"));
            if full.is_file() {
                let serz_path = crate::assets::find_serz_path(&full)
                    .unwrap_or_else(|| PathBuf::from("serz64.exe"));
                let app_for_status = app_state().clone();
                let _ = crate::external_editor::open_and_sync_with_vscode(
                    &full,
                    &serz_path,
                    move |result| {
                        if let Ok(mut app) = app_for_status.lock() {
                            app.status = result.unwrap_or_else(|error| error);
                        }
                    },
                );
            }
        }
    });
    inventory_win.on_replace_inventory_asset(move |selected| {
        if selected < 0 {
            return;
        }
        let Some(inv) = inv_handle11.upgrade() else {
            return;
        };
        let rows = inv.get_inventory_rows();
        if (selected as usize) >= rows.row_count() {
            return;
        }
        let first_cell = rows
            .row_data(selected as usize)
            .and_then(|rm| rm.row_data(0));
        let asset_path = first_cell.map(|c| c.text.to_string()).unwrap_or_default();
        if asset_path.is_empty() {
            return;
        }
        let rw_path = if let Ok(app) = app_state().lock() {
            app.rw_path.clone()
        } else {
            return;
        };
        if rw_path.trim().is_empty() {
            return;
        }
        let asset = if let Ok(app) = app_state().lock() {
            if let Ok(state) = app.asset_inventory_state.lock() {
                state.assets.iter().find(|a| a.path == asset_path).cloned()
            } else {
                None
            }
        } else {
            None
        };
        let Some(asset) = asset else {
            return;
        };
        if !crate::assets::is_safe_for_global_replacement(&asset.path, &asset.uses) {
            if let Ok(mut app) = app_state().lock() {
                app.status = "Замена этого ассета запрещена (путевая инфраструктура).".to_owned();
            }
            return;
        }

        let assets_dir = PathBuf::from(rw_path.trim()).join("Assets");
        let mut dialog = rfd::FileDialog::new()
            .set_title("Заменить в маршруте...")
            .set_directory(&assets_dir);
        dialog = dialog.add_filter("BIN", &["bin"]);
        let Some(picked) = dialog.pick_file() else {
            return;
        };
        let replacement_relative = picked
            .strip_prefix(&assets_dir)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| picked.to_string_lossy().replace('\\', "/"));

        let asset_type = crate::assets::classify_route_asset_type(&asset.path, &asset.uses);
        let uses: Vec<_> = asset
            .uses
            .into_iter()
            .filter(|u| {
                let p = u.source_file.display().to_lowercase();
                !p.contains("tracks.bin")
                    && !p.contains("/networks/")
                    && !p.contains("\\networks\\")
            })
            .collect();
        if uses.is_empty() {
            return;
        }
        let missing = domain::MissingAsset {
            path: asset.path,
            uses,
            suggested_repair: None,
            is_corrupted_link: false,
            asset_type,
        };
        if let Ok(mut app) = app_state().lock() {
            app.inventory_replacement_pending_check = true;
            app.asset_replacement_queue
                .push((missing, picked, replacement_relative));
            app.start_next_asset_replacement();
        }
    });
    inventory_win.on_scan_umlauts(move || {
        if let Ok(app) = app_state().lock() {
            let _ = app
                .inventory_action_tx
                .send(crate::app::InventoryAction::ScanUmlauts);
        }
    });
    let settings_handle3 = settings_win.as_weak();
    ui.on_open_settings(move || {
        if let Some(sw) = settings_handle3.upgrade() {
            let _ = sw.show();
        }
    });
    ui.on_show_inventory(move || {
        if let Ok(mut app) = app_state().lock() {
            app.open_asset_inventory();
            if let Some(inv) = inv_handle.upgrade() {
                if let Ok(state) = app.asset_inventory_state.lock() {
                    inv.set_show_missing_only(false);
                    inv.set_selected_asset(-1);
                    inv.set_search_query(state.search.clone().as_str().into());
                    let (rows, safe) = build_inventory_rows(&state, false);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    inv.set_search_query(state.search.clone().as_str().into());
                }
                inv.show().unwrap();
            }
        }
    });
    let auditor_win = FolderAuditorWindow::new().unwrap();
    let auditor_handle = auditor_win.as_weak();
    let auditor_handle2 = auditor_win.as_weak();
    let all_audit_items: Arc<Mutex<Vec<auditor::BrokenResourceRef>>> =
        Arc::new(Mutex::new(Vec::new()));
    let current_display_items: Arc<Mutex<Vec<auditor::BrokenResourceRef>>> =
        Arc::new(Mutex::new(Vec::new()));
    let audit_sort_col: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let audit_sort_asc: Arc<Mutex<bool>> = Arc::new(Mutex::new(true));
    let all_audit_items_for_start = all_audit_items.clone();
    let current_display_items_for_start = current_display_items.clone();
    let audit_sort_col_for_start = audit_sort_col.clone();
    let audit_sort_asc_for_start = audit_sort_asc.clone();
    let all_audit_items_for_filter = all_audit_items.clone();
    let current_display_items_for_filter = current_display_items.clone();
    let audit_sort_col_for_filter = audit_sort_col.clone();
    let audit_sort_asc_for_filter = audit_sort_asc.clone();
    let all_audit_items_for_sort = all_audit_items;
    let current_display_items_for_sort = current_display_items.clone();
    let audit_sort_col_for_sort = audit_sort_col;
    let audit_sort_asc_for_sort = audit_sort_asc;
    let current_display_items_for_open = current_display_items.clone();
    ui.on_open_folder_auditor(move || {
        if let Some(w) = auditor_handle.upgrade() {
            let _ = w.show();
        }
    });
    auditor_win.on_pick_folder(move || {
        if let Some(w) = auditor_handle2.upgrade() {
            let current = w.get_folder_path().to_string();
            let mut dialog = rfd::FileDialog::new().set_title("Выберите папку для аудита");
            if !current.is_empty() {
                let p = PathBuf::from(&current);
                if p.is_dir() {
                    dialog = dialog.set_directory(&p);
                }
            }
            if let Some(path) = dialog.pick_folder() {
                w.set_folder_path(path.to_string_lossy().to_string().into());
            }
        }
    });
    let auditor_weak_for_start = auditor_win.as_weak();
    auditor_win.on_start_audit(move |folder_path| {
        println!("[Auditor] start-audit callback: {}", folder_path);
        let window_weak = auditor_weak_for_start.clone();
        let path = PathBuf::from(folder_path.as_str());
        if !path.exists() || !path.is_dir() {
            eprintln!("[Auditor] Invalid folder path: {:?}", path);
            return;
        }
        let rw_path = app_state()
            .lock()
            .map(|a| a.rw_path.clone())
            .unwrap_or_default();
        let max_threads = app_state()
            .lock()
            .map(|a| a.serz_process_limit)
            .unwrap_or(8)
            .max(1);
        let serz_path = crate::assets::preferred_serz_path(Path::new(&rw_path));
        let assets_root = path
            .ancestors()
            .find(|p| {
                p.file_name()
                    .map(|name| name.to_string_lossy().eq_ignore_ascii_case("Assets"))
                    .unwrap_or(false)
            })
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| {
                path.parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| path.clone())
            });
        let ap_cache = auditor::ApArchiveCache::new();
        let path_for_thread = path;
        let window_weak_thread = window_weak;
        let all_store = all_audit_items_for_start.clone();
        let current_store = current_display_items_for_start.clone();
        let sort_col_store = audit_sort_col_for_start.clone();
        let sort_asc_store = audit_sort_asc_for_start.clone();
        let results_store = all_store;
        std::thread::spawn(move || {
            println!("[Auditor] Starting audit for: {:?}", path_for_thread);
            let progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync> = {
                let ww = window_weak_thread.clone();
                Arc::new(move |done, total| {
                    let ww = ww.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = ww.upgrade() {
                            w.set_audit_progress(done as f32 / total as f32);
                            w.set_audit_status(
                                format!("Сканирование: {} / {} файлов...", done, total).into(),
                            );
                        }
                    });
                })
            };
            let report = crate::auditor::audit_folder_with_progress(
                &path_for_thread,
                &assets_root,
                &serz_path,
                max_threads,
                &ap_cache,
                Some(progress_cb),
            );
            println!(
                "[Auditor] Audit done, broken: {}",
                report.broken_references.len()
            );
            if let Ok(mut store) = results_store.lock() {
                *store = report.broken_references.clone();
            }
            let col = *sort_col_store.lock().unwrap();
            let asc = *sort_asc_store.lock().unwrap();
            let filter = window_weak_thread
                .upgrade()
                .map(|w| w.get_active_filter().to_string())
                .unwrap_or_else(|| "all".to_string());
            let mut items: Vec<auditor::BrokenResourceRef> = report.broken_references;
            filter_items(&mut items, &filter);
            sort_items(&mut items, col, asc);
            if let Ok(mut store) = current_store.lock() {
                *store = items.clone();
            }
            let ww = window_weak_thread;
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww.upgrade() {
                    let mut rows: Vec<slint::ModelRc<slint::StandardListViewItem>> = Vec::new();
                    for b in &items {
                        let kind = match b.kind {
                            auditor::AssetResourceKind::Geometry => "Геометрия",
                            auditor::AssetResourceKind::Texture => "Текстура",
                            auditor::AssetResourceKind::Animation => "Анимация",
                            auditor::AssetResourceKind::Script => "Скрипт",
                            auditor::AssetResourceKind::Sound => "Звуки",
                            auditor::AssetResourceKind::ChildBlueprint => "Дочерние объекты",
                        };
                        let mut item0 = slint::StandardListViewItem::default();
                        item0.text = kind.into();
                        let mut item1 = slint::StandardListViewItem::default();
                        item1.text = b.expected_path.to_string_lossy().to_string().into();
                        let mut item2 = slint::StandardListViewItem::default();
                        item2.text = b.parent_file.to_string_lossy().to_string().into();
                        rows.push(slint::ModelRc::new(slint::VecModel::from(vec![
                            item0, item1, item2,
                        ])));
                    }
                    w.set_broken_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    w.set_audit_progress(1.0);
                    w.set_audit_status(format!("Готово: {} проблем", items.len()).into());
                    w.set_selected_row(-1);
                }
            })
            .ok();
        });
    });
    let auditor_weak_for_filter = auditor_win.as_weak();
    auditor_win.on_filter_changed(move |filter| {
        println!("[Auditor] filter-changed: {}", filter);
        let ww = auditor_weak_for_filter.clone();
        let all_store = all_audit_items_for_filter.clone();
        let current_store = current_display_items_for_filter.clone();
        let sort_col_store = audit_sort_col_for_filter.clone();
        let sort_asc_store = audit_sort_asc_for_filter.clone();
        let all_items = all_store.lock().map(|g| g.clone()).unwrap_or_default();
        let col = *sort_col_store.lock().unwrap();
        let asc = *sort_asc_store.lock().unwrap();
        let mut items = all_items;
        filter_items(&mut items, filter.as_str());
        sort_items(&mut items, col, asc);
        if let Ok(mut store) = current_store.lock() {
            *store = items.clone();
        }
        slint::invoke_from_event_loop(move || {
            if let Some(w) = ww.upgrade() {
                w.set_active_filter(filter.clone());
                let mut rows: Vec<slint::ModelRc<slint::StandardListViewItem>> = Vec::new();
                for b in &items {
                    let kind = match b.kind {
                        auditor::AssetResourceKind::Geometry => "Геометрия",
                        auditor::AssetResourceKind::Texture => "Текстура",
                        auditor::AssetResourceKind::Animation => "Анимация",
                        auditor::AssetResourceKind::Script => "Скрипт",
                        auditor::AssetResourceKind::Sound => "Звуки",
                        auditor::AssetResourceKind::ChildBlueprint => "Дочерние объекты",
                    };
                    let mut item0 = slint::StandardListViewItem::default();
                    item0.text = kind.into();
                    let mut item1 = slint::StandardListViewItem::default();
                    item1.text = b.expected_path.to_string_lossy().to_string().into();
                    let mut item2 = slint::StandardListViewItem::default();
                    item2.text = b.parent_file.to_string_lossy().to_string().into();
                    rows.push(slint::ModelRc::new(slint::VecModel::from(vec![
                        item0, item1, item2,
                    ])));
                }
                w.set_broken_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                w.set_selected_row(-1);
            }
        })
        .ok();
    });
    let auditor_weak_for_open = auditor_win.as_weak();
    let current_display_for_open = current_display_items_for_open.clone();
    auditor_win.on_open_source_file(move |row| {
        let items = current_display_for_open
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let target_path = items[row as usize].parent_file.clone();
        println!("[Auditor] Opening path: {:?}", target_path);
        let _ = windows::open_in_explorer(&target_path);
        let _ = auditor_weak_for_open;
    });
    let auditor_weak_for_missing = auditor_win.as_weak();
    let current_display_for_missing = current_display_items_for_open;
    auditor_win.on_open_missing_target_folder(move |row| {
        let items = current_display_for_missing
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let target_path = items[row as usize].expected_path.clone();
        println!("[Auditor] Opening path: {:?}", target_path);
        let _ = windows::open_in_explorer(&target_path);
        let _ = auditor_weak_for_missing;
    });
    let auditor_weak_for_sort = auditor_win.as_weak();
    let all_for_sort = all_audit_items_for_sort;
    let current_for_sort = current_display_items_for_sort;
    let sort_col_for_sort = audit_sort_col_for_sort;
    let sort_asc_for_sort = audit_sort_asc_for_sort;
    auditor_win.on_sort_column(move |col| {
        let col = col as usize;
        let mut c = sort_col_for_sort.lock().unwrap();
        let mut a = sort_asc_for_sort.lock().unwrap();
        if *c == col {
            *a = !*a;
        } else {
            *c = col;
            *a = true;
        }
        let col_now = *c;
        let asc_now = *a;
        drop(c);
        drop(a);
        let filter = auditor_weak_for_sort
            .upgrade()
            .map(|w| w.get_active_filter().to_string())
            .unwrap_or_else(|| "all".to_string());
        let all_items = all_for_sort.lock().map(|g| g.clone()).unwrap_or_default();
        let mut items = all_items;
        filter_items(&mut items, &filter);
        sort_items(&mut items, col_now, asc_now);
        if let Ok(mut store) = current_for_sort.lock() {
            *store = items.clone();
        }
        let ww = auditor_weak_for_sort.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(w) = ww.upgrade() {
                w.set_sort_col(col_now as i32);
                w.set_sort_ascending(asc_now);
                let mut rows: Vec<slint::ModelRc<slint::StandardListViewItem>> = Vec::new();
                for b in &items {
                    let kind = match b.kind {
                        auditor::AssetResourceKind::Geometry => "Геометрия",
                        auditor::AssetResourceKind::Texture => "Текстура",
                        auditor::AssetResourceKind::Animation => "Анимация",
                        auditor::AssetResourceKind::Script => "Скрипт",
                        auditor::AssetResourceKind::Sound => "Звуки",
                        auditor::AssetResourceKind::ChildBlueprint => "Дочерние объекты",
                    };
                    let mut item0 = slint::StandardListViewItem::default();
                    item0.text = kind.into();
                    let mut item1 = slint::StandardListViewItem::default();
                    item1.text = b.expected_path.to_string_lossy().to_string().into();
                    let mut item2 = slint::StandardListViewItem::default();
                    item2.text = b.parent_file.to_string_lossy().to_string().into();
                    rows.push(slint::ModelRc::new(slint::VecModel::from(vec![
                        item0, item1, item2,
                    ])));
                }
                w.set_broken_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                w.set_selected_row(-1);
            }
        })
        .ok();
    });
    let auditor_weak_for_edit = auditor_win.as_weak();
    let current_display_for_edit = current_display_items.clone();
    auditor_win.on_edit_in_vscode(move |row| {
        let items = current_display_for_edit
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let item = &items[row as usize];
        let asset_path = if item
            .parent_file
            .extension()
            .map(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("geopcdx"))
            .unwrap_or(false)
        {
            item.parent_file.clone()
        } else if item
            .expected_path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("geopcdx"))
            .unwrap_or(false)
        {
            item.expected_path.clone()
        } else {
            let w = auditor_weak_for_edit.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(win) = w.upgrade() {
                    win.set_editor_status("Выбранный элемент не является .bin/.GeoPcDx".into());
                }
            });
            return;
        };
        let rw_path = app_state()
            .lock()
            .map(|a| a.rw_path.clone())
            .unwrap_or_default();
        let serz_path = crate::assets::find_serz_path(&asset_path)
            .or_else(|| {
                let p = crate::assets::preferred_serz_path(Path::new(&rw_path));
                if p.is_file() { Some(p) } else { None }
            })
            .unwrap_or_else(|| PathBuf::from("serz.exe"));
        let weak = auditor_weak_for_edit.clone();
        std::thread::spawn(move || {
            let cb_weak = weak.clone();
            let callback = move |res: Result<String, String>| {
                let msg = match res {
                    Ok(s) => s,
                    Err(e) => format!("✗ {}", e),
                };
                let w = cb_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_editor_status(msg.into());
                    }
                });
            };
            if let Err(e) =
                crate::external_editor::open_and_sync_with_vscode(&asset_path, &serz_path, callback)
            {
                let w = weak.clone();
                let msg = format!("✗ {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_editor_status(msg.into());
                    }
                });
            }
        });
    });

    // Texture viewer integration
    let current_display_for_preview = current_display_items;
    let viewer_win = TextureViewerWindow::new().unwrap();
    let viewer_weak = viewer_win.as_weak();
    let tex_state: Arc<Mutex<Option<(u32, u32, Vec<u8>, PathBuf)>>> = Arc::new(Mutex::new(None));
    let tex_state_for_preview = tex_state.clone();
    let tex_state_for_toggle = tex_state.clone();
    let tex_state_for_export = tex_state.clone();
    let tex_state_for_replace = tex_state;
    auditor_win.on_preview_texture(move |row| {
        let items = current_display_for_preview
            .lock()
            .map(|g| g.clone())
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
            if let Ok(data) = std::fs::read(&path)
                && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
            {
                if let Ok(mut st) = tex_state.lock() {
                    *st = Some((w, h, rgba.clone(), path.clone()));
                }
                let info = format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display());
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
mod tests {
    use super::{
        ScenarioClueCollection, check_route, decode_xml_text, extract_asset_references,
        extract_scenario_clues, fallback_serz_path, inspect_route_folder,
        is_skipped_route_scan_directory, parse_route_name, preferred_serz_path, scan_routes,
        scan_routes_with_ignored, validate_serz_process,
    };
    use std::collections::HashSet;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    #[cfg(windows)]
    use std::path::Path;

    #[test]
    fn only_skips_scenarios_when_requested_during_asset_check() {
        assert!(!is_skipped_route_scan_directory("Scenarios", false));
        assert!(is_skipped_route_scan_directory("scenarios", true));
        assert!(!is_skipped_route_scan_directory("Networks", true));
    }

    #[test]
    fn route_check_only_collects_files_from_allowed_directories() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("route");
        for directory in [
            "Terrain",
            "Scenery",
            "Scenarios",
            "RouteInfo",
            "Networks",
            "MixMap",
            "ThirdParty",
        ] {
            let folder = route.join(directory);
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("asset.bin"), []).unwrap();
        }
        fs::write(route.join("RouteProperties.xml"), "<route />").unwrap();

        let included = super::assets::collect_route_files(&route, false).unwrap();
        let included_names: Vec<_> = included
            .iter()
            .map(|path| {
                path.strip_prefix(&route)
                    .unwrap()
                    .components()
                    .next()
                    .unwrap()
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        // Три папки маршрута и RouteProperties.xml в корне маршрута.
        assert_eq!(included.len(), 4);
        assert!(!included_names.iter().any(|name| name == "ThirdParty"));
        assert!(included.contains(&route.join("RouteProperties.xml")));

        let without_scenarios = super::assets::collect_route_files(&route, true).unwrap();
        assert_eq!(without_scenarios.len(), 3);
        assert_eq!(
            super::assets::unexpected_route_directories(&route).unwrap(),
            ["ThirdParty"]
        );
    }

    #[test]
    fn uses_serz32_as_fallback_for_serz64() {
        let temp = tempfile::tempdir().unwrap();
        let serz64 = temp.path().join("serz64.exe");
        let serz = temp.path().join("serz.exe");
        fs::write(&serz64, []).unwrap();
        assert_eq!(fallback_serz_path(&serz64), None);
        fs::write(&serz, []).unwrap();
        assert_eq!(fallback_serz_path(&serz64), Some(serz));
        assert_eq!(
            fallback_serz_path(temp.path().join("serz.exe").as_path()),
            None
        );
    }

    #[test]
    fn accepts_serz_progress_output_when_process_succeeds() {
        assert!(validate_serz_process(true, "Converting input.bin to output.xml", "").is_ok());
        assert!(validate_serz_process(true, "", "").is_ok());
        assert!(
            validate_serz_process(false, "Could not open input file", "")
                .unwrap_err()
                .contains("Could not open input file")
        );
        assert!(
            validate_serz_process(true, "Converting input.bin", "warning")
                .unwrap_err()
                .contains("warning")
        );
    }

    #[cfg(windows)]
    #[test]
    fn removes_extended_prefix_from_legacy_windows_tool_paths() {
        assert_eq!(
            super::legacy_windows_path(Path::new(r"\\?\X:\RailWorks\file.bin")),
            PathBuf::from(r"X:\RailWorks\file.bin")
        );
        assert_eq!(
            super::legacy_windows_path(Path::new(r"\\?\UNC\server\share\file.bin")),
            PathBuf::from(r"\\server\share\file.bin")
        );
        assert_eq!(
            super::legacy_windows_path(Path::new(r"X:\RailWorks\file.bin")),
            PathBuf::from(r"X:\RailWorks\file.bin")
        );
    }

    #[test]
    fn parses_english_route_name_and_xml_entities() {
        let xml = r#"<DisplayName><Localisation-cUserLocalisedString><English d:type="cDeltaString">London &amp; Brighton</English></Localisation-cUserLocalisedString></DisplayName>"#;
        assert_eq!(parse_route_name(xml).as_deref(), Some("London & Brighton"));
        assert_eq!(decode_xml_text("Fu&#223;ganger"), "Fußganger");
        assert_eq!(decode_xml_text("Fu&#xDF;ganger"), "Fußganger");
    }

    #[test]
    fn falls_back_to_another_localized_name() {
        let xml = r#"<DisplayName><Localisation-cUserLocalisedString><German>Hamburg</German></Localisation-cUserLocalisedString></DisplayName>"#;
        assert_eq!(parse_route_name(xml).as_deref(), Some("Hamburg"));
    }

    #[test]
    fn falls_back_when_english_route_name_is_empty() {
        let xml = r#"<DisplayName><Localisation-cUserLocalisedString><English d:type="cDeltaString"></English><German d:type="cDeltaString">[EZY] Bad Altburger Streckennetz/German</German></Localisation-cUserLocalisedString></DisplayName>"#;
        assert_eq!(
            parse_route_name(xml).as_deref(),
            Some("[EZY] Bad Altburger Streckennetz/German")
        );
    }

    #[test]
    fn extracts_blueprint_references_across_xml_lines() {
        let xml = r#"
            <Provider d:type="cDeltaString">Kuju</Provider>
            <Product d:type="cDeltaString">RailSimulator</Product>
            <BlueprintID d:type="cDeltaString">
                Scenery\Buildings\Station.XML
            </BlueprintID>
        "#;

        assert_eq!(
            extract_asset_references(xml),
            ["Kuju/RailSimulator/Scenery/Buildings/Station.bin".to_owned()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn ignores_blueprint_paths_that_escape_the_assets_directory() {
        let xml = r#"
            <Provider>Kuju</Provider>
            <Product>RailSimulator</Product>
            <BlueprintID>..\Outside\asset.xml</BlueprintID>
        "#;

        assert!(extract_asset_references(xml).is_empty());
    }

    #[test]
    fn prefers_serz64_and_falls_back_to_serz() {
        let temp = tempfile::tempdir().unwrap();
        let serz = temp.path().join("serz.exe");
        let serz64 = temp.path().join("serz64.exe");
        fs::write(&serz, []).unwrap();
        assert_eq!(preferred_serz_path(temp.path()), serz);

        fs::write(&serz64, []).unwrap();
        assert_eq!(preferred_serz_path(temp.path()), serz64);
    }

    #[test]
    fn separates_complete_routes_from_incomplete_folder_reports() {
        let temp = tempfile::tempdir().unwrap();
        let routes_root = temp.path().join("Content/Routes");
        let valid_route = routes_root.join("valid-id");
        let incomplete = routes_root.join("leftovers");
        let emptier = routes_root.join("empty-folder");
        fs::create_dir_all(&valid_route).unwrap();
        fs::create_dir_all(incomplete.join("Scenery")).unwrap();
        fs::create_dir_all(&emptier).unwrap();
        fs::write(
            valid_route.join("RouteProperties.xml"),
            "<DisplayName><English>Valid route</English></DisplayName>",
        )
        .unwrap();
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            let path = valid_route.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("data.bin"), []).unwrap();
        }
        fs::write(incomplete.join("Scenery/orphan.dat"), [1, 2, 3]).unwrap();

        let result = scan_routes(temp.path()).unwrap();

        assert_eq!(result.routes.len(), 1);
        assert_eq!(result.routes[0].name, "Valid route");
        assert_eq!(result.incomplete_folders.len(), 2);
        assert_eq!(result.incomplete_folders[0].id, "empty-folder");
        assert_eq!(result.incomplete_folders[0].file_count, 0);
        assert_eq!(result.incomplete_folders[1].id, "leftovers");
        assert_eq!(result.incomplete_folders[1].file_count, 1);
        assert!(
            result.incomplete_folders[1]
                .inventory
                .iter()
                .any(|entry| entry.contains("Scenery") && entry.contains("orphan.dat"))
        );
    }

    #[test]
    fn does_not_mark_route_incomplete_when_optional_directory_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let route_path = temp.path().join("Content/Routes/route");
        fs::create_dir_all(&route_path).unwrap();
        fs::write(
            route_path.join("RouteProperties.xml"),
            "<DisplayName><English>Complete route</English></DisplayName>",
        )
        .unwrap();
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            let directory_path = route_path.join(directory);
            fs::create_dir_all(&directory_path).unwrap();
            fs::write(directory_path.join("data.bin"), []).unwrap();
        }
        let ignored_incomplete_path = temp.path().join("Content/Routes/ignored-incomplete");
        fs::create_dir_all(&ignored_incomplete_path).unwrap();

        let scan = scan_routes(temp.path()).unwrap();
        assert_eq!(scan.incomplete_folders.len(), 1);
        assert_eq!(scan.incomplete_folders[0].id, "ignored-incomplete");
        assert_eq!(scan.routes.len(), 1);
        assert_eq!(scan.routes[0].name, "Complete route");

        let ignored_ids = ["route".to_owned(), "ignored-incomplete".to_owned()]
            .into_iter()
            .collect();
        let ignored_scan = scan_routes_with_ignored(temp.path(), &ignored_ids, 4).unwrap();
        assert!(ignored_scan.incomplete_folders.is_empty());
        assert_eq!(ignored_scan.routes.len(), 2);
    }

    #[test]
    fn extracts_scenario_names_descriptions_and_place_names() {
        let xml = r#"
            <ScenarioProperties>
                <ScenarioName>
                    <Localisation-cUserLocalisedString>
                        <English>Express to Brighton</English>
                    </Localisation-cUserLocalisedString>
                </ScenarioName>
                <ScenarioDescription>Stopping at Lewes and Falmer</ScenarioDescription>
                <StartLocationName>London Victoria</StartLocationName>
                <StartLocation>74151eaf-456f-41e7-8b38-6f48ee8ef031</StartLocation>
            </ScenarioProperties>
        "#;

        let clues = extract_scenario_clues(xml);

        assert!(
            clues
                .iter()
                .any(|clue| clue == "ScenarioName: Express to Brighton")
        );
        assert!(
            clues
                .iter()
                .any(|clue| clue == "ScenarioDescription: Stopping at Lewes and Falmer")
        );
        assert!(
            clues
                .iter()
                .any(|clue| clue == "StartLocationName: London Victoria")
        );
        assert!(
            !clues
                .iter()
                .any(|clue| clue.contains("74151eaf-456f-41e7-8b38-6f48ee8ef031"))
        );
    }

    #[test]
    fn combines_repeated_start_locations_across_scenarios() {
        let mut clues = ScenarioClueCollection::default();
        clues.add(
            "<ScenarioProperties><StartLocation>King's Cross</StartLocation></ScenarioProperties>",
            "scenario-1/ScenarioProperties.xml",
        );
        clues.add(
            "<ScenarioProperties><StartLocation> King's   Cross </StartLocation></ScenarioProperties>",
            "scenario-2/ScenarioProperties.xml",
        );
        clues.add(
            "<ScenarioProperties><StartLocation>Victoria</StartLocation></ScenarioProperties>",
            "scenario-3/ScenarioProperties.xml",
        );

        let clues = clues.into_clues();
        assert!(
            clues
                .iter()
                .any(|clue| clue == "StartLocation: King's Cross (упоминается в 2 сценариях)")
        );
        assert!(clues.iter().any(|clue| clue == "StartLocation: Victoria"));
        assert_eq!(
            clues
                .iter()
                .filter(|clue| clue.starts_with("StartLocation: King's Cross"))
                .count(),
            1
        );
    }

    #[test]
    fn includes_scenario_properties_clues_in_incomplete_route_report() {
        let temp = tempfile::tempdir().unwrap();
        let route_path = temp.path().join("Content/Routes/broken-route");
        let scenario_path = route_path.join("Scenarios/scenario-1");
        fs::create_dir_all(&scenario_path).unwrap();
        fs::write(
            scenario_path.join("ScenarioProperties.xml"),
            "<ScenarioProperties><ScenarioName><English>Coastal run</English></ScenarioName><ScenarioDescription>Through Hastings</ScenarioDescription></ScenarioProperties>",
        )
        .unwrap();

        let report = inspect_route_folder(&route_path, "broken-route")
            .unwrap()
            .unwrap();

        assert!(
            report
                .scenario_clues
                .iter()
                .any(|clue| clue.contains("ScenarioName: Coastal run"))
        );
        assert!(
            report
                .scenario_clues
                .iter()
                .any(|clue| clue.contains("ScenarioDescription: Through Hastings"))
        );
    }

    #[test]
    fn reports_empty_required_route_directories() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("empty");
        fs::create_dir_all(&route).unwrap();
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            fs::create_dir(route.join(directory)).unwrap();
        }

        let report = inspect_route_folder(&route, "empty").unwrap().unwrap();

        assert_eq!(report.file_count, 0);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("RouteProperties"))
        );
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            assert!(
                report
                    .problems
                    .iter()
                    .any(|problem| problem.contains(directory) && problem.contains("пуста"))
            );
        }
    }

    #[test]
    fn treats_only_extreme_compression_ratios_as_suspicious() {
        // Настоящие файлы в .ap сжимаются в десятки раз, а не в тысячи.
        assert!(!super::is_suspicious_archive_member(
            50 * 1024 * 1024,
            2 * 1024 * 1024
        ));
        assert!(!super::is_suspicious_archive_member(1024, 10));
        assert!(super::is_suspicious_archive_member(
            1024 * 1024 * 1024,
            1024 * 1024
        ));
        assert!(super::is_suspicious_archive_member(1024 * 1024 * 1024, 0));
    }

    #[test]
    fn finds_route_icon_in_route_information() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("incomplete-route");
        let route_information = route.join("RouteInformation");
        fs::create_dir_all(&route_information).unwrap();
        let dated_image = route_information.join("20220721195917_1.jpg");
        let standard_icon = route_information.join("Image.png");
        fs::write(&dated_image, []).unwrap();
        fs::write(&standard_icon, []).unwrap();

        assert_eq!(super::find_route_icon(&route).unwrap(), Some(standard_icon));
    }

    #[test]
    fn extracts_route_icon_from_ap_archive() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("route");
        fs::create_dir_all(&route).unwrap();
        let archive_path = route.join("route-data.ap");
        let archive_file = fs::File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(archive_file);
        archive
            .start_file(
                "RouteInformation/Image.png",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"archived image").unwrap();
        archive.finish().unwrap();

        let icon_path = super::find_route_icon_with_cache(&route, &temp.path().join("icon-cache"))
            .unwrap()
            .unwrap();

        assert_eq!(fs::read(icon_path).unwrap(), b"archived image");
    }

    #[test]
    fn incomplete_route_report_contains_route_icon_path() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("incomplete-route");
        let icon = route.join("RouteInformation/Image.png");
        fs::create_dir_all(icon.parent().unwrap()).unwrap();
        fs::write(&icon, []).unwrap();

        let report = inspect_route_folder(&route, "incomplete-route")
            .unwrap()
            .unwrap();

        assert_eq!(report.icon_path, Some(icon));
    }

    #[test]
    fn complete_route_scan_includes_route_icon_path() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("route");
        fs::create_dir_all(route.join("RouteInformation")).unwrap();
        fs::write(
            route.join("RouteProperties.xml"),
            "<DisplayName><Localisation-cUserLocalisedString><English>Test route</English></Localisation-cUserLocalisedString></DisplayName>",
        )
        .unwrap();
        let icon = route.join("RouteInformation/Image.png");
        fs::write(&icon, []).unwrap();
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            let path = route.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("content.bin"), []).unwrap();
        }

        let scan_item = super::scan_route_folder(&route, "route", &HashSet::new()).unwrap();

        match scan_item {
            super::RouteScanItem::Route(route) => assert_eq!(route.icon_path, Some(icon)),
            _ => panic!("Ожидался полный маршрут."),
        }
    }

    #[test]
    fn persists_route_thumbnail_setting() {
        let temp = tempfile::tempdir().unwrap();
        let setting_path = temp.path().join("settings/route_thumbnails_enabled.txt");

        assert!(!super::read_route_thumbnail_setting(&setting_path).unwrap());
        super::write_route_thumbnail_setting(&setting_path, true).unwrap();
        assert!(super::read_route_thumbnail_setting(&setting_path).unwrap());
        super::write_route_thumbnail_setting(&setting_path, false).unwrap();
        assert!(!super::read_route_thumbnail_setting(&setting_path).unwrap());
    }

    #[test]
    fn persists_serz_process_limit_with_validation() {
        let temp = tempfile::tempdir().unwrap();
        let setting_path = temp.path().join("settings/serz_process_limit.txt");

        super::write_serz_process_limit(&setting_path, 1).unwrap();
        assert_eq!(fs::read_to_string(&setting_path).unwrap(), "1");
        assert!(super::write_serz_process_limit(&setting_path, 0).is_err());
        assert_eq!(super::read_serz_process_limit(&setting_path).unwrap(), 1);
    }

    #[test]
    fn recognizes_route_structure_and_name_inside_ap_archive() {
        let temp = tempfile::tempdir().unwrap();
        let route_path = temp.path().join("Content/Routes/archive-route");
        fs::create_dir_all(&route_path).unwrap();
        let archive_path = route_path.join("route-data.ap");
        let archive_file = fs::File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(archive_file);
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("RouteProperties.xml", options).unwrap();
        archive
            .write_all(b"<DisplayName><English>Archived Route</English></DisplayName>")
            .unwrap();
        for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
            archive
                .start_file(format!("{directory}/route-data.bin"), options)
                .unwrap();
            archive.write_all(b"data").unwrap();
        }
        archive.finish().unwrap();

        let result = scan_routes(temp.path()).unwrap();

        assert_eq!(result.incomplete_folders.len(), 0);
        assert_eq!(result.routes.len(), 1);
        assert_eq!(result.routes[0].name, "Archived Route");
    }

    #[test]
    fn suggests_ap_filename_when_route_name_files_are_missing() {
        let temp = tempfile::tempdir().unwrap();
        let route_path = temp.path().join("Content/Routes/unknown-route");
        fs::create_dir_all(&route_path).unwrap();
        let archive_file = fs::File::create(route_path.join("Nordbahn.ap")).unwrap();
        zip::ZipWriter::new(archive_file).finish().unwrap();

        for directory in ["Networks", "Scenery", "Terrain"] {
            let directory_path = route_path.join(directory);
            fs::create_dir_all(&directory_path).unwrap();
            fs::write(directory_path.join("content.bin"), b"data").unwrap();
        }

        let result = scan_routes(temp.path()).unwrap();

        assert_eq!(result.incomplete_folders.len(), 1);
        assert_eq!(
            result.incomplete_folders[0].suggested_name.as_deref(),
            Some("Nordbahn")
        );
    }
    #[test]
    fn repro_route_check_hang() {
        let route = PathBuf::from(
            r"X:\SteamLibrary\steamapps\common\RailWorks\Content\Routes\8378d7b6-30ad-4c29-a5ac-16d3c2b26ff7",
        );
        let rw = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks");
        let assets = rw.join("Assets");
        let mut serz = rw.join("serz64.exe");
        if !serz.is_file() {
            serz = rw.join("serz.exe");
        }
        println!("using serz: {} exists={}", serz.display(), serz.is_file());
        let start = std::time::Instant::now();
        let res = check_route(&route, &assets, &serz, true, 8, |msg| {
            println!("[{:.1}s] {}", start.elapsed().as_secs_f32(), msg);
        });
        println!(
            "RESULT after {:.1}s: {:?}",
            start.elapsed().as_secs_f32(),
            res.map(|r| (
                r.asset_inventory.len(),
                r.missing_assets.len(),
                r.conversion_errors.len(),
                r.structure_issues.len()
            ))
        );
    }
}
