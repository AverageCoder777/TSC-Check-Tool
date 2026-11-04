
use slint::ComponentHandle;
use slint::Weak;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{AuditorState, FolderAuditorWindow};

/// Validates that `path` is a real directory strictly inside an `Assets` ancestor
/// (at least one level deep: `Assets/<Provider>` or `Assets/<Provider>/<Product>`).
/// Rejects the `Assets` root itself and any path not inside `Assets`.
pub fn validate_audit_folder(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(crate::gettext!("Указанный путь не существует или не является папкой!").to_string());
    }

    // Locate the 'Assets' ancestor
    let assets_ancestor = path.ancestors().find(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("Assets"))
            .unwrap_or(false)
    });

    let Some(assets_root) = assets_ancestor else {
        return Err(crate::gettext!("Папка должна находиться внутри каталога RailWorks/Assets!").to_string());
    };

    // If path is identical to the Assets root itself, reject it
    if path == assets_root
        || path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Assets"))
    {
        return Err(crate::gettext!("Нельзя сканировать весь каталог Assets целиком! Выберите папку провайдера или продукта (например, Assets/Kuju).").to_string());
    }

    // Ensure it is strictly inside Assets (rel components >= 1)
    if let Ok(rel) = path.strip_prefix(assets_root) && rel.components().count() == 0 {
        return Err(crate::gettext!("Нельзя сканировать весь каталог Assets целиком!").to_string());
    }

    Ok(path.to_path_buf())
}

pub struct AuditorController {
    pub window: FolderAuditorWindow,
    pub state: Arc<Mutex<AuditorState>>,
    pub weak: Weak<FolderAuditorWindow>,
    pub audit_generation: Arc<AtomicU64>,
    pub is_busy: Arc<AtomicBool>,
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
            is_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn bind(&self, _ui_weak: Weak<crate::MainWindow>) {
        // Pick folder
        {
            let handle = self.weak.clone();
            self.window.on_pick_folder(move || {
                let Some(w) = handle.upgrade() else { return };
                let current = w.get_folder_path().to_string();
                let mut dialog = rfd::FileDialog::new().set_title(crate::gettext!("Выберите одну или несколько папок для аудита"));
                if !current.is_empty() {
                    let p = PathBuf::from(current.split(';').next().unwrap_or("").trim());
                    if p.is_dir() {
                        dialog = dialog.set_directory(&p);
                    }
                }
                if let Some(folders) = dialog.pick_folders() {
                    let valid_folders: Vec<PathBuf> = folders
                        .into_iter()
                        .filter_map(|p| validate_audit_folder(&p).ok())
                        .collect();

                    if valid_folders.is_empty() {
                        w.set_scan_status_text(crate::gettext!("Ошибка: выбранные папки должны находится внутри каталога Assets и не быть самим каталогом Assets!").into());
                        return;
                    }

                    let joined = valid_folders
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                    w.set_folder_path(joined.into());
                }
            });
        }

        // Start audit
        {
            let handle = self.weak.clone();
            let state = self.state.clone();
            let audit_generation = self.audit_generation.clone();
            let is_busy = self.is_busy.clone();

            self.window.on_start_audit(move |folder_path| {
                if is_busy.swap(true, Ordering::SeqCst) {
                    return;
                }

                let paths: Vec<PathBuf> = folder_path
                    .as_str()
                    .split([';', ','])
                    .map(|s| s.trim().trim_matches('"').trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();

                if paths.is_empty() {
                    eprintln!("[Auditor] No folder paths provided");
                    is_busy.store(false, Ordering::SeqCst);
                    return;
                }

                // Validate every path strictly inside Assets (not the Assets root itself).
                for p in &paths {
                    match validate_audit_folder(p) {
                        Ok(_) => {}
                        Err(msg) => {
                            eprintln!("[Auditor] Invalid folder path: {:?} -> {}", p, msg);
                            if let Some(w) = handle.upgrade() {
                                w.set_scan_status_text(msg.into());
                                w.set_is_scanning(false);
                                w.set_is_scan_completed(false);
                            }
                            is_busy.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }

                if let Some(w) = handle.upgrade() {
                    w.set_scan_progress(0.0);
                    w.set_scan_status_text(crate::gettext!("Подготовка к аудиту...").into());
                    w.set_filtered_rows(slint::ModelRc::new(slint::VecModel::default()));
                    w.set_selected_row_index(-1);
                    w.set_is_selected_texture(false);
                    w.set_has_missing_file_selected(false);
                    w.set_incoming_refs_preview("".into());
                    w.set_outgoing_refs_preview("".into());
                    w.set_is_scanning(true);
                    w.set_is_scan_completed(false);
                }

                let (rw_path, max_threads) = crate::app_state()
                    .lock()
                    .map(|a| (a.clean_rw_path(), a.serz_process_limit.max(1)))
                    .unwrap_or_else(|_| (PathBuf::new(), 8));

                let serz_path = crate::assets::preferred_serz_path(&rw_path);

                let assets_root = paths[0]
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
                            paths[0]
                                .parent()
                                .and_then(|p| p.parent())
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| paths[0].clone())
                        }
                    });

                let filter = handle
                    .upgrade()
                    .map(|w| w.get_current_filter().to_string())
                    .unwrap_or_else(|| "all".to_string());

                let generation_id = audit_generation.fetch_add(1, Ordering::SeqCst) + 1;
                let generation = audit_generation.clone();
                let thread_handle = handle.clone();
                let thread_state = state.clone();
                let is_busy_thread = is_busy.clone();
                let ap_cache = crate::auditor::ApArchiveCache::new();

                std::thread::spawn(move || {
                    let progress_handle = thread_handle.clone();
                    let progress_gen = generation.clone();
                    let progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
                        Arc::new(move |done, total| {
                            if progress_gen.load(Ordering::Relaxed) != generation_id {
                                return;
                            }
                            let h = progress_handle.clone();
                            let progress_gen2 = progress_gen.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if progress_gen2.load(Ordering::Relaxed) != generation_id {
                                    return;
                                }
                                if let Some(w) = h.upgrade() {
                                    let progress = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                                    w.set_scan_progress(progress);
                                    let template = crate::gettext!("Сканирование: {} / {} файлов...");
                                    w.set_scan_status_text(
                                        template
                                            .replacen("{}", &done.to_string(), 1)
                                            .replacen("{}", &total.to_string(), 1)
                                            .into()
                                    );
                                }
                            });
                        });

                    let audit_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::auditor::audit_folders_with_progress(
                            &paths,
                            &assets_root,
                            &serz_path,
                            max_threads,
                            &ap_cache,
                            Some(progress_cb),
                        )
                    }));

                    let report = match audit_result {
                        Ok(r) => r,
                        Err(panic) => {
                            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                                (*s).to_string()
                            } else if let Some(s) = panic.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            eprintln!("[Auditor] panic during audit: {}", msg);
                            let h = thread_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = h.upgrade() {
                                    w.set_scan_status_text(format!("Ошибка аудита: {}", msg).into());
                                    w.set_is_scanning(false);
                                }
                            });
                            is_busy_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                    if generation.load(Ordering::SeqCst) != generation_id {
                        return;
                    }

                    let (report, full_graph) = report;
                    let total_issues = report.broken_references.len();

                    let mut items = report.broken_references;
                    let graph = report.dependency_graph;
                    let (col, asc) = if let Ok(mut s) = thread_state.lock() {
                        s.all_items = items.clone();
                        s.dependency_graph = graph.clone();
                        s.full_asset_graph = full_graph.clone();
                        s.is_scan_completed = true;
                        (s.sort_col, s.sort_asc)
                    } else {
                        (0, true)
                    };

                    // counts from all items (before filter) need recompute; store all_items already
                    let all_for_counts = thread_state.lock().map(|s| s.all_items.clone()).unwrap_or_default();
                    let count_all = all_for_counts.len() as i32;
                    let count_geometry = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::Geometry).count() as i32;
                    let count_textures = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::Texture).count() as i32;
                    let count_scripts = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::Script).count() as i32;
                    let count_sounds = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::Sound).count() as i32;
                    let count_children = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::ChildBlueprint).count() as i32;
                    let count_dead = all_for_counts.iter().filter(|i| i.kind == crate::auditor::AssetResourceKind::Unused).count() as i32;

                    crate::auditor::filter_items(&mut items, &filter);
                    crate::auditor::sort_items(&mut items, col, asc);

                    if let Ok(mut s) = thread_state.lock() {
                        s.display_items = items.clone();
                    }

                    if generation.load(Ordering::SeqCst) != generation_id {
                        return;
                    }

                    let h = thread_handle;
                    let _ = slint::invoke_from_event_loop(move || {
                        if generation.load(Ordering::Relaxed) != generation_id {
                            return;
                        }
                        if let Some(w) = h.upgrade() {
                            w.set_filtered_rows(map_broken_resources_to_ui(&items));
                            w.set_scan_progress(1.0);
                            let final_msg = if total_issues == 0 {
                                crate::gettext!("Проверка завершена: повреждённых или отсутствующих файлов не обнаружено.").to_string()
                            } else {
                                crate::gettext!("Проверка завершена. Найдено проблем: {}.").replace("{}", &total_issues.to_string())
                            };
                            w.set_scan_status_text(final_msg.into());
                            w.set_selected_row_index(-1);
                            w.set_is_selected_texture(false);
                            w.set_has_missing_file_selected(false);
                            w.set_incoming_refs_preview("".into());
                            w.set_outgoing_refs_preview("".into());
                            w.set_count_all(count_all);
                            w.set_count_geometry(count_geometry);
                            w.set_count_textures(count_textures);
                            w.set_count_scripts(count_scripts);
                            w.set_count_sounds(count_sounds);
                            w.set_count_children(count_children);
                            w.set_count_dead(count_dead);
                            w.set_is_consolidating(false);
                            w.set_is_scanning(false);
                            w.set_is_scan_completed(true);
                        }
                    });

                    is_busy_thread.store(false, Ordering::SeqCst);
                });
            });
        }

        // Filter changed
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

                w.set_current_filter(filter);
                w.set_filtered_rows(map_broken_resources_to_ui(&items));
                w.set_selected_row_index(-1);
                w.set_is_selected_texture(false);
                w.set_has_missing_file_selected(false);
                w.set_incoming_refs_preview("".into());
                w.set_outgoing_refs_preview("".into());
            });
        }

        // Row selected
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            self.window.on_row_selected(move |row| {
                if row < 0 {
                    if let Some(w) = handle.upgrade() {
                        w.set_incoming_refs_preview("".into());
                        w.set_outgoing_refs_preview("".into());
                        w.set_is_selected_texture(false);
                        w.set_has_missing_file_selected(false);
                    }
                    return;
                }

                let selected_item = state
                    .lock()
                    .ok()
                    .and_then(|s| s.display_items.get(row as usize).cloned());

                let (is_tex, has_missing) = selected_item
                    .as_ref()
                    .map(|item| {
                        let is_tex_ext = item
                            .parent_file
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .ends_with(".tgpcdx")
                            || item
                                .parent_file
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .ends_with(".dds")
                            || item
                                .expected_path
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .ends_with(".tgpcdx")
                            || item
                                .expected_path
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .ends_with(".dds");
                        // Texture preview only meaningful if the file physically exists on disk.
                        let target_exists = (item.kind == crate::auditor::AssetResourceKind::Unused && item.parent_file.is_file())
                            || (item.parent_file.is_file() && is_tex_ext)
                            || (item.expected_path.is_file() && is_tex_ext);
                        let is_tex = is_tex_ext && target_exists;
                        let has_missing = item.kind != crate::auditor::AssetResourceKind::Unused
                            && !item.expected_path.to_string_lossy().trim().is_empty();
                        (is_tex, has_missing)
                    })
                    .unwrap_or((false, false));

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
                    w.set_selected_row_index(row);
                    w.set_incoming_refs_preview(incoming.into());
                    w.set_outgoing_refs_preview(outgoing.into());
                    w.set_is_selected_texture(is_tex);
                    w.set_has_missing_file_selected(has_missing);
                }
            });
        }

        // Show dependency graph in browser
        {
            let state = self.state.clone();
            let handle = self.weak.clone();

            self.window.on_show_graph_clicked(move || {
                let Some(w) = handle.upgrade() else { return };
                let row = w.get_selected_row_index();
                if row < 0 {
                    return;
                }

                let extracted = state.lock().ok().and_then(|s| {
                    let item = s.display_items.get(row as usize).cloned()?;
                    let path = if item.kind == crate::auditor::AssetResourceKind::Unused {
                        &item.expected_path
                    } else {
                        &item.parent_file
                    };
                    let np = crate::fs_utils::normalize_path(path);
                    let key = std::path::PathBuf::from(np.to_string_lossy().to_lowercase());
                    let incoming = s
                        .dependency_graph
                        .incoming
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    let outgoing = s
                        .dependency_graph
                        .outgoing
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    let missing_refs: Vec<PathBuf> =
                        if item.kind != crate::auditor::AssetResourceKind::Unused
                            && !item.expected_path.as_os_str().is_empty()
                            && !item.expected_path.is_file()
                        {
                            vec![item.expected_path.clone()]
                        } else {
                            Vec::new()
                        };
                    Some((np, incoming, outgoing, missing_refs))
                });

                let Some((selected_path, incoming, outgoing, missing_refs)) = extracted else {
                    return;
                };

                let incoming: Vec<PathBuf> = incoming.into_iter().collect();
                let outgoing: Vec<PathBuf> = outgoing.into_iter().collect();

                let thread_handle = handle.clone();
                std::thread::spawn(move || {
                    if let Err(e) = crate::graph_view::open_dependency_graph_in_browser(
                        &selected_path,
                        &incoming,
                        &outgoing,
                        &missing_refs,
                    ) {
                        let template =
                            crate::gettext!("Не удалось открыть граф зависимостей: {}");
                        let msg = template.replace("{}", &e.to_string());
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = thread_handle.upgrade() {
                                w.set_editor_status(msg.into());
                            }
                        });
                    }
                });
            });
        }

        // Show full directory graph (all scanned files) in browser
        {
            let state = self.state.clone();
            let handle = self.weak.clone();

            self.window.on_open_full_graph_clicked(move || {
                let thread_handle = handle.clone();
                let state_clone = state.clone();
                std::thread::spawn(move || {
                    let extracted = state_clone.lock().ok().map(|s| {
                        (s.full_asset_graph.clone(), s.is_scan_completed)
                    });
                    let Some((full_graph, completed)) = extracted else { return; };
                    if !completed {
                        return;
                    }
                    if let Err(e) =
                        crate::graph_view::generate_full_directory_graph(&full_graph, std::path::Path::new(""))
                    {
                        let template =
                            crate::gettext!("Не удалось построить полный граф каталога: {}");
                        let msg = template.replace("{}", &e.to_string());
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = thread_handle.upgrade() {
                                w.set_editor_status(msg.into());
                            }
                        });
                    }
                });
            });
        }

        // Open source file folder
        {
            let state = self.state.clone();
            self.window.on_open_source_file(move |row| {
                if row < 0 { return; }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|item| item.parent_file.clone())
                });
                if let Some(path) = target_path {
                    println!("[Auditor] Opening path: {:?}", path);
                    #[cfg(windows)]
                    let _ = crate::windows::open_in_explorer(&path);
                }
            });
        }

        // Open missing target folder
        {
            let state = self.state.clone();
            self.window.on_open_missing_target_folder(move |row| {
                if row < 0 { return; }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|item| item.expected_path.clone())
                });
                if let Some(path) = target_path {
                    // Walk up directory ancestors until an existing folder is found,
                    // then open that folder directly (no /select on a non-existent file).
                    let mut dir = path.parent();
                    while let Some(d) = dir {
                        if d.is_dir() {
                            println!("[Auditor] Opening existing parent folder: {:?}", d);
                            #[cfg(windows)]
                            let _ = crate::windows::open_in_explorer(d);
                            break;
                        }
                        dir = d.parent();
                    }
                }
            });
        }

        // Sort
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

                crate::auditor::sort_items(&mut s.display_items, col_now, asc_now);
                let rows_model = map_broken_resources_to_ui(&s.display_items);

                w.set_sort_col(col_now as i32);
                w.set_sort_ascending(asc_now);
                w.set_filtered_rows(rows_model);
                w.set_selected_row_index(-1);
                w.set_is_selected_texture(false);
                w.set_has_missing_file_selected(false);
                w.set_incoming_refs_preview("".into());
                w.set_outgoing_refs_preview("".into());
            });
        }

        // Edit in VS Code
        {
            let handle = self.weak.clone();
            let state = self.state.clone();

            self.window.on_edit_in_vscode(move |row| {
                if row < 0 { return; }
                let item_paths = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|i| (i.parent_file.clone(), i.expected_path.clone()))
                });
                let Some((parent_file, expected_path)) = item_paths else { return; };

                let is_editable = |p: &PathBuf| {
                    p.extension()
                        .map(|e| {
                            e.eq_ignore_ascii_case("bin")
                                || e.eq_ignore_ascii_case("geopcdx")
                                || e.eq_ignore_ascii_case("xsec")
                        })
                        .unwrap_or(false)
                };

                let asset_path = if is_editable(&parent_file) {
                    parent_file
                } else if is_editable(&expected_path) {
                    expected_path
                } else {
                    if let Some(win) = handle.upgrade() {
                        win.set_editor_status(
                            "Выбранный элемент не является .bin/.GeoPcDx/.XSec".into(),
                        );
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
                        if p.is_file() { Some(p) } else { None }
                    })
                    .unwrap_or_else(|| PathBuf::from("serz.exe"));

                let thread_handle = handle.clone();
                std::thread::spawn(move || {
                    let cb_handle = thread_handle.clone();
                    let callback = move |res: Result<String, String>| {
                        let msg = match res {
                            Ok(s) => s,
                            Err(e) => format!("x {e}"),
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
                                win.set_editor_status(format!("x {e}").into());
                            }
                        });
                    }
                });
            });
        }
    }
}

pub(crate) fn map_broken_resources_to_ui(
    items: &[crate::auditor::BrokenResourceRef],
) -> slint::ModelRc<crate::IssueRowData> {
    let rows: Vec<crate::IssueRowData> = items
        .iter()
        .map(|b| {
            let is_dead = b.kind == crate::auditor::AssetResourceKind::Unused;
            let kind = if b.count > 1 {
                format!("{} (×{})", b.kind.label(), b.count)
            } else {
                b.kind.label().to_string()
            };
            crate::IssueRowData {
                kind_text: kind.into(),
                missing_file: if is_dead {
                    "".into()
                } else {
                    crate::fs_utils::to_assets_relative_display(&b.expected_path).into()
                },
                source_file: crate::fs_utils::to_assets_relative_display(&b.parent_file).into(),
                is_dead_resource: is_dead,
            }
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}
