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
    max_render_dimension: u32,
    opaque_pipeline_cw: wgpu::RenderPipeline,
    opaque_pipeline_ccw: wgpu::RenderPipeline,
    cutout_pipeline_cw: wgpu::RenderPipeline,
    cutout_pipeline_ccw: wgpu::RenderPipeline,
    alpha_pipeline_cw: wgpu::RenderPipeline,
    alpha_pipeline_ccw: wgpu::RenderPipeline,
    wire_pipeline: wgpu::RenderPipeline,
    /// Group 0: persistent camera uniform buffer + bind group, updated via `write_buffer`.
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    /// One bind group per texture (group 1), built at load time.
    texture_bind_groups: Vec<wgpu::BindGroup>,
    /// Group 2: per-batch material uniform (alpha_cutoff), one per submesh.
    material_bind_groups: Vec<wgpu::BindGroup>,
    vertex_buffer: wgpu::Buffer,
    triangle_indices: wgpu::Buffer,
    wire_indices: wgpu::Buffer,
    submeshes: Vec<GeoSubmesh>,
    /// Safe clamped (start, count) index ranges per submesh for triangle and wireframe draws.
    triangle_ranges: Vec<(u32, u32)>,
    wire_ranges: Vec<(u32, u32)>,
    /// Centroid of each submesh in RH space, used to sort blended submeshes back-to-front.
    submesh_centers: Vec<Vec3>,
    /// Per-batch draw flags, kept separate so toggling never touches GPU buffers.
    visible: Vec<bool>,
    /// Cached render targets and CPU readback buffer, recreated only when the size changes.
    color_texture: Option<wgpu::Texture>,
    color_view: Option<wgpu::TextureView>,
    depth_texture: Option<wgpu::Texture>,
    depth_view: Option<wgpu::TextureView>,
    readback_buffer: Option<wgpu::Buffer>,
    padded_bytes_per_row: u32,
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
        let max_render_dimension = max_texture_dimension.min(4096);

        // Train Simulator (DirectX 9) uses Left-Handed coordinates; wgpu uses Right-Handed.
        // Flip X on both positions and normals at load time so world_position, normals,
        // lighting, and screen-space derivatives stay consistent in RH space.
        let packed_vertices = vertices
            .iter()
            .map(|vertex| {
                let pos = Vec3::from_array(vertex.position) - center;
                GpuVertex {
                    position: [-pos.x, pos.y, pos.z],
                    normal: [-vertex.normal[0], vertex.normal[1], vertex.normal[2]],
                    uv: vertex.uv,
                }
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

        // Build wireframe indices and bounding centroids per submesh so non-multiple-of-3
        // offsets or truncated submesh ranges can never read past the index buffer.
        let mut wire_lines = Vec::with_capacity(indices.len() * 2);
        let mut triangle_ranges = Vec::with_capacity(submeshes.len());
        let mut wire_ranges = Vec::with_capacity(submeshes.len());
        let mut submesh_centers = Vec::with_capacity(submeshes.len());
        let total_indices = indices.len();
        for submesh in submeshes {
            let start = (submesh.first_index as usize).min(total_indices);
            let end = start
                .saturating_add(submesh.index_count as usize)
                .min(total_indices);
            let slice = &indices[start..end];
            let tri_count = (slice.len() / 3) * 3;
            triangle_ranges.push((start as u32, tri_count as u32));

            let wire_start = wire_lines.len() as u32;
            let mut min_pt = Vec3::splat(f32::INFINITY);
            let mut max_pt = Vec3::splat(f32::NEG_INFINITY);
            for triangle in slice.chunks_exact(3) {
                wire_lines.extend_from_slice(&[
                    triangle[0],
                    triangle[1],
                    triangle[1],
                    triangle[2],
                    triangle[2],
                    triangle[0],
                ]);
                for &idx in triangle {
                    if let Some(v) = packed_vertices.get(idx as usize) {
                        let p = Vec3::from_array(v.position);
                        min_pt = min_pt.min(p);
                        max_pt = max_pt.max(p);
                    }
                }
            }
            let wire_count = (wire_lines.len() as u32) - wire_start;
            wire_ranges.push((wire_start, wire_count));
            let centroid = if min_pt.x.is_finite() {
                (min_pt + max_pt) * 0.5
            } else {
                Vec3::ZERO
            };
            submesh_centers.push(centroid);
        }
        let wire_indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GeoPcDx wireframe indices"),
            contents: bytemuck::cast_slice(&wire_lines),
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
            let mip_levels = mip_level_count(width, height);
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("GeoPcDx albedo"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: mip_levels,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let mut mip_w = width;
            let mut mip_h = height;
            let mut mip_data = pixels;
            for level in 0..mip_levels {
                queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &texture,
                        mip_level: level,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    mip_data.as_ref(),
                    wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(mip_w * 4),
                        rows_per_image: Some(mip_h),
                    },
                    wgpu::Extent3d {
                        width: mip_w,
                        height: mip_h,
                        depth_or_array_layers: 1,
                    },
                );
                if level + 1 < mip_levels {
                    let (next_w, next_h, next_pixels) =
                        downsample_mip_rgba(mip_w, mip_h, mip_data.as_ref());
                    mip_w = next_w;
                    mip_h = next_h;
                    mip_data = Cow::Owned(next_pixels);
                }
            }
            texture_views.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        }
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Group 0: persistent camera uniform buffer + bind group.
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

        // Group 2: per-batch material uniform (alpha_cutoff).
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
fn shade_sample(input: VertexOutput, color: vec4<f32>, face_normal: vec3<f32>) -> vec4<f32> {
    var normal = input.normal;
    if dot(normal, normal) < 1e-10 {
        normal = face_normal;
        if dot(normal, normal) < 1e-10 {
            normal = vec3<f32>(0.0, 1.0, 0.0);
        }
    }
    normal = normalize(normal);
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let hemisphere = mix(0.28, 0.62, clamp(dot(normal, up) * 0.5 + 0.5, 0.0, 1.0));
    let key = abs(dot(normal, normalize(vec3<f32>(-0.45, 0.72, 0.52)))) * 0.36;
    let fill = abs(dot(normal, normalize(vec3<f32>(0.55, 0.2, -0.65)))) * 0.14;
    let rim_direction = normalize(vec3<f32>(0.35, 0.45, -0.82));
    let rim = pow(1.0 - max(dot(normal, rim_direction), 0.0), 2.0) * 0.10;
    let light = clamp(max(0.25, hemisphere + key + fill + rim), 0.25, 1.25);
    return vec4<f32>(color.rgb * light, color.a);
}
@fragment
fn fs_opaque(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    let color = shade_sample(input, sample, face_normal);
    return vec4<f32>(color.rgb, 1.0);
}
@fragment
fn fs_cutout(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    if sample.a < material.alpha_cutoff {
        discard;
    }
    let color = shade_sample(input, sample, face_normal);
    return vec4<f32>(color.rgb, 1.0);
}
@fragment
fn fs_blended(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    if sample.a < 0.02 {
        discard;
    }
    return shade_sample(input, sample, face_normal);
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
                             cull_mode: Option<wgpu::Face>| {
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
                    cull_mode,
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
            "GeoPcDx opaque pipeline Front",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            Some(wgpu::Face::Front),
        );
        let opaque_pipeline_ccw = make_pipeline(
            "GeoPcDx opaque pipeline Back",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            Some(wgpu::Face::Back),
        );
        let cutout_pipeline_cw = make_pipeline(
            "GeoPcDx cutout pipeline Front",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_cutout",
            Some(wgpu::BlendState::REPLACE),
            true,
            Some(wgpu::Face::Front),
        );
        let cutout_pipeline_ccw = make_pipeline(
            "GeoPcDx cutout pipeline Back",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_cutout",
            Some(wgpu::BlendState::REPLACE),
            true,
            Some(wgpu::Face::Back),
        );
        let alpha_pipeline_cw = make_pipeline(
            "GeoPcDx alpha pipeline Front",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_blended",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            false,
            Some(wgpu::Face::Front),
        );
        let alpha_pipeline_ccw = make_pipeline(
            "GeoPcDx alpha pipeline Back",
            wgpu::PrimitiveTopology::TriangleList,
            "fs_blended",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            false,
            Some(wgpu::Face::Back),
        );
        let wire_pipeline = make_pipeline(
            "GeoPcDx wireframe pipeline",
            wgpu::PrimitiveTopology::LineList,
            "fs_opaque",
            Some(wgpu::BlendState::REPLACE),
            true,
            None,
        );
        Ok(Self {
            device,
            queue,
            max_render_dimension,
            opaque_pipeline_cw,
            opaque_pipeline_ccw,
            cutout_pipeline_cw,
            cutout_pipeline_ccw,
            alpha_pipeline_cw,
            alpha_pipeline_ccw,
            wire_pipeline,
            camera_buffer,
            camera_bind_group,
            vertex_buffer,
            triangle_indices,
            wire_indices,
            submeshes: submeshes.to_vec(),
            triangle_ranges,
            wire_ranges,
            submesh_centers,
            visible: submeshes.iter().map(|submesh| submesh.visible).collect(),
            texture_bind_groups,
            color_texture: None,
            color_view: None,
            depth_texture: None,
            depth_view: None,
            readback_buffer: None,
            padded_bytes_per_row: 0,
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

    /// Ensure render targets and readback buffer exist for `size`, recreating only on resize.
    fn ensure_render_targets(&mut self, width: u32, height: u32) {
        if self.last_size == Some((width, height))
            && self.color_view.is_some()
            && self.depth_view.is_some()
            && self.readback_buffer.is_some()
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
        let bytes_per_row = width * 4;
        let padded_bytes_per_row = bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GeoPcDx CPU readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.color_texture = Some(color);
        self.color_view = Some(color_view);
        self.depth_texture = Some(depth);
        self.depth_view = Some(depth_view);
        self.readback_buffer = Some(readback_buffer);
        self.padded_bytes_per_row = padded_bytes_per_row;
        self.last_size = Some((width, height));
    }

    pub(crate) fn render(
        &mut self,
        camera: super::viewer3d::Camera,
        radius: f32,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let width = camera.width.clamp(1, self.max_render_dimension);
        let height = camera.height.clamp(1, self.max_render_dimension);
        self.ensure_render_targets(width, height);
        let color_view = self.color_view.as_ref().unwrap();
        let depth_view = self.depth_view.as_ref().unwrap();
        let readback_buffer = self.readback_buffer.as_ref().unwrap();

        let aspect = width as f32 / height as f32;
        let safe_radius = radius.max(0.01);
        let distance = camera.distance.max(safe_radius * 1.05).max(0.05);
        // Clamp pitch strictly below +-90 degrees to avoid gimbal lock / NaN in look_at_rh.
        let pitch = camera.pitch.clamp(-1.56, 1.56);
        let direction = Vec3::new(
            camera.yaw.sin() * pitch.cos(),
            pitch.sin(),
            camera.yaw.cos() * pitch.cos(),
        );
        let right = direction.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(direction).normalize_or_zero();
        let target = right * camera.pan_x + up * camera.pan_y;
        let eye = target + direction * distance;
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        let near = (distance * 0.01).max(0.05);
        let far = (distance + safe_radius * 4.0).max(near + 1.0);
        let projection = Mat4::perspective_rh(45.0f32.to_radians(), aspect, near, far);
        let view_projection = projection * view;
        let camera_uniform = CameraUniform {
            view_projection: view_projection.to_cols_array(),
        };
        self.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[camera_uniform]),
        );

        let bytes_per_row = width * 4;
        let padded_bytes_per_row = self.padded_bytes_per_row;
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
            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            let (opaque, mut blended): (Vec<usize>, Vec<usize>) = (0..self.submeshes.len())
                .partition(|&index| self.submeshes[index].render_mode != GeoRenderMode::Blended);
            // Sort blended submeshes back-to-front relative to the camera.
            blended.sort_by(|&a, &b| {
                let da = (self.submesh_centers[a] - eye).length_squared();
                let db = (self.submesh_centers[b] - eye).length_squared();
                db.total_cmp(&da)
            });
            for index in opaque.into_iter().chain(blended) {
                if !self.visible.get(index).copied().unwrap_or(true) {
                    continue;
                }
                let (start, count) = if camera.wireframe {
                    self.wire_ranges[index]
                } else {
                    self.triangle_ranges[index]
                };
                if count == 0 {
                    continue;
                }
                let submesh = &self.submeshes[index];
                let pipeline = if camera.wireframe {
                    &self.wire_pipeline
                } else {
                    let front_cull = submesh.mirrored;
                    match submesh.render_mode {
                        GeoRenderMode::Opaque => {
                            if front_cull {
                                &self.opaque_pipeline_cw
                            } else {
                                &self.opaque_pipeline_ccw
                            }
                        }
                        GeoRenderMode::Cutout => {
                            if front_cull {
                                &self.cutout_pipeline_cw
                            } else {
                                &self.cutout_pipeline_ccw
                            }
                        }
                        GeoRenderMode::Blended => {
                            if front_cull {
                                &self.alpha_pipeline_cw
                            } else {
                                &self.alpha_pipeline_ccw
                            }
                        }
                    }
                };
                pass.set_pipeline(pipeline);
                let texture_index = (submesh.texture_index as usize)
                    .min(self.texture_bind_groups.len().saturating_sub(1));
                pass.set_bind_group(1, &self.texture_bind_groups[texture_index], &[]);
                pass.set_bind_group(2, &self.material_bind_groups[index], &[]);
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
                buffer: readback_buffer,
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
        let slice = readback_buffer.slice(..);
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
        readback_buffer.unmap();
        Ok((width, height, pixels))
    }
}

fn mip_level_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).leading_zeros()
}

fn downsample_mip_rgba(src_w: u32, src_h: u32, src: &[u8]) -> (u32, u32, Vec<u8>) {
    let dst_w = (src_w / 2).max(1);
    let dst_h = (src_h / 2).max(1);
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let sy0 = (y * 2).min(src_h - 1);
        let sy1 = (sy0 + 1).min(src_h - 1);
        for x in 0..dst_w {
            let sx0 = (x * 2).min(src_w - 1);
            let sx1 = (sx0 + 1).min(src_w - 1);
            let i00 = ((sy0 * src_w + sx0) * 4) as usize;
            let i10 = ((sy0 * src_w + sx1) * 4) as usize;
            let i01 = ((sy1 * src_w + sx0) * 4) as usize;
            let i11 = ((sy1 * src_w + sx1) * 4) as usize;
            let out = ((y * dst_w + x) * 4) as usize;
            for c in 0..4 {
                let sum = src[i00 + c] as u32
                    + src[i10 + c] as u32
                    + src[i01 + c] as u32
                    + src[i11 + c] as u32;
                dst[out + c] = ((sum + 2) / 4) as u8;
            }
        }
    }
    (dst_w, dst_h, dst)
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