use crate::geo::{GeoRenderMode, GeoSubmesh, GeoVertex};
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::borrow::Cow;
use std::sync::Arc;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_projection: [f32; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MaterialUniform {
    alpha_cutoff: f32,
    _pad: [f32; 3],
}

pub(crate) struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    opaque_pipeline_cw: wgpu::RenderPipeline,
    opaque_pipeline_ccw: wgpu::RenderPipeline,
    cutout_pipeline_cw: wgpu::RenderPipeline,
    cutout_pipeline_ccw: wgpu::RenderPipeline,
    alpha_pipeline: wgpu::RenderPipeline,
    wire_pipeline: wgpu::RenderPipeline,
    /// Group 0: per-frame camera uniform.
    camera_bind_group_layout: wgpu::BindGroupLayout,
    /// One bind group per texture (group 1), built at load time.
    texture_bind_groups: Vec<wgpu::BindGroup>,
    /// Group 2: per-batch material uniform (alpha_cutoff), one per submesh.
    material_bind_groups: Vec<wgpu::BindGroup>,
    vertex_buffer: wgpu::Buffer,
    triangle_indices: wgpu::Buffer,
    wire_indices: wgpu::Buffer,
    submeshes: Vec<GeoSubmesh>,
    /// Per-batch draw flags, kept separate so toggling never touches GPU buffers.
    visible: Vec<bool>,
    /// Cached render targets, recreated only when the window size changes.
    color_texture: Option<wgpu::Texture>,
    color_view: Option<wgpu::TextureView>,
    depth_texture: Option<wgpu::Texture>,
    depth_view: Option<wgpu::TextureView>,
    last_size: Option<(u32, u32)>,
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

        // Group 0: per-frame camera uniform.
        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("GeoPcDx camera bindings"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        // Group 1: per-texture texture + sampler.
        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("GeoPcDx texture bindings"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        // Pre-build one texture bind group per texture (group 1). These never change after
        // load time, so they are moved out of the per-frame render path.
        let texture_bind_groups = texture_views
            .iter()
            .map(|texture_view| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GeoPcDx texture bindings"),
                    layout: &texture_bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(texture_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                })
            })
            .collect::<Vec<_>>();

        // Group 2: per-batch material uniform (alpha_cutoff). One small uniform
        // buffer + bind group per submesh, created at load time.
        let material_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("GeoPcDx material bindings"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let material_bind_groups = submeshes
            .iter()
            .map(|submesh| {
                let uniform = MaterialUniform {
                    alpha_cutoff: submesh.alpha_cutoff,
                    _pad: [0.0; 3],
                };
                let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("GeoPcDx material uniform"),
                    contents: bytemuck::cast_slice(&[uniform]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GeoPcDx material bindings"),
                    layout: &material_bind_group_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                })
            })
            .collect::<Vec<_>>();

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("GeoPcDx WGSL shader"),
            source: wgpu::ShaderSource::Wgsl(
                r#"
struct Camera {
    view_projection: mat4x4<f32>,
};
struct Material {
    alpha_cutoff: f32,
};
@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var albedo: texture_2d<f32>;
@group(1) @binding(1) var albedo_sampler: sampler;
@group(2) @binding(0) var<uniform> material: Material;

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
    if textureSample(albedo, albedo_sampler, input.uv).a < material.alpha_cutoff {
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
            bind_group_layouts: &[
                &camera_bind_group_layout,
                &texture_bind_group_layout,
                &material_bind_group_layout,
            ],
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
                             depth_write_enabled: bool,
                             cull_mode: wgpu::Face| {
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
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(cull_mode),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth24Plus,
                    depth_write_enabled,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: Default::default(),
                multiview: None,
            })
        };
        let opaque_pipeline_cw = make_pipeline(
            "GeoPcDx opaque pipeline CW",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            wgpu::Face::Front,
        );
        let opaque_pipeline_ccw = make_pipeline(
            "GeoPcDx opaque pipeline Back",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            wgpu::Face::Back,
        );
        let cutout_pipeline_cw = make_pipeline(
            "GeoPcDx cutout pipeline Front",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_cutout",
            Some(wgpu::BlendState::REPLACE),
            true,
            wgpu::Face::Front,
        );
        let cutout_pipeline_ccw = make_pipeline(
            "GeoPcDx cutout pipeline Back",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_cutout",
            Some(wgpu::BlendState::REPLACE),
            true,
            wgpu::Face::Back,
        );
        let alpha_pipeline = make_pipeline(
            "GeoPcDx alpha pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_blended",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            false,
            wgpu::Face::Back,
        );
        let wire_pipeline = make_pipeline(
            "GeoPcDx wireframe pipeline",
            wgpu::PrimitiveTopology::LineList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            wgpu::Face::Back,
        );
        Ok(Self {
            device,
            queue,
            opaque_pipeline_cw,
            opaque_pipeline_ccw,
            cutout_pipeline_cw,
            cutout_pipeline_ccw,
            alpha_pipeline,
            wire_pipeline,
            camera_bind_group_layout,
            vertex_buffer,
            triangle_indices,
            wire_indices,
            submeshes: submeshes.to_vec(),
            visible: submeshes.iter().map(|submesh| submesh.visible).collect(),
            texture_bind_groups,
            color_texture: None,
            color_view: None,
            depth_texture: None,
            depth_view: None,
            last_size: None,
            material_bind_groups,
        })
    }

    pub(crate) fn set_batch_visible(&mut self, batch_idx: usize, visible: bool) {
        if let Some(slot) = self.visible.get_mut(batch_idx) {
            *slot = visible;
        }
    }

    /// `"lod0"`, `"all"`, `"none"`; unknown presets change nothing.
    pub(crate) fn apply_batch_preset(&mut self, preset: &str) {
        self.visible = self
            .submeshes
            .iter()
            .map(|submesh| submesh.preset_visible(preset))
            .collect();
    }

    /// Ensure render targets exist for `size`, recreating them only when the size changes.
    fn ensure_render_targets(&mut self, width: u32, height: u32) {
        if self.last_size == Some((width, height))
            && self.color_view.is_some()
            && self.depth_view.is_some()
        {
            return;
        }
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
        self.color_texture = Some(color);
        self.color_view = Some(color_view);
        self.depth_texture = Some(depth);
        self.depth_view = Some(depth_view);
        self.last_size = Some((width, height));
    }

    pub(crate) fn render(
        &mut self,
        camera: super::viewer3d::Camera,
        radius: f32,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let width = camera.width.clamp(1, 1200);
        let height = camera.height.clamp(1, 1200);
        self.ensure_render_targets(width, height);
        let color_view = self.color_view.as_ref().unwrap();
        let depth_view = self.depth_view.as_ref().unwrap();

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
        // Dynamic near plane: near = max(distance * 0.01, 0.05) avoids z-fighting on
        // large models while keeping close geometry visible.
        let near = (distance * 0.01).max(0.05);
        let projection = Mat4::perspective_rh(
            45.0f32.to_radians(),
            aspect,
            near,
            distance + radius * 4.0,
        );
        let view_projection = projection * view;
        // Train Simulator (DirectX 9) uses Left-Handed coordinates; wgpu uses Right-Handed.
        // Flip X to convert LH -> RH, which mirrors the model horizontally.
        let view_projection = view_projection * Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let camera_uniform = CameraUniform {
            view_projection: view_projection.to_cols_array(),
        };
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("GeoPcDx camera uniform"),
                contents: bytemuck::cast_slice(&[camera_uniform]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let camera_bind_group = self
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("GeoPcDx frame camera bindings"),
                layout: &self.camera_bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });

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
                    view: color_view,
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
                    view: depth_view,
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
            pass.set_bind_group(0, &camera_bind_group, &[]);
            let (opaque, blended): (Vec<usize>, Vec<usize>) =
                (0..self.submeshes.len()).partition(|&index| {
                    self.submeshes[index].render_mode != GeoRenderMode::Blended
                });
            for index in opaque.into_iter().chain(blended) {
                if !self.visible.get(index).copied().unwrap_or(true) {
                    continue;
                }
                let submesh = &self.submeshes[index];
                let pipeline = if camera.wireframe {
                    &self.wire_pipeline
                } else {
                    let cull_face = if submesh.mirrored {
                        wgpu::Face::Front
                    } else {
                        wgpu::Face::Back
                    };
                    match submesh.render_mode {
                        GeoRenderMode::Opaque => {
                            if cull_face == wgpu::Face::Front {
                                &self.opaque_pipeline_cw
                            } else {
                                &self.opaque_pipeline_ccw
                            }
                        }
                        GeoRenderMode::Cutout => {
                            if cull_face == wgpu::Face::Front {
                                &self.cutout_pipeline_cw
                            } else {
                                &self.cutout_pipeline_ccw
                            }
                        }
                        GeoRenderMode::Blended => &self.alpha_pipeline,
                    }
                };
                pass.set_pipeline(pipeline);
                let texture_index =
                    (submesh.texture_index as usize).min(self.texture_bind_groups.len().saturating_sub(1));
                pass.set_bind_group(1, &self.texture_bind_groups[texture_index], &[]);
                pass.set_bind_group(2, &self.material_bind_groups[index], &[]);
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
                texture: self.color_texture.as_ref().unwrap(),
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
        let (sender, receiver) = std::sync::mpsc::channel();
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
