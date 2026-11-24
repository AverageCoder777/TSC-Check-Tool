use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, Weak};

use crate::AssetInventoryWindow;
use crate::app::app_controller::{AssetInventoryWindowState, CheckerApp, LogLevel};
use crate::assets::inventory_ui::*;
use crate::assets::{
    classify_route_asset_type, delete_loft_asset, delete_scenery_asset,
    is_safe_for_global_replacement,
};
use crate::common::domain::{MissingAsset, TileKind};
use crate::common::external_editor::open_and_sync_with_vscode;
#[cfg(windows)]
use crate::common::windows::open_in_explorer;
use crate::schedule_ui_sync;

const INV_DOUBLE_CLICK_MS: u64 = 400;

pub struct InventoryController {
    pub window: AssetInventoryWindow,
    pub weak: Weak<AssetInventoryWindow>,
    pub app: Arc<Mutex<CheckerApp>>,
}

impl InventoryController {
    pub fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
        let window = AssetInventoryWindow::new().unwrap();
        let weak = window.as_weak();
        Self { window, weak, app }
    }

    pub(crate) fn update_inventory_view(
        inv: &AssetInventoryWindow,
        state: &mut AssetInventoryWindowState,
    ) {
        let show_missing = inv.get_show_missing_only();
        let (rows, safe, delete_flags, filtered_indices) =
            build_inventory_rows(state, show_missing);
        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
        inv.set_inventory_can_delete(slint::ModelRc::new(slint::VecModel::from(delete_flags)));
        state.filtered_indices = filtered_indices;
    }

    pub fn bind(&self) {
        self.bind_filters_and_sort();
        self.bind_selection_and_editor();
        self.bind_mutations();
    }

    fn bind_filters_and_sort(&self) {
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_toggle_missing_only(move || {
                let Some(inv) = handle.upgrade() else { return };
                let cur = inv.get_show_missing_only();
                inv.set_show_missing_only(!cur);
                let state_arc = match app.lock() {
                    Ok(a) => a.asset_inventory_state.clone(),
                    Err(_) => return,
                };
                if let Ok(mut state) = state_arc.lock() {
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }

        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_sort_ascending(move |column| {
                apply_sort(&handle, &app, column, false);
            });
        }

        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_sort_descending(move |column| {
                apply_sort(&handle, &app, column, true);
            });
        }

        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_search_changed(move |text| {
                let Some(inv) = handle.upgrade() else { return };
                let state_arc = match app.lock() {
                    Ok(a) => a.asset_inventory_state.clone(),
                    Err(_) => return,
                };
                if let Ok(mut state) = state_arc.lock() {
                    state.search = text.to_string();
                    Self::update_inventory_view(&inv, &mut state);
                }
            });
        }

        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_scan_umlauts(move || {
                let state_arc = match app.lock() {
                    Ok(a) => a.asset_inventory_state.clone(),
                    Err(_) => return,
                };
                let (suspicious_count, new_filter_state) = {
                    let Ok(mut state) = state_arc.lock() else {
                        return;
                    };
                    state.umlaut_filter = !state.umlaut_filter;
                    let suspicious_count = state
                        .assets
                        .iter()
                        .filter(|a| {
                            has_suspicious_chars(&a.path)
                                || a.uses.iter().any(|u| {
                                    has_suspicious_chars(&u.tile)
                                        || has_suspicious_chars(&u.source_file_search)
                                })
                        })
                        .count();
                    if let Some(inv) = handle.upgrade() {
                        Self::update_inventory_view(&inv, &mut state);
                    }
                    (suspicious_count, state.umlaut_filter)
                };
                if let Ok(mut app_guard) = app.lock() {
                    if new_filter_state {
                        if suspicious_count == 0 {
                            app_guard.status =
                                crate::gettext!("No objects with non-standard symbols were found.");
                        } else {
                            app_guard.status = crate::gettext!(
                                "Found objects with non-standard symbols: {}.",
                                suspicious_count
                            );
                        }
                    } else {
                        app_guard.status = crate::gettext!("Symbol filter is disabled.");
                    }
                    schedule_ui_sync();
                }
            });
        }
    }

    fn bind_selection_and_editor(&self) {
        {
            let mut inv_last_row: i32 = -1;
            let mut inv_last_click = Instant::now();

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

                if !is_double {
                    return;
                }
                let Some(path_text) = selected_row_path(&inv, row) else {
                    return;
                };
                let rw_path = match app.lock() {
                    Ok(a) => a.clean_rw_path(),
                    Err(_) => return,
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                #[cfg(windows)]
                {
                    let full = resolve_asset_full_path(&rw_path, &path_text);
                    if full.exists() {
                        open_in_explorer(&full);
                    } else if let Some(target) = existing_directory_upwards(&full) {
                        open_in_explorer(&target);
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = path_text;
                }
            });
        }

        {
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_open_in_editor(move || {
                let Some(inv) = handle.upgrade() else { return };
                let row = inv.get_current_row();
                let Some(path_text) = selected_row_path(&inv, row) else {
                    return;
                };
                let rw_path = match app.lock() {
                    Ok(a) => a.clean_rw_path(),
                    Err(_) => return,
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                let full = resolve_asset_full_path(&rw_path, &path_text);
                if full.is_file() {
                    let app_for_status = app.clone();
                    let _ = open_and_sync_with_vscode(&full, move |result| {
                        if let Ok(mut app_guard) = app_for_status.lock() {
                            let (msg, level) = match result {
                                Ok(msg) => (msg, LogLevel::Info),
                                Err(err) => (format!("X {err}"), LogLevel::Error),
                            };
                            app_guard.log_event(msg, level);
                            schedule_ui_sync();
                        }
                    });
                }
            });
        }
    }

    fn bind_mutations(&self) {
        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_delete_inventory_asset(move |idx| {
                if idx < 0 {
                    return;
                }
                let state_arc = match app.lock() {
                    Ok(a) => a.asset_inventory_state.clone(),
                    Err(_) => return,
                };
                let asset = {
                    let Ok(state) = state_arc.lock() else { return };
                    let Some(&master_idx) = state.filtered_indices.get(idx as usize) else {
                        return;
                    };
                    let Some(asset) = state.assets.get(master_idx).cloned() else {
                        return;
                    };
                    asset
                };
                let route_path = {
                    let Ok(app_guard) = app.lock() else { return };
                    app_guard
                        .selected_route
                        .and_then(|i| app_guard.routes.get(i))
                        .map(|r| r.path.clone())
                };
                let Some(route_path) = route_path else { return };

                if asset
                    .uses
                    .iter()
                    .any(|u| u.source_file.is_protected_pathway())
                {
                    if let Ok(mut app_guard) = app.lock() {
                        app_guard.log_event(
                            crate::gettext!("You can't change the track infrastructure parts through the program!"),
                            LogLevel::Warning,
                        );
                        schedule_ui_sync();
                    }
                    return;
                }

                let scenery_uses: Vec<_> = asset
                    .uses
                    .iter()
                    .filter(|u| u.tile_kind == TileKind::Scenery)
                    .cloned()
                    .collect();
                let loft_road_uses: Vec<_> = asset
                    .uses
                    .iter()
                    .filter(|u| matches!(u.tile_kind, TileKind::Loft | TileKind::Road))
                    .cloned()
                    .collect();

                let loft_tiles: HashSet<String> = loft_road_uses
                    .iter()
                    .map(|u| u.source_file.display())
                    .collect();
                let scenery_tiles: HashSet<String> = scenery_uses
                    .iter()
                    .map(|u| u.source_file.display())
                    .collect();
                let loft_segments: usize = loft_road_uses.iter().map(|u| u.count as usize).sum();
                let scenery_segments: usize = scenery_uses.iter().map(|u| u.count as usize).sum();

                let description = match (!loft_road_uses.is_empty(), !scenery_uses.is_empty()) {
                    (true, false) => crate::gettext!(
                    "Delete loft {0} (Loft tiles: {1}, segments: {2})? This action will physically change the route files.",
                    asset.path,
                    loft_tiles.len(),
                    loft_segments
                    ),
                    (false, true) => crate::ngettext!(
                        "Delete object {0} from all Scenery tiles ({1} entry)? This action will physically change the route files.",
                        "Delete object {0} from all Scenery tiles ({1} entries)? This action will physically change the route files.",
                        scenery_tiles.len(),
                        asset.path,
                        scenery_tiles.len()
                    ),
                    (true, true) => crate::gettext!(
                        "Delete object {0}?\nScenery — tiles: {1}, segments: {2}\nLoft — tiles: {3}, segments: {4}\nThis action will physically modify the route files.",
                        asset.path,
                        scenery_tiles.len(),
                        scenery_segments,
                        loft_tiles.len(),
                        loft_segments
                    ),
                    (false, false) => {
                        if let Ok(mut app_guard) = app.lock() {
                            app_guard.log_event(
                                crate::gettext!(
                                    "No suitable tiles for object {0} were found to delete.",
                                    asset.path
                                ),
                                LogLevel::Warning,
                            );
                            schedule_ui_sync();
                        }
                        return;
                    }
                };

                let confirmed = rfd::MessageDialog::new()
                    .set_title(crate::gettext!("Object deletion"))
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
                    match delete_scenery_asset(&route_path, &asset.path, &scenery_uses) {
                        Ok((tiles, occurrences, untouched)) => {
                            total_tiles += tiles;
                            total_occurrences += occurrences;
                            untouched_files.extend(untouched);
                        }
                        Err(error) => errors.push(error),
                    }
                }
                if !loft_road_uses.is_empty() {
                    match delete_loft_asset(&route_path, &asset.path, &loft_road_uses) {
                        Ok((tiles, occurrences, untouched)) => {
                            total_tiles += tiles;
                            total_occurrences += occurrences;
                            untouched_files.extend(untouched);
                        }
                        Err(error) => errors.push(error),
                    }
                }

                if errors.is_empty()
                    && let Ok(mut state) = state_arc.lock()
                {
                    state.assets.retain(|a| a.path != asset.path);
                    if let Some(inv) = handle.upgrade() {
                        Self::update_inventory_view(&inv, &mut state);
                        inv.set_selected_asset(-1);
                        inv.set_current_row(-1);
                    }
                }

                if let Ok(mut app_guard) = app.lock() {
                    if errors.is_empty() {
                        app_guard.log_event(
                            crate::gettext!(
                                "Object {0} deleted (tiles: {1}, uses: {2})",
                                asset.path,
                                total_tiles,
                                total_occurrences
                            ),
                            LogLevel::Info,
                        );
                        if !untouched_files.is_empty() {
                            app_guard.log_event(
                                crate::gettext!(
                                    "Warning: it was not possible to delete uses of the object in the following tiles {0} (probably a composite loft, manual removal is required): {1}",
                                    asset.path,
                                    untouched_files.join(", ")
                                ),
                                LogLevel::Warning,
                            );
                        }
                    } else {
                        for error in &errors {
                            app_guard.log_event(error.clone(), LogLevel::Error);
                        }
                    }
                    schedule_ui_sync();
                }
            });
        }

        {
            let app = self.app.clone();
            let handle = self.weak.clone();
            self.window.on_replace_inventory_asset(move |selected| {
                let Some(inv) = handle.upgrade() else { return };
                let Some(asset_path) = selected_row_path(&inv, selected) else {
                    return;
                };
                let (rw_path, state_arc) = match app.lock() {
                    Ok(a) => (a.clean_rw_path(), a.asset_inventory_state.clone()),
                    Err(_) => return,
                };
                if rw_path.as_os_str().is_empty() {
                    return;
                }
                let asset = state_arc
                    .lock()
                    .ok()
                    .and_then(|state| state.assets.iter().find(|a| a.path == asset_path).cloned());
                let Some(asset) = asset else { return };

                if !is_safe_for_global_replacement(&asset.path, &asset.uses) {
                    if let Ok(mut app_guard) = app.lock() {
                        app_guard.log_event(
                            crate::gettext!("You can't change the track infrastructure parts through the program!"),
                            LogLevel::Warning,
                        );
                        schedule_ui_sync();
                    }
                    return;
                }

                let assets_dir = rw_path.join("Assets");
                let start_dir =
                    existing_directory_upwards(&asset_directory(&assets_dir, &asset.path))
                        .unwrap_or_else(|| assets_dir.clone());
                let Some(picked) = rfd::FileDialog::new()
                    .set_title(crate::gettext!("Replace on route..."))
                    .add_filter(crate::gettext!("RailWorks Blueprint (*.bin)"), &["bin"])
                    .set_directory(&start_dir)
                    .pick_file()
                else {
                    return;
                };

                let replacement_relative = picked
                    .strip_prefix(&assets_dir)
                    .map(|rel| normalize_rel_slashes(&rel.to_string_lossy()))
                    .unwrap_or_else(|_| normalize_rel_slashes(&picked.to_string_lossy()));

                let asset_type = classify_route_asset_type(&asset.path, &asset.uses);
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

                let missing = MissingAsset {
                    path: asset.path,
                    uses,
                    suggested_repair: None,
                    is_corrupted_link: false,
                    asset_type,
                };
                if let Ok(mut app_guard) = app.lock() {
                    app_guard.inventory_replacement_pending_check = true;
                    app_guard.asset_replacement_queue.push_back((
                        missing,
                        picked,
                        replacement_relative,
                    ));
                    app_guard.start_next_asset_replacement();
                }
            });
        }
    }

    pub fn refresh(&self) {
        let state_arc = {
            let Ok(mut app_guard) = self.app.lock() else {
                return;
            };
            if !app_guard.inventory_refresh_pending {
                return;
            }
            app_guard.inventory_refresh_pending = false;
            app_guard.asset_inventory_state.clone()
        };
        let Some(inv) = self.weak.upgrade() else {
            return;
        };
        if let Ok(mut state) = state_arc.lock() {
            Self::update_inventory_view(&inv, &mut state);
        }
    }
}

fn apply_sort(
    handle: &Weak<AssetInventoryWindow>,
    app: &Arc<Mutex<CheckerApp>>,
    column: i32,
    descending: bool,
) {
    let Some(inv) = handle.upgrade() else { return };
    let state_arc = match app.lock() {
        Ok(a) => a.asset_inventory_state.clone(),
        Err(_) => return,
    };
    if let Ok(mut state) = state_arc.lock() {
        match column {
            0 => {
                state.sort.column = InventorySortColumn::Path;
                state.sort.descending = descending;
            }
            1 => {
                state.sort.column = InventorySortColumn::Usage;
                state.sort.descending = descending;
            }
            _ => {}
        }
        InventoryController::update_inventory_view(&inv, &mut state);
    }
}

fn selected_row_path(inv: &AssetInventoryWindow, row: i32) -> Option<String> {
    if row < 0 {
        return None;
    }
    let rows = inv.get_inventory_rows();
    if (row as usize) >= rows.row_count() {
        return None;
    }
    let path_text = rows
        .row_data(row as usize)
        .and_then(|rm| rm.row_data(0))
        .map(|c| c.text.to_string())
        .unwrap_or_default();
    (!path_text.is_empty()).then_some(path_text)
}

fn normalize_rel_slashes(s: &str) -> String {
    if s.contains('\\') {
        s.replace('\\', "/")
    } else {
        s.to_owned()
    }
}

fn resolve_asset_full_path(rw_path: &Path, path_text: &str) -> PathBuf {
    let rel = if path_text.contains('/') {
        path_text.replace('/', "\\")
    } else {
        path_text.to_owned()
    };
    rw_path.join("Assets").join(rel)
}
