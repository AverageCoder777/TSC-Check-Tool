use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::MissingAssetsWindow;

pub struct MissingAssetsController {
    pub window: MissingAssetsWindow,
    pub weak: Weak<MissingAssetsWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
    pub clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl MissingAssetsController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = MissingAssetsWindow::new().unwrap();
        let weak = window.as_weak();
        let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().ok()));
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
            self.window.on_copy_to_clipboard(move |text| {
                if let Ok(mut lock) = clipboard.lock() {
                    if lock.is_none() {
                        *lock = arboard::Clipboard::new().ok();
                    }
                    if let Some(ctx) = lock.as_mut() {
                        let _ = ctx.set_text(text.to_string());
                    }
                }
            });
        }
        // copy all
        {
            let clipboard = self.clipboard.clone();
            let handle = self.weak.clone();
            self.window.on_copy_all_to_clipboard(move || {
                if let Some(mw) = handle.upgrade() {
                    let model = mw.get_missing_assets();
                    let count = model.row_count();
                    let mut paths = Vec::with_capacity(count);
                    for i in 0..count {
                        if let Some(value) = model.row_data(i) {
                            paths.push(value.to_string());
                        }
                    }
                    if let Ok(mut lock) = clipboard.lock() {
                        if lock.is_none() {
                            *lock = arboard::Clipboard::new().ok();
                        }
                        if let Some(ctx) = lock.as_mut() {
                            let _ = ctx.set_text(paths.join("\n"));
                        }
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
                    let missing = app
                        .result
                        .as_ref()
                        .and_then(|r| r.missing_assets.get(idx as usize))
                        .cloned();

                    if let Some(missing) = missing
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
                    let assets_path = PathBuf::from(app.rw_path.trim()).join("Assets");
                    let queue: VecDeque<_> = match app.result.as_ref() {
                        Some(res) => res
                            .missing_assets
                            .iter()
                            .filter_map(|missing| {
                                let repair = missing.suggested_repair.as_ref()?;
                                if repair.trim().is_empty() {
                                    return None;
                                }
                                let replacement = assets_path.join(repair.replace('/', "\\"));
                                Some((missing.clone(), replacement, repair.clone()))
                            })
                            .collect(),
                        None => VecDeque::new(),
                    };

                    let repairable_count = queue.len();
                    eprintln!("[Repair] Found {} repairable assets", repairable_count);
                    if repairable_count == 0 {
                        eprintln!(
                            "[Repair] No candidates resolved; fuzzy directory matching failed."
                        );
                        return;
                    }

                    app.asset_replacement_queue = queue;
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
                let (asset, tiles) = app
                    .lock()
                    .ok()
                    .and_then(|app| {
                        let missing = app.result.as_ref()?.missing_assets.get(idx as usize)?;
                        let mut counts: BTreeMap<&str, (crate::domain::TileKind, usize)> =
                            BTreeMap::new();
                        for usage in &missing.uses {
                            match counts.get_mut(usage.tile.as_str()) {
                                Some(entry) => entry.1 += 1,
                                None => {
                                    counts.insert(usage.tile.as_str(), (usage.tile_kind, 1));
                                }
                            }
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
                                format!("{badge} {tile} ({count})").into()
                            })
                            .collect();
                        Some((missing.path.clone(), tiles))
                    })
                    .unwrap_or_default();

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
                let selection = app.lock().ok().and_then(|app| {
                    let missing = app.result.as_ref()?.missing_assets.get(idx as usize).cloned()?;
                    Some((missing, app.rw_path.clone()))
                });

                let Some((missing, rw_path)) = selection else {
                    return;
                };

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
        let Some(result) = app.result.as_ref() else {
            return;
        };
        let missing_assets = &result.missing_assets;
        let n = missing_assets.len();

        let mut paths = Vec::with_capacity(n);
        let mut counts = Vec::with_capacity(n);
        let mut repairs = Vec::with_capacity(n);
        let mut corrupted = Vec::with_capacity(n);
        let mut types = Vec::with_capacity(n);
        let mut safe = Vec::with_capacity(n);

        for m in missing_assets {
            paths.push(slint::SharedString::from(m.path.as_str()));
            counts.push(m.uses.len() as i32);
            repairs.push(slint::SharedString::from(m.suggested_repair.as_deref().unwrap_or("")));
            corrupted.push(m.is_corrupted_link);
            types.push(slint::SharedString::from(crate::assets::RouteAssetType::label(m.asset_type)));
            safe.push(!crate::assets::is_protected_network_asset(&crate::assets::RouteAssetItem {
                path: m.path.clone(),
                uses: m.uses.clone(),
            }));
        }

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