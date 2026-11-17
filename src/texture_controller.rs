use slint::ComponentHandle;
use slint::Weak;
use std::path::{Path, PathBuf};
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
                    let info = build_texture_info_text(&path, &data, w, h, false);
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
                            v.set_pan_x(0.0);
                            v.set_pan_y(0.0);
                            v.set_show_alpha(false);
                            v.set_info_text(info.into());
                            let _ = v.show();
                        }
                    });
                }
            });
        });

        self.bind_buttons();
    }

    pub fn bind_buttons(&self) {
        // Переключение альфа-канала
        let viewer_weak_toggle = self.weak.clone();
        let tex_state_for_toggle = self.tex_state.clone();
        self.window.on_toggle_alpha(move || {
            let Some(v) = viewer_weak_toggle.upgrade() else { return };
            let show_alpha = !v.get_show_alpha();
            v.set_show_alpha(show_alpha);

            let Some(st) = tex_state_for_toggle.lock().ok().and_then(|g| g.clone()) else {
                return;
            };
            let (w, h, rgba, path) = st;
            let mut data = rgba.clone();
            if show_alpha {
                for i in (0..data.len()).step_by(4) {
                    if i + 3 < data.len() {
                        let a = data[i + 3];
                        data[i] = a;
                        data[i + 1] = a;
                        data[i + 2] = a;
                        data[i + 3] = 255;
                    }
                }
            }
            let img = crate::texture::rgba_to_slint_image(w, h, &data);
            v.set_texture_image(img);
            let info = build_texture_info_text(&path, &data, w, h, show_alpha);
            v.set_info_text(info.into());
        });

        // Экспорт в DDS
        let tex_state_for_export = self.tex_state.clone();
        let viewer_weak_export = self.weak.clone();
        let ui_weak_export = crate::UI_WEAK.get().cloned();
        self.window.on_export_dds(move || {
            let path = tex_state_for_export
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()));
            let Some(path) = path else {
                let msg = "Нет загруженной текстуры для экспорта".to_string();
                notify(&viewer_weak_export, &ui_weak_export, &msg, crate::app::LogLevel::Warning);
                return;
            };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("texture");
            let default_name = format!("{}.dds", stem);
            let out = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .set_file_name(&default_name)
                .save_file();
            let Some(out) = out else { return };

            let viewer_weak = viewer_weak_export.clone();
            let ui_weak = ui_weak_export.clone();
            std::thread::spawn(move || {
                let res = crate::texture::export_tgpcdx_to_dds(&path, &out);
                let (msg, level) = match res {
                    Ok(()) => (
                        format!("Экспортировано DDS: {}", out.display()),
                        crate::app::LogLevel::Success,
                    ),
                    Err(e) => (
                        format!("Ошибка экспорта DDS: {e}"),
                        crate::app::LogLevel::Error,
                    ),
                };
                notify(&viewer_weak, &ui_weak, &msg, level);
            });
        });

        // Замена из DDS
        let viewer_weak_replace = self.weak.clone();
        let tex_state_for_replace = self.tex_state.clone();
        let ui_weak_replace = crate::UI_WEAK.get().cloned();
        self.window.on_replace_from_dds(move || {
            let path = tex_state_for_replace
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()));
            let Some(path) = path else {
                let msg = "Нет загруженной текстуры для замены".to_string();
                notify(&viewer_weak_replace, &ui_weak_replace, &msg, crate::app::LogLevel::Warning);
                return;
            };

            if std::fs::metadata(&path)
                .map(|m| m.permissions().readonly())
                .unwrap_or(false)
            {
                let msg = format!(
                    "Файл доступен только для чтения (возможно, внутри .ap-архива): {}. Распакуйте перед заменой.",
                    path.display()
                );
                notify(&viewer_weak_replace, &ui_weak_replace, &msg, crate::app::LogLevel::Warning);
            }

            let input = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .pick_file();
            let Some(input) = input else { return };

            let viewer_weak = viewer_weak_replace.clone();
            let ui_weak = ui_weak_replace.clone();
            let tex_state = tex_state_for_replace.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::external_editor::create_backup_file(&path) {
                    let msg = format!("Не удалось создать резервную копию: {e}");
                    notify(&viewer_weak, &ui_weak, &msg, crate::app::LogLevel::Error);
                    return;
                }

                match crate::texture::convert_dds_to_tgpcdx_report(&input, &path) {
                    Ok(report) => {
                        let msg = format!(
                            "Импорт DDS выполнен: формат {}, мипмапов {}, всего {} байт",
                            report.dds_format, report.mip_count, report.total_bytes
                        );
                        notify(&viewer_weak, &ui_weak, &msg, crate::app::LogLevel::Success);
                    }
                    Err(e) => {
                        let msg = format!("Ошибка замены из DDS: {e}");
                        notify(&viewer_weak, &ui_weak, &msg, crate::app::LogLevel::Error);
                        return;
                    }
                }

                if let Ok(data) = std::fs::read(&path)
                    && let Ok((w, h, rgba)) = crate::texture::decode_tgpcdx_to_rgba(&data)
                {
                    if let Ok(mut s) = tex_state.lock() {
                        *s = Some((w, h, rgba.clone(), path.clone()));
                    }
                    let info = build_texture_info_text(&path, &data, w, h, false);
                    let vw = viewer_weak.clone();
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
                            v.set_pan_x(0.0);
                            v.set_pan_y(0.0);
                            v.set_show_alpha(false);
                            v.set_info_text(info.into());
                        }
                    });
                    let msg = format!("Текстура заменена из DDS: {}", path.display());
                    notify(&viewer_weak, &ui_weak, &msg, crate::app::LogLevel::Success);
                } else {
                    let msg = "Не удалось перезагрузить превью после замены".to_string();
                    notify(&viewer_weak, &ui_weak, &msg, crate::app::LogLevel::Warning);
                }
            });
        });
    }
}

fn notify(
    viewer_weak: &Weak<TextureViewerWindow>,
    ui_weak: &Option<Weak<crate::MainWindow>>,
    msg: &str,
    level: crate::app::LogLevel,
) {
    let msg_v = msg.to_string();
    let vw = viewer_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(v) = vw.upgrade() {
            v.set_info_text(msg_v.clone().into());
        }
    });
    if let Some(uiw) = ui_weak {
        let msg_u = msg.to_string();
        let uiw = uiw.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uiw.upgrade() {
                crate::log_event(&ui, &msg_u, level);
            }
        });
    }
}

/// Собирает текст информационной панели просмотрщика из метаданных .TgPcDx.
/// При неудаче парсинга возвращает старую компактную строку.
fn build_texture_info_text(path: &Path, data: &[u8], w: u32, h: u32, show_alpha: bool) -> String {
    match crate::texture::inspect_tgpcdx_with_path(data, Some(path)) {
        Ok(info) => crate::texture::texture_info_text(&info, path, show_alpha),
        Err(_) => {
            let alpha = if show_alpha { "  [альфа]" } else { "" };
            format!("{} x {} px  TGPCDX  {}{}", w, h, path.display(), alpha)
        }
    }
}