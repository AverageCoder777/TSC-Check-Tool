use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::MissingAssetsWindow;

pub struct MissingAssetsController {
    pub window: MissingAssetsWindow,
    pub weak: Weak<MissingAssetsWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
    pub clipboard: Arc<Mutex<arboard::Clipboard>>,
}

impl MissingAssetsController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = MissingAssetsWindow::new().unwrap();
        let weak = window.as_weak();
        let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().unwrap()));
        Self {
            window,
            weak,
            app,
            clipboard,
        }
    }

    pub fn bind(&self) {
        // copy to clipboard
        {
            let clipboard = self.clipboard.clone();
            let handle = self.weak.clone();
            self.window.on_copy_to_clipboard(move |text| {
                if let Ok(mut ctx) = clipboard.lock() {
                    let _ = ctx.set_text(text.to_string());
                }
                let _ = handle;
            });
        }
        // copy all
        {
            let clipboard = self.clipboard.clone();
            let handle = self.weak.clone();
            self.window.on_copy_all_to_clipboard(move || {
                if let Some(mw) = handle.upgrade() {
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
            });
        }
        // close
        {
            let handle = self.weak.clone();
            self.window.on_close_missing(move || {
                if let Some(mw) = handle.upgrade() {
                    let _ = mw.hide();
                }
            });
        }
        // repair corrupted
        {
            let app = self.app.clone();
            self.window.on_repair_corrupted(move |idx| {
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
            });
        }
        // repair all corrupted
        {
            let app = self.app.clone();
            self.window.on_repair_all_corrupted(move || {
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
                        eprintln!(
                            "[Repair] No candidates resolved; fuzzy directory matching failed."
                        );
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
                    app.asset_replacement_queue = repairable
                        .into_iter()
                        .filter_map(|missing| {
                            let repair = missing.suggested_repair.clone()?;
                            let replacement = assets_path.join(repair.replace('/', "\\"));
                            Some((missing, replacement, repair))
                        })
                        .collect();
                    app.asset_replacement = None;
                    app.missing_assets_shown = false;
                    app.start_next_asset_replacement();
                }
            });
        }
        // inspect tiles
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_inspect_tiles(move |idx| {
                let (asset, tiles) = if let Ok(app) = app.lock() {
                    if let Some(result) = app.result.as_ref() {
                        if let Some(missing) = result.missing_assets.get(idx as usize) {
                            let mut counts: BTreeMap<String, (crate::domain::TileKind, usize)> =
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
                                        crate::domain::TileKind::Scenery => "[Сценери]",
                                        crate::domain::TileKind::Loft => "[Лофт]",
                                        crate::domain::TileKind::Road => "[Дорога]",
                                        crate::domain::TileKind::Track => "[Пути]",
                                        crate::domain::TileKind::TracksMaster
                                        | crate::domain::TileKind::LoftsMaster
                                        | crate::domain::TileKind::RoadsMaster => "[Сеть]",
                                        crate::domain::TileKind::RouteProps => "[Свойства]",
                                        crate::domain::TileKind::Other => "[Прочее]",
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
            });
        }
        // replace missing
        {
            let app = self.app.clone();
            self.window.on_replace_missing(move |idx| {
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
                    let start_dir = crate::inventory_ui::existing_directory_upwards(
                        &crate::inventory_ui::asset_directory(&assets_path, &missing.path),
                    )
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
                            crate::AP_PICKER_CONTROLLER.with(|cell| {
                                if let Some(pc) = cell.borrow().as_ref() {
                                    if let Ok(mut st) = pc.state.lock() {
                                        *st = Some(crate::ap_picker_controller::ApPickerState {
                                            archive_path: replacement.clone(),
                                            missing: missing.clone(),
                                            rw_path: rw_path.clone(),
                                            entries: entries.clone(),
                                        });
                                    }
                                    if let Some(pw) = pc.weak.upgrade() {
                                        pw.set_archive_name(archive_name.into());
                                        let all: Vec<slint::SharedString> =
                                            entries.iter().map(|e| e.clone().into()).collect();
                                        pw.set_entries(slint::ModelRc::new(slint::VecModel::from(all.clone())));
                                        pw.set_filtered_entries(slint::ModelRc::new(slint::VecModel::from(all)));
                                        pw.set_search_query("".into());
                                        pw.set_selected_index(-1);
                                        pw.set_extracting(false);
                                        let _ = pw.show();
                                    }
                                }
                            });
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
        }
    }

    pub fn sync_missing_popup(&self, app: &mut crate::app::CheckerApp) {
        if app.is_checking()
            || app.result.is_none()
            || app.missing_assets_popup.is_none()
            || app.missing_assets_shown
        {
            return;
        }
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
        let corrupted: Vec<bool> = missing_assets.iter().map(|m| m.is_corrupted_link).collect();
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
        if let Some(mw) = self.weak.upgrade() {
            mw.set_missing_assets(slint::ModelRc::new(slint::VecModel::from(paths)));
            mw.set_missing_counts(slint::ModelRc::new(slint::VecModel::from(counts)));
            mw.set_missing_suggested_repairs(slint::ModelRc::new(slint::VecModel::from(repairs)));
            mw.set_missing_corrupted(slint::ModelRc::new(slint::VecModel::from(corrupted)));
            mw.set_missing_asset_types(slint::ModelRc::new(slint::VecModel::from(types)));
            mw.set_missing_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
            mw.set_active_tile_list(slint::ModelRc::new(
                slint::VecModel::<slint::SharedString>::default(),
            ));
            mw.set_active_tile_asset("".into());
            let _ = mw.show();
            app.missing_assets_shown = true;
        }
    }
}
