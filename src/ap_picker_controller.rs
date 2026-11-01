use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{ApAssetPickerWindow, domain};

#[derive(Clone)]
pub struct ApPickerState {
    pub archive_path: PathBuf,
    pub missing: domain::MissingAsset,
    pub rw_path: String,
    pub entries: Vec<String>,
}

pub struct ApPickerController {
    pub window: ApAssetPickerWindow,
    pub weak: Weak<ApAssetPickerWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
    pub state: Arc<Mutex<Option<ApPickerState>>>,
}

impl ApPickerController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = ApAssetPickerWindow::new().unwrap();
        let weak = window.as_weak();
        let state = Arc::new(Mutex::new(None::<ApPickerState>));
        Self {
            window,
            weak,
            app,
            state,
        }
    }

    pub fn bind(&self) {
        // Поиск по содержимому архива без избыточного клонирования памяти
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            self.window.on_search_changed(move |query| {
                let q = query.trim().to_lowercase();
                let filtered: Vec<slint::SharedString> = if let Ok(guard) = state.lock() {
                    if let Some(s) = guard.as_ref() {
                        s.entries
                            .iter()
                            .filter(|e| q.is_empty() || e.to_lowercase().contains(&q))
                            .map(|e| slint::SharedString::from(e.as_str()))
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                if let Some(pw) = handle.upgrade() {
                    pw.set_filtered_entries(slint::ModelRc::new(slint::VecModel::from(filtered)));
                    pw.set_selected_index(-1); // Сброс выбора при изменении выборки
                }
            });
        }

        // Выбор элемента
        {
            let handle = self.weak.clone();
            self.window.on_select_entry(move |idx| {
                if let Some(pw) = handle.upgrade() {
                    pw.set_selected_index(idx);
                }
            });
        }

        // Отмена
        {
            let handle = self.weak.clone();
            self.window.on_cancel(move || {
                if let Some(pw) = handle.upgrade() {
                    pw.set_extracting(false);
                    let _ = pw.hide();
                }
            });
        }

        // Подтверждение выбора и фоновое извлечение
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_confirm_selection(move || {
                let pw = match handle.upgrade() {
                    Some(p) => p,
                    None => return,
                };

                let idx = pw.get_selected_index();
                if idx < 0 {
                    return;
                }

                let filtered = pw.get_filtered_entries();
                let selected_entry = match filtered.row_data(idx as usize) {
                    Some(v) => v.to_string(),
                    None => return,
                };

                let (archive_path, missing, rw_path) = {
                    let guard = match state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let Some(s) = guard.as_ref() else { return };
                    (s.archive_path.clone(), s.missing.clone(), s.rw_path.clone())
                };

                pw.set_extracting(true);

                let handle_t = handle.clone();
                let app_t = app.clone();
                let entry = selected_entry;

                std::thread::spawn(move || {
                    let assets_path =
                        crate::app::clean_rw_path(&rw_path)
                            .join("Assets");
                    let target_file_path =
                        resolve_extraction_path(&assets_path, &archive_path, &entry);

                    let extract_result =
                        crate::archive::extract_archive_member(&archive_path, &entry, &target_file_path);

                    match extract_result {
                        Ok(()) => {
                            let replacement_relative = target_file_path
                                .strip_prefix(&assets_path)
                                .map(|p| p.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_else(|_| entry.replace('\\', "/"));

                            if let Ok(mut app) = app_t.lock() {
                                app.start_asset_replacement(
                                    missing,
                                    target_file_path,
                                    false,
                                    replacement_relative,
                                    BTreeMap::new(),
                                );
                            }

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(pw) = handle_t.upgrade() {
                                    pw.set_extracting(false);
                                    let _ = pw.hide();
                                }
                            });
                        }
                        Err(err) => {
                            eprintln!("[ApPicker] Ошибка извлечения: {err}");
                            if let Ok(mut app) = app_t.lock() {
                                app.status = format!("Ошибка распаковки: {err}");
                                crate::schedule_ui_sync();
                            }
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(pw) = handle_t.upgrade() {
                                    pw.set_extracting(false);
                                }
                            });
                            crate::external_editor::show_native_error("Ошибка распаковки", &err);
                        }
                    }
                });
            });
        }
    }
}

/// Вычисляет правильный путь распаковки с сохранением иерархии папок чертежа RailWorks.
fn resolve_extraction_path(assets_path: &Path, archive_path: &Path, entry: &str) -> PathBuf {
    let norm_entry = entry.replace('\\', "/");
    let norm_entry = norm_entry.trim_start_matches('/');

    let archive_rel = archive_path.strip_prefix(assets_path).ok();
    let mut parts: Vec<String> = archive_rel
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();

    if !parts.is_empty() {
        parts.pop(); // Отбрасываем имя .ap файла
    }

    let provider = parts.first().map(|s| s.as_str()).unwrap_or("");
    let product = parts.get(1).map(|s| s.as_str()).unwrap_or("");

    let prefix_prov_prod = format!("{provider}/{product}/");
    let prefix_prod = format!("{product}/");

    // Защита от дублирования каталогов, если запись в архиве уже содержала префикс продукта
    let relative_in_product = if !provider.is_empty()
        && !product.is_empty()
        && norm_entry
            .to_ascii_lowercase()
            .starts_with(&prefix_prov_prod.to_ascii_lowercase())
    {
        &norm_entry[prefix_prov_prod.len()..]
    } else if !product.is_empty()
        && norm_entry
            .to_ascii_lowercase()
            .starts_with(&prefix_prod.to_ascii_lowercase())
    {
        &norm_entry[prefix_prod.len()..]
    } else {
        norm_entry
    };

    if !provider.is_empty() && !product.is_empty() {
        assets_path
            .join(provider)
            .join(product)
            .join(relative_in_product)
    } else if let Some(parent) = archive_path.parent() {
        parent.join(norm_entry)
    } else {
        assets_path.join("TempAp").join(norm_entry)
    }
}
