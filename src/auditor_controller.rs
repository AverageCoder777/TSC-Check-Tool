use slint::ComponentHandle;
use slint::Weak;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{AuditorState, FolderAuditorWindow};
use std::path::PathBuf;

pub struct AuditorController {
    pub window: FolderAuditorWindow,
    pub state: Arc<Mutex<AuditorState>>,
    pub weak: Weak<FolderAuditorWindow>,
    pub audit_generation: Arc<AtomicU64>,
}

impl AuditorController {
    pub fn new() -> Self {
        let window = FolderAuditorWindow::new().unwrap();
        let weak = window.as_weak();
        let state = Arc::new(Mutex::new(AuditorState::default()));
        Self { window, state, weak, audit_generation: Arc::new(AtomicU64::new(0)) }
    }

    pub fn bind(&self, _ui_weak: Weak<crate::MainWindow>) {
        let audit_generation = self.audit_generation.clone();
        let auditor_handle2 = self.weak.clone();
        let auditor_state_for_start = self.state.clone();
        let auditor_state_for_filter = self.state.clone();
        let auditor_state_for_sort = self.state.clone();
        let auditor_state_for_open = self.state.clone();
        let auditor_state_for_edit = self.state.clone();

        self.window.on_pick_folder(move || {
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
    let auditor_weak_for_start = self.weak.clone();
        self.window.on_start_audit(move |folder_path| {
        println!("[Auditor] start-audit callback: {}", folder_path);
        let window_weak = auditor_weak_for_start.clone();
        let path = PathBuf::from(folder_path.as_str());
        if !path.exists() || !path.is_dir() {
            eprintln!("[Auditor] Invalid folder path: {:?}", path);
            return;
        }
        let rw_path = crate::app_state()
            .lock()
            .map(|a| a.clean_rw_path())
            .unwrap_or_default();
        let max_threads = crate::app_state()
            .lock()
            .map(|a| a.serz_process_limit)
            .unwrap_or(8)
            .max(1);
        let serz_path = crate::assets::preferred_serz_path(&rw_path);
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
        let ap_cache = crate::auditor::ApArchiveCache::new();
        let generation = audit_generation.clone();
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        let path_for_thread = path;
        let window_weak_thread = window_weak;
        let state_store = auditor_state_for_start.clone();
        let results_store = state_store.clone();
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
            if generation.load(Ordering::SeqCst) != generation_id {
                return;
            }
            if let Ok(mut state) = results_store.lock() {
                state.all_items = report.broken_references.clone();
            }
            let (col, asc) = match state_store.lock() {
                Ok(s) => (s.sort_col, s.sort_asc),
                Err(_) => (0, true),
            };
            let filter = window_weak_thread
                .upgrade()
                .map(|w| w.get_active_filter().to_string())
                .unwrap_or_else(|| "all".to_string());
            let mut items: Vec<crate::auditor::BrokenResourceRef> = report.broken_references;
            crate::auditor::filter_items(&mut items, &filter);
            crate::auditor::sort_items(&mut items, col, asc);
            if let Ok(mut state) = state_store.lock() {
                state.display_items = items.clone();
            }
            if generation.load(Ordering::SeqCst) != generation_id {
                return;
            }
            let ww = window_weak_thread;
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww.upgrade() {
                    w.set_broken_rows(crate::map_broken_resources_to_ui(&items));
                    w.set_audit_progress(1.0);
                    w.set_audit_status(format!("Готово: {} проблем", items.len()).into());
                    w.set_selected_row(-1);
                }
            })
            .ok();
        });
    });
    let auditor_weak_for_filter = self.weak.clone();
        self.window.on_filter_changed(move |filter| {
        println!("[Auditor] filter-changed: {}", filter);
        let ww = auditor_weak_for_filter.clone();
        let state_store = auditor_state_for_filter.clone();
        let all_items = state_store.lock().map(|g| g.all_items.clone()).unwrap_or_default();
        let (col, asc) = state_store.lock().map(|g| (g.sort_col, g.sort_asc)).unwrap_or((0, true));
        let mut items = all_items;
        crate::auditor::filter_items(&mut items, filter.as_str());
        crate::auditor::sort_items(&mut items, col, asc);
        if let Ok(mut state) = state_store.lock() {
            state.display_items = items.clone();
        }
        slint::invoke_from_event_loop(move || {
            if let Some(w) = ww.upgrade() {
                w.set_active_filter(filter.clone());
                w.set_broken_rows(crate::map_broken_resources_to_ui(&items));
                w.set_selected_row(-1);
            }
        })
        .ok();
    });
    let auditor_weak_for_open = self.weak.clone();
    let auditor_state_for_open_source = auditor_state_for_open.clone();
        self.window.on_open_source_file(move |row| {
        let items = auditor_state_for_open_source
            .lock()
            .map(|g| g.display_items.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let target_path = items[row as usize].parent_file.clone();
        println!("[Auditor] Opening path: {:?}", target_path);
        let _ = crate::windows::open_in_explorer(&target_path);
        let _ = auditor_weak_for_open;
    });
    let auditor_weak_for_missing = self.weak.clone();
    let auditor_state_for_missing_folder = auditor_state_for_open.clone();
        self.window.on_open_missing_target_folder(move |row| {
        let items = auditor_state_for_missing_folder
            .lock()
            .map(|g| g.display_items.clone())
            .unwrap_or_default();
        if row < 0 || row as usize >= items.len() {
            return;
        }
        let target_path = items[row as usize].expected_path.clone();
        println!("[Auditor] Opening path: {:?}", target_path);
        let _ = crate::windows::open_in_explorer(&target_path);
        let _ = auditor_weak_for_missing;
    });
    let auditor_weak_for_sort = self.weak.clone();
    let auditor_state_for_sort_closure = auditor_state_for_sort.clone();
        self.window.on_sort_column(move |col| {
        let col = col as usize;
        let col_now;
        let asc_now;
        let all_items_snapshot;
        {
            let mut state = auditor_state_for_sort_closure.lock().unwrap();
            if state.sort_col == col {
                state.sort_asc = !state.sort_asc;
            } else {
                state.sort_col = col;
                state.sort_asc = true;
            }
            col_now = state.sort_col;
            asc_now = state.sort_asc;
            all_items_snapshot = state.all_items.clone();
        }
        let filter = auditor_weak_for_sort
            .upgrade()
            .map(|w| w.get_active_filter().to_string())
            .unwrap_or_else(|| "all".to_string());
        let mut items = all_items_snapshot;
        crate::auditor::filter_items(&mut items, &filter);
        crate::auditor::sort_items(&mut items, col_now, asc_now);
        if let Ok(mut state) = auditor_state_for_sort_closure.lock() {
            state.display_items = items.clone();
        }
        let ww = auditor_weak_for_sort.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(w) = ww.upgrade() {
                w.set_sort_col(col_now as i32);
                w.set_sort_ascending(asc_now);
                w.set_broken_rows(crate::map_broken_resources_to_ui(&items));
                w.set_selected_row(-1);
            }
        })
        .ok();
    });
    let auditor_weak_for_edit = self.weak.clone();
    let auditor_state_for_edit_closure = auditor_state_for_edit.clone();
        self.window.on_edit_in_vscode(move |row| {
        let items = auditor_state_for_edit_closure
            .lock()
            .map(|g| g.display_items.clone())
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
        let rw_path = crate::app_state()
            .lock()
            .map(|a| a.clean_rw_path())
            .unwrap_or_default();
        let serz_path = crate::assets::find_serz_path(&asset_path)
            .or_else(|| {
                let p = crate::assets::preferred_serz_path(&rw_path);
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

    }
}
