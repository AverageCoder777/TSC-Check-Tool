use crate::geo::{GeoRenderMode, GeoSubmesh, GeoVertex};
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::mpsc;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

pub(crate) struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    opaque_pipeline: wgpu::RenderPipeline,
    cutout_pipeline: wgpu::RenderPipeline,
    alpha_pipeline: wgpu::RenderPipeline,
    wire_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    vertex_buffer: wgpu::Buffer,
    triangle_indices: wgpu::Buffer,
    wire_indices: wgpu::Buffer,
    submeshes: Vec<GeoSubmesh>,
    texture_views: Vec<wgpu::TextureView>,
    sampler: wgpu::Sampler,
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

        let packed_vertices = vertices
            .iter()
            .map(|vertex| GpuVertex {
                position: (Vec3::from_array(vertex.position) - center).to_array(),
                normal: vertex.normal,
                uv: vertex.uv,
            })
            .collect::<Vec<_>>();
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
        let lines = indices
            .chunks_exact(3)
            .flat_map(|triangle| {
                [
                    triangle[0],
                    triangle[1],
                    triangle[1],
                    triangle[2],
                    triangle[2],
                    triangle[0],
                ]
            })
            .collect::<Vec<_>>();
        let wire_indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx wireframe indices"),
            contents: bytemuck::cast_slice(&lines),
            usage: wgpu::BufferUsages::INDEX,
        });

        let mut texture_views = Vec::with_capacity(textures.len());
        for texture_data in textures {
            let expected_bytes = texture_data
                .width
                .checked_mul(texture_data.height)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or("texture dimensions overflow")? as usize;
            if texture_data.width == 0
                || texture_data.height == 0
                || expected_bytes != texture_data.pixels.len()
            {
                return Err("invalid RGBA texture dimensions or data length".into());
            }
            let (width, height, pixels) = if texture_data.width > max_texture_dimension
                || texture_data.height > max_texture_dimension
            {
                let (scaled_width, scaled_height, scaled) = downscale_rgba(
                    texture_data.width,
                    texture_data.height,
                    &texture_data.pixels,
                    max_texture_dimension,
                )?;
                (scaled_width, scaled_height, Cow::Owned(scaled))
            } else {
                (
                    texture_data.width,
                    texture_data.height,
                    Cow::Borrowed(texture_data.pixels.as_slice()),
                )
            };
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("GeoPcDx albedo"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels.as_ref(),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            texture_views.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        }
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("GeoPcDx shader resources"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("GeoPcDx WGSL shader"),
            source: wgpu::ShaderSource::Wgsl(
                r#"
struct Camera {
    view_projection: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var albedo: texture_2d<f32>;
@group(0) @binding(2) var albedo_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) world_position: vec3<f32>,
};
@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.normal = input.normal;
    output.uv = input.uv;
    output.world_position = input.position;
    return output;
}
fn shade(input: VertexOutput) -> vec4<f32> {
    let color = textureSample(albedo, albedo_sampler, input.uv);
    var normal = input.normal;
    if dot(normal, normal) < 1e-10 {
        normal = cross(dpdx(input.world_position), dpdy(input.world_position));
        if dot(normal, normal) < 1e-10 {
            normal = vec3<f32>(0.0, 1.0, 0.0);
        }
    }
    normal = normalize(normal);
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let hemisphere = mix(0.58, 0.82, clamp(dot(normal, up) * 0.5 + 0.5, 0.0, 1.0));
    let key = abs(dot(normal, normalize(vec3<f32>(-0.45, 0.72, 0.52)))) * 0.32;
    let fill = abs(dot(normal, normalize(vec3<f32>(0.55, 0.2, -0.65)))) * 0.14;
    let rim_direction = normalize(vec3<f32>(0.35, 0.45, -0.82));
    let rim = pow(1.0 - max(dot(normal, rim_direction), 0.0), 2.0) * 0.10;
    let light = clamp(max(0.55, hemisphere + key + fill + rim), 0.55, 1.2);
    return vec4<f32>(color.rgb * light, color.a);
}
@fragment
fn fs_opaque(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = shade(input);
    return vec4<f32>(color.rgb, 1.0);
}
@fragment
fn fs_cutout(input: VertexOutput) -> @location(0) vec4<f32> {
    if textureSample(albedo, albedo_sampler, input.uv).a < 0.5 {
        discard;
    }
    let color = shade(input);
    return vec4<f32>(color.rgb, 1.0);
}
@fragment
fn fs_blended(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = shade(input);
    if color.a < 0.02 {
        discard;
    }
    return color;
}
"#
                .into(),
            ),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("GeoPcDx pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        };
        let make_pipeline = |label: &'static str,
                             topology: wgpu::PrimitiveTopology,
                             fragment_entry_point: &'static str,
                             blend: Option<wgpu::BlendState>,
                             depth_write_enabled: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: std::slice::from_ref(&vertex_layout),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: fragment_entry_point,
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth24Plus,
                    depth_write_enabled,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview: None,
            })
        };
        let opaque_pipeline = make_pipeline(
            "GeoPcDx opaque pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
        );
        let cutout_pipeline = make_pipeline(
            "GeoPcDx cutout pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_cutout",
            Some(wgpu::BlendState::REPLACE),
            true,
        );
        let alpha_pipeline = make_pipeline(
            "GeoPcDx alpha pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_blended",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            false,
        );
        let wire_pipeline = make_pipeline(
            "GeoPcDx wireframe pipeline",
            wgpu::PrimitiveTopology::LineList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
        );
        Ok(Self {
            device,
            queue,
            opaque_pipeline,
            cutout_pipeline,
            alpha_pipeline,
            wire_pipeline,
            bind_group_layout,
            vertex_buffer,
            triangle_indices,
            wire_indices,
            submeshes: submeshes.to_vec(),
            texture_views,
            sampler,
        })
    }

    pub(crate) fn render(
        &mut self,
        camera: super::viewer3d::Camera,
        radius: f32,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let width = camera.width.clamp(1, 1200);
        let height = camera.height.clamp(1, 1200);
        let aspect = width as f32 / height as f32;
        let distance = camera.distance.max(radius * 1.05);
        let direction = Vec3::new(
            camera.yaw.sin() * camera.pitch.cos(),
            camera.pitch.sin(),
            camera.yaw.cos() * camera.pitch.cos(),
        );
        let right = direction.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(direction).normalize_or_zero();
        let target = right * camera.pan_x + up * camera.pan_y;
        let eye = target + direction * distance;
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        let projection = Mat4::perspective_rh(
            45.0f32.to_radians(),
            aspect,
            (distance - radius * 1.5).max(0.01),
            distance + radius * 4.0,
        );
        let view_projection = projection * view;
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("GeoPcDx camera uniform"),
                contents: bytemuck::cast_slice(view_projection.as_ref()),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_groups = self
            .texture_views
            .iter()
            .map(|texture_view| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GeoPcDx frame bindings"),
                    layout: &self.bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(texture_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                    ],
                })
            })
            .collect::<Vec<_>>();
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GeoPcDx offscreen color target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GeoPcDx depth target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let bytes_per_row = width * 4;
        let padded_bytes_per_row = bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let output = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GeoPcDx CPU readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("GeoPcDx render encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("GeoPcDx offscreen pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.11,
                            g: 0.125,
                            b: 0.157,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
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
            let ordered_submeshes = self
                .submeshes
                .iter()
                .filter(|submesh| submesh.render_mode != GeoRenderMode::Blended)
                .chain(
                    self.submeshes
                        .iter()
                        .filter(|submesh| submesh.render_mode == GeoRenderMode::Blended),
                );
            for submesh in ordered_submeshes {
                let pipeline = if camera.wireframe {
                    &self.wire_pipeline
                } else {
                    match submesh.render_mode {
                        GeoRenderMode::Opaque => &self.opaque_pipeline,
                        GeoRenderMode::Cutout => &self.cutout_pipeline,
                        GeoRenderMode::Blended => &self.alpha_pipeline,
                    }
                };
                pass.set_pipeline(pipeline);
                let texture_index =
                    (submesh.texture_index as usize).min(bind_groups.len().saturating_sub(1));
                pass.set_bind_group(0, &bind_groups[texture_index], &[]);
                let start = if camera.wireframe {
                    submesh.first_index * 2
                } else {
                    submesh.first_index
                };
                let count = if camera.wireframe {
                    submesh.index_count * 2
                } else {
                    submesh.index_count
                };
                pass.draw_indexed(start..start + count, 0, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &color,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &output,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
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
        let slice = output.slice(..);
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
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for row in 0..height as usize {
            let source = row * padded_bytes_per_row as usize;
            let target = row * bytes_per_row as usize;
            pixels[target..target + bytes_per_row as usize]
                .copy_from_slice(&mapped[source..source + bytes_per_row as usize]);
        }
        drop(mapped);
        output.unmap();
        Ok((width, height, pixels))
    }
}

fn downscale_rgba(
    width: u32,
    height: u32,
    pixels: &[u8],
    max_dimension: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    if width == 0 || height == 0 || max_dimension == 0 {
        return Err("invalid RGBA texture dimensions".into());
    }
    let expected_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("texture dimensions overflow")? as usize;
    if pixels.len() != expected_bytes {
        return Err("invalid RGBA texture dimensions or data length".into());
    }
    let largest_dimension = width.max(height);
    if largest_dimension <= max_dimension {
        return Ok((width, height, pixels.to_vec()));
    }
    let scale = max_dimension as f64 / largest_dimension as f64;
    let scaled_width = ((width as f64 * scale).floor() as u32).max(1);
    let scaled_height = ((height as f64 * scale).floor() as u32).max(1);
    let scaled_len = scaled_width
        .checked_mul(scaled_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("scaled texture dimensions overflow")? as usize;
    let mut scaled = vec![0; scaled_len];
    for y in 0..scaled_height {
        let source_y = (y as u64 * height as u64 / scaled_height as u64) as u32;
        for x in 0..scaled_width {
            let source_x = (x as u64 * width as u64 / scaled_width as u64) as u32;
            let source = ((source_y * width + source_x) * 4) as usize;
            let target = ((y * scaled_width + x) * 4) as usize;
            scaled[target..target + 4].copy_from_slice(&pixels[source..source + 4]);
        }
    }
    Ok((scaled_width, scaled_height, scaled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_rgba_texture_to_maximum_dimension() {
        let pixels = (0..32).map(|value| value as u8).collect::<Vec<_>>();
        let (width, height, scaled) = downscale_rgba(4, 2, &pixels, 2).unwrap();

        assert_eq!((width, height), (2, 1));
        assert_eq!(scaled, [0, 1, 2, 3, 8, 9, 10, 11]);
    }

    #[test]
    fn rejects_zero_sized_rgba_texture() {
        assert!(downscale_rgba(0, 2, &[], 1).is_err());
    }
}
