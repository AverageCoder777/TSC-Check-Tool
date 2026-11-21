use std::mem::size_of;
use std::slice;

use bytemuck::{Pod, Zeroable};

use crate::geo::GeoRenderMode;

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
pub(super) struct MaterialUniform {
    pub(super) alpha_cutoff: f32,
    pub(super) _pad: [f32; 3],
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
}

impl PipelineKey {
    pub(super) fn select(mode: GeoRenderMode, mirrored: bool, wireframe: bool) -> Self {
        if wireframe {
            return Self::Wireframe;
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
}

impl Pipelines {
    pub(super) fn new(
        device: &wgpu::Device,
        camera_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        material_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("GeoPcDx WGSL shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("GeoPcDx pipeline layout"),
            bind_group_layouts: &[camera_layout, texture_layout, material_layout],
            push_constant_ranges: &[],
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<GpuVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        };

        let make = |label: &'static str,
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
                    buffers: slice::from_ref(&vertex_layout),
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

        Self {
            opaque_cw: make(
                "GeoPcDx opaque pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_opaque",
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Front),
            ),
            opaque_ccw: make(
                "GeoPcDx opaque pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_opaque",
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Back),
            ),
            cutout_cw: make(
                "GeoPcDx cutout pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_cutout",
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Front),
            ),
            cutout_ccw: make(
                "GeoPcDx cutout pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_cutout",
                Some(wgpu::BlendState::REPLACE),
                true,
                Some(wgpu::Face::Back),
            ),
            alpha_cw: make(
                "GeoPcDx alpha pipeline Front",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_blended",
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
                Some(wgpu::Face::Front),
            ),
            alpha_ccw: make(
                "GeoPcDx alpha pipeline Back",
                wgpu::PrimitiveTopology::TriangleList,
                "fs_blended",
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
                Some(wgpu::Face::Back),
            ),
            wireframe: make(
                "GeoPcDx wireframe pipeline",
                wgpu::PrimitiveTopology::LineList,
                "fs_opaque",
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