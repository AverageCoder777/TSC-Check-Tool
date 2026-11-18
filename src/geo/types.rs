//! Public geometry data types for GeoPcDx parsing.

#![allow(dead_code)]

#[cfg(test)]
use glam::Mat4;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default)]
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
    /// Alpha cutout threshold (`AlphaRef / 255.0`), only honored for `Cutout` batches.
    pub alpha_cutoff: f32,
    /// Draw flag only: buffers stay resident, so toggling never re-uploads geometry.
    pub visible: bool,
    pub node_name: String,
    pub material_name: String,
    pub shader_name: String,
    pub lod_level: u8,
    pub triangle_count: u32,
    /// True when the batch's world transform has a negative determinant (mirrored),
    /// meaning winding order is flipped and front-face culling must be inverted.
    pub mirrored: bool,
}

impl GeoSubmesh {
    /// Visibility a batch should take for a named preset. Unknown presets are a no-op.
    pub fn preset_visible(&self, preset: &str) -> bool {
        match preset {
            "lod0" => {
                self.lod_level == 0
                    && !crate::geo::texture::is_auxiliary_node(&self.node_name)
                    && !crate::geo::texture::is_auxiliary_node(&self.material_name)
                    && !crate::geo::texture::is_auxiliary_node(&self.shader_name)
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

impl std::fmt::Display for PrimitiveTopology {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TriangleList => "TriangleList",
            Self::TriangleStrip => "TriangleStrip",
        })
    }
}

// Re-exported by parse.rs when `GeoModel::parse` / `GeoModel::load` are implemented there.
pub(crate) use crate::geo::parse::parse_model;

impl GeoModel {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let model = parse_model(path, &bytes)?;
        crate::geo::diagnostics::log_parse_diagnostics(path, &model.diagnostics);
        Ok(model)
    }

    #[allow(dead_code)]
    pub fn parse(path: &Path, data: &[u8]) -> Result<Self, String> {
        parse_model(path, data)
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct GeoAssemblyPart {
    pub path: PathBuf,
    pub model: std::sync::Arc<GeoModel>,
    pub transform: Mat4,
    pub blueprint_attached: bool,
}

#[cfg(test)]
pub struct GeoAssembly {
    pub parts: Vec<GeoAssemblyPart>,
    pub has_blueprint_attachments: bool,
}

#[cfg(test)]
impl GeoAssembly {
    pub fn load(path: &Path) -> Result<Self, String> {
        crate::geo::assembly::load_assembly(path)
    }

    pub fn compose(
        &self,
        side_by_side: bool,
    ) -> Result<(GeoModel, Vec<(PathBuf, String)>), String> {
        crate::geo::assembly::compose_assembly(self, side_by_side)
    }
}
