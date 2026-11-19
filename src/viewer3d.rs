use crate::geo::{GeoModel, GeoRenderMode, GeoVertex};
use glam::{Mat4, Vec2, Vec3, Vec4};
use rayon::prelude::*;
use slint::ComponentHandle;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Copy)]
pub(crate) struct Camera {
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) distance: f32,
    pub(crate) pan_x: f32,
    pub(crate) pan_y: f32,
    /// World-space orbit pivot in model-centered RH coordinates.
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
        let direction = Vec3::new(
            self.yaw.sin() * pitch.cos(),
            pitch.sin(),
            self.yaw.cos() * pitch.cos(),
        );
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

enum ViewCommand {
    Camera(Camera),
    ToggleBatch { index: usize, visible: bool },
    Preset(String),
    Filter(String),
}

#[derive(Clone)]
pub(crate) struct Texture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

impl Texture {
    fn fallback() -> Self {
        Self {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 255, 255, 24, 24, 24, 255, 24, 24, 24, 255, 255, 0, 255, 255,
            ],
        }
    }

    fn sample(&self, uv: [f32; 2]) -> [u8; 4] {
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
        if offset + 4 > self.pixels.len() {
            return [255, 0, 255, 255];
        }
        [
            self.pixels[offset],
            self.pixels[offset + 1],
            self.pixels[offset + 2],
            self.pixels[offset + 3],
        ]
    }
}

struct LoadedModel {
    model: GeoModel,
    texture_sources: Vec<(PathBuf, String)>,
    texture: Arc<Texture>,
    textures: Vec<Arc<Texture>>,
    texture_status: String,
    center: Vec3,
    radius: f32,
    texture_catalog: HashMap<(PathBuf, String), (String, Result<Arc<Texture>, String>)>,
}

impl LoadedModel {
    fn load(path: &Path) -> Result<Self, String> {
        let model = GeoModel::load(path)?;
        let texture_sources = model
            .textures
            .iter()
            .map(|name| (path.to_path_buf(), name.clone()))
            .collect::<Vec<_>>();
        let mut archive_toc_cache = HashMap::new();
        let resolved = texture_sources
            .iter()
            .map(|(geo_path, name)| {
                crate::geo::texture::resolve_texture_data(geo_path, name, &mut archive_toc_cache)
            })
            .collect::<Vec<_>>();
        let mut unique_resolved = HashMap::<PathBuf, (String, Vec<u8>)>::new();
        let resolved_paths = resolved
            .into_iter()
            .map(|entry| {
                entry.map(|(resolved_path, bytes)| {
                    let key = PathBuf::from(&resolved_path);
                    unique_resolved
                        .entry(key.clone())
                        .or_insert((resolved_path, bytes));
                    key
                })
            })
            .collect::<Vec<_>>();
        let decoded = unique_resolved
            .into_par_iter()
            .map(|(key, (resolved_path, bytes))| {
                let result = crate::texture::decode_tgpcdx_for_viewer(&bytes).map(
                    |(width, height, pixels)| {
                        Arc::new(Texture {
                            width,
                            height,
                            pixels,
                        })
                    },
                );
                (key, (resolved_path, result))
            })
            .collect::<HashMap<_, _>>();
        let texture_catalog = texture_sources
            .iter()
            .cloned()
            .zip(resolved_paths)
            .filter_map(|(reference, resolved_path)| {
                let path_key = resolved_path?;
                let (display_path, result) = decoded.get(&path_key)?;
                Some((reference, (display_path.clone(), result.clone())))
            })
            .collect();
        let mut loaded = Self {
            model,
            texture_sources,
            texture: Arc::new(Texture::fallback()),
            textures: Vec::new(),
            texture_status: String::new(),
            center: Vec3::ZERO,
            radius: 0.01,
            texture_catalog,
        };
        loaded.update_bounds();
        loaded.update_textures();
        Ok(loaded)
    }

    fn update_bounds(&mut self) {
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for &index in &self.model.indices {
            if let Some(vertex) = self.model.vertices.get(index as usize) {
                let position = Vec3::from_array(vertex.position);
                min = min.min(position);
                max = max.max(position);
            }
        }
        if min.is_finite() && max.is_finite() {
            self.center = (min + max) * 0.5;
            self.radius = ((max - min) * 0.5).length().max(0.01);
        }
    }

    fn update_textures(&mut self) {
        let fallback = Arc::new(Texture::fallback());
        let mut textures = Vec::new();
        let mut statuses = Vec::new();
        for (index, name) in self.model.textures.iter().enumerate() {
            let source = &self.texture_sources[index];
            let resolved_texture =
                if let Some((resolved_path, decoded)) = self.texture_catalog.get(source) {
                    match decoded {
                        Ok(texture) => {
                            statuses.push(format!("Loaded: {name} ({resolved_path})"));
                            Arc::clone(texture)
                        }
                        Err(error) => {
                            statuses.push(format!("Missing / invalid: {name} ({error})"));
                            fallback.clone()
                        }
                    }
                } else {
                    statuses.push(format!("Missing: {name}"));
                    fallback.clone()
                };
            textures.push(resolved_texture);
        }
        if statuses.is_empty() {
            statuses.push("No texture references found".into());
            textures.push(fallback.clone());
        }
        self.texture = textures.first().cloned().unwrap_or(fallback);
        self.textures = textures;
        self.texture_status = statuses.join("    •    ");
    }

    fn stats(&self) -> String {
        let visible_batches = self
            .model
            .submeshes
            .iter()
            .filter(|submesh| submesh.visible)
            .count();
        let visible_triangles: u32 = self
            .model
            .submeshes
            .iter()
            .filter(|submesh| submesh.visible)
            .map(|submesh| submesh.triangle_count)
            .sum();
        format!(
            "{} vertices  •  {} / {} triangles  •  {} / {} draw batches  •  {} textures",
            self.model.vertices.len(),
            visible_triangles,
            self.model.indices.len() / 3,
            visible_batches,
            self.model.submeshes.len(),
            self.model.textures.len()
        )
    }
}

pub fn show_geo_viewer(path: &Path) -> Result<(), String> {
    let window = crate::GeoViewerWindow::new().map_err(|error| error.to_string())?;
    let weak = window.as_weak();
    let editor_weak = window.as_weak();
    let editor_path = path.to_path_buf();
    window.on_open_in_editor(move || {
        let callback_weak = editor_weak.clone();
        if let Err(error) =
            crate::external_editor::open_and_sync_with_vscode(&editor_path, move |result| {
                let status = match result {
                    Ok(message) => message,
                    Err(error) => format!("Ошибка редактора: {error}"),
                };
                let viewer = callback_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(viewer) = viewer.upgrade() {
                        viewer.set_status_text(status.into());
                    }
                });
            })
        {
            let viewer = editor_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(viewer) = viewer.upgrade() {
                    viewer.set_status_text(format!("Не удалось открыть редактор: {error}").into());
                }
            });
        }
    });
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
    std::thread::Builder::new()
        .name("geo-viewer-render".into())
        .spawn(move || render_worker(model_path, weak, command_rx))
        .map_err(|error| format!("Failed to start model renderer: {error}"))?;
    window.run().map_err(|error| error.to_string())
}

fn bind_camera(window: &crate::GeoViewerWindow, sender: Sender<ViewCommand>) {
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

fn bind_batch_panel(window: &crate::GeoViewerWindow, sender: Sender<ViewCommand>) {
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

fn push_frame(
    window: &slint::Weak<crate::GeoViewerWindow>,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
) {
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
    window: slint::Weak<crate::GeoViewerWindow>,
    receiver: Receiver<ViewCommand>,
) {
    let mut loaded = match LoadedModel::load(&path) {
        Ok(model) => model,
        Err(error) => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    window.set_status_text(format!("Unable to load model: {error}").into());
                }
            });
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
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_status_text(format!("Render failed: {error}").into());
                }
            });
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
            Err(error) => {
                let weak = window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_status_text(format!("Render failed: {error}").into());
                    }
                });
            }
        }
    }
}

fn set_batch_visibility(
    loaded: &mut LoadedModel,
    gpu: Option<&mut crate::gpu_renderer::GpuRenderer>,
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

fn apply_batch_preset(
    loaded: &mut LoadedModel,
    gpu: Option<&mut crate::gpu_renderer::GpuRenderer>,
    preset: &str,
) {
    for submesh in &mut loaded.model.submeshes {
        let visible = submesh.preset_visible(preset);
        submesh.visible = visible;
    }
    if let Some(gpu) = gpu {
        gpu.apply_batch_preset(preset);
    }
}

fn batch_entries(model: &GeoModel, filter: &str) -> Vec<crate::BatchEntry> {
    let needle = filter.trim().to_ascii_lowercase();
    model
        .submeshes
        .iter()
        .enumerate()
        .filter(|(_, submesh)| {
            needle.is_empty()
                || [
                    &submesh.node_name,
                    &submesh.material_name,
                    &submesh.shader_name,
                ]
                .iter()
                .any(|value| value.to_ascii_lowercase().contains(&needle))
        })
        .map(|(index, submesh)| crate::BatchEntry {
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

fn create_gpu_renderer(loaded: &LoadedModel) -> (Option<crate::gpu_renderer::GpuRenderer>, String) {
    match crate::gpu_renderer::GpuRenderer::new(
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
    gpu: Option<&mut crate::gpu_renderer::GpuRenderer>,
    camera: Camera,
) -> Result<(u32, u32, Vec<u8>), String> {
    if let Some(gpu) = gpu {
        return gpu
            .render(camera, model.radius)
            .map_err(|error| format!("offscreen GPU render failed: {error}"));
    }
    Ok(render_cpu(model, camera))
}

fn render_cpu(model: &LoadedModel, camera: Camera) -> (u32, u32, Vec<u8>) {
    const MAX_DIMENSION: u32 = 1200;
    let width = camera.width.clamp(1, MAX_DIMENSION);
    let height = camera.height.clamp(1, MAX_DIMENSION);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[28, 32, 40, 255]);
    }
    let mut depth = vec![f32::INFINITY; (width * height) as usize];
    let aspect = width as f32 / height as f32;
    let matrices = camera.compute_matrices(aspect, model.radius);

    let mut projected = Vec::with_capacity(model.model.vertices.len());
    for vertex in &model.model.vertices {
        projected.push(project_vertex(
            vertex,
            matrices.view_projection,
            model.center,
            width,
            height,
        ));
    }

    let total_indices = model.model.indices.len();
    let ordered_submeshes =
        model
            .model
            .submeshes
            .iter()
            .filter(|submesh| submesh.visible && submesh.render_mode != GeoRenderMode::Blended)
            .chain(model.model.submeshes.iter().filter(|submesh| {
                submesh.visible && submesh.render_mode == GeoRenderMode::Blended
            }));

    for submesh in ordered_submeshes {
        let start = (submesh.first_index as usize).min(total_indices);
        let end = start
            .saturating_add(submesh.index_count as usize)
            .min(total_indices);
        let texture = model
            .textures
            .get(submesh.texture_index as usize)
            .unwrap_or(&model.texture);
        let alpha_cutoff = (submesh.alpha_cutoff.clamp(0.0, 1.0) * 255.0) as u8;
        for triangle in model.model.indices[start..end].chunks_exact(3) {
            let a = projected.get(triangle[0] as usize).copied().flatten();
            let b = projected.get(triangle[1] as usize).copied().flatten();
            let c = projected.get(triangle[2] as usize).copied().flatten();
            let (Some(a), Some(b), Some(c)) = (a, b, c) else {
                continue;
            };
            if camera.wireframe {
                draw_line(&mut pixels, width, height, a.position, b.position);
                draw_line(&mut pixels, width, height, b.position, c.position);
                draw_line(&mut pixels, width, height, c.position, a.position);
            } else {
                raster_triangle(
                    &mut pixels,
                    &mut depth,
                    width,
                    height,
                    [a, b, c],
                    texture,
                    matrices.view,
                    submesh.render_mode,
                    alpha_cutoff,
                );
            }
        }
    }
    (width, height, pixels)
}

#[derive(Clone, Copy)]
struct ProjectedVertex {
    position: Vec3,
    normal: Vec3,
    uv: Vec2,
}

fn project_vertex(
    vertex: &GeoVertex,
    view_projection: Mat4,
    center: Vec3,
    width: u32,
    height: u32,
) -> Option<ProjectedVertex> {
    // Convert Train Simulator LH coordinates to RH by flipping X on both position and normal.
    let centered = Vec3::from_array(vertex.position) - center;
    let world = Vec3::new(-centered.x, centered.y, centered.z);
    let clip = view_projection * Vec4::new(world.x, world.y, world.z, 1.0);
    if clip.w <= f32::EPSILON {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if !ndc.is_finite() {
        return None;
    }
    let normal =
        Vec3::new(-vertex.normal[0], vertex.normal[1], vertex.normal[2]).normalize_or_zero();
    Some(ProjectedVertex {
        position: Vec3::new(
            (ndc.x * 0.5 + 0.5) * width as f32,
            (0.5 - ndc.y * 0.5) * height as f32,
            ndc.z,
        ),
        normal,
        uv: Vec2::from_array(vertex.uv),
    })
}

#[allow(clippy::too_many_arguments)]
fn raster_triangle(
    pixels: &mut [u8],
    depth: &mut [f32],
    width: u32,
    height: u32,
    vertices: [ProjectedVertex; 3],
    texture: &Texture,
    view: Mat4,
    render_mode: GeoRenderMode,
    alpha_cutoff: u8,
) {
    let [a, b, c] = vertices;
    let edge1 = b.position.truncate() - a.position.truncate();
    let edge2 = c.position.truncate() - a.position.truncate();
    let area = edge1.perp_dot(edge2);
    if area.abs() < f32::EPSILON {
        return;
    }
    let raw_min_x = a.position.x.min(b.position.x).min(c.position.x);
    let raw_max_x = a.position.x.max(b.position.x).max(c.position.x);
    let raw_min_y = a.position.y.min(b.position.y).min(c.position.y);
    let raw_max_y = a.position.y.max(b.position.y).max(c.position.y);
    if raw_max_x < 0.0 || raw_max_y < 0.0 || raw_min_x >= width as f32 || raw_min_y >= height as f32
    {
        return;
    }
    let min_x = raw_min_x.floor().max(0.0) as u32;
    let max_x = raw_max_x.ceil().min(width as f32 - 1.0) as u32;
    let min_y = raw_min_y.floor().max(0.0) as u32;
    let max_y = raw_max_y.ceil().min(height as f32 - 1.0) as u32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let point = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
            let w0 = (b.position.truncate() - point).perp_dot(c.position.truncate() - point) / area;
            let w1 = (c.position.truncate() - point).perp_dot(a.position.truncate() - point) / area;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let z = a.position.z * w0 + b.position.z * w1 + c.position.z * w2;
            if !(0.0..=1.0).contains(&z) {
                continue;
            }
            let offset = (y * width + x) as usize;
            if z >= depth[offset] {
                continue;
            }
            let uv = a.uv * w0 + b.uv * w1 + c.uv * w2;
            let texel = texture.sample(uv.to_array());
            if (render_mode == GeoRenderMode::Cutout && texel[3] < alpha_cutoff)
                || (render_mode == GeoRenderMode::Blended && texel[3] < 5)
            {
                continue;
            }
            let normal = (a.normal * w0 + b.normal * w1 + c.normal * w2).normalize_or_zero();
            let normal_view = view.transform_vector3(normal).normalize_or_zero();
            let diffuse = normal_view.dot(Vec3::new(-0.4, 0.7, 0.6).normalize()).abs();
            let light = 0.35 + diffuse * 0.65;
            let alpha = if render_mode == GeoRenderMode::Blended {
                texel[3] as f32 / 255.0
            } else {
                1.0
            };
            let start = offset * 4;
            for channel in 0..3 {
                let source = texel[channel] as f32 * light;
                let destination = pixels[start + channel] as f32;
                pixels[start + channel] = (source * alpha + destination * (1.0 - alpha)) as u8;
            }
            pixels[start + 3] = 255;
            if render_mode != GeoRenderMode::Blended {
                depth[offset] = z;
            }
        }
    }
}

fn draw_line(pixels: &mut [u8], width: u32, height: u32, start: Vec3, end: Vec3) {
    let steps = (end.truncate() - start.truncate())
        .abs()
        .max_element()
        .ceil() as usize;
    if steps == 0 {
        return;
    }
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let point = start.lerp(end, t);
        let x = point.x as i32;
        let y = point.y as i32;
        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
            continue;
        }
        let offset = ((y as u32 * width + x as u32) * 4) as usize;
        pixels[offset..offset + 4].copy_from_slice(&[105, 190, 245, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_triangle_geometry(path: &Path, texture: &str) {
        let mut vertex_bytes = Vec::new();
        for position in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for value in position
                .into_iter()
                .chain([0.0, 0.0, 1.0])
                .chain([0.0, 0.0])
            {
                vertex_bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        let vertex_hex = vertex_bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let index_hex = [0_u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <VertexData><blob>{vertex_hex}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <TextureByName><e d:type="cDeltaString">{texture}</e></TextureByName>
            </Root>"#
        );
        std::fs::write(path, xml).unwrap();
    }

    #[test]
    fn loads_only_the_selected_geopcdx_without_blueprint_children() {
        let directory = tempfile::tempdir().unwrap();
        let geometry_path = directory.path().join("Main.GeoPcDx");
        let child_geometry_path = directory.path().join("Bogie.GeoPcDx");
        write_triangle_geometry(&geometry_path, r"Textures\main.TgPcDx");
        write_triangle_geometry(&child_geometry_path, r"Textures\bogie.TgPcDx");
        std::fs::write(
            directory.path().join("Main.bin"),
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cWagonBlueprint>
                    <GeometryID d:type="cDeltaString">Main.GeoPcDx</GeometryID>
                    <ContainerComponent><Children>
                        <cEntityContainerBlueprint-sChild>
                            <BlueprintID d:type="cDeltaString">Bogie.xml</BlueprintID>
                        </cEntityContainerBlueprint-sChild>
                    </Children></ContainerComponent>
                </cWagonBlueprint>
            </Root>"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Bogie.bin"),
            r#"<Root><cEngineBlueprint>
                <GeometryID d:type="cDeltaString">Bogie.GeoPcDx</GeometryID>
            </cEngineBlueprint></Root>"#,
        )
        .unwrap();

        let loaded = LoadedModel::load(&geometry_path).unwrap();

        assert_eq!(loaded.model.vertices.len(), 3);
        assert_eq!(loaded.model.indices.len(), 3);
        assert_eq!(loaded.model.textures, [r"Textures\main.TgPcDx"]);
        assert_eq!(
            loaded.stats(),
            "3 vertices  •  1 / 1 triangles  •  1 / 1 draw batches  •  1 textures"
        );
    }
}
