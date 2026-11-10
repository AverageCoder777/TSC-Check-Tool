use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::{BTreeMap, VecDeque};
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
        {
            let clipboard = self.clipboard.clone();
            let handle = self.weak.clone();
            self.window.on_copy_all_to_clipboard(move || {
                let Some(mw) = handle.upgrade() else { return };
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
            });
        }
        {
            let handle = self.weak.clone();
            self.window.on_close_missing(move || {
                if let Some(mw) = handle.upgrade() {
                    let _ = mw.hide();
                }
            });
        }
        {
            let app = self.app.clone();
            self.window.on_repair_corrupted(move |idx| {
                let Ok(mut app) = app.lock() else { return };
                let missing = app
                    .result
                    .as_ref()
                    .and_then(|r| r.missing_assets.get(idx as usize))
                    .cloned();
                if let Some(missing) = missing
                    && let Some(repair) = missing.suggested_repair.clone()
                {
                    let clean_repair = repair.trim_start_matches(['/', '\\']).replace('/', "\\");
                    let assets_path = app.clean_rw_path().join("Assets");
                    let replacement = assets_path.join(&clean_repair);
                    let corrupted = missing.path.clone();
                    app.start_asset_replacement(
                        missing,
                        replacement,
                        false,
                        repair,
                        BTreeMap::new(),
                    );
                    app.log_event(
                        format!("Починен путь {} -> {}.", corrupted, clean_repair),
                        crate::app::LogLevel::Info,
                    );
                    crate::schedule_ui_sync();
                }
            });
        }
        {
            let app = self.app.clone();
            self.window.on_repair_all_corrupted(move || {
                let Ok(mut app) = app.lock() else { return };
                let assets_path = app.clean_rw_path().join("Assets");
                let queue: VecDeque<_> = match app.result.as_ref() {
                    Some(res) => res
                        .missing_assets
                        .iter()
                        .filter_map(|missing| {
                            let repair = missing.suggested_repair.as_ref()?;
                            if repair.trim().is_empty() {
                                return None;
                            }
                            let clean = repair.trim_start_matches(['/', '\\']).replace('/', "\\");
                            let replacement = assets_path.join(clean);
                            Some((missing.clone(), replacement, repair.clone()))
                        })
                        .collect(),
                    None => VecDeque::new(),
                };
                let repairable_count = queue.len();
                if repairable_count == 0 {
                    return;
                }
                app.asset_replacement_queue = queue;
                app.missing_assets_shown = false;
                app.log_event(
                    format!(
                        "Массовая починка путей: исправлено {} объектов.",
                        repairable_count
                    ),
                    crate::app::LogLevel::Info,
                );
                app.start_next_asset_replacement();
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_inspect_tiles(move |idx| {
                let (asset, tiles) = app
                    .lock()
                    .ok()
                    .and_then(|app| {
                        let missing = app.result.as_ref()?.missing_assets.get(idx as usize)?;
                        let mut counts: BTreeMap<&str, (crate::domain::TileKind, usize, String)> =
                            BTreeMap::new();
                        for usage in &missing.uses {
                            let file_path = match &usage.source_file {
                                crate::domain::RouteFile::Path(path) => {
                                    path.to_string_lossy().to_string()
                                }
                                crate::domain::RouteFile::Archive { name, .. } => name.clone(),
                            };
                            counts
                                .entry(usage.tile.as_str())
                                .and_modify(|e| e.1 += 1)
                                .or_insert((usage.tile_kind, 1, file_path));
                        }
                        let tiles: Vec<crate::TileEntry> = counts
                            .into_iter()
                            .map(|(tile, (kind, count, file_path))| {
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
                                crate::TileEntry {
                                    label: format!("{badge} {tile} ({count})").into(),
                                    file_path: file_path.into(),
                                }
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
        {
            let app = self.app.clone();
            self.window.on_replace_missing(move |idx| {
                let (missing, rw_path) = {
                    let Ok(app) = app.lock() else { return };
                    let Some(missing) = app.result.as_ref().and_then(|r| r.missing_assets.get(idx as usize)).cloned() else { return };
                    (missing, app.clean_rw_path())
                };
                if !crate::assets::is_safe_for_global_replacement(&missing.path, &missing.uses) {
                    if let Ok(mut app) = app.lock() {
                        app.log_event(
                            "Замена путевых объектов заблокирована для предотвращения повреждения маршрута.".to_owned(),
                            crate::app::LogLevel::Warning,
                        );
                        crate::schedule_ui_sync();
                    }
                    return;
                }
                let assets_path = rw_path.join("Assets");
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
                let Some(replacement) = selected else { return };
                let is_ap = replacement
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("ap"));
                if is_ap {
                    let entries = crate::blueprint::ap_archive_entries(&replacement);
                    let archive_name = replacement
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    crate::AP_PICKER_CONTROLLER.with(|cell| {
                        let borrowed = cell.borrow();
                        let Some(pc) = borrowed.as_ref() else { return };
                        if let Ok(mut st) = pc.state.lock() {
                            *st = Some(crate::ap_picker_controller::ApPickerState {
                                archive_path: replacement.clone(),
                                missing: missing.clone(),
                                rw_path: rw_path.to_string_lossy().to_string(),
                                entries: entries.clone(),
                        });
                        }
                        if let Some(pw) = pc.weak.upgrade() {
                        pw.set_archive_name(archive_name.into());
                        let all: Vec<slint::SharedString> =
                            entries.into_iter().map(|e| e.into()).collect();
                        let model = slint::ModelRc::new(slint::VecModel::from(all));
                        pw.set_entries(model.clone());
                        pw.set_filtered_entries(model);
                        pw.set_search_query("".into());
                        pw.set_selected_index(-1);
                        pw.set_extracting(false);
                        let _ = pw.show();
                        }
                    });
                } else if let Ok(mut app) = app.lock() {
                    let replacement_relative = replacement
                        .strip_prefix(&assets_path)
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
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
            repairs.push(slint::SharedString::from(
                m.suggested_repair.as_deref().unwrap_or(""),
            ));
            corrupted.push(m.is_corrupted_link);
            types.push(slint::SharedString::from(
                crate::assets::RouteAssetType::label(m.asset_type),
            ));
            safe.push(crate::assets::is_safe_for_global_replacement(
                &m.path, &m.uses,
            ));
        }
        if let Some(mw) = self.weak.upgrade() {
            mw.set_missing_assets(slint::ModelRc::new(slint::VecModel::from(paths)));
            mw.set_missing_counts(slint::ModelRc::new(slint::VecModel::from(counts)));
            mw.set_missing_suggested_repairs(slint::ModelRc::new(slint::VecModel::from(repairs)));
            mw.set_missing_corrupted(slint::ModelRc::new(slint::VecModel::from(corrupted)));
            mw.set_missing_asset_types(slint::ModelRc::new(slint::VecModel::from(types)));
            mw.set_missing_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
            mw.set_active_tile_list(slint::ModelRc::new(
                slint::VecModel::<crate::TileEntry>::default(),
            ));
            mw.set_active_tile_asset("".into());
            let _ = mw.show();
            app.missing_assets_shown = true;
        }
    }
}
