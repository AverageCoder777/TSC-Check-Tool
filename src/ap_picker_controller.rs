use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::collections::BTreeMap;

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
        Self { window, weak, app, state }
    }

    pub fn bind(&self) {
        // search changed
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            self.window.on_search_changed(move |query| {
                let entries = state
                    .lock()
                    .ok()
                    .and_then(|s| s.as_ref().map(|s| s.entries.clone()))
                    .unwrap_or_default();
                let q = query.to_lowercase();
                let filtered: Vec<slint::SharedString> = entries
                    .iter()
                    .filter(|e| q.is_empty() || e.to_lowercase().contains(&q))
                    .map(|e| e.clone().into())
                    .collect();
                if let Some(pw) = handle.upgrade() {
                    pw.set_filtered_entries(slint::ModelRc::new(slint::VecModel::from(filtered)));
                }
            });
        }
        // select entry
        {
            let handle = self.weak.clone();
            self.window.on_select_entry(move |idx| {
                if let Some(pw) = handle.upgrade() {
                    pw.set_selected_index(idx);
                }
            });
        }
        // cancel
        {
            let handle = self.weak.clone();
            self.window.on_cancel(move || {
                if let Some(pw) = handle.upgrade() {
                    pw.set_extracting(false);
                    let _ = pw.hide();
                }
            });
        }
        // confirm selection (background extraction)
        {
            let state = self.state.clone();
            let handle = self.weak.clone();
            let app = self.app.clone();
            self.window.on_confirm_selection(move || {
                let (archive_path, missing, rw_path, entry) = {
                    let guard = match state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let Some(s) = guard.as_ref() else { return };
                    let pw = match handle.upgrade() {
                        Some(p) => p,
                        None => return,
                    };
                    let idx = pw.get_selected_index() as usize;
                    let filtered = pw.get_filtered_entries();
                    let selected_entry = filtered
                        .row_data(idx)
                        .map(|v| v.to_string())
                        .or_else(|| s.entries.get(idx).cloned());
                    let Some(entry) = selected_entry else { return };
                    (
                        s.archive_path.clone(),
                        s.missing.clone(),
                        s.rw_path.clone(),
                        entry,
                    )
                };

                // show extracting
                if let Some(pw) = handle.upgrade() {
                    pw.set_extracting(true);
                }

                let handle_t = handle.clone();
                let app_t = app.clone();
                std::thread::spawn(move || {
                    // derive provider/product
                    let assets_path = PathBuf::from(rw_path.trim()).join("Assets");
                    let archive_rel = archive_path.strip_prefix(&assets_path).ok();
                    let mut parts: Vec<String> = archive_rel
                        .map(|p| {
                            p.components()
                                .map(|c| c.as_os_str().to_string_lossy().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    if !parts.is_empty() {
                        parts.pop();
                    }
                    let provider = parts.first().cloned().unwrap_or_default();
                    let product = parts.get(1).cloned().unwrap_or_default();
                    let temp_rel_dir = if provider.is_empty() || product.is_empty() {
                        PathBuf::from("TempAp")
                    } else {
                        PathBuf::from(&provider).join(&product)
                    };
                    let temp_dir = assets_path.join(&temp_rel_dir);
                    let _ = std::fs::create_dir_all(&temp_dir);
                    let entry_file_name = Path::new(&entry)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "asset.bin".to_owned());
                    let temp_file = temp_dir.join(&entry_file_name);

                    // extract from archive (background)
                    if let Ok(file) = std::fs::File::open(&archive_path)
                        && let Ok(mut zip) = zip::ZipArchive::new(file)
                        && let Ok(mut member) = zip.by_name(&entry)
                        && let Ok(mut out) = std::fs::File::create(&temp_file)
                    {
                        let _ = std::io::copy(&mut member, &mut out);
                    }

                    let replacement_relative = temp_file
                        .strip_prefix(&assets_path)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| entry.clone());

                    // update app
                    if let Ok(mut app) = app_t.lock() {
                        app.start_asset_replacement(
                            missing,
                            temp_file,
                            false,
                            replacement_relative,
                            BTreeMap::new(),
                        );
                    }

                    // hide window on UI thread
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(pw) = handle_t.upgrade() {
                            pw.set_extracting(false);
                            let _ = pw.hide();
                        }
                    });
                });
            });
        }
    }
}
