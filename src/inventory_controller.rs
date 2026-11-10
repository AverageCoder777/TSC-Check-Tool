use crate::AssetInventoryWindow;
use crate::domain::TileKind;
use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct InventoryController {
    pub window: AssetInventoryWindow,
    pub weak: Weak<AssetInventoryWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
}

impl InventoryController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = AssetInventoryWindow::new().unwrap();
        let weak = window.as_weak();
        Self { window, weak, app }
    }
    pub(crate) fn update_inventory_view(
        inv: &AssetInventoryWindow,
        state: &mut crate::app::AssetInventoryWindowState,
    ) {
        let show_missing = inv.get_show_missing_only();
        let (rows, safe, delete_flags, filtered_indices) =
            crate::inventory_ui::build_inventory_rows(state, show_missing);
        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
        inv.set_inventory_can_delete(slint::ModelRc::new(slint::VecModel::from(delete_flags)));
        state.filtered_indices = filtered_indices;
    }
    pub fn bind(&self) {
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_toggle_missing_only(move || {
                let Some(inv) = handle.upgrade() else { return };
                let cur = inv.get_show_missing_only();
                inv.set_show_missing_only(!cur);
                if let Ok(app) = app.lock()
                    && let Ok(mut state) = app.asset_inventory_state.lock()
                {
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_sort_ascending(move |column| {
                let Some(inv) = handle.upgrade() else { return };
                if let Ok(app) = app.lock()
                    && let Ok(mut state) = app.asset_inventory_state.lock()
                {
                    match column {
                        0 => {
                            state.sort.column = crate::inventory_ui::InventorySortColumn::Path;
                            state.sort.descending = false;
                        }
                        1 => {
                            state.sort.column = crate::inventory_ui::InventorySortColumn::Usage;
                            state.sort.descending = false;
                        }
                        _ => {}
                    }
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_sort_descending(move |column| {
                let Some(inv) = handle.upgrade() else { return };
                if let Ok(app) = app.lock()
                    && let Ok(mut state) = app.asset_inventory_state.lock()
                {
                    match column {
                        0 => {
                            state.sort.column = crate::inventory_ui::InventorySortColumn::Path;
                            state.sort.descending = true;
                        }
                        1 => {
                            state.sort.column = crate::inventory_ui::InventorySortColumn::Usage;
                            state.sort.descending = true;
                        }
                        _ => {}
                    }
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_search_changed(move |text| {
                let Some(inv) = handle.upgrade() else { return };
                if let Ok(app) = app.lock()
                    && let Ok(mut state) = app.asset_inventory_state.lock()
                {
                    state.search = text.to_string();
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }
        {
            let mut inv_last_row: i32 = -1;
            let mut inv_last_click = Instant::now();
            const INV_DOUBLE_CLICK_MS: u64 = 400;

            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_row_selected(move |row| {
                let Some(inv) = handle.upgrade() else { return };
                inv.set_selected_asset(row);
                let now = Instant::now();
                let is_double = row == inv_last_row
                    && now.saturating_duration_since(inv_last_click)
                        <= Duration::from_millis(INV_DOUBLE_CLICK_MS);
                inv_last_row = row;
                inv_last_click = now;
                if !is_double || row < 0 {
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
                let rw_path = if let Ok(app) = app.lock() {
                    app.clean_rw_path()
                } else {
                    return;
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                {
                    let full = rw_path.join("Assets").join(path_text.replace('/', "\\"));
                    if full.exists() {
                        crate::windows::open_in_explorer(&full);
                    } else if let Some(target) =
                        crate::inventory_ui::existing_directory_upwards(&full)
                    {
                        crate::windows::open_in_file_explorer(&target);
                    }
                }
            });
        }
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_open_in_editor(move || {
                let Some(inv) = handle.upgrade() else { return };
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
                let rw_path = if let Ok(app) = app.lock() {
                    app.clean_rw_path()
                } else {
                    return;
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                let full = rw_path.join("Assets").join(path_text.replace('/', "\\"));
                if full.is_file() {
                    let app_for_status = app.clone();
                    let _ =
                        crate::external_editor::open_and_sync_with_vscode(&full, move |result| {
                            if let Ok(mut app) = app_for_status.lock() {
                                let (msg, level) = match result {
                                    Ok(msg) => (msg, crate::app::LogLevel::Info),
                                    Err(err) => (format!("✗ {err}"), crate::app::LogLevel::Error),
                                };
                                app.log_event(msg, level);
                                crate::schedule_ui_sync();
                            }
                        });
                }
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_delete_inventory_asset(move |idx| {
                let (asset, route_path) = {
                    let Ok(app) = app.lock() else { return };
                    let Ok(state) = app.asset_inventory_state.lock() else { return };
                    let filtered_idx = idx as usize;
                    let master_idx = state.filtered_indices.get(filtered_idx).copied();
                    let Some(master_idx) = master_idx else { return };
                    let Some(asset) = state.assets.get(master_idx).cloned() else {
                        return;
                    };
                    let route_path = app
                        .selected_route
                        .and_then(|i| app.routes.get(i))
                        .map(|r| r.path.clone());
                    (asset, route_path)
                };
                let Some(route_path) = route_path else { return };

                let scenery_uses: Vec<_> = asset
                    .uses
                    .iter()
                    .filter(|u| u.tile_kind == TileKind::Scenery)
                    .cloned()
                    .collect();
                let loft_road_uses: Vec<_> = asset
                    .uses
                    .iter()
                    .filter(|u| {
                        matches!(
                            u.tile_kind,
                            TileKind::Loft | TileKind::Road
                        )
                    })
                    .cloned()
                    .collect();

                let protected = asset
                    .uses
                    .iter()
                    .any(|u| u.source_file.is_protected_pathway());
                if protected {
                    if let Ok(mut app) = app.lock() {
                        app.log_event(
                            "Удаление этого объекта запрещено (путевая инфраструктура).".to_owned(),
                            crate::app::LogLevel::Warning,
                        );
                        crate::schedule_ui_sync();
                    }
                    return;
                }

                let loft_tiles: HashSet<String> = loft_road_uses
                    .iter()
                    .map(|u| u.source_file.display().to_string())
                    .collect();
                let scenery_tiles: HashSet<String> = scenery_uses
                    .iter()
                    .map(|u| u.source_file.display().to_string())
                    .collect();
                let loft_segments: usize = loft_road_uses.iter().map(|u| u.count as usize).sum();
                let scenery_segments: usize =
                    scenery_uses.iter().map(|u| u.count as usize).sum();

                let description = match (!loft_road_uses.is_empty(), !scenery_uses.is_empty()) {
                    (true, false) => format!(
                        "Удалить лофт {} из {} тайлов Loft ({} сегментов)? Это действие физически изменит файлы маршрута",
                        asset.path,
                        loft_tiles.len(),
                        loft_segments
                    ),
                    (false, true) => format!(
                        "Удалить объект {} из всех Scenery-тайлов ({} шт.)? Это действие физически изменит файлы маршрута",
                        asset.path,
                        scenery_tiles.len()
                    ),
                    (true, true) => format!(
                        "Удалить объект {} из {} тайлов Scenery ({} сегментов) и {} тайлов Loft ({} сегментов)? Это действие физически изменит файлы маршрута",
                        asset.path,
                        scenery_tiles.len(),
                        scenery_segments,
                        loft_tiles.len(),
                        loft_segments
                    ),
                    (false, false) => {
                        if let Ok(mut app) = app.lock() {
                            app.log_event(
                                format!(
                                    "Для объекта {} не найдено подходящих тайлов для удаления.",
                                    asset.path
                                ),
                                crate::app::LogLevel::Warning,
                            );
                            crate::schedule_ui_sync();
                        }
                        return;
                    }
                };

                let confirmed = rfd::MessageDialog::new()
                    .set_title("Удаление объекта")
                    .set_description(description)
                    .set_buttons(rfd::MessageButtons::OkCancel)
                    .show();
                if confirmed != rfd::MessageDialogResult::Ok {
                    return;
                }

                let mut total_tiles = 0usize;
                let mut total_occurrences = 0usize;
                let mut errors = Vec::new();
                let mut untouched_files = Vec::new();

                if !scenery_uses.is_empty() {
                    match crate::assets::delete_scenery_asset(
                        &route_path,
                        &asset.path,
                        &scenery_uses,
                    ) {
                        Ok((tiles, occurrences, untouched)) => {
                            total_tiles += tiles;
                            total_occurrences += occurrences;
                            untouched_files.extend(untouched);
                        }
                        Err(error) => errors.push(error),
                    }
                }
                if !loft_road_uses.is_empty() {
                    match crate::assets::delete_loft_asset(
                        &route_path,
                        &asset.path,
                        &loft_road_uses,
                    ) {
                        Ok((tiles, occurrences, untouched)) => {
                            total_tiles += tiles;
                            total_occurrences += occurrences;
                            untouched_files.extend(untouched);
                        }
                        Err(error) => errors.push(error),
                    }
                }

                if let Ok(mut app) = app.lock() {
                    if errors.is_empty() {
                        if let Ok(mut state) = app.asset_inventory_state.lock() {
                            state.assets.retain(|a| a.path != asset.path);
                        }
                        app.log_event(
                            format!(
                                "Удален объект {} из {} тайлов ({} вхождений)",
                                asset.path, total_tiles, total_occurrences
                            ),
                            crate::app::LogLevel::Info,
                        );
                        if !untouched_files.is_empty() {
                            app.log_event(
                                format!(
                                    "Внимание: в следующих тайлах не удалось удалить вхождения объекта {} (вероятно, составной лофт, требуется ручное удаление): {}",
                                    asset.path,
                                    untouched_files.join(", ")
                                ),
                                crate::app::LogLevel::Warning,
                            );
                        }
                    } else {
                        for error in &errors {
                            app.log_event(error.clone(), crate::app::LogLevel::Error);
                        }
                    }
                    crate::schedule_ui_sync();
                }
                if let Some(inv) = handle.upgrade()
                    && let Ok(app) = app.lock()
                    && let Ok(mut state) = app.asset_inventory_state.lock()
                {
                    Self::update_inventory_view(&inv, &mut state);
                    inv.set_selected_asset(-1);
                    inv.set_current_row(-1);
                }
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_replace_inventory_asset(move |selected| {
                if selected < 0 {
                    return;
                }
                let Some(inv) = handle.upgrade() else { return };
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
                let (rw_path, asset) = {
                    let Ok(app) = app.lock() else { return };
                    let rw = app.clean_rw_path();
                    let asset = if let Ok(state) = app.asset_inventory_state.lock() {
                        state.assets.iter().find(|a| a.path == asset_path).cloned()
                    } else {
                        None
                    };
                    (rw, asset)
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                let Some(asset) = asset else {
                    return;
                };
                if !crate::assets::is_safe_for_global_replacement(&asset.path, &asset.uses) {
                    if let Ok(mut app) = app.lock() {
                        app.log_event(
                            "Замена этого ассета запрещена (путевая инфраструктура).".to_owned(),
                            crate::app::LogLevel::Warning,
                        );
                        crate::schedule_ui_sync();
                    }
                    return;
                }
                let assets_dir = rw_path.join("Assets");
                let start_dir = crate::inventory_ui::existing_directory_upwards(
                    &crate::inventory_ui::asset_directory(&assets_dir, &asset.path),
                )
                .unwrap_or_else(|| assets_dir.clone());
                let selected_file = rfd::FileDialog::new()
                    .set_title("Заменить в маршруте...")
                    .add_filter("RailWorks Blueprint (*.bin)", &["bin"])
                    .set_directory(&start_dir)
                    .pick_file();
                let Some(picked) = selected_file else {
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
                        !u.source_file_search.contains("tracks.bin")
                            && !u.source_file_search.contains("/networks/")
                            && !u.source_file_search.contains("\\networks\\")
                    })
                    .collect();
                if uses.is_empty() {
                    return;
                }
                let missing = crate::domain::MissingAsset {
                    path: asset.path,
                    uses,
                    suggested_repair: None,
                    is_corrupted_link: false,
                    asset_type,
                };
                if let Ok(mut app) = app.lock() {
                    app.inventory_replacement_pending_check = true;
                    app.asset_replacement_queue
                        .push_back((missing, picked, replacement_relative));
                    app.start_next_asset_replacement();
                }
            });
        }
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_scan_umlauts(move || {
                let (suspicious_count, new_filter_state) = {
                    let Ok(app) = app.lock() else { return };
                    let Ok(mut state) = app.asset_inventory_state.lock() else {
                        return;
                    };
                    state.umlaut_filter = !state.umlaut_filter;
                    let suspicious_count = state
                        .assets
                        .iter()
                        .filter(|a| {
                            crate::inventory_ui::has_suspicious_chars(&a.path)
                                || a.uses.iter().any(|u| {
                                    crate::inventory_ui::has_suspicious_chars(&u.tile)
                                        || crate::inventory_ui::has_suspicious_chars(
                                            &u.source_file_search,
                                        )
                                })
                        })
                        .count();
                    if let Some(inv) = handle.upgrade() {
                        Self::update_inventory_view(&inv, &mut state);
                    }
                    (suspicious_count, state.umlaut_filter)
                };
                if let Ok(mut app) = app.lock() {
                    if new_filter_state {
                        if suspicious_count == 0 {
                            app.status =
                                "Объектов с подозрительными символами не обнаружено".to_owned();
                        } else {
                            app.status = format!(
                                "Найдено объектов с нестандартными символами: {} шт.",
                                suspicious_count
                            );
                        }
                    } else {
                        app.status = "Фильтр по символам отключен.".to_owned();
                    }
                    crate::schedule_ui_sync();
                }
            });
        }
    }
    pub fn refresh(&self) {
        let Ok(mut app) = self.app.lock() else {
            return;
        };
        if !app.inventory_refresh_pending {
            return;
        }
        let Some(inv) = self.weak.upgrade() else {
            return;
        };
        if let Ok(mut state) = app.asset_inventory_state.lock() {
            Self::update_inventory_view(&inv, &mut state);
        }
        app.inventory_refresh_pending = false;
    }
}
