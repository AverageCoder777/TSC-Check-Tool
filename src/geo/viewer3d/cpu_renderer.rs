use glam::{Mat4, Vec2, Vec3, Vec4};

use super::model::LoadedModel;
use super::{Camera, Texture};
use crate::geo::{GeoRenderMode, GeoVertex};

// Pre-normalized Vec3::new(-0.4, 0.7, 0.6)
const LIGHT_DIR: Vec3 = Vec3::new(-0.398_014_9, 0.696_526_05, 0.597_022_35);
const BG_COLOR: [u8; 4] = [28, 32, 40, 255];
const WIRE_COLOR: [u8; 4] = [105, 190, 245, 255];

#[derive(Clone, Copy)]
struct ProjectedVertex {
    position: Vec3,
    normal_view: Vec3,
    uv: Vec2,
}

pub(super) fn render_cpu(model: &LoadedModel, camera: Camera) -> (u32, u32, Vec<u8>) {
    const MAX_DIMENSION: u32 = 1200;
    let width = camera.width.clamp(1, MAX_DIMENSION);
    let height = camera.height.clamp(1, MAX_DIMENSION);
    let pixel_count = (width * height) as usize;

    let mut pixels = vec![0u8; pixel_count * 4];
    pixels.as_chunks_mut::<4>().0.fill(BG_COLOR);
    let mut depth = vec![f32::INFINITY; pixel_count];

    let aspect = width as f32 / height as f32;
    let matrices = camera.compute_matrices(aspect, model.radius);

    let projected: Vec<Option<ProjectedVertex>> = model
        .model
        .vertices
        .iter()
        .map(|vertex| {
            project_vertex(
                vertex,
                matrices.view,
                matrices.view_projection,
                model.center,
                width,
                height,
            )
        })
        .collect();

    let total_indices = model.model.indices.len();
    let ordered_submeshes = model
        .model
        .submeshes
        .iter()
        .filter(|s| s.visible && s.render_mode != GeoRenderMode::Blended)
        .chain(
            model
                .model
                .submeshes
                .iter()
                .filter(|s| s.visible && s.render_mode == GeoRenderMode::Blended),
        );

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

        for &[i0, i1, i2] in model.model.indices[start..end].as_chunks::<3>().0 {
            let (Some(a), Some(b), Some(c)) = (
                projected.get(i0 as usize).copied().flatten(),
                projected.get(i1 as usize).copied().flatten(),
                projected.get(i2 as usize).copied().flatten(),
            ) else {
                continue;
            };
            if camera.wireframe {
                draw_line(&mut pixels, width, height, a.position.truncate(), b.position.truncate());
                draw_line(&mut pixels, width, height, b.position.truncate(), c.position.truncate());
                draw_line(&mut pixels, width, height, c.position.truncate(), a.position.truncate());
            } else {
                raster_triangle(
                    &mut pixels,
                    &mut depth,
                    width,
                    height,
                    [a, b, c],
                    texture,
                    submesh.render_mode,
                    alpha_cutoff,
                );
            }
        }
    }

    (width, height, pixels)
}

fn project_vertex(
    vertex: &GeoVertex,
    view: Mat4,
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
    let normal_view = view.transform_vector3(normal).normalize_or_zero();
    Some(ProjectedVertex {
        position: Vec3::new(
            (ndc.x * 0.5 + 0.5) * width as f32,
            (0.5 - ndc.y * 0.5) * height as f32,
            ndc.z,
        ),
        normal_view,
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
    render_mode: GeoRenderMode,
    alpha_cutoff: u8,
) {
    let [a, b, c] = vertices;
    let a2 = a.position.truncate();
    let b2 = b.position.truncate();
    let c2 = c.position.truncate();

    let area = (b2 - a2).perp_dot(c2 - a2);
    if area.abs() < f32::EPSILON {
        return;
    }
    let inv_area = 1.0 / area;

    let raw_min_x = a2.x.min(b2.x).min(c2.x);
    let raw_max_x = a2.x.max(b2.x).max(c2.x);
    let raw_min_y = a2.y.min(b2.y).min(c2.y);
    let raw_max_y = a2.y.max(b2.y).max(c2.y);
    if raw_max_x < 0.0 || raw_max_y < 0.0 || raw_min_x >= width as f32 || raw_min_y >= height as f32
    {
        return;
    }

    let min_x = raw_min_x.floor().max(0.0) as u32;
    let max_x = raw_max_x.ceil().min(width as f32 - 1.0) as u32;
    let min_y = raw_min_y.floor().max(0.0) as u32;
    let max_y = raw_max_y.ceil().min(height as f32 - 1.0) as u32;

    for y in min_y..=max_y {
        let py = y as f32 + 0.5;
        let row_offset = (y * width) as usize;
        for x in min_x..=max_x {
            let point = Vec2::new(x as f32 + 0.5, py);
            let w0 = (b2 - point).perp_dot(c2 - point) * inv_area;
            let w1 = (c2 - point).perp_dot(a2 - point) * inv_area;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let z = a.position.z * w0 + b.position.z * w1 + c.position.z * w2;
            if !(0.0..=1.0).contains(&z) {
                continue;
            }
            let offset = row_offset + x as usize;
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
            let normal_view =
                (a.normal_view * w0 + b.normal_view * w1 + c.normal_view * w2).normalize_or_zero();
            let light = 0.35 + normal_view.dot(LIGHT_DIR).abs() * 0.65;
            let start = offset * 4;
            if render_mode == GeoRenderMode::Blended {
                let alpha = texel[3] as f32 * (1.0 / 255.0);
                let inv_alpha = 1.0 - alpha;
                for channel in 0..3 {
                    let source = texel[channel] as f32 * light;
                    let destination = pixels[start + channel] as f32;
                    pixels[start + channel] = (source * alpha + destination * inv_alpha) as u8;
                }
                pixels[start + 3] = 255;
            } else {
                pixels[start] = (texel[0] as f32 * light) as u8;
                pixels[start + 1] = (texel[1] as f32 * light) as u8;
                pixels[start + 2] = (texel[2] as f32 * light) as u8;
                pixels[start + 3] = 255;
                depth[offset] = z;
            }
        }
    }
}

fn draw_line(pixels: &mut [u8], width: u32, height: u32, start: Vec2, end: Vec2) {
    let steps = (end - start).abs().max_element().ceil() as usize;
    if steps == 0 {
        return;
    }
    let inv_steps = 1.0 / steps as f32;
    let max_w = width as i32;
    let max_h = height as i32;
    for step in 0..=steps {
        let point = start.lerp(end, step as f32 * inv_steps);
        let x = point.x as i32;
        let y = point.y as i32;
        if x < 0 || y < 0 || x >= max_w || y >= max_h {
            continue;
        }
        let offset = ((y as u32 * width + x as u32) * 4) as usize;
        pixels[offset..offset + 4].copy_from_slice(&WIRE_COLOR);
    }
}