use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use slint::{ComponentHandle, Weak};

use super::{
    TextureInfo, convert_dds_to_tgpcdx_report, decode_tgpcdx_to_rgba, export_tgpcdx_to_dds,
    inspect_tgpcdx_with_path, rgba_to_slint_image, texture_info_text, update_tgpcdx_internal_name,
};
use crate::app::LogLevel;
use crate::auditor::AuditorController;
use crate::common::external_editor;
use crate::{AuditorState, MainWindow, TextureViewerWindow, log_event};

pub type LoadedTexture = (u32, u32, Vec<u8>, PathBuf);
pub type SharedTextureState = Arc<Mutex<Option<LoadedTexture>>>;
type SharedTextureInfo = Arc<Mutex<Option<TextureInfo>>>;

struct TextureSnapshot {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    info: Option<TextureInfo>,
    info_text: String,
    name: String,
    writable: bool,
}

pub struct TextureController {
    pub window: TextureViewerWindow,
    pub weak: Weak<TextureViewerWindow>,
    pub tex_state: SharedTextureState,
    tex_info: SharedTextureInfo,
    pub preview_generation: Arc<AtomicU64>,
    pub auditor_state: Arc<Mutex<AuditorState>>,
}

impl TextureController {
    pub fn new(auditor_state: Arc<Mutex<AuditorState>>) -> Self {
        let window = TextureViewerWindow::new().unwrap();
        let weak = window.as_weak();
        Self {
            window,
            weak,
            tex_state: Arc::new(Mutex::new(None)),
            tex_info: Arc::new(Mutex::new(None)),
            preview_generation: Arc::new(AtomicU64::new(0)),
            auditor_state,
        }
    }

    pub fn bind(&self, auditor: &AuditorController) {
        let auditor_state_for_preview = self.auditor_state.clone();
        let tex_state_for_preview = self.tex_state.clone();
        let tex_info_for_preview = self.tex_info.clone();
        let viewer_weak = self.weak.clone();
        let preview_generation = self.preview_generation.clone();

        auditor.window.on_preview_texture(move |row| {
            if row < 0 {
                return;
            }
            let generation_id = preview_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let preview_gen = preview_generation.clone();

            let target_item = auditor_state_for_preview
                .lock()
                .ok()
                .and_then(|g| g.display_items.get(row as usize).cloned());

            let Some(item) = target_item else { return };
            let path = if item
                .expected_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tgpcdx"))
            {
                item.expected_path
            } else if item
                .parent_file
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tgpcdx"))
            {
                item.parent_file
            } else {
                return;
            };

            if !path.is_file() {
                return;
            }

            let tex_state = tex_state_for_preview.clone();
            let tex_info = tex_info_for_preview.clone();
            let viewer_weak = viewer_weak.clone();

            thread::spawn(move || {
                if preview_gen.load(Ordering::Acquire) != generation_id {
                    return;
                }
                let Ok(snapshot) = load_texture_snapshot(&path) else {
                    return;
                };
                if preview_gen.load(Ordering::Acquire) != generation_id {
                    return;
                }
                store_snapshot(&tex_state, &tex_info, &path, &snapshot);

                let _ = slint::invoke_from_event_loop(move || {
                    if preview_gen.load(Ordering::Acquire) != generation_id {
                        return;
                    }
                    if let Some(v) = viewer_weak.upgrade() {
                        apply_texture_to_viewer(&v, &snapshot, true);
                        v.set_texture_name_enabled(snapshot.writable);
                        let _ = v.show();
                    }
                });
            });
        });

        self.bind_buttons();
    }

    pub fn bind_buttons(&self) {
        // Открытие в VS Code
        let editor_weak = self.weak.clone();
        let editor_state = self.tex_state.clone();
        let editor_info = self.tex_info.clone();
        let editor_ui_weak = crate::UI_WEAK.get().cloned();
        self.window.on_open_in_editor(move || {
            let Some(path) = current_texture_path(&editor_state) else {
                notify(
                    &editor_weak,
                    &editor_ui_weak,
                    "There is no loaded texture to open in editor.",
                    LogLevel::Warning,
                );
                return;
            };
            let callback_path = path.clone();
            let callback_viewer = editor_weak.clone();
            let callback_ui = editor_ui_weak.clone();
            let callback_state = editor_state.clone();
            let callback_info = editor_info.clone();
            if let Err(error) =
                external_editor::open_and_sync_with_vscode(&path, move |result| match result {
                    Ok(message) => match load_texture_snapshot(&callback_path) {
                        Ok(snapshot) => {
                            store_snapshot(
                                &callback_state,
                                &callback_info,
                                &callback_path,
                                &snapshot,
                            );
                            notify(&callback_viewer, &callback_ui, &message, LogLevel::Info);
                            let viewer = callback_viewer.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(v) = viewer.upgrade() {
                                    apply_texture_to_viewer(&v, &snapshot, true);
                                }
                            });
                        }
                        Err(error) => {
                            let message = format!(
                                "File was changed, but preview could not be updated: {error}"
                            );
                            notify(&callback_viewer, &callback_ui, &message, LogLevel::Error);
                        }
                    },
                    Err(error) => {
                        let message = format!("Editor error: {error}");
                        notify(&callback_viewer, &callback_ui, &message, LogLevel::Error);
                    }
                })
            {
                let message = format!("Failed to open file in editor: {error}");
                notify(&editor_weak, &editor_ui_weak, &message, LogLevel::Error);
            }
        });

        let viewer_weak_toggle = self.weak.clone();
        let tex_state_for_toggle = self.tex_state.clone();
        let tex_info_for_toggle = self.tex_info.clone();
        self.window.on_toggle_alpha(move || {
            let Some(v) = viewer_weak_toggle.upgrade() else {
                return;
            };
            let show_alpha = !v.get_show_alpha();
            v.set_show_alpha(show_alpha);

            let Ok(guard) = tex_state_for_toggle.lock() else {
                return;
            };
            let Some((w, h, rgba, path)) = guard.as_ref() else {
                return;
            };
            let img = if show_alpha {
                let mut alpha_rgba = Vec::with_capacity(rgba.len());
                for &[_, _, _, a] in rgba.as_chunks::<4>().0 {
                    alpha_rgba.extend_from_slice(&[a, a, a, 255]);
                }
                rgba_to_slint_image(*w, *h, &alpha_rgba)
            } else {
                rgba_to_slint_image(*w, *h, rgba)
            };
            let info_text = tex_info_for_toggle
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|i| texture_info_text(i, path, show_alpha)))
                .unwrap_or_else(|| fallback_info_text(path, *w, *h, show_alpha));
            drop(guard);

            v.set_texture_image(img);
            v.set_info_text(info_text.into());
        });

        let tex_state_for_export = self.tex_state.clone();
        let viewer_weak_export = self.weak.clone();
        let ui_weak_export = crate::UI_WEAK.get().cloned();
        self.window.on_export_dds(move || {
            let Some(path) = current_texture_path(&tex_state_for_export) else {
                notify(
                    &viewer_weak_export,
                    &ui_weak_export,
                    "There is no loaded texture to export",
                    LogLevel::Warning,
                );
                return;
            };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("texture");
            let default_name = format!("{stem}.dds");
            let Some(out) = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .set_file_name(&default_name)
                .save_file()
            else {
                return;
            };

            let viewer_weak = viewer_weak_export.clone();
            let ui_weak = ui_weak_export.clone();
            thread::spawn(move || {
                let (msg, level) = match export_tgpcdx_to_dds(&path, &out) {
                    Ok(()) => (
                        format!("DDS exported: {}", out.display()),
                        LogLevel::Success,
                    ),
                    Err(e) => (format!("DDS export failure: {e}"), LogLevel::Error),
                };
                notify(&viewer_weak, &ui_weak, &msg, level);
            });
        });

        // Замена из DDS
        let viewer_weak_replace = self.weak.clone();
        let tex_state_for_replace = self.tex_state.clone();
        let tex_info_for_replace = self.tex_info.clone();
        let ui_weak_replace = crate::UI_WEAK.get().cloned();
        self.window.on_replace_from_dds(move || {
            let Some(path) = current_texture_path(&tex_state_for_replace) else {
                notify(
                    &viewer_weak_replace,
                    &ui_weak_replace,
                    "There is no loaded texture to replace",
                    LogLevel::Warning,
                );
                return;
            };

            if is_readonly(&path) {
                let msg = format!(
                    "File is read-only (possibly inside the .ap archive):: {}. Unpack before replacing.",
                    path.display()
                );
                notify(&viewer_weak_replace, &ui_weak_replace, &msg, LogLevel::Warning);
                return;
            }

            let Some(input) = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .pick_file()
            else {
                return;
            };

            let viewer_weak = viewer_weak_replace.clone();
            let ui_weak = ui_weak_replace.clone();
            let tex_state = tex_state_for_replace.clone();
            let tex_info = tex_info_for_replace.clone();
            thread::spawn(move || {
                if let Err(e) = external_editor::create_backup_file(&path) {
                    let msg = format!("Failed to create backup: {e}");
                    notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                    return;
                }

                match convert_dds_to_tgpcdx_report(&input, &path) {
                    Ok(report) => {
                        let msg = format!(
                            "DDS import completed: format {}, mipmaps {}, {} bytes",
                            report.dds_format, report.mip_count, report.total_bytes
                        );
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                    }
                    Err(e) => {
                        let msg = format!("Error replacing from DDS: {e}");
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                        return;
                    }
                }

                if let Ok(snapshot) = load_texture_snapshot(&path) {
                    store_snapshot(&tex_state, &tex_info, &path, &snapshot);
                    let vw = viewer_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(v) = vw.upgrade() {
                            apply_texture_to_viewer(&v, &snapshot, true);
                        }
                    });
                    let msg = format!("Texture replaced from DDS: {}", path.display());
                    notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                } else {
                    notify(
                        &viewer_weak,
                        &ui_weak,
                        "Couldn't restart preview after replacement",
                        LogLevel::Warning,
                    );
                }
            });
        });

        // Сохранение внутреннего имени текстуры
        let tex_state_for_name = self.tex_state.clone();
        let tex_info_for_name = self.tex_info.clone();
        let viewer_weak_name = self.weak.clone();
        let ui_weak_name = crate::UI_WEAK.get().cloned();
        self.window.on_save_texture_internal_name(move |new_name| {
            let Some(path) = current_texture_path(&tex_state_for_name) else {
                return;
            };
            let viewer_weak = viewer_weak_name.clone();
            let ui_weak = ui_weak_name.clone();
            let tex_state = tex_state_for_name.clone();
            let tex_info = tex_info_for_name.clone();
            let new_name = new_name.to_string();
            thread::spawn(
                move || match update_tgpcdx_internal_name(&path, &new_name) {
                    Ok(()) => {
                        if let Ok(snapshot) = load_texture_snapshot(&path) {
                            store_snapshot(&tex_state, &tex_info, &path, &snapshot);
                            let vw = viewer_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(v) = vw.upgrade() {
                                    apply_texture_to_viewer(&v, &snapshot, false);
                                }
                            });
                        }
                        let msg = format!("Internal texture name is saved: {}", path.display());
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                    }
                    Err(e) => {
                        let msg = format!("Failed to save name: {e}");
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                    }
                },
            );
        });

        // Подстановка имени по имени файла
        let tex_state_for_fn = self.tex_state.clone();
        let tex_info_for_fn = self.tex_info.clone();
        let viewer_weak_fn = self.weak.clone();
        self.window.on_texture_name_from_file(move || {
            let Some(v) = viewer_weak_fn.upgrade() else {
                return;
            };
            let Some(path) = current_texture_path(&tex_state_for_fn) else {
                return;
            };
            let current_name = tex_info_for_fn
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|i| i.name.clone()))
                .unwrap_or_default();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let new_name = match current_name.rfind(['\\', '/']) {
                Some(pos) => format!("{}{stem}", &current_name[..=pos]),
                None => stem.to_owned(),
            };
            v.set_texture_name(new_name.into());
        });
    }
}

fn current_texture_path(tex_state: &SharedTextureState) -> Option<PathBuf> {
    tex_state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()))
}

fn is_readonly(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
}

fn load_texture_snapshot(path: &Path) -> Result<TextureSnapshot, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let (width, height, rgba) = decode_tgpcdx_to_rgba(&data)?;
    let info = inspect_tgpcdx_with_path(&data, Some(path)).ok();
    let info_text = info
        .as_ref()
        .map(|i| texture_info_text(i, path, false))
        .unwrap_or_else(|| fallback_info_text(path, width, height, false));
    let name = info.as_ref().map(|i| i.name.clone()).unwrap_or_default();
    let writable = !is_readonly(path);
    Ok(TextureSnapshot {
        width,
        height,
        rgba,
        info,
        info_text,
        name,
        writable,
    })
}

fn store_snapshot(
    tex_state: &SharedTextureState,
    tex_info: &SharedTextureInfo,
    path: &Path,
    snapshot: &TextureSnapshot,
) {
    if let Ok(mut st) = tex_state.lock() {
        *st = Some((
            snapshot.width,
            snapshot.height,
            snapshot.rgba.clone(),
            path.to_path_buf(),
        ));
    }
    if let Ok(mut info) = tex_info.lock() {
        *info = snapshot.info.clone();
    }
}

fn apply_texture_to_viewer(v: &TextureViewerWindow, snapshot: &TextureSnapshot, reset_view: bool) {
    v.set_texture_image(rgba_to_slint_image(
        snapshot.width,
        snapshot.height,
        &snapshot.rgba,
    ));
    if reset_view {
        v.set_texture_orig_width(snapshot.width as i32);
        v.set_texture_orig_height(snapshot.height as i32);
        v.set_zoom_factor(if snapshot.width > 640 || snapshot.height > 480 {
            (640.0 / snapshot.width as f32).min(480.0 / snapshot.height as f32)
        } else {
            1.0
        });
        v.set_pan_x(0.0);
        v.set_pan_y(0.0);
        v.set_show_alpha(false);
    }
    v.set_info_text(snapshot.info_text.as_str().into());
    v.set_texture_name(snapshot.name.as_str().into());
}

fn notify(
    viewer_weak: &Weak<TextureViewerWindow>,
    ui_weak: &Option<Weak<MainWindow>>,
    msg: &str,
    level: LogLevel,
) {
    let msg = msg.to_owned();
    let vw = viewer_weak.clone();
    let uiw = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(v) = vw.upgrade() {
            v.set_info_text(msg.as_str().into());
        }
        if let Some(ui) = uiw.and_then(|w| w.upgrade()) {
            log_event(&ui, &msg, level);
        }
    });
}

fn fallback_info_text(path: &Path, w: u32, h: u32, show_alpha: bool) -> String {
    let alpha = if show_alpha { "  [альфа]" } else { "" };
    format!("{w} x {h} px  TGPCDX  {}{alpha}", path.display())
}
