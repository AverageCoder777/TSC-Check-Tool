mod cpu_renderer;
mod model;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use glam::{Mat4, Vec3};
use slint::ComponentHandle;

use self::cpu_renderer::render_cpu;
use self::model::LoadedModel;
use crate::common::external_editor;
use crate::geo::GeoModel;
use crate::geo::gpu_renderer::GpuRenderer;
use crate::{BatchEntry, GeoViewerWindow};

#[derive(Clone, Copy)]
pub(crate) struct Camera {
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) distance: f32,
    pub(crate) pan_x: f32,
    pub(crate) pan_y: f32,
    pub(crate) target: Vec3,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) wireframe: bool,
}

pub(crate) struct CameraMatrices {
    pub(crate) eye: Vec3,
    pub(crate) view: Mat4,
    pub(crate) view_projection: Mat4,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.7,
            pitch: 0.25,
            distance: 4.0,
            pan_x: 0.0,
            pan_y: 0.0,
            target: Vec3::ZERO,
            width: 900,
            height: 560,
            wireframe: false,
        }
    }
}

impl Camera {
    /// Orthonormal camera basis `(direction_to_eye, screen_right, screen_up)` in RH space.
    pub(crate) fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let pitch = self.pitch.clamp(-1.56, 1.56);
        let (sin_p, cos_p) = pitch.sin_cos();
        let (sin_y, cos_y) = self.yaw.sin_cos();
        let direction = Vec3::new(sin_y * cos_p, sin_p, cos_y * cos_p);
        let forward = -direction;
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        (direction, right, up)
    }

    /// Computes view/projection matrices allowing close-up inspection down to millimeters/centimeters.
    pub(crate) fn compute_matrices(&self, aspect: f32, radius: f32) -> CameraMatrices {
        let safe_radius = radius.max(0.01);
        let min_distance = (safe_radius * 0.002).clamp(0.005, 0.05);
        let distance = self.distance.max(min_distance);
        let (direction, _, _) = self.basis();
        let eye = self.target + direction * distance;
        let view = Mat4::look_at_rh(eye, self.target, Vec3::Y);
        let near = (distance * 0.01).clamp(0.001, 0.5);
        let far = (distance + safe_radius * 4.0 + self.target.length() * 2.0).max(near + 1.0);
        let projection = Mat4::perspective_rh(45.0_f32.to_radians(), aspect, near, far);
        CameraMatrices {
            eye,
            view,
            view_projection: projection * view,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Texture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

impl Texture {
    pub(crate) fn fallback() -> Self {
        Self {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 255, 255, 24, 24, 24, 255, 24, 24, 24, 255, 255, 0, 255, 255,
            ],
        }
    }

    pub(crate) fn sample(&self, uv: [f32; 2]) -> [u8; 4] {
        if self.width == 0 || self.height == 0 {
            return [255, 0, 255, 255];
        }
        let u = if uv[0].is_finite() {
            uv[0].rem_euclid(1.0)
        } else {
            0.0
        };
        let v = if uv[1].is_finite() {
            uv[1].rem_euclid(1.0)
        } else {
            0.0
        };
        let x = ((u * self.width as f32).floor() as u32).min(self.width - 1);
        let y = ((v * self.height as f32).floor() as u32).min(self.height - 1);
        let offset = ((y * self.width + x) * 4) as usize;
        match self.pixels.get(offset..offset + 4) {
            Some(px) => [px[0], px[1], px[2], px[3]],
            None => [255, 0, 255, 255],
        }
    }
}

enum ViewCommand {
    Camera(Camera),
    ToggleBatch { index: usize, visible: bool },
    Preset(String),
    Filter(String),
}

pub fn show_geo_viewer(path: &Path) -> Result<(), String> {
    let window = GeoViewerWindow::new().map_err(|error| error.to_string())?;
    let weak = window.as_weak();
    bind_external_editor(&window, path);
    window.set_model_name(
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    let (command_tx, command_rx) = mpsc::channel();
    bind_camera(&window, command_tx.clone());
    bind_batch_panel(&window, command_tx);
    let model_path = path.to_path_buf();
    thread::Builder::new()
        .name("geo-viewer-render".into())
        .spawn(move || render_worker(model_path, weak, command_rx))
        .map_err(|error| format!("Failed to start model renderer: {error}"))?;
    window.run().map_err(|error| error.to_string())
}

fn bind_external_editor(window: &GeoViewerWindow, path: &Path) {
    let editor_weak = window.as_weak();
    let editor_path = path.to_path_buf();
    window.on_open_in_editor(move || {
        let callback_weak = editor_weak.clone();
        if let Err(error) =
            external_editor::open_and_sync_with_vscode(&editor_path, move |result| {
                let status = match result {
                    Ok(message) => message,
                    Err(error) => format!("Ошибка редактора: {error}"),
                };
                set_status(&callback_weak, status);
            })
        {
            set_status(
                &editor_weak,
                format!("Не удалось открыть редактор: {error}"),
            );
        }
    });
}

fn bind_camera(window: &GeoViewerWindow, sender: Sender<ViewCommand>) {
    let weak = window.as_weak();
    window.on_camera_changed(
        move |yaw, pitch, distance, pan_x, pan_y, width, height, wireframe| {
            let scale = weak
                .upgrade()
                .map(|w| w.window().scale_factor())
                .unwrap_or(1.0);
            let _ = sender.send(ViewCommand::Camera(Camera {
                yaw,
                pitch,
                distance,
                pan_x,
                pan_y,
                target: Vec3::ZERO,
                width: (width * scale).round().max(1.0) as u32,
                height: (height * scale).round().max(1.0) as u32,
                wireframe,
            }));
        },
    );
}

fn bind_batch_panel(window: &GeoViewerWindow, sender: Sender<ViewCommand>) {
    let toggle_sender = sender.clone();
    window.on_toggle_batch(move |index, visible| {
        if index >= 0 {
            let _ = toggle_sender.send(ViewCommand::ToggleBatch {
                index: index as usize,
                visible,
            });
        }
    });
    let preset_sender = sender.clone();
    window.on_apply_batch_preset(move |preset| {
        let _ = preset_sender.send(ViewCommand::Preset(preset.to_string()));
    });
    window.on_filter_batches(move |text| {
        let _ = sender.send(ViewCommand::Filter(text.to_string()));
    });
}

fn set_status(window: &slint::Weak<GeoViewerWindow>, status: String) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_status_text(status.into());
        }
    });
}

fn push_frame(window: &slint::Weak<GeoViewerWindow>, width: u32, height: u32, pixels: Vec<u8>) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                bytemuck::cast_slice(&pixels),
                width,
                height,
            );
            window.set_frame(slint::Image::from_rgba8(buffer));
        }
    });
}

fn render_worker(
    path: PathBuf,
    window: slint::Weak<GeoViewerWindow>,
    receiver: Receiver<ViewCommand>,
) {
    let mut loaded = match LoadedModel::load(&path) {
        Ok(model) => model,
        Err(error) => {
            set_status(&window, format!("Unable to load model: {error}"));
            return;
        }
    };

    let texture_status = loaded.texture_status.clone();
    let initial_distance = loaded.radius * 2.8;
    let (mut gpu, renderer_status) = create_gpu_renderer(&loaded);
    let stats = loaded.stats();
    let entries = batch_entries(&loaded.model, "");
    let initial_window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = initial_window.upgrade() {
            window.set_status_text(renderer_status.into());
            window.set_stats_text(stats.into());
            window.set_texture_status(texture_status.into());
            window.set_initial_distance(initial_distance);
            window.set_distance(initial_distance);
            window.set_batch_items(slint::ModelRc::new(slint::VecModel::from(entries)));
        }
    });

    let mut camera = Camera {
        distance: initial_distance,
        ..Camera::default()
    };
    match render_frame(&loaded, gpu.as_mut(), camera) {
        Ok((width, height, pixels)) => push_frame(&window, width, height, pixels),
        Err(error) => {
            set_status(&window, format!("Render failed: {error}"));
            return;
        }
    }

    let mut filter = String::new();
    while let Ok(command) = receiver.recv() {
        let mut need_render = false;
        let mut list_dirty = false;
        let mut pending = Some(command);
        while let Some(current) = pending.take() {
            match current {
                ViewCommand::Camera(mut next) => {
                    // Convert incoming pan deltas into world-space target translation along the
                    // current camera's screen-right and screen-up axes so orbiting stays centered
                    // on the panned spot. Resetting pan_x/pan_y to (0, 0) re-centers on the model.
                    if next.pan_x == 0.0
                        && next.pan_y == 0.0
                        && (camera.pan_x != 0.0 || camera.pan_y != 0.0)
                    {
                        next.target = Vec3::ZERO;
                    } else {
                        let dx = next.pan_x - camera.pan_x;
                        let dy = next.pan_y - camera.pan_y;
                        let (_, right, up) = next.basis();
                        let max_pan = loaded.radius * 4.0;
                        next.target = (camera.target + right * dx + up * dy)
                            .clamp(Vec3::splat(-max_pan), Vec3::splat(max_pan));
                    }
                    camera = next;
                    need_render = true;
                }
                ViewCommand::ToggleBatch { index, visible } => {
                    set_batch_visibility(&mut loaded, gpu.as_mut(), index, visible);
                    list_dirty = true;
                    need_render = true;
                }
                ViewCommand::Preset(preset) => {
                    apply_batch_preset(&mut loaded, gpu.as_mut(), &preset);
                    list_dirty = true;
                    need_render = true;
                }
                ViewCommand::Filter(text) => {
                    filter = text;
                    list_dirty = true;
                }
            }
            pending = receiver.try_recv().ok();
        }
        if list_dirty {
            let entries = batch_entries(&loaded.model, &filter);
            let stats = loaded.stats();
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_batch_items(slint::ModelRc::new(slint::VecModel::from(entries)));
                    window.set_stats_text(stats.into());
                }
            });
        }
        if !need_render {
            continue;
        }
        match render_frame(&loaded, gpu.as_mut(), camera) {
            Ok((width, height, pixels)) => push_frame(&window, width, height, pixels),
            Err(error) => set_status(&window, format!("Render failed: {error}")),
        }
    }
}

fn set_batch_visibility(
    loaded: &mut LoadedModel,
    gpu: Option<&mut GpuRenderer>,
    index: usize,
    visible: bool,
) {
    if let Some(submesh) = loaded.model.submeshes.get_mut(index) {
        submesh.visible = visible;
    }
    if let Some(gpu) = gpu {
        gpu.set_batch_visible(index, visible);
    }
}

fn apply_batch_preset(loaded: &mut LoadedModel, gpu: Option<&mut GpuRenderer>, preset: &str) {
    for submesh in &mut loaded.model.submeshes {
        submesh.visible = submesh.preset_visible(preset);
    }
    if let Some(gpu) = gpu {
        gpu.apply_batch_preset(preset);
    }
}

fn batch_entries(model: &GeoModel, filter: &str) -> Vec<BatchEntry> {
    let needle = filter.trim().as_bytes();
    model
        .submeshes
        .iter()
        .enumerate()
        .filter(|(_, submesh)| {
            needle.is_empty()
                || contains_ignore_ascii_case(&submesh.node_name, needle)
                || contains_ignore_ascii_case(&submesh.material_name, needle)
                || contains_ignore_ascii_case(&submesh.shader_name, needle)
        })
        .map(|(index, submesh)| BatchEntry {
            index: index as i32,
            node_name: submesh.node_name.as_str().into(),
            material_name: submesh.material_name.as_str().into(),
            shader_name: submesh.shader_name.as_str().into(),
            lod_level: submesh.lod_level as i32,
            triangle_count: submesh.triangle_count as i32,
            visible: submesh.visible,
        })
        .collect()
}

fn contains_ignore_ascii_case(haystack: &str, needle: &[u8]) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn create_gpu_renderer(loaded: &LoadedModel) -> (Option<GpuRenderer>, String) {
    match GpuRenderer::new(
        &loaded.model.vertices,
        &loaded.model.indices,
        &loaded.model.submeshes,
        loaded.center,
        &loaded.textures,
    ) {
        Ok(renderer) => (Some(renderer), String::new()),
        Err(error) => (
            None,
            format!("GPU unavailable; using CPU fallback: {error}"),
        ),
    }
}

fn render_frame(
    model: &LoadedModel,
    gpu: Option<&mut GpuRenderer>,
    camera: Camera,
) -> Result<(u32, u32, Vec<u8>), String> {
    if let Some(gpu) = gpu {
        return gpu
            .render(camera, model.radius)
            .map_err(|error| format!("offscreen GPU render failed: {error}"));
    }
    Ok(render_cpu(model, camera))
}

#[cfg(test)]
#[path = "../../tests/viewer3d.rs"]
mod tests;
