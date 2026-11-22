use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Weak};

use crate::app::app_controller::{CheckerApp, LogLevel};
use crate::auditor::auditor_main::{
    AssetResourceKind, BrokenResourceRef, audit_folders_with_progress, filter_items, sort_items,
};
use crate::auditor::graph_view::{generate_full_directory_graph, open_dependency_graph_in_browser};
use crate::common::archive::ApArchiveCache;
use crate::common::external_editor::open_and_sync_with_vscode;
use crate::common::fs_utils::{normalize_path, to_assets_relative_display};
#[cfg(windows)]
use crate::common::windows::open_in_explorer;
use crate::{AuditorState, FolderAuditorWindow};

struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn find_assets_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Assets"))
        })
        .map(Path::to_path_buf)
}

pub fn validate_audit_folder(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(
            crate::gettext!("Specified path does not exist or is not a folder!").to_string(),
        );
    }
    let normalized = normalize_path(path);
    let Some(assets_root) = find_assets_root(&normalized) else {
        return Err(crate::gettext!("Folder must be inside RailWorks/Assets!").to_string());
    };
    if normalized == assets_root
        || normalized
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Assets"))
    {
        return Err(crate::gettext!("You can't scan the entire Assets catalog! Select the provider or product folder (for example, Assets/Kuju).").to_string());
    }
    Ok(path.to_path_buf())
}

pub struct AuditorController {
    pub window: FolderAuditorWindow,
    pub state: Arc<Mutex<AuditorState>>,
    pub weak: Weak<FolderAuditorWindow>,
    pub app: Arc<Mutex<CheckerApp>>,
    pub audit_generation: Arc<AtomicU64>,
    pub is_busy: Arc<AtomicBool>,
}

impl AuditorController {
    pub fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
        let window = FolderAuditorWindow::new().unwrap();
        let weak = window.as_weak();
        let state = Arc::new(Mutex::new(AuditorState::default()));
        Self {
            window,
            state,
            weak,
            app,
            audit_generation: Arc::new(AtomicU64::new(0)),
            is_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn bind(&self, _ui_weak: Weak<crate::MainWindow>) {
        self.bind_pick_folder();
        self.bind_start_audit();
        self.bind_filter_and_sort();
        self.bind_row_selected();
        self.bind_graphs();
        self.bind_file_actions();
    }

    fn bind_pick_folder(&self) {
        let handle = self.weak.clone();
        self.window.on_pick_folder(move || {
            let Some(w) = handle.upgrade() else { return };
            let current = w.get_folder_path().to_string();
            let mut dialog = rfd::FileDialog::new()
                .set_title(crate::gettext!("Select folder(s) to check."));
            if !current.is_empty() {
                let first = current.split(';').next().unwrap_or("").trim();
                let p = Path::new(first);
                if p.is_dir() {
                    dialog = dialog.set_directory(p);
                }
            }
            if let Some(folders) = dialog.pick_folders() {
                let valid_folders: Vec<PathBuf> = folders
                    .into_iter()
                    .filter_map(|p| validate_audit_folder(&p).ok())
                    .collect();
                if valid_folders.is_empty() {
                    w.set_scan_status_text(crate::gettext!("Error: selected folders must be located inside Assets directory and not be Assets directory itself!").into());
                    return;
                }
                let joined = valid_folders
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("; ");
                w.set_folder_path(joined.into());
            }
        });
    }

    fn bind_start_audit(&self) {
        let handle = self.weak.clone();
        let state = self.state.clone();
        let audit_generation = self.audit_generation.clone();
        let is_busy = self.is_busy.clone();

        self.window.on_start_audit(move |folder_path| {
            if is_busy.swap(true, Ordering::SeqCst) {
                return;
            }
            let busy_guard = BusyGuard(is_busy.clone());

            let paths: Vec<PathBuf> = folder_path
                .as_str()
                .split([';', ','])
                .map(|s| s.trim().trim_matches('"').trim())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();

            if paths.is_empty() {
                eprintln!("[Auditor] No folder paths provided");
                return;
            }
            for p in &paths {
                if let Err(msg) = validate_audit_folder(p) {
                    eprintln!("[Auditor] Invalid folder path: {p:?} -> {msg}");
                    if let Some(w) = handle.upgrade() {
                        w.set_scan_status_text(msg.into());
                        w.set_is_scanning(false);
                        w.set_is_scan_completed(false);
                    }
                    return;
                }
            }

            if let Some(w) = handle.upgrade() {
                w.set_scan_progress(0.0);
                w.set_scan_status_text(crate::gettext!("Preparing to check...").into());
                w.set_filtered_rows(slint::ModelRc::new(slint::VecModel::default()));
                clear_selection(&w);
                w.set_is_scanning(true);
                w.set_is_scan_completed(false);
            }

            let (rw_path, max_threads) = crate::app_state()
                .lock()
                .map(|a| (a.clean_rw_path(), a.serz_process_limit.max(1)))
                .unwrap_or_else(|_| (PathBuf::new(), 8));

            let assets_root = find_assets_root(&paths[0]).unwrap_or_else(|| {
                let rw_assets = rw_path.join("Assets");
                if rw_assets.is_dir() {
                    rw_assets
                } else {
                    paths[0]
                        .parent()
                        .and_then(Path::parent)
                        .map(Path::to_path_buf)
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
            let ap_cache = ApArchiveCache::new();

            std::thread::spawn(move || {
                let _busy = busy_guard;

                let progress_handle = thread_handle.clone();
                let progress_gen = generation.clone();
                let progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
                    Arc::new(move |done, total| {
                        if progress_gen.load(Ordering::Relaxed) != generation_id {
                            return;
                        }
                        let h = progress_handle.clone();
                        let gen_inner = progress_gen.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if gen_inner.load(Ordering::Relaxed) != generation_id {
                                return;
                            }
                            if let Some(w) = h.upgrade() {
                                let progress = if total > 0 {
                                    done as f32 / total as f32
                                } else {
                                    0.0
                                };
                                w.set_scan_progress(progress);
                                let template = crate::gettext!("Scanning: {} / {} files...");
                                w.set_scan_status_text(
                                    template
                                        .replacen("{}", &done.to_string(), 1)
                                        .replacen("{}", &total.to_string(), 1)
                                        .into(),
                                );
                            }
                        });
                    });

                let audit_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    audit_folders_with_progress(
                        &paths,
                        &assets_root,
                        max_threads,
                        &ap_cache,
                        Some(progress_cb),
                    )
                }));

                let (report, full_graph) = match audit_result {
                    Ok(r) => r,
                    Err(panic) => {
                        let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = panic.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        eprintln!("[Auditor] panic during audit: {msg}");
                        let h = thread_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = h.upgrade() {
                                w.set_scan_status_text(format!("Check failure: {msg}").into());
                                w.set_is_scanning(false);
                            }
                        });
                        return;
                    }
                };

                if generation.load(Ordering::SeqCst) != generation_id {
                    return;
                }

                let all_items = report.broken_references;
                let total_issues = all_items.len();
                let counts = CategoryCounts::from_items(&all_items);

                let (col, asc) = thread_state
                    .lock()
                    .map(|s| (s.sort_col, s.sort_asc))
                    .unwrap_or((0, true));

                let mut display_items = all_items.clone();
                filter_items(&mut display_items, &filter);
                sort_items(&mut display_items, col, asc);

                if let Ok(mut s) = thread_state.lock() {
                    s.all_items = all_items;
                    s.display_items = display_items.clone();
                    s.dependency_graph = report.dependency_graph;
                    s.full_asset_graph = full_graph;
                    s.is_scan_completed = true;
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
                        w.set_filtered_rows(map_broken_resources_to_ui(&display_items));
                        w.set_scan_progress(1.0);
                        let final_msg = if total_issues == 0 {
                            crate::gettext!("Check completed: no missing or corrupted files.")
                                .to_string()
                        } else {
                            crate::gettext!("Check completed. Found problems: {}.")
                                .replace("{}", &total_issues.to_string())
                        };
                        w.set_scan_status_text(final_msg.into());
                        clear_selection(&w);
                        w.set_count_all(counts.all);
                        w.set_count_geometry(counts.geometry);
                        w.set_count_textures(counts.textures);
                        w.set_count_scripts(counts.scripts);
                        w.set_count_sounds(counts.sounds);
                        w.set_count_children(counts.children);
                        w.set_count_dead(counts.dead);
                        w.set_is_scanning(false);
                        w.set_is_scan_completed(true);
                    }
                });
            });
        });
    }

    fn bind_filter_and_sort(&self) {
        {
            let handle = self.weak.clone();
            let state = self.state.clone();
            self.window.on_filter_changed(move |filter| {
                let Some(w) = handle.upgrade() else { return };
                let Ok(mut s) = state.lock() else { return };

                let mut items = s.all_items.clone();
                filter_items(&mut items, filter.as_str());
                sort_items(&mut items, s.sort_col, s.sort_asc);
                let rows_model = map_broken_resources_to_ui(&items);
                s.display_items = items;

                w.set_current_filter(filter);
                w.set_filtered_rows(rows_model);
                clear_selection(&w);
            });
        }

        {
            let handle = self.weak.clone();
            let state = self.state.clone();
            self.window.on_sort_column(move |col| {
                let Some(w) = handle.upgrade() else { return };
                let Ok(mut s) = state.lock() else { return };

                let col = col as usize;
                if s.sort_col == col {
                    s.sort_asc = !s.sort_asc;
                } else {
                    s.sort_col = col;
                    s.sort_asc = true;
                }

                let col_now = s.sort_col;
                let asc_now = s.sort_asc;
                sort_items(&mut s.display_items, col_now, asc_now);
                let rows_model = map_broken_resources_to_ui(&s.display_items);

                w.set_sort_col(col_now as i32);
                w.set_sort_ascending(asc_now);
                w.set_filtered_rows(rows_model);
                clear_selection(&w);
            });
        }
    }

    fn bind_row_selected(&self) {
        let state = self.state.clone();
        let handle = self.weak.clone();

        self.window.on_row_selected(move |row| {
            let Some(w) = handle.upgrade() else { return };
            if row < 0 {
                w.set_incoming_refs_preview("".into());
                w.set_outgoing_refs_preview("".into());
                w.set_is_selected_texture(false);
                w.set_has_missing_file_selected(false);
                return;
            }

            const NO_PARENTS_MSG: &str =
                "Not used by any blueprint (.bin) or a mesh (.GeoPcDx) in the product";

            let (is_tex, has_missing, incoming, outgoing) = state
                .lock()
                .ok()
                .and_then(|s| {
                    let item = s.display_items.get(row as usize)?;
                    let is_tex = is_existing_texture(&item.parent_file)
                        || is_existing_texture(&item.expected_path);
                    let has_missing = item.kind != AssetResourceKind::Unused
                        && !item.expected_path.to_string_lossy().trim().is_empty();

                    let key = dependency_key(primary_item_path(item));
                    let outgoing_set = s.dependency_graph.outgoing.get(&key);

                    let incoming_str = if item.all_parents.is_empty() {
                        NO_PARENTS_MSG.to_string()
                    } else {
                        item.all_parents
                            .iter()
                            .map(|p| to_assets_relative_display(p))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let outgoing_str = format_refs_preview(outgoing_set, "No dependencies");
                    Some((is_tex, has_missing, incoming_str, outgoing_str))
                })
                .unwrap_or_else(|| {
                    (
                        false,
                        false,
                        NO_PARENTS_MSG.to_string(),
                        "No dependencies".to_string(),
                    )
                });

            w.set_selected_row_index(row);
            w.set_incoming_refs_preview(incoming.into());
            w.set_outgoing_refs_preview(outgoing.into());
            w.set_is_selected_texture(is_tex);
            w.set_has_missing_file_selected(has_missing);
        });
    }

    fn bind_graphs(&self) {
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
                    let item = s.display_items.get(row as usize)?;
                    let np = normalize_path(primary_item_path(item));
                    let key = dependency_key(&np);
                    let incoming: Vec<PathBuf> = s
                        .dependency_graph
                        .incoming
                        .get(&key)
                        .map(|set| set.iter().cloned().collect())
                        .unwrap_or_default();
                    let outgoing: Vec<PathBuf> = s
                        .dependency_graph
                        .outgoing
                        .get(&key)
                        .map(|set| set.iter().cloned().collect())
                        .unwrap_or_default();
                    let missing_refs = if item.kind != AssetResourceKind::Unused
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
                let thread_handle = handle.clone();
                std::thread::spawn(move || {
                    if let Err(e) = open_dependency_graph_in_browser(
                        &selected_path,
                        &incoming,
                        &outgoing,
                        &missing_refs,
                    ) {
                        let template = crate::gettext!("Failed to open dependency graph: {}");
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

        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            self.window.on_open_full_graph(move || {
                let thread_handle = handle.clone();
                let state_clone = state.clone();
                std::thread::spawn(move || {
                    let extracted = state_clone
                        .lock()
                        .ok()
                        .filter(|s| s.is_scan_completed)
                        .map(|s| s.full_asset_graph.clone());
                    let Some(full_graph) = extracted else {
                        return;
                    };
                    if let Err(e) = generate_full_directory_graph(&full_graph, Path::new("")) {
                        let template = crate::gettext!("Failed to build full graph: {}");
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
    }

    fn bind_file_actions(&self) {
        {
            let state = self.state.clone();
            let app = self.app.clone();
            self.window.on_open_source_file(move |row| {
                if row < 0 {
                    return;
                }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|item| item.parent_file.clone())
                });
                if let Some(path) = target_path {
                    if let Ok(mut app) = app.lock() {
                        app.log_event(
                            format!("Parent folder opened in explorer: {}.", path.display()),
                            LogLevel::Info,
                        );
                        crate::schedule_ui_sync();
                    }
                    println!("[Auditor] Opening path: {path:?}");
                    #[cfg(windows)]
                    open_in_explorer(&path);
                }
            });
        }

        {
            let state = self.state.clone();
            let app = self.app.clone();
            self.window.on_open_missing_target_folder(move |row| {
                if row < 0 {
                    return;
                }
                let target_path = state.lock().ok().and_then(|s| {
                    s.display_items
                        .get(row as usize)
                        .map(|item| item.expected_path.clone())
                });
                if let Some(path) = target_path
                    && let Some(dir) = path.ancestors().skip(1).find(|d| d.is_dir())
                {
                    if let Ok(mut app) = app.lock() {
                        app.log_event(
                            format!("Parent folder opened in explorer: {}.", dir.display()),
                            LogLevel::Info,
                        );
                        crate::schedule_ui_sync();
                    }
                    println!("[Auditor] Opening existing parent folder: {dir:?}");
                    #[cfg(windows)]
                    open_in_explorer(dir);
                }
            });
        }

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

                let asset_path = if is_editable_asset(&parent_file) {
                    parent_file
                } else if is_editable_asset(&expected_path) {
                    expected_path
                } else {
                    if let Some(win) = handle.upgrade() {
                        win.set_editor_status("Select element is not .bin/.GeoPcDx/.XSec".into());
                    }
                    return;
                };

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

                    if let Err(e) = open_and_sync_with_vscode(&asset_path, callback) {
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

#[derive(Default)]
struct CategoryCounts {
    all: i32,
    geometry: i32,
    textures: i32,
    scripts: i32,
    sounds: i32,
    children: i32,
    dead: i32,
}

impl CategoryCounts {
    fn from_items(items: &[BrokenResourceRef]) -> Self {
        let mut counts = Self {
            all: items.len() as i32,
            ..Self::default()
        };
        for item in items {
            match item.kind {
                AssetResourceKind::Geometry => counts.geometry += 1,
                AssetResourceKind::Texture => counts.textures += 1,
                AssetResourceKind::Script => counts.scripts += 1,
                AssetResourceKind::Sound => counts.sounds += 1,
                AssetResourceKind::ChildBlueprint => counts.children += 1,
                AssetResourceKind::Unused => counts.dead += 1,
                _ => {}
            }
        }
        counts
    }
}

fn clear_selection(w: &FolderAuditorWindow) {
    w.set_selected_row_index(-1);
    w.set_is_selected_texture(false);
    w.set_has_missing_file_selected(false);
    w.set_incoming_refs_preview("".into());
    w.set_outgoing_refs_preview("".into());
}

fn primary_item_path(item: &BrokenResourceRef) -> &Path {
    if item.kind == AssetResourceKind::Unused {
        &item.expected_path
    } else {
        &item.parent_file
    }
}

fn dependency_key(path: &Path) -> PathBuf {
    let np = normalize_path(path);
    PathBuf::from(np.to_string_lossy().to_lowercase())
}

fn is_existing_texture(path: &Path) -> bool {
    let is_tex_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tgpcdx") || e.eq_ignore_ascii_case("dds"));
    is_tex_ext && path.is_file()
}

fn is_editable_asset(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        e.eq_ignore_ascii_case("bin")
            || e.eq_ignore_ascii_case("geopcdx")
            || e.eq_ignore_ascii_case("xsec")
    })
}

fn format_refs_preview(set: Option<&HashSet<PathBuf>>, empty: &str) -> String {
    let Some(s) = set.filter(|s| !s.is_empty()) else {
        return empty.to_string();
    };
    let display_name = |p: &PathBuf| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string())
    };
    if s.len() > 3 {
        let names: Vec<String> = s.iter().take(3).map(display_name).collect();
        format!("{} files: {}", s.len(), names.join(", "))
    } else {
        s.iter().map(display_name).collect::<Vec<_>>().join("; ")
    }
}

pub(crate) fn map_broken_resources_to_ui(
    items: &[BrokenResourceRef],
) -> slint::ModelRc<crate::BrokenResourceItem> {
    let rows: Vec<crate::BrokenResourceItem> = items
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let is_dead = r.kind == AssetResourceKind::Unused;
            let parent_display = if r.all_parents.len() > 1 {
                let first = r
                    .parent_file
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                format!("{first} +{} sources", r.all_parents.len() - 1)
            } else {
                r.parent_file.to_string_lossy().into_owned()
            };
            crate::BrokenResourceItem {
                id: idx as i32,
                kind_str: r.kind.label().to_string().into(),
                expected_path: if is_dead {
                    "".into()
                } else {
                    to_assets_relative_display(&r.expected_path).into()
                },
                parent_file: parent_display.into(),
                sources_count: r.all_parents.len() as i32,
                is_dead,
            }
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}
