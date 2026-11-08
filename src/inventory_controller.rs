use crate::AssetInventoryWindow;
use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
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
        state: &crate::app::AssetInventoryWindowState,
    ) {
        let show_missing = inv.get_show_missing_only();
        let (rows, safe) = crate::inventory_ui::build_inventory_rows(state, show_missing);
        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
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
                    && let Ok(state) = app.asset_inventory_state.lock()
                {
                    Self::update_inventory_view(&inv, &state);
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
                    Self::update_inventory_view(&inv, &state);
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
                    Self::update_inventory_view(&inv, &state);
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
                    Self::update_inventory_view(&inv, &state);
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
                        let _ = crate::windows::open_in_explorer(&full);
                    } else if let Some(target) =
                        crate::inventory_ui::existing_directory_upwards(&full)
                    {
                        let _ = crate::windows::open_in_file_explorer(&target);
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
                                app.log_event(match result {
                                    Ok(msg) => msg,
                                    Err(err) => format!("✗ {err}"),
                                });
                                crate::schedule_ui_sync();
                            }
                        });
                }
            });
        }
        {
            let handle = self.weak.clone();
            let app = self.app.clone();
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
            self.window.on_scan_umlauts(move || {
                if let Ok(app) = app.lock() {
                    let _ = app
                        .inventory_action_tx
                        .send(crate::app::InventoryAction::ScanUmlauts);
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
        if let Ok(state) = app.asset_inventory_state.lock() {
            Self::update_inventory_view(&inv, &state);
        }
        app.inventory_refresh_pending = false;
    }
}
