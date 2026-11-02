use slint::ComponentHandle;
use slint::Weak;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{AuditorState, FolderAuditorWindow};

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
        Self {
            window,
            state,
            weak,
            audit_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn bind(&self, _ui_weak: Weak<crate::MainWindow>) {
        // Выбор папки для аудита
        {
            let handle = self.weak.clone();
            self.window.on_pick_folder(move || {
                let Some(w) = handle.upgrade() else { return };
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
            });
        }

        // Запуск аудита
        {
            let handle = self.weak.clone();
            let state = self.state.clone();
            let audit_generation = self.audit_generation.clone();

            self.window.on_start_audit(move |folder_path| {
                println!("[Auditor] start-audit callback: {}", folder_path);
                let path = PathBuf::from(folder_path.as_str());
                if !path.is_dir() {
                    eprintln!("[Auditor] Invalid folder path: {:?}", path);
                    return;
                }

                // Сбрасываем UI в исходное состояние
                if let Some(w) = handle.upgrade() {
                    w.set_audit_progress(0.0);
                    w.set_audit_status("Подготовка к аудиту...".into());
                    w.set_broken_rows(slint::ModelRc::new(slint::VecModel::default()));
                    w.set_selected_row(-1);
                }

                // Получаем параметры игры под единой блокировкой
                let (rw_path, max_threads) = crate::app_state()
                    .lock()
                    .map(|a| (a.clean_rw_path(), a.serz_process_limit.max(1)))
                    .unwrap_or_else(|_| (PathBuf::new(), 8));

                let serz_path = crate::assets::preferred_serz_path(&rw_path);

                // Определение корня Assets
                let assets_root = path
                    .ancestors()
                    .find(|p| {
                        p.file_name()
                            .map(|name| name.to_string_lossy().eq_ignore_ascii_case("Assets"))
                            .unwrap_or(false)
                    })
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| {
                        let rw_assets = rw_path.join("Assets");
                        if rw_assets.is_dir() {
                            rw_assets
                        } else {
                            path.parent()
                                .and_then(|p| p.parent())
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| path.clone())
                        }
                    });

                let filter = handle
                    .upgrade()
                    .map(|w| w.get_active_filter().to_string())
                    .unwrap_or_else(|| "all".to_string());

                let generation_id = audit_generation.fetch_add(1, Ordering::SeqCst) + 1;
                let generation = audit_generation.clone();
                let thread_handle = handle.clone();
                let thread_state = state.clone();
                let ap_cache = crate::auditor::ApArchiveCache::new();

                std::thread::spawn(move || {
                    println!("[Auditor] Starting audit for: {:?}", path);

                    let progress_handle = thread_handle.clone();
                    let progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
                        Arc::new(move |done, total| {
                            let h = progress_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = h.upgrade() {
                                    w.set_audit_progress(done as f32 / total as f32);
                                    w.set_audit_status(
                                        format!("Сканирование: {} / {} файлов...", done, total).into(),
                                    );
                                }
                            });
                        });

                    let report = crate::auditor::audit_folder_with_progress(
                        &path,
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

                    // Проверяем, не был ли запущен новый аудит, пока этот завершался
                    if generation.load(Ordering::SeqCst) != generation_id {
                        return;
                    }

                    let mut items = report.broken_references;
                    let graph = report.dependency_graph;
                    let summer_candidates = report.summer_candidates;
                    let (col, asc) = if let Ok(mut s) = thread_state.lock() {
                        s.all_items = items.clone();
                        s.dependency_graph = graph;
                        s.summer_candidates = summer_candidates.clone();
                        (s.sort_col, s.sort_asc)
                    } else {
                        (0, true)
                    };

                    crate::auditor::filter_items(&mut items, &filter);
                    crate::auditor::sort_items(&mut items, col, asc);

                    if let Ok(mut s) = thread_state.lock() {
                        s.display_items = items.clone();
                    }

                    if generation.load(Ordering::SeqCst) != generation_id {
                        return;
                    }

                    let total_bytes: u64 = summer_candidates.iter().map(|c| c.size_bytes).sum();
                    let size_mb = total_bytes as f64 / (1024.0 * 1024.0);
                    let size_text = if summer_candidates.is_empty() {
                        String::new()
                    } else if size_mb >= 1.0 {
                        format!("{:.1} МБ", size_mb)
                    } else {
                        format!("{:.0} КБ", total_bytes as f64 / 1024.0)
                    };
                    let cand_count = summer_candidates.len() as i32;

                    let h = thread_handle;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = h.upgrade() {
                            w.set_broken_rows(crate::map_broken_resources_to_ui(&items));
                            w.set_audit_progress(1.0);
                            w.set_audit_status(format!("Готово: {} проблем", items.len()).into());
                            w.set_selected_row(-1);
                            w.set_summer_candidate_count(cand_count);
                            w.set_summer_candidate_size_text(slint::SharedString::from(size_text));
                            w.set_is_consolidating(false);
                        }
                    });
                });
            });
        }

        // Изменение фильтра
        {
            let handle = self.weak.clone();
            let state = self.state.clone();

            self.window.on_filter_changed(move |filter| {
                let Some(w) = handle.upgrade() else { return };
                let mut items = state
                    .lock()
                    .map(|s| s.all_items.clone())
                    .unwrap_or_default();

                let (col, asc) = state
                    .lock()
                    .map(|s| (s.sort_col, s.sort_asc))
                    .unwrap_or((0, true));

                crate::auditor::filter_items(&mut items, filter.as_str());
                crate::auditor::sort_items(&mut items, col, asc);

                if let Ok(mut s) = state.lock() {
                    s.display_items = items.clone();
                }

                w.set_active_filter(filter);
                w.set_broken_rows(crate::map_broken_resources_to_ui(&items));
                w.set_selected_row(-1);
            });
        }

        // Обновление панели зависимостей при выборе строки
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            self.window.on_row_selected(move |row| {
                if row < 0 {
                    if let Some(w) = handle.upgrade() {
                        w.set_incoming_text("".into());
                        w.set_outgoing_text("".into());
                    }
                    return;
                }
                let (incoming, outgoing) = state.lock().ok().map(|s| {
                    let item = s.display_items.get(row as usize);
                    let path = item.map(|i| {
                        let p = if i.kind == crate::auditor::AssetResourceKind::Unused {
                            &i.expected_path
                        } else {
                            &i.parent_file
                        };
                        let np = crate::fs_utils::normalize_path(p);
                        std::path::PathBuf::from(np.to_string_lossy().to_lowercase())
                    });
                    let incoming = path.as_ref().and_then(|p| s.dependency_graph.incoming.get(p));
                    let outgoing = path.as_ref().and_then(|p| s.dependency_graph.outgoing.get(p));
                    let fmt = |set: Option<&std::collections::HashSet<std::path::PathBuf>>, empty: &str| {
                        match set {
                            Some(s) if !s.is_empty() => {
                                if s.len() > 3 {
                                    let names: Vec<String> = s
                                        .iter()
                                        .take(3)
                                        .map(|p| {
                                            p.file_name()
                                                .map(|n| n.to_string_lossy().to_string())
                                                .unwrap_or_else(|| p.display().to_string())
                                        })
                                        .collect();
                                    format!("{} файлов: {}", s.len(), names.join(", "))
                                } else {
                                    s.iter()
                                        .map(|p| {
                                            p.file_name()
                                                .map(|n| n.to_string_lossy().to_string())
                                                .unwrap_or_else(|| p.display().to_string())
                                        })
                                        .collect::<Vec<_>>()
                                        .join("; ")
                                }
                            }
                            _ => empty.to_string(),
                        }
                    };
                    let incoming_str = fmt(incoming, "Не используется ни одним чертежом (.bin) или мешем (.GeoPcDx) в продукте");
                    let outgoing_str = fmt(outgoing, "Нет зависимостей");
                    (incoming_str, outgoing_str)
                }).unwrap_or_else(|| ("Не используется ни одним чертежом (.bin) или мешем (.GeoPcDx) в продукте".to_string(), "Нет зависимостей".to_string()));
                if let Some(w) = handle.upgrade() {
                    w.set_incoming_text(incoming.into());
                    w.set_outgoing_text(outgoing.into());
                }
            });
        }
        {
            let state = self.state.clone();
            self.window.on_open_source_file(move |row| {
                if row < 0 {
                    return;
                }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items.get(row as usize).map(|item| item.parent_file.clone())
                });

                if let Some(path) = target_path {
                    println!("[Auditor] Opening path: {:?}", path);
                    let _ = crate::windows::open_in_explorer(&path);
                }
            });
        }

        // Открыть ожидаемую целевую папку в проводнике
        {
            let state = self.state.clone();
            self.window.on_open_missing_target_folder(move |row| {
                if row < 0 {
                    return;
                }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items.get(row as usize).map(|item| item.expected_path.clone())
                });

                if let Some(path) = target_path {
                    println!("[Auditor] Opening target path: {:?}", path);
                    let _ = crate::windows::open_in_explorer(&path);
                }
            });
        }

        // Сортировка по колонкам (in-place без повторного клонирования всей базы)
        {
            let handle = self.weak.clone();
            let state = self.state.clone();

            self.window.on_sort_column(move |col| {
                let Some(w) = handle.upgrade() else { return };
                let col = col as usize;

                let Ok(mut s) = state.lock() else { return };

                if s.sort_col == col {
                    s.sort_asc = !s.sort_asc;
                } else {
                    s.sort_col = col;
                    s.sort_asc = true;
                }

                let col_now = s.sort_col;
                let asc_now = s.sort_asc;

                // Сортируем текущую выборку на месте
                crate::auditor::sort_items(&mut s.display_items, col_now, asc_now);
                let rows_model = crate::map_broken_resources_to_ui(&s.display_items);

                w.set_sort_col(col_now as i32);
                w.set_sort_ascending(asc_now);
                w.set_broken_rows(rows_model);
                w.set_selected_row(-1);
            });
        }

        // Редактирование в VS Code
        {
            let handle = self.weak.clone();
            let state = self.state.clone();

            self.window.on_edit_in_vscode(move |row| {
                if row < 0 {
                    return;
                }

                let item_paths = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|i| (i.parent_file.clone(), i.expected_path.clone()))
                });

                let Some((parent_file, expected_path)) = item_paths else {
                    return;
                };

                let is_editable = |p: &PathBuf| {
                    p.extension()
                        .map(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("geopcdx") || e.eq_ignore_ascii_case("xsec"))
                        .unwrap_or(false)
                };

                let asset_path = if is_editable(&parent_file) {
                    parent_file
                } else if is_editable(&expected_path) {
                    expected_path
                } else {
                    if let Some(win) = handle.upgrade() {
                        win.set_editor_status("Выбранный элемент не является .bin/.GeoPcDx/.XSec".into());
                    }
                    return;
                };

                let rw_path = crate::app_state()
                    .lock()
                    .map(|a| a.clean_rw_path())
                    .unwrap_or_default();

                let serz_path = crate::assets::find_serz_path(&asset_path)
                    .or_else(|| {
                        let p = crate::assets::preferred_serz_path(&rw_path);
                        if p.is_file() {
                            Some(p)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| PathBuf::from("serz.exe"));

                let thread_handle = handle.clone();
                std::thread::spawn(move || {
                    let cb_handle = thread_handle.clone();
                    let callback = move |res: Result<String, String>| {
                        let msg = match res {
                            Ok(s) => s,
                            Err(e) => format!("✗ {e}"),
                        };
                        let h = cb_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(win) = h.upgrade() {
                                win.set_editor_status(msg.into());
                            }
                        });
                    };

                    if let Err(e) = crate::external_editor::open_and_sync_with_vscode(
                        &asset_path,
                        &serz_path,
                        callback,
                    ) {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(win) = thread_handle.upgrade() {
                                win.set_editor_status(format!("✗ {e}").into());
                            }
                        });
                    }
                });
            });
        }

        // Консолидация летних текстур
        {
            let handle = self.weak.clone();
            let state = self.state.clone();

            self.window.on_consolidate_summer_textures(move || {
                let Some(w) = handle.upgrade() else { return };
                let candidates = state
                    .lock()
                    .map(|s| s.summer_candidates.clone())
                    .unwrap_or_default();
                if candidates.is_empty() {
                    return;
                }
                w.set_is_consolidating(true);

                let ui_weak_async = handle.clone();
                let state_async = state.clone();

                std::thread::spawn(move || {
                    let mut succeeded = 0usize;
                    let mut freed_bytes: u64 = 0;

                    for cand in &candidates {
                        if crate::auditor::apply_summer_consolidation(cand).is_ok() {
                            succeeded += 1;
                            freed_bytes += cand.size_bytes;
                        }
                    }

                    let freed_mb = freed_bytes as f64 / (1024.0 * 1024.0);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = ui_weak_async.upgrade() {
                            w.set_is_consolidating(false);
                            w.set_summer_candidate_count(0);
                            w.set_summer_candidate_size_text(slint::SharedString::from(""));
                            let msg = format!(
                                "Оптимизировано {} текстур, освобождено {:.1} МБ",
                                succeeded, freed_mb
                            );
                            w.set_audit_status(slint::SharedString::from(msg));
                        }
                    });

                    if let Ok(mut s) = state_async.lock() {
                        s.summer_candidates.clear();
                    }
                });
            });
        }
    }}
