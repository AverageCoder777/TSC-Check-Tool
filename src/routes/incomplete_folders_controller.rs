use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use crate::{IncompleteFolderItem, IncompleteFoldersWindow};
use crate::app::app_controller::{CheckerApp, LogLevel};
use crate::common::domain::IncompleteRouteFolder;

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
            let _ = std::process::Command::new("explorer").arg(&path).spawn();
        });

        let weak_del = self.window.as_weak();
        let app_del = self.app.clone();
        self.window.on_delete_selected(move |index| {
            let confirmed = rfd::MessageDialog::new()
                .set_title("Удаление папки")
                .set_description("Удалить выбранную неполную папку навсегда?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if confirmed != rfd::MessageDialogResult::Ok {
                return;
            }
            if let Ok(mut app) = app_del.lock() {
                let idx = index as usize;
                if idx >= app.incomplete_folders.len() {
                    return;
                }
                let path = app.incomplete_folders[idx].path.clone();
                match validate_delete_path(&path, &app) {
                    Ok(()) => match std::fs::remove_dir_all(&path) {
                        Ok(()) => {
                            app.incomplete_folders.remove(idx);
                            app.log_event(
                                format!("Удалена неполная папка: {}.", path.display()),
                                LogLevel::Info,
                            );
                            if let Some(w) = weak_del.upgrade() {
                                update_window(&w, &app.incomplete_folders);
                            }
                            crate::schedule_ui_sync();
                        }
                        Err(error) => {
                            app.log_event(
                                format!("Не удалось удалить {}: {error}.", path.display()),
                                LogLevel::Error,
                            );
                        }
                    },
                    Err(e) => {
                        app.log_event(
                            format!("Ошибка безопасности при удалении {}: {e}.", path.display()),
                            LogLevel::Error,
                        );
                    }
                }
            }
        });

        let weak_all = self.window.as_weak();
        let app_all = self.app.clone();
        self.window.on_delete_all(move || {
            let confirmed = rfd::MessageDialog::new()
                .set_title("Удаление папок")
                .set_description("Удалить все найденные неполные папки навсегда?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if confirmed != rfd::MessageDialogResult::Ok {
                return;
            }
            if let Ok(mut app) = app_all.lock() {
                let mut remaining = Vec::new();
                for folder in app.incomplete_folders.clone() {
                    match validate_delete_path(&folder.path, &app) {
                        Ok(()) => {
                            if std::fs::remove_dir_all(&folder.path).is_err() {
                                remaining.push(folder);
                            }
                        }
                        Err(e) => {
                            app.log_event(
                                format!(
                                    "Ошибка безопасности при удалении {}: {e}.",
                                    folder.path.display()
                                ),
                                LogLevel::Error,
                            );
                            remaining.push(folder);
                        }
                    }
                }
                let removed = app.incomplete_folders.len() - remaining.len();
                app.incomplete_folders = remaining;
                app.log_event(
                    format!("Удалены все неполные папки ({removed} шт.)."),
                    LogLevel::Info,
                );
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

fn update_window(
    window: &IncompleteFoldersWindow,
    folders: &[IncompleteRouteFolder],
) {
    let items = build_incomplete_folder_items(folders);
    let total_bytes: u64 = folders.iter().map(|f| f.total_size_bytes).sum();
    let count = folders.len();
    let summary = format!(
        "Всего: {} {} • общий вес: {}",
        count,
        folder_count_word(count),
        format_size_bytes(total_bytes)
    );
    window.set_folders(slint::ModelRc::new(slint::VecModel::from(items)));
    window.set_selected_index(-1);
    window.set_summary(summary.into());
}

/// Проверяет, безопасно ли удалять директорию: путь должен находиться
/// внутри `<game_path>/Content/Routes` и не быть самой этой папкой.
fn validate_delete_path(
    path: &Path,
    app: &CheckerApp,
) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Путь к удалению пустой.".to_owned());
    }
    let routes_dir = PathBuf::from(&app.rw_path).join("Content").join("Routes");
    let canon_routes = routes_dir
        .canonicalize()
        .map_err(|e| format!("Не удалось определить папку маршрутов: {e}"))?;
    let canon_target = path
        .canonicalize()
        .map_err(|e| format!("Не удалось определить целевой путь: {e}"))?;
    if canon_target == canon_routes {
        return Err("Удаление корневой папки маршрутов запрещено.".to_owned());
    }
    if !canon_target.starts_with(&canon_routes) {
        return Err("Целевой путь находится вне папки маршрутов. Удаление запрещено.".to_owned());
    }
    Ok(())
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} МБ", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} КБ", bytes as f64 / 1024.0)
    } else {
        "< 1 КБ".to_owned()
    }
}

fn folder_count_word(count: usize) -> &'static str {
    let n = count % 100;
    if (5..=20).contains(&n) {
        "папок"
    } else {
        match n % 10 {
            1 => "папка",
            2..=4 => "папки",
            _ => "папок",
        }
    }
}

fn build_incomplete_folder_items(
    folders: &[IncompleteRouteFolder],
) -> Vec<IncompleteFolderItem> {
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
