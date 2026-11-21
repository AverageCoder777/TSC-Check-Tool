mod pipelines;
mod textures;

use std::collections::HashMap;
use std::sync::{Arc, mpsc};

use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::geo::{GeoRenderMode, GeoSubmesh, GeoVertex};
use pipelines::{
    CameraUniform, GpuVertex, MaterialUniform, PipelineKey, Pipelines, create_uniform_layout,
};
use textures::create_texture_bind_groups;

#[cfg(test)]
use textures::downscale_rgba;

struct SubmeshDraw {
    submesh: GeoSubmesh,
    triangle_range: (u32, u32),
    wire_range: (u32, u32),
    center: Vec3,
    texture_idx: usize,
    material_idx: usize,
    visible: bool,
}

struct RenderTargets {
    size: (u32, u32),
    color_texture: wgpu::Texture,
    color_view: wgpu::TextureView,
    _depth_texture: wgpu::Texture,
    depth_view: wgpu::TextureView,
    readback_buffer: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl RenderTargets {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let color_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GeoPcDx offscreen color target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let depth_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GeoPcDx depth target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let padded_bytes_per_row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GeoPcDx CPU readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Self {
            size: (width, height),
            color_texture,
            color_view,
            _depth_texture: depth_texture,
            depth_view,
            readback_buffer,
            padded_bytes_per_row,
        }
    }
}

pub(crate) struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    max_render_dimension: u32,
    pipelines: Pipelines,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    texture_bind_groups: Vec<wgpu::BindGroup>,
    material_bind_groups: Vec<wgpu::BindGroup>,
    vertex_buffer: wgpu::Buffer,
    triangle_indices: wgpu::Buffer,
    wire_indices: wgpu::Buffer,
    draws: Vec<SubmeshDraw>,
    opaque_order: Vec<usize>,
    blended_order: Vec<usize>,
    targets: Option<RenderTargets>,
}

impl GpuRenderer {
    pub(crate) fn new(
        vertices: &[GeoVertex],
        indices: &[u32],
        submeshes: &[GeoSubmesh],
        center: Vec3,
        textures: &[Arc<super::viewer3d::Texture>],
    ) -> Result<Self, String> {
        pollster::block_on(Self::new_async(
            vertices, indices, submeshes, center, textures,
        ))
    }

    async fn new_async(
        vertices: &[GeoVertex],
        indices: &[u32],
        submeshes: &[GeoSubmesh],
        center: Vec3,
        textures: &[Arc<super::viewer3d::Texture>],
    ) -> Result<Self, String> {
        if vertices.len() > u32::MAX as usize || indices.len() > (u32::MAX / 2) as usize {
            return Err("mesh exceeds the GPU's indexed draw limits".into());
        }
        if textures.is_empty() {
            return Err("model has no GPU textures".into());
        }

        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok_or("No compatible GPU adapter is available")?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("GeoPcDx offscreen renderer"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter.limits(),
                },
                None,
            )
            .await
            .map_err(|error| format!("Failed to create GPU device: {error}"))?;

        let max_texture_dimension = device.limits().max_texture_dimension_2d;
        let max_render_dimension = max_texture_dimension.min(4096);

        let packed_vertices: Vec<GpuVertex> = vertices
            .iter()
            .map(|vertex| {
                let pos = Vec3::from_array(vertex.position) - center;
                GpuVertex {
                    position: [-pos.x, pos.y, pos.z],
                    normal: [-vertex.normal[0], vertex.normal[1], vertex.normal[2]],
                    uv: vertex.uv,
                }
            })
            .collect();
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx vertices"),
            contents: bytemuck::cast_slice(&packed_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let triangle_indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx triangle indices"),
            contents: bytemuck::cast_slice(indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        let (texture_bind_group_layout, texture_bind_groups) =
            create_texture_bind_groups(&device, &queue, textures, max_texture_dimension)?;
        let max_texture_idx = texture_bind_groups.len().saturating_sub(1);

        let material_bind_group_layout = create_uniform_layout(
            &device,
            "GeoPcDx material bindings",
            wgpu::ShaderStages::FRAGMENT,
        );
        let mut material_bind_groups = Vec::new();
        let mut material_cache: HashMap<u32, usize> = HashMap::new();

        let mut wire_lines = Vec::with_capacity(indices.len() * 2);
        let mut draws = Vec::with_capacity(submeshes.len());
        let mut opaque_order = Vec::new();
        let mut blended_order = Vec::new();
        let total_indices = indices.len();

        for (index, submesh) in submeshes.iter().enumerate() {
            let start = (submesh.first_index as usize).min(total_indices);
            let end = start
                .saturating_add(submesh.index_count as usize)
                .min(total_indices);
            let (triangles, _) = indices[start..end].as_chunks::<3>();
            let triangle_range = (start as u32, (triangles.len() * 3) as u32);

            let wire_start = u32::try_from(wire_lines.len())
                .map_err(|_| "wireframe index buffer exceeds u32::MAX")?;
            let mut min_pt = Vec3::splat(f32::INFINITY);
            let mut max_pt = Vec3::splat(f32::NEG_INFINITY);
            for &[a, b, c] in triangles {
                wire_lines.extend_from_slice(&[a, b, b, c, c, a]);
                for idx in [a, b, c] {
                    if let Some(v) = packed_vertices.get(idx as usize) {
                        let p = Vec3::from_array(v.position);
                        min_pt = min_pt.min(p);
                        max_pt = max_pt.max(p);
                    }
                }
            }
            let wire_end = u32::try_from(wire_lines.len())
                .map_err(|_| "wireframe index buffer exceeds u32::MAX")?;
            let wire_range = (wire_start, wire_end - wire_start);
            let submesh_center = if min_pt.x.is_finite() {
                (min_pt + max_pt) * 0.5
            } else {
                Vec3::ZERO
            };

            let material_idx = *material_cache
                .entry(submesh.alpha_cutoff.to_bits())
                .or_insert_with(|| {
                    let uniform = MaterialUniform {
                        alpha_cutoff: submesh.alpha_cutoff,
                        _pad: [0.0; 3],
                    };
                    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("GeoPcDx material uniform"),
                        contents: bytemuck::cast_slice(&[uniform]),
                        usage: wgpu::BufferUsages::UNIFORM,
                    });
                    let idx = material_bind_groups.len();
                    material_bind_groups.push(device.create_bind_group(
                        &wgpu::BindGroupDescriptor {
                            label: Some("GeoPcDx material bindings"),
                            layout: &material_bind_group_layout,
                            entries: &[wgpu::BindGroupEntry {
                                binding: 0,
                                resource: buffer.as_entire_binding(),
                            }],
                        },
                    ));
                    idx
                });

            if submesh.render_mode == GeoRenderMode::Blended {
                blended_order.push(index);
            } else {
                opaque_order.push(index);
            }

            draws.push(SubmeshDraw {
                visible: submesh.visible,
                texture_idx: (submesh.texture_index as usize).min(max_texture_idx),
                material_idx,
                triangle_range,
                wire_range,
                center: submesh_center,
                submesh: submesh.clone(),
            });
        }

        // Pre-sort opaque/cutout batches to minimize GPU pipeline and bind-group switches.
        opaque_order.sort_unstable_by_key(|&i| {
            let d = &draws[i];
            let mode_key = match d.submesh.render_mode {
                GeoRenderMode::Opaque => 0u8,
                GeoRenderMode::Cutout => 1u8,
                GeoRenderMode::Blended => 2u8,
            };
            (mode_key, d.submesh.mirrored, d.texture_idx, d.material_idx)
        });

        let wire_indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx wireframe indices"),
            contents: bytemuck::cast_slice(&wire_lines),
            usage: wgpu::BufferUsages::INDEX,
        });

        let camera_bind_group_layout = create_uniform_layout(
            &device,
            "GeoPcDx camera bindings",
            wgpu::ShaderStages::VERTEX,
        );
        let initial_camera = CameraUniform {
            view_projection: Mat4::IDENTITY.to_cols_array(),
        };
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx camera uniform"),
            contents: bytemuck::cast_slice(&[initial_camera]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("GeoPcDx frame camera bindings"),
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        let pipelines = Pipelines::new(
            &device,
            &camera_bind_group_layout,
            &texture_bind_group_layout,
            &material_bind_group_layout,
        );

        Ok(Self {
            device,
            queue,
            max_render_dimension,
            pipelines,
            camera_buffer,
            camera_bind_group,
            texture_bind_groups,
            material_bind_groups,
            vertex_buffer,
            triangle_indices,
            wire_indices,
            draws,
            opaque_order,
            blended_order,
            targets: None,
        })
    }

    pub(crate) fn set_batch_visible(&mut self, batch_idx: usize, visible: bool) {
        if let Some(draw) = self.draws.get_mut(batch_idx) {
            draw.visible = visible;
        }
    }

    pub(crate) fn apply_batch_preset(&mut self, preset: &str) {
        for draw in &mut self.draws {
            draw.visible = draw.submesh.preset_visible(preset);
        }
    }

    fn ensure_render_targets(&mut self, width: u32, height: u32) {
        if self.targets.as_ref().is_some_and(|t| t.size == (width, height)) {
            return;
        }
        self.targets = Some(RenderTargets::new(&self.device, width, height));
    }

    pub(crate) fn render(
        &mut self,
        camera: super::viewer3d::Camera,
        radius: f32,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let width = camera.width.clamp(1, self.max_render_dimension);
        let height = camera.height.clamp(1, self.max_render_dimension);
        self.ensure_render_targets(width, height);

        let aspect = width as f32 / height as f32;
        let matrices = camera.compute_matrices(aspect, radius);
        let eye = matrices.eye;
        let camera_uniform = CameraUniform {
            view_projection: matrices.view_projection.to_cols_array(),
        };
        self.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[camera_uniform]),
        );

        // Sort blended submeshes back-to-front relative to the camera.
        self.blended_order.sort_unstable_by(|&a, &b| {
            let da = (self.draws[a].center - eye).length_squared();
            let db = (self.draws[b].center - eye).length_squared();
            db.total_cmp(&da)
        });

        let targets = self
            .targets
            .as_ref()
            .ok_or("render targets were not initialized")?;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("GeoPcDx render encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("GeoPcDx offscreen pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Linear RGB corresponding to sRGB #1C2028.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0116,
                            g: 0.0144,
                            b: 0.0212,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(
                if camera.wireframe {
                    self.wire_indices.slice(..)
                } else {
                    self.triangle_indices.slice(..)
                },
                wgpu::IndexFormat::Uint32,
            );
            pass.set_bind_group(0, &self.camera_bind_group, &[]);

            let mut last_pipeline: Option<PipelineKey> = None;
            let mut last_texture: Option<usize> = None;
            let mut last_material: Option<usize> = None;

            for &index in self.opaque_order.iter().chain(&self.blended_order) {
                let draw = &self.draws[index];
                if !draw.visible {
                    continue;
                }
                let (start, count) = if camera.wireframe {
                    draw.wire_range
                } else {
                    draw.triangle_range
                };
                if count == 0 {
                    continue;
                }

                let pipeline_key = PipelineKey::select(
                    draw.submesh.render_mode,
                    draw.submesh.mirrored,
                    camera.wireframe,
                );
                if last_pipeline != Some(pipeline_key) {
                    pass.set_pipeline(self.pipelines.get(pipeline_key));
                    last_pipeline = Some(pipeline_key);
                }
                if last_texture != Some(draw.texture_idx) {
                    pass.set_bind_group(1, &self.texture_bind_groups[draw.texture_idx], &[]);
                    last_texture = Some(draw.texture_idx);
                }
                if last_material != Some(draw.material_idx) {
                    pass.set_bind_group(2, &self.material_bind_groups[draw.material_idx], &[]);
                    last_material = Some(draw.material_idx);
                }
                pass.draw_indexed(start..start + count, 0, 0..1);
            }
        }

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &targets.color_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &targets.readback_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(targets.padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = targets.readback_buffer.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .map_err(|error| format!("GPU readback channel failed: {error}"))?
            .map_err(|error| format!("GPU readback failed: {error}"))?;

        let mapped = slice.get_mapped_range();
        let bytes_per_row = (width as usize) * 4;
        let padded_bytes_per_row = targets.padded_bytes_per_row as usize;
        let pixels = if padded_bytes_per_row == bytes_per_row {
            mapped[..bytes_per_row * height as usize].to_vec()
        } else {
            let mut pixels = Vec::with_capacity(bytes_per_row * height as usize);
            for row in mapped
                .chunks_exact(padded_bytes_per_row)
                .take(height as usize)
            {
                pixels.extend_from_slice(&row[..bytes_per_row]);
            }
            pixels
        };
        drop(mapped);
        targets.readback_buffer.unmap();

        Ok((width, height, pixels))
    }
}

#[cfg(test)]
#[path = "../../tests/gpu_renderer.rs"]
mod tests;