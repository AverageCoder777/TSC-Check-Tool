use crate::geo::{GeoAssembly, GeoModel, GeoRenderMode, GeoVertex};
use glam::{Mat4, Vec2, Vec3, Vec4};
use rayon::prelude::*;
use slint::ComponentHandle;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Copy)]
pub(crate) struct Camera {
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) distance: f32,
    pub(crate) pan_x: f32,
    pub(crate) pan_y: f32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) wireframe: bool,
    pub(crate) side_by_side: bool,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.7,
            pitch: 0.25,
            distance: 4.0,
            pan_x: 0.0,
            pan_y: 0.0,
            width: 900,
            height: 560,
            wireframe: false,
            side_by_side: false,
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
        let u = uv[0].rem_euclid(1.0);
        let v = (1.0 - uv[1]).rem_euclid(1.0);
        let x = (u * self.width as f32).floor() as u32 % self.width;
        let y = (v * self.height as f32).floor() as u32 % self.height;
        let offset = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[offset],
            self.pixels[offset + 1],
            self.pixels[offset + 2],
            self.pixels[offset + 3],
        ]
    }
}

struct LoadedModel {
    assembly: GeoAssembly,
    model: GeoModel,
    texture_sources: Vec<(PathBuf, String)>,
    texture: Arc<Texture>,
    textures: Vec<Arc<Texture>>,
    texture_status: String,
    center: Vec3,
    radius: f32,
    side_by_side: bool,
    texture_catalog: HashMap<(PathBuf, String), (String, Result<Arc<Texture>, String>)>,
}

impl LoadedModel {
    fn load(path: &Path) -> Result<Self, String> {
        let assembly = GeoAssembly::load(path)?;
        let side_by_side = !assembly.has_blueprint_attachments;
        let (model, texture_sources) = assembly.compose(side_by_side)?;
        let mut texture_references = HashMap::new();
        for part in &assembly.parts {
            for name in &part.model.textures {
                texture_references.insert((part.path.clone(), name.clone()), ());
            }
        }
        let texture_references = texture_references.into_keys().collect::<Vec<_>>();
        let resolved = texture_references
            .par_iter()
            .map(|(geo_path, name)| crate::geo::resolve_texture_data(geo_path, name))
            .collect::<Vec<_>>();
        let mut unique_resolved = HashMap::<PathBuf, (String, Vec<u8>)>::new();
        let resolved_paths = resolved
            .iter()
            .map(|entry| {
                entry.as_ref().map(|(resolved_path, bytes)| {
                    let key = PathBuf::from(resolved_path);
                    unique_resolved
                        .entry(key.clone())
                        .or_insert_with(|| (resolved_path.clone(), bytes.clone()));
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
        let texture_catalog = texture_references
            .into_iter()
            .zip(resolved_paths)
            .filter_map(|(reference, resolved_path)| {
                let path_key = resolved_path?;
                let (display_path, result) = decoded.get(&path_key)?;
                Some((reference, (display_path.clone(), result.clone())))
            })
            .collect();
        let mut loaded = Self {
            assembly,
            model,
            texture_sources,
            texture: Arc::new(Texture::fallback()),
            textures: Vec::new(),
            texture_status: String::new(),
            center: Vec3::ZERO,
            radius: 0.01,
            side_by_side,
            texture_catalog,
        };
        loaded.update_bounds();
        loaded.update_textures();
        Ok(loaded)
    }

    fn set_layout(&mut self, side_by_side: bool) -> Result<(), String> {
        let (model, texture_sources) = self.assembly.compose(side_by_side)?;
        self.model = model;
        self.texture_sources = texture_sources;
        self.side_by_side = side_by_side;
        self.update_bounds();
        self.update_textures();
        Ok(())
    }

    fn update_bounds(&mut self) {
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for &index in &self.model.indices {
            let position = Vec3::from_array(self.model.vertices[index as usize].position);
            min = min.min(position);
            max = max.max(position);
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
        format!(
            "{} vertices  •  {} triangles  •  {} draw batches  •  {} textures  •  {} assembly parts",
            self.model.vertices.len(),
            self.model.indices.len() / 3,
            self.model.submeshes.len(),
            self.model.textures.len(),
            self.assembly.parts.len().saturating_sub(1)
        )
    }
}

pub fn show_geo_viewer(path: &Path) -> Result<(), String> {
    let window = crate::GeoViewerWindow::new().map_err(|error| error.to_string())?;
    let weak = window.as_weak();
    window.set_model_name(
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    let (render_tx, render_rx) = mpsc::channel();
    bind_camera(&window, render_tx);
    let model_path = path.to_path_buf();
    std::thread::Builder::new()
        .name("geo-viewer-render".into())
        .spawn(move || render_worker(model_path, weak, render_rx))
        .map_err(|error| format!("Failed to start model renderer: {error}"))?;
    window.run().map_err(|error| error.to_string())
}

fn bind_camera(window: &crate::GeoViewerWindow, sender: Sender<Camera>) {
    window.on_camera_changed(
        move |yaw, pitch, distance, pan_x, pan_y, width, height, wireframe, side_by_side| {
            let _ = sender.send(Camera {
                yaw,
                pitch,
                distance,
                pan_x,
                pan_y,
                width: width.max(1.0) as u32,
                height: height.max(1.0) as u32,
                wireframe,
                side_by_side,
            });
        },
    );
}

fn render_worker(
    path: std::path::PathBuf,
    window: slint::Weak<crate::GeoViewerWindow>,
    receiver: Receiver<Camera>,
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
    let stats = loaded.stats();
    let texture_status = loaded.texture_status.clone();
    let initial_distance = loaded.radius * 2.8;
    let (mut gpu, renderer_status) = create_gpu_renderer(&loaded);
    let sibling_parts_available = loaded.assembly.parts.len() > 1;
    let side_by_side = loaded.side_by_side;
    let initial_window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = initial_window.upgrade() {
            window.set_status_text(renderer_status.into());
            window.set_stats_text(stats.into());
            window.set_texture_status(texture_status.into());
            window.set_initial_distance(initial_distance);
            window.set_distance(initial_distance);
            window.set_sibling_parts_available(sibling_parts_available);
            window.set_side_by_side(side_by_side);
        }
    });

    let mut camera = Camera {
        distance: initial_distance,
        side_by_side,
        ..Camera::default()
    };
    let frame = render_frame(&loaded, gpu.as_mut(), camera);
    let (width, height, pixels) = match frame {
        Ok(frame) => frame,
        Err(error) => {
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_status_text(format!("Render failed: {error}").into());
                }
            });
            return;
        }
    };
    let first_frame_window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = first_frame_window.upgrade() {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                bytemuck::cast_slice(&pixels),
                width,
                height,
            );
            window.set_frame(slint::Image::from_rgba8(buffer));
        }
    });
    loop {
        let Ok(next) = receiver.recv() else {
            break;
        };
        camera = next;
        while let Ok(next) = receiver.try_recv() {
            camera = next;
        }
        if camera.side_by_side != loaded.side_by_side {
            match loaded.set_layout(camera.side_by_side) {
                Ok(()) => {
                    let initial_distance = loaded.radius * 2.8;
                    camera.distance = initial_distance;
                    camera.pan_x = 0.0;
                    camera.pan_y = 0.0;
                    let (new_gpu, status) = create_gpu_renderer(&loaded);
                    gpu = new_gpu;
                    let stats = loaded.stats();
                    let texture_status = loaded.texture_status.clone();
                    let side_by_side = loaded.side_by_side;
                    let weak = window.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.set_status_text(status.into());
                            window.set_stats_text(stats.into());
                            window.set_texture_status(texture_status.into());
                            window.set_initial_distance(initial_distance);
                            window.set_distance(initial_distance);
                            window.set_side_by_side(side_by_side);
                        }
                    });
                }
                Err(error) => {
                    crate::geo::log_assembly_warning(
                        &path,
                        &format!("Unable to switch assembly layout: {error}"),
                    );
                    camera.side_by_side = loaded.side_by_side;
                    let side_by_side = loaded.side_by_side;
                    let weak = window.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.set_side_by_side(side_by_side);
                        }
                    });
                }
            }
        }
        let (width, height, pixels) = match render_frame(&loaded, gpu.as_mut(), camera) {
            Ok(frame) => frame,
            Err(error) => {
                let weak = window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_status_text(format!("Render failed: {error}").into());
                    }
                });
                continue;
            }
        };
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
        match gpu.render(camera, model.radius) {
            Ok(frame) => return Ok(frame),
            Err(error) => {
                return Err(format!("offscreen GPU render failed: {error}"));
            }
        }
    }
    Ok(render(model, camera))
}

fn render(model: &LoadedModel, camera: Camera) -> (u32, u32, Vec<u8>) {
    const MAX_DIMENSION: u32 = 1200;
    let width = camera.width.clamp(1, MAX_DIMENSION);
    let height = camera.height.clamp(1, MAX_DIMENSION);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[28, 32, 40, 255]);
    }
    let mut depth = vec![f32::INFINITY; (width * height) as usize];
    let aspect = width as f32 / height as f32;
    let radius = model.radius.max(0.01);
    let distance = camera.distance.max(radius * 1.05);
    let eye_offset = Vec3::new(
        camera.yaw.sin() * camera.pitch.cos(),
        camera.pitch.sin(),
        camera.yaw.cos() * camera.pitch.cos(),
    ) * distance;
    let eye = model.center + eye_offset;
    let forward = (model.center - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    let up = right.cross(forward).normalize_or_zero();
    let target = model.center + right * camera.pan_x + up * camera.pan_y;
    let view = Mat4::look_at_rh(
        eye + right * camera.pan_x + up * camera.pan_y,
        target,
        Vec3::Y,
    );
    let projection =
        Mat4::perspective_rh(45.0f32.to_radians(), aspect, 0.01, distance + radius * 4.0);
    let transform = projection * view;
    let mut projected = Vec::with_capacity(model.model.vertices.len());
    for vertex in &model.model.vertices {
        projected.push(project_vertex(
            vertex,
            transform,
            model.center,
            width,
            height,
        ));
    }
    let ordered_submeshes = model
        .model
        .submeshes
        .iter()
        .filter(|submesh| submesh.render_mode != GeoRenderMode::Blended)
        .chain(
            model
                .model
                .submeshes
                .iter()
                .filter(|submesh| submesh.render_mode == GeoRenderMode::Blended),
        );
    for submesh in ordered_submeshes {
        let start = submesh.first_index as usize;
        let end = start + submesh.index_count as usize;
        let texture = model
            .textures
            .get(submesh.texture_index as usize)
            .unwrap_or(&model.texture);
        for triangle in model.model.indices[start..end].chunks_exact(3) {
            let a = projected[triangle[0] as usize];
            let b = projected[triangle[1] as usize];
            let c = projected[triangle[2] as usize];
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
                    view,
                    submesh.render_mode,
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
    transform: Mat4,
    center: Vec3,
    width: u32,
    height: u32,
) -> Option<ProjectedVertex> {
    let world = Vec3::from_array(vertex.position) - center;
    let clip = transform * Vec4::new(world.x, world.y, world.z, 1.0);
    if clip.w <= f32::EPSILON {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if !ndc.is_finite() {
        return None;
    }
    Some(ProjectedVertex {
        position: Vec3::new(
            (ndc.x * 0.5 + 0.5) * width as f32,
            (0.5 - ndc.y * 0.5) * height as f32,
            ndc.z,
        ),
        normal: Vec3::from_array(vertex.normal).normalize_or_zero(),
        uv: Vec2::from_array(vertex.uv),
    })
}

fn raster_triangle(
    pixels: &mut [u8],
    depth: &mut [f32],
    width: u32,
    height: u32,
    vertices: [ProjectedVertex; 3],
    texture: &Texture,
    view: Mat4,
    render_mode: GeoRenderMode,
) {
    let [a, b, c] = vertices;
    let edge1 = b.position.truncate() - a.position.truncate();
    let edge2 = c.position.truncate() - a.position.truncate();
    let area = edge1.perp_dot(edge2);
    if area.abs() < f32::EPSILON {
        return;
    }
    let min_x = a
        .position
        .x
        .min(b.position.x)
        .min(c.position.x)
        .floor()
        .max(0.0) as u32;
    let max_x = a
        .position
        .x
        .max(b.position.x)
        .max(c.position.x)
        .ceil()
        .min(width as f32 - 1.0) as u32;
    let min_y = a
        .position
        .y
        .min(b.position.y)
        .min(c.position.y)
        .floor()
        .max(0.0) as u32;
    let max_y = a
        .position
        .y
        .max(b.position.y)
        .max(c.position.y)
        .ceil()
        .min(height as f32 - 1.0) as u32;
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
            let offset = (y * width + x) as usize;
            if z >= depth[offset] {
                continue;
            }
            let uv = a.uv * w0 + b.uv * w1 + c.uv * w2;
            let texel = texture.sample(uv.to_array());
            if (render_mode == GeoRenderMode::Cutout && texel[3] < 128)
                || (render_mode == GeoRenderMode::Blended && texel[3] == 0)
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
