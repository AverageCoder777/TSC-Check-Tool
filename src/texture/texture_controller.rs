use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use slint::{ComponentHandle, Weak};

use super::decode::{ChannelMode, apply_channel_mode, decode_mip_to_rgba};
use super::inspect::{
    load_tgpcdx_bundle, restore_tgpcdx_from_backup, texture_info_text_with_view,
};
use super::serz_parse::ImageMip;
use super::{
    TextureInfo, convert_dds_to_tgpcdx_report, export_tgpcdx_to_dds, rgba_to_slint_image,
    update_tgpcdx_internal_name,
};
use crate::app::LogLevel;
use crate::auditor::AuditorController;
use crate::common::external_editor;
use crate::{AuditorState, MainWindow, TextureViewerWindow, log_event};

pub type LoadedTexture = (u32, u32, Vec<u8>, PathBuf);
pub type SharedTextureState = Arc<Mutex<Option<LoadedTexture>>>;

struct ViewerState {
    path: PathBuf,
    mips: Vec<ImageMip>,
    current_mip: usize,
    current_rgba: Arc<Vec<u8>>,
    channel_mode: ChannelMode,
    info: Option<TextureInfo>,
}

type SharedViewerState = Arc<Mutex<Option<ViewerState>>>;

struct TextureSnapshot {
    path: PathBuf,
    mips: Vec<ImageMip>,
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
    preview_rgba: Vec<u8>,
    info: Option<TextureInfo>,
    info_text: String,
    name: String,
    writable: bool,
    has_bak: bool,
}

pub struct TextureController {
    pub window: TextureViewerWindow,
    pub weak: Weak<TextureViewerWindow>,
    pub tex_state: SharedTextureState,
    viewer_state: SharedViewerState,
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
            viewer_state: Arc::new(Mutex::new(None)),
            preview_generation: Arc::new(AtomicU64::new(0)),
            auditor_state,
        }
    }

    pub fn bind(&self, auditor: &AuditorController) {
        self.bind_preview(auditor);
        self.bind_buttons();
    }

    pub fn bind_buttons(&self) {
        self.bind_editor();
        self.bind_view_controls();
        self.bind_dds_actions();
        self.bind_name_actions();
        self.bind_pixel_probe();
    }

    fn bind_preview(&self, auditor: &AuditorController) {
        let auditor_state = self.auditor_state.clone();
        let tex_state = self.tex_state.clone();
        let viewer_state = self.viewer_state.clone();
        let viewer_weak = self.weak.clone();
        let ui_weak = crate::UI_WEAK.get().cloned();
        let preview_generation = self.preview_generation.clone();

        auditor.window.on_preview_texture(move |row| {
            if row < 0 {
                return;
            }
            let target_item = auditor_state
                .lock()
                .ok()
                .and_then(|g| g.display_items.get(row as usize).cloned());
            let Some(item) = target_item else { return };

            let path = if is_tgpcdx_file(&item.expected_path) {
                item.expected_path
            } else if is_tgpcdx_file(&item.parent_file) {
                item.parent_file
            } else {
                return;
            };

            let generation_id = preview_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let gen_ref = preview_generation.clone();
            let tex_state = tex_state.clone();
            let viewer_state = viewer_state.clone();
            let viewer_weak = viewer_weak.clone();
            let ui_weak = ui_weak.clone();

            thread::spawn(move || {
                if gen_ref.load(Ordering::Acquire) != generation_id {
                    return;
                }
                let snapshot = match load_texture_snapshot(&path, ChannelMode::Rgb) {
                    Ok(s) => s,
                    Err(e) => {
                        let msg = format!("Failed to open texture {}: {e}", path.display());
                        notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
                        return;
                    }
                };
                if gen_ref.load(Ordering::Acquire) != generation_id {
                    return;
                }
                store_snapshot(&tex_state, &viewer_state, &snapshot, ChannelMode::Rgb);

                let _ = slint::invoke_from_event_loop(move || {
                    if gen_ref.load(Ordering::Acquire) != generation_id {
                        return;
                    }
                    if let Some(v) = viewer_weak.upgrade() {
                        apply_texture_to_viewer(&v, &snapshot, true);
                        let _ = v.show();
                    }
                });
            });
        });
    }

    fn bind_editor(&self) {
        let viewer_weak = self.weak.clone();
        let tex_state = self.tex_state.clone();
        let viewer_state = self.viewer_state.clone();
        let preview_gen = self.preview_generation.clone();
        let ui_weak = crate::UI_WEAK.get().cloned();

        self.window.on_open_in_editor(move || {
            let Some(path) = current_texture_path(&viewer_state) else {
                notify(
                    &viewer_weak,
                    &ui_weak,
                    "There is no loaded texture to open in editor.",
                    LogLevel::Warning,
                );
                return;
            };
            let gen_at_start = preview_gen.load(Ordering::Acquire);
            let cb_path = path.clone();
            let cb_viewer = viewer_weak.clone();
            let cb_ui = ui_weak.clone();
            let cb_tex = tex_state.clone();
            let cb_state = viewer_state.clone();
            let cb_gen = preview_gen.clone();

            if let Err(error) =
                external_editor::open_and_sync_with_vscode(&path, move |result| match result {
                    Ok(message) => {
                        if cb_gen.load(Ordering::Acquire) != gen_at_start {
                            return;
                        }
                        let mode = current_channel_mode(&cb_state);
                        match load_texture_snapshot(&cb_path, mode) {
                            Ok(snapshot) => {
                                store_snapshot(&cb_tex, &cb_state, &snapshot, mode);
                                notify(&cb_viewer, &cb_ui, &message, LogLevel::Info);
                                let vw = cb_viewer.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(v) = vw.upgrade() {
                                        apply_texture_to_viewer(&v, &snapshot, false);
                                    }
                                });
                            }
                            Err(error) => {
                                let msg = format!(
                                    "File changed, but preview could not be updated: {error}"
                                );
                                notify(&cb_viewer, &cb_ui, &msg, LogLevel::Error);
                            }
                        }
                    }
                    Err(error) => {
                        let msg = format!("Editor error: {error}");
                        notify(&cb_viewer, &cb_ui, &msg, LogLevel::Error);
                    }
                })
            {
                let msg = format!("Failed to open file in editor: {error}");
                notify(&viewer_weak, &ui_weak, &msg, LogLevel::Error);
            }
        });
    }

    fn bind_view_controls(&self) {
        // Переключение режима каналов (RGB / RGBA / Alpha / R / G / B) в фоновом потоке
        let viewer_weak = self.weak.clone();
        let viewer_state = self.viewer_state.clone();
        let preview_gen = self.preview_generation.clone();
        self.window.on_cycle_channel_mode(move || {
            let Some(v) = viewer_weak.upgrade() else {
                return;
            };
            let next_mode = ChannelMode::from_index(v.get_channel_mode() + 1);
            v.set_channel_mode(next_mode.as_index());

            let gen_id = preview_gen.fetch_add(1, Ordering::AcqRel) + 1;
            let (rgba, w, h, info_text) = {
                let Ok(mut guard) = viewer_state.lock() else {
                    return;
                };
                let Some(st) = guard.as_mut() else { return };
                st.channel_mode = next_mode;
                let mip = &st.mips[st.current_mip];
                let info_text = format_info_text(
                    st.info.as_ref(),
                    &st.path,
                    next_mode,
                    st.current_mip,
                    mip.width,
                    mip.height,
                );
                (st.current_rgba.clone(), mip.width, mip.height, info_text)
            };

            let vw = viewer_weak.clone();
            let gen_ref = preview_gen.clone();
            thread::spawn(move || {
                let preview = apply_channel_mode(&rgba, next_mode).into_owned();
                if gen_ref.load(Ordering::Acquire) != gen_id {
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if gen_ref.load(Ordering::Acquire) != gen_id {
                        return;
                    }
                    if let Some(v) = vw.upgrade() {
                        v.set_texture_image(rgba_to_slint_image(w, h, &preview));
                        v.set_info_text(info_text.into());
                    }
                });
            });
        });

        // Выбор уровня мипмапа (+1 / -1)
        let viewer_weak_mip = self.weak.clone();
        let tex_state_mip = self.tex_state.clone();
        let viewer_state_mip = self.viewer_state.clone();
        let preview_gen_mip = self.preview_generation.clone();
        let ui_weak_mip = crate::UI_WEAK.get().cloned();
        self.window.on_step_mip(move |delta| {
            let gen_id = preview_gen_mip.fetch_add(1, Ordering::AcqRel) + 1;
            let (target_mip, mip_index, mip_count, mode, path, info) = {
                let Ok(guard) = viewer_state_mip.lock() else {
                    return;
                };
                let Some(st) = guard.as_ref() else { return };
                if st.mips.len() <= 1 {
                    return;
                }
                let next_idx = (st.current_mip as i32 + delta)
                    .clamp(0, st.mips.len().saturating_sub(1) as i32)
                    as usize;
                if next_idx == st.current_mip {
                    return;
                }
                (
                    st.mips[next_idx].clone(),
                    next_idx,
                    st.mips.len(),
                    st.channel_mode,
                    st.path.clone(),
                    st.info.clone(),
                )
            };

            let vw = viewer_weak_mip.clone();
            let uiw = ui_weak_mip.clone();
            let tex_state = tex_state_mip.clone();
            let viewer_state = viewer_state_mip.clone();
            let gen_ref = preview_gen_mip.clone();

            thread::spawn(move || {
                let rgba = match decode_mip_to_rgba(&target_mip) {
                    Ok(r) => Arc::new(r),
                    Err(e) => {
                        let msg = format!("Failed to decode mip #{mip_index}: {e}");
                        notify(&vw, &uiw, &msg, LogLevel::Error);
                        return;
                    }
                };
                let preview = apply_channel_mode(&rgba, mode).into_owned();
                if gen_ref.load(Ordering::Acquire) != gen_id {
                    return;
                }

                if let Ok(mut guard) = viewer_state.lock()
                    && let Some(st) = guard.as_mut()
                {
                    st.current_mip = mip_index;
                    st.current_rgba = rgba.clone();
                }
                if let Ok(mut ts) = tex_state.lock() {
                    *ts = Some((
                        target_mip.width,
                        target_mip.height,
                        (*rgba).clone(),
                        path.clone(),
                    ));
                }

                let info_text = format_info_text(
                    info.as_ref(),
                    &path,
                    mode,
                    mip_index,
                    target_mip.width,
                    target_mip.height,
                );
                let mip_label = format!(
                    "Mip {}/{} ({}×{})",
                    mip_index + 1,
                    mip_count,
                    target_mip.width,
                    target_mip.height
                );

                let _ = slint::invoke_from_event_loop(move || {
                    if gen_ref.load(Ordering::Acquire) != gen_id {
                        return;
                    }
                    if let Some(v) = vw.upgrade() {
                        v.set_texture_image(rgba_to_slint_image(
                            target_mip.width,
                            target_mip.height,
                            &preview,
                        ));
                        v.set_texture_orig_width(target_mip.width as i32);
                        v.set_texture_orig_height(target_mip.height as i32);
                        v.set_mip_index(mip_index as i32);
                        v.set_mip_label(mip_label.into());
                        v.set_info_text(info_text.into());
                        v.set_pixel_info("".into());
                    }
                });
            });
        });
    }

    fn bind_dds_actions(&self) {
        // Экспорт в DDS
        let viewer_state = self.viewer_state.clone();
        let viewer_weak = self.weak.clone();
        let ui_weak = crate::UI_WEAK.get().cloned();
        self.window.on_export_dds(move || {
            let Some(path) = current_texture_path(&viewer_state) else {
                notify(
                    &viewer_weak,
                    &ui_weak,
                    "There is no loaded texture to export.",
                    LogLevel::Warning,
                );
                return;
            };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("texture");
            let Some(out) = rfd::FileDialog::new()
                .add_filter("DirectDraw Surface (*.dds)", &["dds"])
                .set_file_name(format!("{stem}.dds"))
                .save_file()
            else {
                return;
            };

            let vw = viewer_weak.clone();
            let uiw = ui_weak.clone();
            thread::spawn(move || {
                let (msg, level) = match export_tgpcdx_to_dds(&path, &out) {
                    Ok(()) => (
                        format!("DDS exported: {}", out.display()),
                        LogLevel::Success,
                    ),
                    Err(e) => (format!("DDS export failure: {e}"), LogLevel::Error),
                };
                notify(&vw, &uiw, &msg, level);
            });
        });

        // Замена из DDS
        let viewer_weak_replace = self.weak.clone();
        let tex_state_replace = self.tex_state.clone();
        let viewer_state_replace = self.viewer_state.clone();
        let preview_gen_replace = self.preview_generation.clone();
        let ui_weak_replace = crate::UI_WEAK.get().cloned();
        self.window.on_replace_from_dds(move || {
            let Some(path) = current_texture_path(&viewer_state_replace) else {
                notify(
                    &viewer_weak_replace,
                    &ui_weak_replace,
                    "There is no loaded texture to replace.",
                    LogLevel::Warning,
                );
                return;
            };
            if is_readonly(&path) {
                let msg = format!(
                    "File is read-only (possibly inside a .ap archive): {}. Unpack before replacing.",
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

            let gen_id = preview_gen_replace.fetch_add(1, Ordering::AcqRel) + 1;
            let gen_ref = preview_gen_replace.clone();
            let vw = viewer_weak_replace.clone();
            let uiw = ui_weak_replace.clone();
            let tex_state = tex_state_replace.clone();
            let viewer_state = viewer_state_replace.clone();

            thread::spawn(move || {
                if let Err(e) = external_editor::create_backup_file(&path) {
                    notify(&vw, &uiw, &format!("Failed to create backup: {e}"), LogLevel::Error);
                    return;
                }
                let report = match convert_dds_to_tgpcdx_report(&input, &path) {
                    Ok(r) => r,
                    Err(e) => {
                        notify(&vw, &uiw, &format!("Error replacing from DDS: {e}"), LogLevel::Error);
                        return;
                    }
                };
                let msg = format!(
                    "DDS imported: {} ({} mipmaps, {} B)",
                    report.dds_format, report.mip_count, report.total_bytes
                );
                notify(&vw, &uiw, &msg, LogLevel::Success);

                if gen_ref.load(Ordering::Acquire) != gen_id {
                    return;
                }
                let mode = current_channel_mode(&viewer_state);
                if let Ok(snapshot) = load_texture_snapshot(&path, mode) {
                    store_snapshot(&tex_state, &viewer_state, &snapshot, mode);
                    let vw_ui = vw.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if gen_ref.load(Ordering::Acquire) != gen_id {
                            return;
                        }
                        if let Some(v) = vw_ui.upgrade() {
                            apply_texture_to_viewer(&v, &snapshot, true);
                        }
                    });
                } else {
                    notify(
                        &vw,
                        &uiw,
                        "Texture replaced, but preview could not be reloaded.",
                        LogLevel::Warning,
                    );
                }
            });
        });

        // Восстановление из .bak
        let viewer_weak_bak = self.weak.clone();
        let tex_state_bak = self.tex_state.clone();
        let viewer_state_bak = self.viewer_state.clone();
        let preview_gen_bak = self.preview_generation.clone();
        let ui_weak_bak = crate::UI_WEAK.get().cloned();
        self.window.on_restore_backup(move || {
            let Some(path) = current_texture_path(&viewer_state_bak) else {
                return;
            };
            let gen_id = preview_gen_bak.fetch_add(1, Ordering::AcqRel) + 1;
            let gen_ref = preview_gen_bak.clone();
            let vw = viewer_weak_bak.clone();
            let uiw = ui_weak_bak.clone();
            let tex_state = tex_state_bak.clone();
            let viewer_state = viewer_state_bak.clone();

            thread::spawn(move || {
                if let Err(e) = restore_tgpcdx_from_backup(&path) {
                    notify(&vw, &uiw, &format!("Restore failed: {e}"), LogLevel::Error);
                    return;
                }
                let mode = current_channel_mode(&viewer_state);
                match load_texture_snapshot(&path, mode) {
                    Ok(snapshot) => {
                        if gen_ref.load(Ordering::Acquire) != gen_id {
                            return;
                        }
                        store_snapshot(&tex_state, &viewer_state, &snapshot, mode);
                        let vw_ui = vw.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if gen_ref.load(Ordering::Acquire) != gen_id {
                                return;
                            }
                            if let Some(v) = vw_ui.upgrade() {
                                apply_texture_to_viewer(&v, &snapshot, true);
                            }
                        });
                        notify(
                            &vw,
                            &uiw,
                            &format!("Restored from .bak: {}", path.display()),
                            LogLevel::Success,
                        );
                    }
                    Err(e) => {
                        notify(
                            &vw,
                            &uiw,
                            &format!("Restored, but preview failed: {e}"),
                            LogLevel::Warning,
                        );
                    }
                }
            });
        });
    }

    fn bind_name_actions(&self) {
        // Сохранение внутреннего имени
        let tex_state = self.tex_state.clone();
        let viewer_state = self.viewer_state.clone();
        let preview_gen = self.preview_generation.clone();
        let viewer_weak = self.weak.clone();
        let ui_weak = crate::UI_WEAK.get().cloned();
        self.window.on_save_texture_internal_name(move |new_name| {
            let Some(path) = current_texture_path(&viewer_state) else {
                return;
            };
            let gen_id = preview_gen.fetch_add(1, Ordering::AcqRel) + 1;
            let gen_ref = preview_gen.clone();
            let vw = viewer_weak.clone();
            let uiw = ui_weak.clone();
            let tex_state = tex_state.clone();
            let viewer_state = viewer_state.clone();
            let new_name = new_name.to_string();

            thread::spawn(move || match update_tgpcdx_internal_name(&path, &new_name) {
                Ok(()) => {
                    let mode = current_channel_mode(&viewer_state);
                    if let Ok(snapshot) = load_texture_snapshot(&path, mode)
                        && gen_ref.load(Ordering::Acquire) == gen_id
                    {
                        store_snapshot(&tex_state, &viewer_state, &snapshot, mode);
                        let vw_ui = vw.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if gen_ref.load(Ordering::Acquire) != gen_id {
                                return;
                            }
                            if let Some(v) = vw_ui.upgrade() {
                                apply_texture_to_viewer(&v, &snapshot, false);
                            }
                        });
                    }
                    notify(
                        &vw,
                        &uiw,
                        &format!("Internal texture name saved: {}", path.display()),
                        LogLevel::Success,
                    );
                }
                Err(e) => {
                    notify(&vw, &uiw, &format!("Failed to save name: {e}"), LogLevel::Error);
                }
            });
        });

        // Подстановка имени по имени файла
        let viewer_state_fn = self.viewer_state.clone();
        let viewer_weak_fn = self.weak.clone();
        self.window.on_texture_name_from_file(move || {
            let Some(v) = viewer_weak_fn.upgrade() else {
                return;
            };
            let (path, current_name) = {
                let Ok(guard) = viewer_state_fn.lock() else {
                    return;
                };
                let Some(st) = guard.as_ref() else { return };
                (
                    st.path.clone(),
                    st.info.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
                )
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let new_name = match current_name.rfind(['\\', '/']) {
                Some(pos) => format!("{}{stem}", &current_name[..=pos]),
                None => stem.to_owned(),
            };
            v.set_texture_name(new_name.into());
        });
    }

    fn bind_pixel_probe(&self) {
        let viewer_weak = self.weak.clone();
        let viewer_state = self.viewer_state.clone();
        self.window.on_probe_pixel(move |mx, my, vw, vh| {
            let Some(v) = viewer_weak.upgrade() else {
                return;
            };
            let zoom = v.get_zoom_factor();
            if zoom <= 0.0 {
                return;
            }
            let pan_x = v.get_pan_x();
            let pan_y = v.get_pan_y();

            let text = {
                let Ok(guard) = viewer_state.lock() else {
                    return;
                };
                let Some(st) = guard.as_ref() else { return };
                let mip = &st.mips[st.current_mip];
                let w = mip.width as f32;
                let h = mip.height as f32;
                let img_x = (vw - w * zoom) * 0.5 + pan_x;
                let img_y = (vh - h * zoom) * 0.5 + pan_y;
                let px = ((mx - img_x) / zoom).floor() as i64;
                let py = ((my - img_y) / zoom).floor() as i64;

                if px >= 0 && py >= 0 && (px as u32) < mip.width && (py as u32) < mip.height {
                    let offset = ((py as usize) * (mip.width as usize) + (px as usize)) * 4;
                    if let Some(&[r, g, b, a]) = st.current_rgba.get(offset..offset + 4) {
                        format!(
                            "X: {px}, Y: {py} | RGBA({r}, {g}, {b}, {a}) #{r:02X}{g:02X}{b:02X}{a:02X}"
                        )
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            };
            v.set_pixel_info(text.into());
        });
    }
    pub fn load_file_sync(&self, path: &Path) -> Result<(), String> {
        let snapshot = load_texture_snapshot(path, ChannelMode::Rgb)?;
        store_snapshot(
            &self.tex_state,
            &self.viewer_state,
            &snapshot,
            ChannelMode::Rgb,
        );
        apply_texture_to_viewer(&self.window, &snapshot, true);
        Ok(())
    }
}

fn is_tgpcdx_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tgpcdx"))
        && path.is_file()
}

fn current_texture_path(viewer_state: &SharedViewerState) -> Option<PathBuf> {
    viewer_state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.path.clone()))
}

fn current_channel_mode(viewer_state: &SharedViewerState) -> ChannelMode {
    viewer_state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.channel_mode))
        .unwrap_or_default()
}

fn is_readonly(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
}

fn load_texture_snapshot(path: &Path, mode: ChannelMode) -> Result<TextureSnapshot, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let (mips, info) = load_tgpcdx_bundle(&data, Some(path))?;
    let base = mips.first().ok_or("no image blocks")?;
    let width = base.width;
    let height = base.height;
    let rgba = Arc::new(decode_mip_to_rgba(base)?);
    let preview_rgba = apply_channel_mode(&rgba, mode).into_owned();
    let info_text = format_info_text(info.as_ref(), path, mode, 0, width, height);
    let name = info.as_ref().map(|i| i.name.clone()).unwrap_or_default();
    let has_bak = info.as_ref().is_some_and(|i| i.has_bak);
    let writable = !is_readonly(path);

    Ok(TextureSnapshot {
        path: path.to_path_buf(),
        mips,
        width,
        height,
        rgba,
        preview_rgba,
        info,
        info_text,
        name,
        writable,
        has_bak,
    })
}

fn store_snapshot(
    tex_state: &SharedTextureState,
    viewer_state: &SharedViewerState,
    snapshot: &TextureSnapshot,
    mode: ChannelMode,
) {
    if let Ok(mut st) = tex_state.lock() {
        *st = Some((
            snapshot.width,
            snapshot.height,
            (*snapshot.rgba).clone(),
            snapshot.path.clone(),
        ));
    }
    if let Ok(mut vs) = viewer_state.lock() {
        *vs = Some(ViewerState {
            path: snapshot.path.clone(),
            mips: snapshot.mips.clone(),
            current_mip: 0,
            current_rgba: snapshot.rgba.clone(),
            channel_mode: mode,
            info: snapshot.info.clone(),
        });
    }
}

fn apply_texture_to_viewer(v: &TextureViewerWindow, snapshot: &TextureSnapshot, reset_view: bool) {
    v.set_texture_image(rgba_to_slint_image(
        snapshot.width,
        snapshot.height,
        &snapshot.preview_rgba,
    ));
    v.set_texture_orig_width(snapshot.width as i32);
    v.set_texture_orig_height(snapshot.height as i32);
    v.set_mip_index(0);
    v.set_mip_count(snapshot.mips.len() as i32);
    v.set_mip_label(
        format!(
            "Mip 1/{} ({}×{})",
            snapshot.mips.len(),
            snapshot.width,
            snapshot.height
        )
        .into(),
    );
    if reset_view {
        v.set_zoom_factor(if snapshot.width > 640 || snapshot.height > 480 {
            (640.0 / snapshot.width as f32).min(480.0 / snapshot.height as f32)
        } else {
            1.0
        });
        v.set_pan_x(0.0);
        v.set_pan_y(0.0);
        v.set_channel_mode(ChannelMode::Rgb.as_index());
        v.set_status_text("".into());
        v.set_pixel_info("".into());
    }
    v.set_info_text(snapshot.info_text.as_str().into());
    v.set_texture_name(snapshot.name.as_str().into());
    v.set_texture_name_enabled(snapshot.writable);
    v.set_has_bak(snapshot.has_bak);
}

fn format_info_text(
    info: Option<&TextureInfo>,
    path: &Path,
    mode: ChannelMode,
    mip_index: usize,
    mip_w: u32,
    mip_h: u32,
) -> String {
    match info {
        Some(i) => texture_info_text_with_view(i, path, mode, mip_index, mip_w, mip_h),
        None => format!(
            "{mip_w} × {mip_h} px  [{}]  TGPCDX  {}",
            mode.label(),
            path.display()
        ),
    }
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
            v.set_status_text(msg.as_str().into());
        }
        if let Some(ui) = uiw.and_then(|w| w.upgrade()) {
            log_event(&ui, &msg, level);
        }
    });
}
