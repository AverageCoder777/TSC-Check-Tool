//! Public geometry data types for GeoPcDx parsing.

use std::fmt;
use std::fs;
use std::path::Path;

use super::diagnostics::log_parse_diagnostics;
pub(crate) use super::parse::parse_model;
use super::texture::is_auxiliary_node;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GeoVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

#[derive(Clone, Debug)]
pub struct GeoModel {
    pub vertices: Vec<GeoVertex>,
    pub indices: Vec<u32>,
    pub textures: Vec<String>,
    pub submeshes: Vec<GeoSubmesh>,
    pub diagnostics: GeoParseDiagnostics,
}

impl GeoModel {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let model = parse_model(path, &bytes)?;
        log_parse_diagnostics(path, &model.diagnostics);
        Ok(model)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoSubmesh {
    pub first_index: u32,
    pub index_count: u32,
    pub material_index: u32,
    pub texture_index: u32,
    pub render_mode: GeoRenderMode,
    pub vertex_buffer_index: u32,
    pub base_vertex: i32,
    pub vertex_buffer_offset: i32,
    pub alpha_cutoff: f32,
    pub visible: bool,
    pub node_name: String,
    pub material_name: String,
    pub shader_name: String,
    pub lod_level: u8,
    pub triangle_count: u32,
    pub mirrored: bool,
}

impl GeoSubmesh {
    #[must_use]
    pub fn preset_visible(&self, preset: &str) -> bool {
        match preset {
            "lod0" => {
                self.lod_level == 0
                    && !is_auxiliary_node(&self.node_name)
                    && !is_auxiliary_node(&self.material_name)
                    && !is_auxiliary_node(&self.shader_name)
            }
            "all" => true,
            "none" => false,
            _ => self.visible,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GeoRenderMode {
    #[default]
    Opaque,
    Cutout,
    Blended,
}

#[derive(Clone, Debug)]
pub struct GeoParseDiagnostics {
    pub vertex_stride: usize,
    pub vertex_count: usize,
    pub declared_vertex_count: usize,
    pub fvf: u32,
    pub submesh_count: usize,
    pub local_transform_count: usize,
    pub index_element_type: &'static str,
    pub raw_index_count: usize,
    pub primitive_type: PrimitiveTopology,
    pub vertex_layout: String,
    pub index_layout: String,
    pub batch_ranges: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrimitiveTopology {
    #[default]
    TriangleList,
    TriangleStrip,
}

impl fmt::Display for PrimitiveTopology {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TriangleList => "TriangleList",
            Self::TriangleStrip => "TriangleStrip",
        })
    }
}