use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::AssetInventoryWindow;

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

    pub fn bind(&self) {
        let inv_handle2 = self.weak.clone();
        let inv_handle3 = self.weak.clone();
        let inv_handle4 = self.weak.clone();
        let inv_handle5 = self.weak.clone();
        let inv_handle9 = self.weak.clone();
        let inv_handle10 = self.weak.clone();
        let inv_handle11 = self.weak.clone();

        // toggle missing only
        let app = self.app.clone();
        self.window.on_toggle_missing_only(move || {
            if let Some(inv) = inv_handle2.upgrade() {
                let cur = inv.get_show_missing_only();
                inv.set_show_missing_only(!cur);
                let show_missing = !cur;
                if let Ok(app) = app.lock()
                    && let Ok(state) = app.asset_inventory_state.lock()
                {
                    let (rows, safe) = crate::inventory_ui::build_inventory_rows(&state, show_missing);
                    inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                    inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                }
            }
        });

        // sort ascending
        let app = self.app.clone();
        self.window.on_sort_ascending(move |column| {
            if let Some(inv) = inv_handle3.upgrade() {
                let show_missing = inv.get_show_missing_only();
                if let Ok(app) = app.lock() {
                    if let Ok(mut state) = app.asset_inventory_state.lock() {
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
                    }
                    if let Ok(state) = app.asset_inventory_state.lock() {
                        let (rows, safe) = crate::inventory_ui::build_inventory_rows(&state, show_missing);
                        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    }
                }
            }
        });

        // sort descending
        let app = self.app.clone();
        self.window.on_sort_descending(move |column| {
            if let Some(inv) = inv_handle4.upgrade() {
                let show_missing = inv.get_show_missing_only();
                if let Ok(app) = app.lock() {
                    if let Ok(mut state) = app.asset_inventory_state.lock() {
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
                    }
                    if let Ok(state) = app.asset_inventory_state.lock() {
                        let (rows, safe) = crate::inventory_ui::build_inventory_rows(&state, show_missing);
                        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    }
                }
            }
        });

        // search changed
        let app = self.app.clone();
        self.window.on_search_changed(move |text| {
            if let Some(inv) = inv_handle5.upgrade() {
                let show_missing = inv.get_show_missing_only();
                if let Ok(app) = app.lock() {
                    if let Ok(mut state) = app.asset_inventory_state.lock() {
                        state.search = text.to_string();
                    }
                    if let Ok(state) = app.asset_inventory_state.lock() {
                        let (rows, safe) = crate::inventory_ui::build_inventory_rows(&state, show_missing);
                        inv.set_inventory_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                        inv.set_inventory_is_safe(slint::ModelRc::new(slint::VecModel::from(safe)));
                    }
                }
            }
        });

        // row selected (double click)
        let mut inv_last_row: i32 = -1;
        let mut inv_last_click = Instant::now();
        const INV_DOUBLE_CLICK_MS: u64 = 400;
        let app = self.app.clone();
        self.window.on_row_selected(move |row| {
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
                let rw_path = if let Ok(app) = app.lock() {
                    app.rw_path.clone()
                } else {
                    String::new()
                };
                if rw_path.is_empty() {
                    return;
                }
                let full = crate::app::clean_rw_path(&rw_path)
                    .join("Assets")
                    .join(path_text.replace('/', "\\"));
                let target = if full.exists() {
                    let _ = crate::windows::open_in_explorer(&full);
                    return;
                } else {
                    let mut cur = full.as_path();
                    while !cur.exists() {
                        match cur.parent() {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                    cur.to_path_buf()
                };
                let _ = crate::windows::open_in_file_explorer(&target);
            }
        });

        // open in editor
        let app = self.app.clone();
        self.window.on_open_in_editor(move || {
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
                let rw_path = if let Ok(app) = app.lock() {
                    app.rw_path.clone()
                } else {
                    String::new()
                };
                if rw_path.is_empty() {
                    return;
                }
                let full = crate::app::clean_rw_path(&rw_path)
                    .join("Assets")
                    .join(path_text.replace('/', "\\"));
                if full.is_file() {
                    let serz_path = crate::assets::find_serz_path(&full)
                        .unwrap_or_else(|| PathBuf::from("serz64.exe"));
                    let app_for_status = app.clone();
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

        // replace inventory asset
        let app = self.app.clone();
        self.window.on_replace_inventory_asset(move |selected| {
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
            let rw_path = if let Ok(app) = app.lock() {
                app.rw_path.clone()
            } else {
                return;
            };
            if crate::app::clean_rw_path(&rw_path).as_os_str().is_empty() {
                return;
            }
            let asset = if let Ok(app) = app.lock() {
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
                if let Ok(mut app) = app.lock() {
                    app.status = "Замена этого ассета запрещена (путевая инфраструктура).".to_owned();
                }
                return;
            }

            let assets_dir = crate::app::clean_rw_path(&rw_path).join("Assets");
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

        // scan umlauts
        let app = self.app.clone();
        self.window.on_scan_umlauts(move || {
            if let Ok(app) = app.lock() {
                let _ = app
                    .inventory_action_tx
                    .send(crate::app::InventoryAction::ScanUmlauts);
            }
        });
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
