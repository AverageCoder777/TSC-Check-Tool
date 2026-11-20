use slint::ComponentHandle;
use slint::Weak;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    convert_dds_to_tgpcdx_report, decode_tgpcdx_to_rgba, export_tgpcdx_to_dds, inspect_tgpcdx_file,
    inspect_tgpcdx_with_path, rgba_to_slint_image, texture_info_text, update_tgpcdx_internal_name,
};
use crate::app::LogLevel;
use crate::auditor::AuditorController;
use crate::common::external_editor;
use crate::{AuditorState, MainWindow, TextureViewerWindow, log_event};

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

    pub fn bind(&self, auditor: &AuditorController) {
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
                    && let Ok((w, h, rgba)) = decode_tgpcdx_to_rgba(&data)
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
                            let img = rgba_to_slint_image(w, h, &rgba);
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
                            if let Ok(i) = inspect_tgpcdx_with_path(&data, Some(&path)) {
                                v.set_texture_name(i.name.into());
                            }
                            let enabled = !std::fs::metadata(&path)
                                .map(|m| m.permissions().readonly())
                                .unwrap_or(false);
                            v.set_texture_name_enabled(enabled);
                            let _ = v.show();
                        }
                    });
                }
            });
        });

        self.bind_buttons();
    }

    pub fn bind_buttons(&self) {
        let editor_weak = self.weak.clone();
        let editor_state = self.tex_state.clone();
        let editor_ui_weak = crate::UI_WEAK.get().cloned();
        self.window.on_open_in_editor(move || {
            let path = editor_state
                .lock()
                .ok()
                .and_then(|state| state.as_ref().map(|(_, _, _, path)| path.clone()));
            let Some(path) = path else {
                let message = "Нет загруженной текстуры для открытия в редакторе".to_string();
                notify(&editor_weak, &editor_ui_weak, &message, LogLevel::Warning);
                return;
            };
            let callback_path = path.clone();
            let callback_viewer = editor_weak.clone();
            let callback_ui = editor_ui_weak.clone();
            let callback_state = editor_state.clone();
            if let Err(error) =
                external_editor::open_and_sync_with_vscode(&path, move |result| match result {
                    Ok(message) => {
                        let refreshed = std::fs::read(&callback_path)
                            .map_err(|error| error.to_string())
                            .and_then(|data| {
                                decode_tgpcdx_to_rgba(&data).map(|preview| (data, preview))
                            });
                        match refreshed {
                            Ok((data, (width, height, rgba))) => {
                                if let Ok(mut state) = callback_state.lock() {
                                    *state =
                                        Some((width, height, rgba.clone(), callback_path.clone()));
                                }
                                let info = build_texture_info_text(
                                    &callback_path,
                                    &data,
                                    width,
                                    height,
                                    false,
                                );
                                let name = inspect_tgpcdx_with_path(&data, Some(&callback_path))
                                    .map(|info| info.name)
                                    .unwrap_or_default();
                                notify(&callback_viewer, &callback_ui, &message, LogLevel::Info);
                                let viewer = callback_viewer.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(viewer) = viewer.upgrade() {
                                        viewer.set_texture_image(rgba_to_slint_image(
                                            width, height, &rgba,
                                        ));
                                        viewer.set_texture_orig_width(width as i32);
                                        viewer.set_texture_orig_height(height as i32);
                                        viewer.set_zoom_factor(if width > 640 || height > 480 {
                                            (640.0 / width as f32).min(480.0 / height as f32)
                                        } else {
                                            1.0
                                        });
                                        viewer.set_pan_x(0.0);
                                        viewer.set_pan_y(0.0);
                                        viewer.set_show_alpha(false);
                                        viewer.set_info_text(info.into());
                                        viewer.set_texture_name(name.into());
                                    }
                                });
                            }
                            Err(error) => {
                                let message =
                                    format!("Файл изменён, но превью не удалось обновить: {error}");
                                notify(&callback_viewer, &callback_ui, &message, LogLevel::Error);
                            }
                        }
                    }
                    Err(error) => {
                        let message = format!("Ошибка редактора: {error}");
                        notify(&callback_viewer, &callback_ui, &message, LogLevel::Error);
                    }
                })
            {
                let message = format!("Не удалось открыть файл в редакторе: {error}");
                notify(&editor_weak, &editor_ui_weak, &message, LogLevel::Error);
            }
        });

        // Переключение альфа-канала
        let viewer_weak_toggle = self.weak.clone();
        let tex_state_for_toggle = self.tex_state.clone();
        self.window.on_toggle_alpha(move || {
            let Some(v) = viewer_weak_toggle.upgrade() else {
                return;
            };
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
            let img = rgba_to_slint_image(w, h, &data);
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
                notify(
                    &viewer_weak_export,
                    &ui_weak_export,
                    &msg,
                    LogLevel::Warning,
                );
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
                let res = export_tgpcdx_to_dds(&path, &out);
                let (msg, level) = match res {
                    Ok(()) => (
                        format!("Экспортировано DDS: {}", out.display()),
                        LogLevel::Success,
                    ),
                    Err(e) => (format!("Ошибка экспорта DDS: {e}"), LogLevel::Error),
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
                notify(&viewer_weak_replace, &ui_weak_replace, &msg, LogLevel::Warning);
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
                notify(&viewer_weak_replace, &ui_weak_replace, &msg, LogLevel::Warning);
                return;
            }

            let input = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .pick_file();
            let Some(input) = input else { return };

            let viewer_weak = viewer_weak_replace.clone();
            let ui_weak = ui_weak_replace.clone();
            let tex_state = tex_state_for_replace.clone();
            std::thread::spawn(move || {
                if let Err(e) = external_editor::create_backup_file(&path) {
                    let msg = format!("Не удалось создать резервную копию: {e}");
                    notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                    return;
                }

                match convert_dds_to_tgpcdx_report(&input, &path) {
                    Ok(report) => {
                        let msg = format!(
                            "Импорт DDS выполнен: формат {}, мипмапов {}, всего {} байт",
                            report.dds_format, report.mip_count, report.total_bytes
                        );
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                    }
                    Err(e) => {
                        let msg = format!("Ошибка замены из DDS: {e}");
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                        return;
                    }
                }

                        if let Ok(data) = std::fs::read(&path)
                            && let Ok((w, h, rgba)) = decode_tgpcdx_to_rgba(&data)
                        {
                            if let Ok(mut s) = tex_state.lock() {
                                *s = Some((w, h, rgba.clone(), path.clone()));
                            }
                            let info = build_texture_info_text(&path, &data, w, h, false);
                            let edited_name = inspect_tgpcdx_with_path(&data, Some(&path))
                                .map(|i| i.name)
                                .unwrap_or_default();
                            let vw = viewer_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(v) = vw.upgrade() {
                                    let img = rgba_to_slint_image(w, h, &rgba);
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
                                    v.set_texture_name(edited_name.into());
                                }
                            });
                            let msg = format!("Текстура заменена из DDS: {}", path.display());
                            notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                } else {
                    let msg = "Не удалось перезагрузить превью после замены".to_string();
                    notify(&viewer_weak, &ui_weak, &msg, LogLevel::Warning);
                }
            });
        });

        // Сохранение внутреннего имени текстуры
        let tex_state_for_name = self.tex_state.clone();
        let viewer_weak_name = self.weak.clone();
        let ui_weak_name = crate::UI_WEAK.get().cloned();
        self.window.on_save_texture_internal_name(move |new_name| {
            let path = tex_state_for_name
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(_, _, _, p)| p.clone()));
            let Some(path) = path else {
                return;
            };
            let viewer_weak = viewer_weak_name.clone();
            let ui_weak = ui_weak_name.clone();
            let tex_state = tex_state_for_name.clone();
            let new_name = new_name.to_string();
            std::thread::spawn(
                move || match update_tgpcdx_internal_name(&path, &new_name) {
                    Ok(()) => {
                        if let Ok(data) = std::fs::read(&path)
                            && let Ok((w, h, rgba)) = decode_tgpcdx_to_rgba(&data)
                        {
                            if let Ok(mut s) = tex_state.lock() {
                                *s = Some((w, h, rgba.clone(), path.clone()));
                            }
                            let info = build_texture_info_text(&path, &data, w, h, false);
                            let name = inspect_tgpcdx_with_path(&data, Some(&path))
                                .map(|i| i.name)
                                .unwrap_or_default();
                            let vw = viewer_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(v) = vw.upgrade() {
                                    let img = rgba_to_slint_image(w, h, &rgba);
                                    v.set_texture_image(img);
                                    v.set_info_text(info.into());
                                    v.set_texture_name(name.into());
                                }
                            });
                        }
                        let msg = format!("Внутреннее имя текстуры сохранено: {}", path.display());
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Success);
                    }
                    Err(e) => {
                        let msg = format!("Ошибка сохранения имени: {e}");
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                    }
                },
            );
        });

        // Подстановка имени по имени файла
        let tex_state_for_fn = self.tex_state.clone();
        let viewer_weak_fn = self.weak.clone();
        self.window.on_texture_name_from_file(move || {
            let (path, current_name) = match tex_state_for_fn.lock().ok().and_then(|g| g.clone()) {
                Some((_, _, _, p)) => {
                    let nm = inspect_tgpcdx_file(&p).map(|i| i.name).unwrap_or_default();
                    (p, nm)
                }
                None => return,
            };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let prefix = if let Some(pos) = current_name.rfind('\\') {
                format!("{}\\", &current_name[..pos])
            } else if let Some(pos) = current_name.rfind('/') {
                format!("{}/", &current_name[..pos])
            } else {
                String::new()
            };
            let new_name = format!("{}{}", prefix, stem);
            let vw = viewer_weak_fn.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(v) = vw.upgrade() {
                    v.set_texture_name(new_name.into());
                }
            });
        });
    }
}

fn notify(
    viewer_weak: &Weak<TextureViewerWindow>,
    ui_weak: &Option<Weak<MainWindow>>,
    msg: &str,
    level: LogLevel,
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
                log_event(&ui, &msg_u, level);
            }
        });
    }
}

/// Собирает текст информационной панели просмотрщика из метаданных .TgPcDx.
/// При неудаче парсинга возвращает старую компактную строку.
fn build_texture_info_text(path: &Path, data: &[u8], w: u32, h: u32, show_alpha: bool) -> String {
    match inspect_tgpcdx_with_path(data, Some(path)) {
        Ok(info) => texture_info_text(&info, path, show_alpha),
        Err(_) => {
            let alpha = if show_alpha { "  [альфа]" } else { "" };
            format!("{} x {} px  TGPCDX  {}{}", w, h, path.display(), alpha)
        }
    }
}
