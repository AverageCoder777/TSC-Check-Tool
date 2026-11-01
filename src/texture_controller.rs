use slint::ComponentHandle;
use slint::Weak;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{AuditorState, TextureViewerWindow};

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
        Self {
            window,
            weak,
            tex_state,
            preview_generation,
            auditor_state,
        }
    }

    pub fn bind(&self, auditor: &crate::auditor_controller::AuditorController) {
        let auditor_state_for_preview = self.auditor_state.clone();
        let tex_state_for_preview = self.tex_state.clone();
        let viewer_weak = self.weak.clone();
        let preview_generation = self.preview_generation.clone();

        auditor.window.on_preview_texture(move |row| {
            if row < 0 {
                return;
            }
            let generation_id = preview_generation.fetch_add(1, Ordering::SeqCst) + 1;
            let preview_gen = preview_generation.clone();

            // Извлекаем только один целевой PathBuf без клонирования всего списка
            let target_path = auditor_state_for_preview
                .lock()
                .ok()
                .and_then(|g| g.display_items.get(row as usize).cloned());

            let Some(item) = target_path else { return };
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
            let viewer_weak = viewer_weak.clone();

            std::thread::spawn(move || {
                if preview_gen.load(Ordering::SeqCst) != generation_id {
                    return;
                }
                if let Ok(data) = std::fs::read(&path)
                    && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                {
                    if let Ok(mut st) = tex_state.lock() {
                        *st = Some((w, h, rgba.clone(), path.clone()));
                    }
                    let info = format!("{} x {} px  TGPCDX  {}", w, h, path.display());
                    if preview_gen.load(Ordering::SeqCst) != generation_id {
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

        // Переключение альфа-канала
        let viewer_weak_toggle = self.weak.clone();
        let tex_state_for_toggle = self.tex_state.clone();
        self.window.on_toggle_alpha(move || {
            let Some(st) = tex_state_for_toggle.lock().ok().and_then(|g| g.clone()) else {
                return;
            };
            let (w, h, mut data, _) = st;
            let show = viewer_weak_toggle
                .upgrade()
                .map(|v| v.get_show_alpha())
                .unwrap_or(true);

            if !show {
                for i in (3..data.len()).step_by(4) {
                    data[i] = 255;
                }
            }
            let img = crate::texture::rgba_to_slint_image(w, h, &data);
            if let Some(v) = viewer_weak_toggle.upgrade() {
                v.set_texture_image(img);
            }
        });

        // Экспорт в DDS (копируется только PathBuf, а не 16 МБ текстуры)
        let tex_state_for_export = self.tex_state.clone();
        self.window.on_export_dds(move || {
            let path = tex_state_for_export
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()));

            if let Some(path) = path {
                let out = rfd::FileDialog::new()
                    .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                    .save_file();
                if let Some(out) = out {
                    let _ = crate::texture::export_tgpcdx_to_dds(&path, &out);
                }
            }
        });

        // Замена из DDS
        let viewer_weak_replace = self.weak.clone();
        let tex_state_for_replace = self.tex_state.clone();
        self.window.on_replace_from_dds(move || {
            let path = tex_state_for_replace
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()));

            let Some(path) = path else { return };

            let input = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .pick_file();

            if let Some(input) = input {
                let _ = crate::texture::convert_dds_to_tgpcdx(&input, &path);
                if let Ok(data) = std::fs::read(&path)
                    && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                {
                    if let Ok(mut s) = tex_state_for_replace.lock() {
                        *s = Some((w, h, rgba.clone(), path.clone()));
                    }
                    let info = format!("{} x {} px  TGPCDX  {}", w, h, path.display());
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
        });
    }
}