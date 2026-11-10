use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::MissingAssetsWindow;
use crate::app::CheckerApp;

pub struct MissingAssetsController {
    pub window: MissingAssetsWindow,
    pub weak: Weak<MissingAssetsWindow>,
    pub app: Arc<Mutex<CheckerApp>>,
    pub clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl MissingAssetsController {
    pub fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
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

    /// Ребуild всех отображаемых массивов (с учётом текстового фильтра) без
    /// сброса выделения: выделенный ассет сохраняется, если он попадает под
    /// фильтр, иначе сбрасывается в -1.
    fn refresh_missing_view(app: &CheckerApp, mw: &MissingAssetsWindow, query: &str) {
        let Some(result) = app.result.as_ref() else {
            return;
        };
        let missing = &result.missing_assets;
        let q = query.trim().to_lowercase();

        let sel = mw.get_selected_asset();
        let sel_path = (sel >= 0)
            .then(|| {
                mw.get_missing_assets()
                    .row_data(sel as usize)
                    .map(|s| s.to_string())
            })
            .flatten();

        let mut paths = Vec::with_capacity(missing.len());
        let mut counts = Vec::with_capacity(missing.len());
        let mut repairs = Vec::with_capacity(missing.len());
        let mut corrupted = Vec::with_capacity(missing.len());
        let mut types = Vec::with_capacity(missing.len());
        let mut safe = Vec::with_capacity(missing.len());
        let mut dirs = Vec::with_capacity(missing.len());
        let mut files = Vec::with_capacity(missing.len());
        let mut new_sel = -1i32;

        for m in missing
            .iter()
            .filter(|m| q.is_empty() || m.path.to_lowercase().contains(&q))
        {
            let p = m.path.as_str();
            let cut = p.rfind(['/', '\\']).map_or(0, |i| i + 1);
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
            dirs.push(slint::SharedString::from(&p[..cut]));
            files.push(slint::SharedString::from(&p[cut..]));
            if let Some(sp) = sel_path.as_ref()
                && m.path == *sp
            {
                new_sel = (paths.len() - 1) as i32;
            }
        }

        let has_repair = repairs.iter().any(|r| !r.is_empty());
        mw.set_missing_assets(slint::ModelRc::new(slint::VecModel::from(paths)));
        mw.set_missing_counts(slint::ModelRc::new(slint::VecModel::from(counts)));
        mw.set_missing_suggested_repairs(slint::ModelRc::new(slint::VecModel::from(repairs)));
        mw.set_missing_corrupted(slint::ModelRc::new(slint::VecModel::from(corrupted)));
        mw.set_missing_asset_types(slint::ModelRc::new(slint::VecModel::from(types)));
        mw.set_missing_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
        mw.set_missing_asset_dirs(slint::ModelRc::new(slint::VecModel::from(dirs)));
        mw.set_missing_asset_files(slint::ModelRc::new(slint::VecModel::from(files)));
        mw.set_can_repair_all(has_repair);
        mw.set_active_tile_list(slint::ModelRc::new(
            slint::VecModel::<crate::TileEntry>::default(),
        ));
        mw.set_active_tile_asset("".into());
        mw.set_selected_asset(new_sel);
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
            let app = self.app.clone();
            self.window.on_copy_all_to_clipboard(move || {
                let paths = {
                    let Ok(app) = app.lock() else { return };
                    match &app.result {
                        Some(result) => result
                            .missing_assets
                            .iter()
                            .map(|m| m.path.clone())
                            .collect::<Vec<String>>(),
                        None => Vec::new(),
                    }
                };
                if let Ok(mut lock) = clipboard.lock() {
                    if lock.is_none() {
                        *lock = arboard::Clipboard::new().ok();
                    }
                    if let Some(ctx) = lock.as_mut() {
                        let _ = ctx.set_text(paths.join("\n"));
                    }
                }
                let _ = handle;
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
            let handle = self.weak.clone();
            self.window.on_repair_corrupted(move |idx| {
                let Some(mw) = handle.upgrade() else { return };
                let selected_path = mw
                    .get_missing_assets()
                    .row_data(idx as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if selected_path.is_empty() {
                    return;
                }
                let Ok(mut app) = app.lock() else { return };
                let Some(missing) = app.result.as_ref().and_then(|r| {
                    r.missing_assets
                        .iter()
                        .find(|m| m.path.eq_ignore_ascii_case(&selected_path))
                        .cloned()
                }) else {
                    return;
                };
                if let Some(repair) = missing.suggested_repair.clone() {
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
                let Some(mw) = handle.upgrade() else { return };
                let selected_path = mw
                    .get_missing_assets()
                    .row_data(idx as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if selected_path.is_empty() {
                    return;
                }
                let (asset, tiles) = app
                    .lock()
                    .ok()
                    .and_then(|app| {
                        let missing = app
                            .result
                            .as_ref()?
                            .missing_assets
                            .iter()
                            .find(|m| m.path.eq_ignore_ascii_case(&selected_path))?;
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
                                    crate::domain::TileKind::Scenery => "[Scenery]",
                                    crate::domain::TileKind::Loft => "[Loft]",
                                    crate::domain::TileKind::Road => "[Road]",
                                    crate::domain::TileKind::Track => "[Track]",
                                    crate::domain::TileKind::TracksMaster => "[Tracks.bin]",
                                    crate::domain::TileKind::LoftsMaster => "[Lofts.bin]",
                                    crate::domain::TileKind::RoadsMaster => "[Roads.bin]",
                                    crate::domain::TileKind::RouteProps => "[Properties]",
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
                mw.set_active_tile_asset(asset.into());
                mw.set_active_tile_list(slint::ModelRc::new(slint::VecModel::from(tiles)));
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_replace_missing(move |idx| {
                let Some(mw) = handle.upgrade() else { return };
                let selected_path = mw
                    .get_missing_assets()
                    .row_data(idx as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if selected_path.is_empty() {
                    return;
                }
                let (missing, rw_path) = {
                    let Ok(app) = app.lock() else { return };
                    let Some(missing) = app
                        .result
                        .as_ref()
                        .and_then(|r| r.missing_assets.iter().find(|m| m.path.eq_ignore_ascii_case(&selected_path)).cloned())
                    else { return };
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
                    .add_filter("RW Assets (*.bin, *.ap)", &["bin", "ap"])
                    .add_filter("RW Blueprint (*.bin)", &["bin"])
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
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_delete_from_route(move |idx| {
                let Some(mw) = handle.upgrade() else { return };
                if idx < 0 {
                    return;
                }
                let selected_path = mw
                    .get_missing_assets()
                    .row_data(idx as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if selected_path.is_empty() {
                    return;
                }

                let (route_path, asset_type, uses) = {
                    let Ok(app) = app.lock() else { return };
                    let Some(result) = app.result.as_ref() else { return };
                    let Some(missing) = result
                        .missing_assets
                        .iter()
                        .find(|m| m.path.eq_ignore_ascii_case(&selected_path))
                    else { return };
                    let route_path = app
                        .selected_route
                        .and_then(|i| app.routes.get(i))
                        .map(|r| r.path.clone())
                        .unwrap_or_default();
                    (route_path, missing.asset_type, missing.uses.clone())
                };
                if route_path.as_os_str().is_empty() {
                    return;
                }
                if !matches!(
                    asset_type,
                    crate::assets::RouteAssetType::Scenery | crate::assets::RouteAssetType::Loft
                ) {
                    return;
                }

                let confirmed = rfd::MessageDialog::new()
                    .set_title("Удаление объекта")
                    .set_description(format!("Удалить объект из маршрута:\n{}", selected_path))
                    .set_buttons(rfd::MessageButtons::OkCancel)
                    .show();
                if confirmed != rfd::MessageDialogResult::Ok {
                    return;
                }

                let outcome = match asset_type {
                    crate::assets::RouteAssetType::Scenery => {
                        crate::assets::delete_scenery_asset(&route_path, &selected_path, &uses)
                    }
                    crate::assets::RouteAssetType::Loft => {
                        crate::assets::delete_loft_asset(&route_path, &selected_path, &uses)
                    }
                    _ => return,
                };
                match outcome {
                    Ok((files, occurrences, untouched)) => {
                        if let Ok(mut app) = app.lock() {
                            if let Some(result) = app.result.as_mut() {
                                result
                                    .missing_assets
                                    .retain(|m| !m.path.eq_ignore_ascii_case(&selected_path));
                            }
                            app.log_event(
                                format!(
                                    "Удален объект {} из {} тайлов ({} вхождений).",
                                    selected_path, files, occurrences
                                ),
                                crate::app::LogLevel::Info,
                            );
                            if !untouched.is_empty() {
                                app.log_event(
                                    format!(
                                        "Внимание: в следующих тайлах не удалось удалить вхождения объекта {}: {}",
                                        selected_path,
                                        untouched.join(", ")
                                    ),
                                    crate::app::LogLevel::Warning,
                                );
                            }
                            let remaining: Vec<String> = app
                                .result
                                .as_ref()
                                .map(|r| {
                                    r.missing_assets
                                        .iter()
                                        .map(|m| m.path.clone())
                                        .collect()
                                })
                                .unwrap_or_default();
                            app.missing_assets_popup = if remaining.is_empty() {
                                None
                            } else {
                                Some(remaining)
                            };
                            crate::schedule_ui_sync();
                        }
                    }
                    Err(error) => {
                        if let Ok(mut app) = app.lock() {
                            app.log_event(error, crate::app::LogLevel::Error);
                            crate::schedule_ui_sync();
                        }
                    }
                }

                let mw2 = match handle.upgrade() {
                    Some(m) => m,
                    None => return,
                };
                let query = mw2.get_missing_search_query().to_string();
                if let Ok(app) = app.lock() {
                    Self::refresh_missing_view(&app, &mw2, &query);
                }
                mw2.set_selected_asset(-1);
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_search_changed(move |query| {
                let Some(mw) = handle.upgrade() else { return };
                let Ok(app) = app.lock() else { return };
                Self::refresh_missing_view(&app, &mw, &query);
            });
        }
    }

    pub fn sync_missing_popup(&self, app: &mut CheckerApp) {
        if app.is_checking()
            || app.result.is_none()
            || app.missing_assets_popup.is_none()
            || app.missing_assets_shown
        {
            return;
        }
        let Some(mw) = self.weak.upgrade() else {
            return;
        };
        Self::refresh_missing_view(app, &mw, "");
        // Сброс поля поиска откладываем, чтобы не заблокировать мьютекс приложения.
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(mw) = weak.upgrade() {
                mw.set_missing_search_query("".into());
            }
        });
        let _ = mw.show();
        app.missing_assets_shown = true;
    }
}
