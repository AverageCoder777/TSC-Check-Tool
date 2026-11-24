use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::ApAssetPickerWindow;
use crate::app::app_controller::{CheckerApp, LogLevel};
use crate::common::archive::extract_archive_member;
use crate::common::domain;
use crate::common::external_editor::show_native_error;
use crate::common::fs_utils::clean_rw_path;

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
    pub app: Arc<Mutex<CheckerApp>>,
    pub state: Arc<Mutex<Option<ApPickerState>>>,
}

impl ApPickerController {
    pub fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
        let window = ApAssetPickerWindow::new().unwrap();
        let weak = window.as_weak();
        let state = Arc::new(Mutex::new(None));
        Self {
            window,
            weak,
            app,
            state,
        }
    }

    pub fn bind(&self) {
        self.bind_search();
        self.bind_select_and_cancel();
        self.bind_confirm_selection();
    }

    fn bind_search(&self) {
        let state = self.state.clone();
        let handle = self.weak.clone();
        self.window.on_search_changed(move |query| {
            let trimmed = query.trim();
            let filtered: Vec<SharedString> = state
                .lock()
                .ok()
                .and_then(|guard| {
                    let s = guard.as_ref()?;
                    let entries = if trimmed.is_empty() {
                        s.entries
                            .iter()
                            .map(|e| SharedString::from(e.as_str()))
                            .collect()
                    } else if trimmed.is_ascii() {
                        let needle = trimmed.as_bytes();
                        s.entries
                            .iter()
                            .filter(|e| contains_ignore_ascii_case(e.as_bytes(), needle))
                            .map(|e| SharedString::from(e.as_str()))
                            .collect()
                    } else {
                        let q = trimmed.to_lowercase();
                        s.entries
                            .iter()
                            .filter(|e| e.to_lowercase().contains(&q))
                            .map(|e| SharedString::from(e.as_str()))
                            .collect()
                    };
                    Some(entries)
                })
                .unwrap_or_default();

            if let Some(pw) = handle.upgrade() {
                pw.set_filtered_entries(ModelRc::new(VecModel::from(filtered)));
                pw.set_selected_index(-1); // Сброс выбора при изменении выборки
            }
        });
    }

    fn bind_select_and_cancel(&self) {
        {
            let handle = self.weak.clone();
            self.window.on_select_entry(move |idx| {
                if let Some(pw) = handle.upgrade() {
                    pw.set_selected_index(idx);
                }
            });
        }
        {
            let handle = self.weak.clone();
            self.window.on_cancel(move || {
                if let Some(pw) = handle.upgrade() {
                    pw.set_extracting(false);
                    let _ = pw.hide();
                }
            });
        }
    }

    fn bind_confirm_selection(&self) {
        let state = self.state.clone();
        let handle = self.weak.clone();
        let app = self.app.clone();

        self.window.on_confirm_selection(move || {
            let Some(pw) = handle.upgrade() else { return };
            if pw.get_extracting() {
                return;
            }
            let idx = pw.get_selected_index();
            if idx < 0 {
                return;
            }
            let Some(entry) = pw
                .get_filtered_entries()
                .row_data(idx as usize)
                .map(|v| v.to_string())
            else {
                return;
            };
            let Some((archive_path, missing, rw_path)) = state.lock().ok().and_then(|guard| {
                let s = guard.as_ref()?;
                Some((s.archive_path.clone(), s.missing.clone(), s.rw_path.clone()))
            }) else {
                return;
            };

            pw.set_extracting(true);
            let handle_t = handle.clone();
            let app_t = app.clone();

            std::thread::spawn(move || {
                let assets_path = clean_rw_path(&rw_path).join("Assets");
                let target_file_path = resolve_extraction_path(&assets_path, &archive_path, &entry);
                match extract_archive_member(&archive_path, &entry, &target_file_path) {
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
                        eprintln!("[ApPicker] Extracting error: {err}");
                        if let Ok(mut app) = app_t.lock() {
                            app.log_event(crate::gettext!("Unpacking error: {}", err), LogLevel::Error);
                            crate::schedule_ui_sync();
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(pw) = handle_t.upgrade() {
                                pw.set_extracting(false);
                            }
                        });
                        show_native_error("Unpacking error", &err);
                    }
                }
            });
        });
    }
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn strip_path_prefix_ignore_ascii_case<'a>(path: &'a str, segment: &str) -> Option<&'a str> {
    let bytes = path.as_bytes();
    let seg_bytes = segment.as_bytes();
    if bytes.len() > seg_bytes.len()
        && bytes[..seg_bytes.len()].eq_ignore_ascii_case(seg_bytes)
        && bytes[seg_bytes.len()] == b'/'
    {
        Some(&path[seg_bytes.len() + 1..])
    } else {
        None
    }
}

fn resolve_extraction_path(assets_path: &Path, archive_path: &Path, entry: &str) -> PathBuf {
    let norm_entry_owned = entry.replace('\\', "/");
    let norm_entry = norm_entry_owned.trim_start_matches('/');

    let mut rel_components = archive_path
        .parent()
        .and_then(|parent| parent.strip_prefix(assets_path).ok())
        .into_iter()
        .flat_map(|rel| rel.components())
        .map(|c| c.as_os_str().to_string_lossy());

    let provider = rel_components.next();
    let product = rel_components.next();

    if let (Some(provider), Some(product)) = (provider.as_deref(), product.as_deref())
        && !provider.is_empty()
        && !product.is_empty()
    {
        let relative_in_product = strip_path_prefix_ignore_ascii_case(norm_entry, provider)
            .and_then(|rest| strip_path_prefix_ignore_ascii_case(rest, product))
            .or_else(|| strip_path_prefix_ignore_ascii_case(norm_entry, product))
            .unwrap_or(norm_entry);

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
