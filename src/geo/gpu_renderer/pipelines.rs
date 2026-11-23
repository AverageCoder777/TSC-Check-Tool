use std::mem::size_of;
use std::slice;

use bytemuck::{Pod, Zeroable};

use crate::geo::GeoRenderMode;

pub(super) const MSAA_SAMPLE_COUNT: u32 = 4;

const ALPHA_MODE_OPAQUE: u32 = 0;
const ALPHA_MODE_CUTOUT: u32 = 1;
const ALPHA_MODE_BLENDED: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct GpuVertex {
    pub(super) position: [f32; 3],
    pub(super) normal: [f32; 3],
    pub(super) uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct CameraUniform {
    pub(super) view_projection: [f32; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct MaterialImmediate {
    pub(super) alpha_cutoff: f32,
    pub(super) highlight: f32,
    pub(super) debug_mode: u32,
    pub(super) mirrored: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PipelineKey {
    OpaqueCw,
    OpaqueCcw,
    CutoutCw,
    CutoutCcw,
    AlphaCw,
    AlphaCcw,
    Wireframe,
    Backface,
}

impl PipelineKey {
    pub(super) fn select(
        mode: GeoRenderMode,
        mirrored: bool,
        wireframe: bool,
        debug_mode: u32,
    ) -> Self {
        if wireframe {
            return Self::Wireframe;
        }
        if debug_mode == 4 {
            return Self::Backface;
        }
        match (mode, mirrored) {
            (GeoRenderMode::Opaque, true) => Self::OpaqueCw,
            (GeoRenderMode::Opaque, false) => Self::OpaqueCcw,
            (GeoRenderMode::Cutout, true) => Self::CutoutCw,
            (GeoRenderMode::Cutout, false) => Self::CutoutCcw,
            (GeoRenderMode::Blended, true) => Self::AlphaCw,
            (GeoRenderMode::Blended, false) => Self::AlphaCcw,
        }
    }
}

pub(super) struct Pipelines {
    opaque_cw: wgpu::RenderPipeline,
    opaque_ccw: wgpu::RenderPipeline,
    cutout_cw: wgpu::RenderPipeline,
    cutout_ccw: wgpu::RenderPipeline,
    alpha_cw: wgpu::RenderPipeline,
    alpha_ccw: wgpu::RenderPipeline,
    wireframe: wgpu::RenderPipeline,
    backface: wgpu::RenderPipeline,
}

impl Pipelines {
    pub(super) fn new(
        device: &wgpu::Device,
        camera_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        cache: Option<&wgpu::PipelineCache>,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("GeoPcDx WGSL shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("GeoPcDx pipeline layout"),
            bind_group_layouts: &[Some(camera_layout), Some(texture_layout)],
            immediate_size: size_of::<MaterialImmediate>() as u32,
        });
        let vertex_layout = Some(wgpu::VertexBufferLayout {
            array_stride: size_of::<GpuVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        });

        let make = |label: &'static str,
                    topology: wgpu::PrimitiveTopology,
                    alpha_mode: u32,
                    blend: Option<wgpu::BlendState>,
                    depth_write_enabled: bool,
                    cull_mode: Option<wgpu::Face>| {
            let constants = [("ALPHA_MODE", f64::from(alpha_mode))];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: slice::from_ref(&vertex_layout),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &constants,
                        ..Default::default()
                    },
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
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(depth_write_enabled),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: wgpu::StencilState::default(),
                    bias: if alpha_mode == ALPHA_MODE_BLENDED {
                        wgpu::DepthBiasState {
                            constant: -2,
                            slope_scale: -1.0,
                            clamp: 0.0,
                        }
                    } else {
                        wgpu::DepthBiasState::default()
                    },
                }),
                multisample: wgpu::MultisampleState {
                    count: MSAA_SAMPLE_COUNT,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview_mask: None,
                cache,
            })
        };

        Self {
            opaque_cw: make(
                "GeoPcDx opaque pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_OPAQUE,
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Front),
            ),
            opaque_ccw: make(
                "GeoPcDx opaque pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_OPAQUE,
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Back),
            ),
            cutout_cw: make(
                "GeoPcDx cutout pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_CUTOUT,
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Front),
            ),
            cutout_ccw: make(
                "GeoPcDx cutout pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_CUTOUT,
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Back),
            ),
            alpha_cw: make(
                "GeoPcDx alpha pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_BLENDED,
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
                Some(wgpu::Face::Front),
            ),
            alpha_ccw: make(
                "GeoPcDx alpha pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_BLENDED,
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
                Some(wgpu::Face::Back),
            ),
            wireframe: make(
                "GeoPcDx wireframe pipeline",
                wgpu::PrimitiveTopology::LineList,
                ALPHA_MODE_OPAQUE,
                Some(wgpu::BlendState::REPLACE),
                true,
                None,
            ),
            backface: make(
                "GeoPcDx backface debug pipeline",
                wgpu::PrimitiveTopology::TriangleList,
                ALPHA_MODE_OPAQUE,
                Some(wgpu::BlendState::REPLACE),
                true,
                None,
            ),
        }
    }

    pub(super) fn get(&self, key: PipelineKey) -> &wgpu::RenderPipeline {
        match key {
            PipelineKey::OpaqueCw => &self.opaque_cw,
            PipelineKey::OpaqueCcw => &self.opaque_ccw,
            PipelineKey::CutoutCw => &self.cutout_cw,
            PipelineKey::CutoutCcw => &self.cutout_ccw,
            PipelineKey::AlphaCw => &self.alpha_cw,
            PipelineKey::AlphaCcw => &self.alpha_ccw,
            PipelineKey::Wireframe => &self.wireframe,
            PipelineKey::Backface => &self.backface,
        }
    }
}

pub(super) fn create_uniform_layout(
    device: &wgpu::Device,
    label: &'static str,
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}
