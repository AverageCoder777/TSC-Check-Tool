use slint::ComponentHandle;
use slint::Weak;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;

use crate::{TextureViewerWindow, AuditorState};

pub struct TextureController {
    pub window: TextureViewerWindow,
    pub weak: Weak<TextureViewerWindow>,
    pub tex_state: Arc<Mutex<Option<(u32, u32, Vec<u8>, PathBuf)>>>,
    pub preview_generation: Arc<AtomicU64>,
    pub auditor_state: Arc<Mutex<AuditorState>>,
}

impl TextureController {
    pub fn new(auditor_state: Arc<Mutex<AuditorState>>) -> Self {
        let window = TextureViewerWindow::new().unwrap();
        let weak = window.as_weak();
        let tex_state = Arc::new(Mutex::new(None));
        let preview_generation = Arc::new(AtomicU64::new(0));
        Self { window, weak, tex_state, preview_generation, auditor_state }
    }

    pub fn bind(&self, auditor: &crate::auditor_controller::AuditorController) {
        let auditor_state_for_preview = self.auditor_state.clone();
        let tex_state_for_preview = self.tex_state.clone();
        let viewer_weak = self.weak.clone();
        let preview_generation = self.preview_generation.clone();
        auditor.window.on_preview_texture(move |row| {
            let generation_id = preview_generation.fetch_add(1, Ordering::SeqCst) + 1;
            let preview_generation = preview_generation.clone();
            let items = auditor_state_for_preview
                .lock()
                .map(|g| g.display_items.clone())
                .unwrap_or_default();
            if row < 0 || row as usize >= items.len() {
                return;
            }
            let item = &items[row as usize];
            let path = if item
                .expected_path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
                .unwrap_or(false)
            {
                item.expected_path.clone()
            } else if item
                .parent_file
                .extension()
                .map(|e| e.eq_ignore_ascii_case("tgpcdx"))
                .unwrap_or(false)
            {
                item.parent_file.clone()
            } else {
                return;
            };
            if !path.is_file() {
                return;
            }
            let tex_state = tex_state_for_preview.clone();
            let viewer_weak = viewer_weak.clone();
            std::thread::spawn(move || {
                if preview_generation.load(Ordering::SeqCst) != generation_id {
                    return;
                }
                if let Ok(data) = std::fs::read(&path)
                    && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                {
                    if let Ok(mut st) = tex_state.lock() {
                        *st = Some((w, h, rgba.clone(), path.clone()));
                    }
                    let info = format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display());
                    if preview_generation.load(Ordering::SeqCst) != generation_id {
                        return;
                    }
                    let vw = viewer_weak;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(v) = vw.upgrade() {
                            let img = crate::texture::rgba_to_slint_image(w, h, &rgba);
                            v.set_texture_image(img);
                            v.set_texture_orig_width(w as i32);
                            v.set_texture_orig_height(h as i32);
                            v.set_zoom_factor(if w > 640 || h > 480 {
                                (640.0 / w as f32).min(480.0 / h as f32)
                            } else {
                                1.0
                            });
                            v.set_info_text(info.into());
                            let _ = v.show();
                        }
                    });
                }
            });
        });

        // toggle alpha
        let viewer_weak_toggle = self.weak.clone();
        let tex_state_for_toggle = self.tex_state.clone();
        self.window.on_toggle_alpha(move || {
            let st = tex_state_for_toggle
                .lock()
                .map(|g| g.clone())
                .unwrap_or(None);
            if let Some((w, h, rgba, _)) = st {
                let show = viewer_weak_toggle
                    .upgrade()
                    .map(|v| v.get_show_alpha())
                    .unwrap_or(true);
                let mut data = rgba;
                if !show {
                    for i in (3..data.len()).step_by(4) {
                        data[i] = 255;
                    }
                }
                let img = crate::texture::rgba_to_slint_image(w, h, &data);
                if let Some(v) = viewer_weak_toggle.upgrade() {
                    v.set_texture_image(img);
                }
            }
        });

        // export dds
        let viewer_weak_export = self.weak.clone();
        let tex_state_for_export = self.tex_state.clone();
        self.window.on_export_dds(move || {
            let st = tex_state_for_export
                .lock()
                .map(|g| g.clone())
                .unwrap_or(None);
            if let Some((_, _, _, path)) = st {
                let out = rfd::FileDialog::new()
                    .add_filter("DDS", &["dds"])
                    .save_file();
                if let Some(out) = out {
                    let _ = crate::texture::export_tgpcdx_to_dds(&path, &out);
                }
            }
            let _ = viewer_weak_export;
        });

        // replace from dds
        let viewer_weak_replace = self.weak.clone();
        let tex_state_for_replace = self.tex_state.clone();
        self.window.on_replace_from_dds(move || {
            let st = tex_state_for_replace
                .lock()
                .map(|g| g.clone())
                .unwrap_or(None);
            if let Some((_, _, _, path)) = st {
                let input = rfd::FileDialog::new()
                    .add_filter("DDS", &["dds"])
                    .pick_file();
                if let Some(input) = input {
                    let _ = crate::texture::convert_dds_to_tgpcdx(&input, &path);
                    if let Ok(data) = std::fs::read(&path)
                        && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                    {
                        if let Ok(mut s) = tex_state_for_replace.lock() {
                            *s = Some((w, h, rgba.clone(), path.clone()));
                        }
                        let info = format!("{} x {} px  {}  {}", w, h, "TGPCDX", path.display());
                        let vw = viewer_weak_replace.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(v) = vw.upgrade() {
                                let img = crate::texture::rgba_to_slint_image(w, h, &rgba);
                                v.set_texture_image(img);
                                v.set_texture_orig_width(w as i32);
                                v.set_texture_orig_height(h as i32);
                                v.set_zoom_factor(if w > 640 || h > 480 {
                                    (640.0 / w as f32).min(480.0 / h as f32)
                                } else {
                                    1.0
                                });
                                v.set_info_text(info.into());
                            }
                        });
                    }
                }
            }
        });
    }
}
