use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app::app_controller::{CheckerApp, LogLevel};
use crate::common::domain::IncompleteRouteFolder;
use crate::{IncompleteFolderItem, IncompleteFoldersWindow};

pub(crate) struct IncompleteFoldersController {
    app: Arc<Mutex<CheckerApp>>,
    window: IncompleteFoldersWindow,
}

impl IncompleteFoldersController {
    pub(crate) fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
        Self {
            app,
            window: IncompleteFoldersWindow::new().unwrap(),
        }
    }

    pub(crate) fn show(&self, app: &CheckerApp) {
        update_window(&self.window, &app.incomplete_folders);
        let _ = self.window.show();
    }

    pub(crate) fn bind(&self) {
        self.window.on_open_in_explorer(move |path| {
            if !path.is_empty() {
                let _ = Command::new("explorer").arg(path.as_str()).spawn();
            }
        });

        let weak_del = self.window.as_weak();
        let app_del = self.app.clone();
        self.window.on_delete_selected(move |index| {
            let Ok(idx) = usize::try_from(index) else {
                return;
            };
            let has_target = app_del
                .lock()
                .map(|app| idx < app.incomplete_folders.len())
                .unwrap_or(false);
            if !has_target {
                return;
            }

            let confirmed = rfd::MessageDialog::new()
                .set_title("Folder deletion")
                .set_description("Delete selected incomplete folder?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if confirmed != rfd::MessageDialogResult::Ok {
                return;
            }

            if let Ok(mut app) = app_del.lock() {
                let Some(path) = app.incomplete_folders.get(idx).map(|f| f.path.clone()) else {
                    return;
                };
                match validate_delete_path(&path, &app) {
                    Ok(()) => match fs::remove_dir_all(&path) {
                        Ok(()) => {
                            app.incomplete_folders.remove(idx);
                            app.log_event(
                                format!("Deleted incomplete folder: {}.", path.display()),
                                LogLevel::Info,
                            );
                            if let Some(w) = weak_del.upgrade() {
                                update_window(&w, &app.incomplete_folders);
                            }
                            crate::schedule_ui_sync();
                        }
                        Err(error) => {
                            app.log_event(
                                format!("Failed to delete {}: {error}.", path.display()),
                                LogLevel::Error,
                            );
                        }
                    },
                    Err(e) => {
                        app.log_event(
                            format!("Security error during deletion {}: {e}.", path.display()),
                            LogLevel::Error,
                        );
                    }
                }
            }
        });

        let weak_all = self.window.as_weak();
        let app_all = self.app.clone();
        self.window.on_delete_all(move || {
            let has_folders = app_all
                .lock()
                .map(|app| !app.incomplete_folders.is_empty())
                .unwrap_or(false);
            if !has_folders {
                return;
            }

            let confirmed = rfd::MessageDialog::new()
                .set_title("Folder deletion")
                .set_description("Delete all detected incomplete folders?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if confirmed != rfd::MessageDialogResult::Ok {
                return;
            }

            if let Ok(mut app) = app_all.lock() {
                if app.incomplete_folders.is_empty() {
                    return;
                }
                let canon_routes = match canonical_routes_dir(&app) {
                    Ok(dir) => dir,
                    Err(e) => {
                        app.log_event(
                            format!("Security error during deletion folders: {e}."),
                            LogLevel::Error,
                        );
                        return;
                    }
                };

                let folders = mem::take(&mut app.incomplete_folders);
                let total = folders.len();
                let mut remaining = Vec::new();

                for folder in folders {
                    match validate_delete_path_against(&folder.path, &canon_routes) {
                        Ok(()) => {
                            if let Err(error) = fs::remove_dir_all(&folder.path) {
                                app.log_event(
                                    format!(
                                        "Failed to delete {}: {error}.",
                                        folder.path.display()
                                    ),
                                    LogLevel::Error,
                                );
                                remaining.push(folder);
                            }
                        }
                        Err(e) => {
                            app.log_event(
                                format!(
                                    "Security error during deletion {}: {e}.",
                                    folder.path.display()
                                ),
                                LogLevel::Error,
                            );
                            remaining.push(folder);
                        }
                    }
                }

                let removed = total - remaining.len();
                let failed = remaining.len();
                app.incomplete_folders = remaining;

                if failed == 0 {
                    app.log_event(
                        format!("Deleted all incomplete folders ({removed} entries)."),
                        LogLevel::Info,
                    );
                } else if removed > 0 {
                    app.log_event(
                        format!(
                            "Deleted incomplete folders: {removed} (failed to delete: {failed})."
                        ),
                        LogLevel::Warning,
                    );
                } else {
                    app.log_event(
                        "Couldn't delete any incomplete folders.",
                        LogLevel::Error,
                    );
                }

                if let Some(w) = weak_all.upgrade() {
                    update_window(&w, &app.incomplete_folders);
                }
                crate::schedule_ui_sync();
            }
        });

        let weak_close = self.window.as_weak();
        self.window.on_close_window(move || {
            if let Some(w) = weak_close.upgrade() {
                let _ = w.hide();
            }
        });
    }
}

fn update_window(window: &IncompleteFoldersWindow, folders: &[IncompleteRouteFolder]) {
    let items = build_incomplete_folder_items(folders);
    let total_bytes: u64 = folders.iter().map(|f| f.total_size_bytes).sum();
    let count = folders.len();
    let summary = format!(
        "All: {} folders, total size: {}",
        count,
        format_size_bytes(total_bytes)
    );
    window.set_folders(ModelRc::new(VecModel::from(items)));
    window.set_selected_index(-1);
    window.set_summary(summary.into());
}

fn canonical_routes_dir(app: &CheckerApp) -> Result<PathBuf, String> {
    let routes_dir = app.clean_rw_path().join("Content").join("Routes");
    routes_dir
        .canonicalize()
        .map_err(|e| format!("Failed to determine routes folder: {e}"))
}

/// Проверяет, безопасно ли удалять директорию: путь должен находиться
/// внутри `<game_path>/Content/Routes` и не быть самой этой папкой.
fn validate_delete_path(path: &Path, app: &CheckerApp) -> Result<(), String> {
    let canon_routes = canonical_routes_dir(app)?;
    validate_delete_path_against(path, &canon_routes)
}

fn validate_delete_path_against(path: &Path, canon_routes: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Path to deletion is empty.".to_owned());
    }
    let canon_target = path
        .canonicalize()
        .map_err(|e| format!("Failed to determine target path: {e}"))?;
    if canon_target == canon_routes {
        return Err("Deleting root folder of routes is prohibited.".to_owned());
    }
    if !canon_target.starts_with(canon_routes) {
        return Err("Target path is located outside routes folder. Deletion is prohibited.".to_owned());
    }
    Ok(())
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        "< 1 KiB".to_owned()
    }
}

fn build_incomplete_folder_items(folders: &[IncompleteRouteFolder]) -> Vec<IncompleteFolderItem> {
    folders
        .iter()
        .map(|folder| IncompleteFolderItem {
            folder_name: folder.id.clone().into(),
            full_path: folder.path.display().to_string().into(),
            missing_criteria: folder.problems.join(" • ").into(),
            file_count: folder.file_count as i32,
            size_mb: format_size_bytes(folder.total_size_bytes).into(),
        })
        .collect()
}
