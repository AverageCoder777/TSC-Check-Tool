//! Core GeoPcDx model parsing: SERZ traversal, vertex/index decoding, batch assembly.

mod buffers;
mod materials;
mod scene;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use glam::{Mat4, Vec3};

use crate::auditor::extract_geo_texture_references;
use crate::common::serz::SerzReader;
use crate::geo::diagnostics::{
    log_batch_diagnostics, log_material_diagnostics, log_vertex_buffer_diagnostics, warn_parse_skip,
};
use crate::geo::helpers::{
    build_id_map, collect_float_values, collect_i32_values, collect_transform_names,
    compute_world_transforms, find_named_values, matrix_from_values, parse_serz_document,
};
use crate::geo::texture::is_auxiliary_node;
use crate::geo::types::{
    GeoModel, GeoParseDiagnostics, GeoRenderMode, GeoSubmesh, GeoVertex, PrimitiveTopology,
};
use crate::geo::vertex::{
    PositionFormat, fill_missing_normals, read_position, read_vec2, read_vec3,
};

pub(crate) use buffers::{
    collect_index_buffers, collect_vertex_buffers, 
};
pub(crate) use materials::find_material_bindings;
pub(crate) use scene::{
    NodeContext, TraversalState, deduplicate_submeshes, 
    find_primitive_topology, find_submeshes,
};

#[derive(Clone, Debug)]
pub(crate) struct VertexBuffer {
    pub source_index: usize,
    pub bytes: Vec<u8>,
    pub stride: usize,
    pub declared_vertex_count: usize,
    pub fvf: u32,
    pub position_offset: usize,
    pub position_format: PositionFormat,
    pub normal_offset: Option<usize>,
    pub uv_offset: Option<usize>,
    pub layout: String,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexBuffer {
    pub source_index: usize,
    pub indices: Vec<u32>,
    pub element_type: &'static str,
}

#[derive(Clone, Debug)]
pub(crate) struct MaterialBinding {
    pub material_index: u32,
    pub texture_index: u32,
    pub render_mode: GeoRenderMode,
    pub excluded: bool,
    pub material_name: String,
    pub shader_name: String,
    pub alpha_ref: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct RawSubmesh {
    pub first_index: u32,
    pub index_count: u32,
    pub material_reference: Option<u32>,
    pub material_index: Option<u32>,
    pub texture_index: Option<u32>,
    pub vertex_buffer_index: u32,
    pub index_buffer_index: u32,
    pub base_vertex: i32,
    pub max_local_vertex_ref: Option<u64>,
    pub topology: PrimitiveTopology,
    pub transform: Mat4,
    pub excluded: bool,
    pub node_name: String,
    pub material_name: String,
    pub shader_name: String,
    pub render_mode: GeoRenderMode,
    pub lod_level: u8,
}

/// Batch metadata forwarded into `GeoSubmesh` when a draw range is appended.
#[derive(Clone, Debug)]
pub(crate) struct BatchInfo {
    pub node_name: String,
    pub material_name: String,
    pub shader_name: String,
    pub lod_level: u8,
}

impl Default for BatchInfo {
    fn default() -> Self {
        Self {
            node_name: "Node #0".into(),
            material_name: "default".into(),
            shader_name: String::new(),
            lod_level: 0,
        }
    }
}

pub(crate) struct SubmeshParams<'a> {
    pub(crate) material_index: u32,
    pub(crate) texture_index: u32,
    pub(crate) render_mode: GeoRenderMode,
    pub(crate) alpha_cutoff: f32,
    pub(crate) info: &'a BatchInfo,
    pub(crate) vertex_buffer_index: u32,
    pub(crate) base_vertex: i32,
    pub(crate) vertex_buffer_offset: i32,
    pub(crate) vertex_count: usize,
}

pub(super) fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

pub(crate) fn parse_model(path: &Path, data: &[u8]) -> Result<GeoModel, String> {
    let root = parse_serz_document(data)?;
    let id_map = build_id_map(&root);
    let local_transforms: Vec<Mat4> = find_named_values(&root, "SourceLToPTransform")
        .into_iter()
        .flatten()
        .flat_map(collect_float_values)
        .collect::<Vec<_>>()
        .as_chunks::<16>()
        .0
        .iter()
        .map(|values| matrix_from_values(values).unwrap_or(Mat4::IDENTITY))
        .collect();
    let transform_dependencies: Vec<i32> = find_named_values(&root, "TransformDependency")
        .into_iter()
        .flatten()
        .flat_map(collect_i32_values)
        .collect();
    let world_transforms = compute_world_transforms(&local_transforms, &transform_dependencies);

    let xml: Cow<'_, str> = if data.starts_with(b"SERZ") {
        Cow::Owned(SerzReader::to_xml(&root, 0, true))
    } else {
        String::from_utf8_lossy(data)
    };
    let textures = extract_geo_texture_references(&xml);
    let material_bindings = find_material_bindings(&root, &textures, &id_map);
    log_material_diagnostics(path, &material_bindings);
    let transform_names = collect_transform_names(&root);

    // Collect vertex buffers first to build the SERZ ID -> source_index map.
    let mut vertex_data = Vec::new();
    let (vb_id_to_index, prim_id_to_vbuf) =
        collect_vertex_buffers(&root, &id_map, &mut vertex_data);
    let context = NodeContext {
        material_bindings: &material_bindings,
        ids: &id_map,
        world_transforms: &world_transforms,
        transform_names: &transform_names,
        vb_id_to_index: &vb_id_to_index,
        prim_id_to_vbuf: &prim_id_to_vbuf,
    };
    let mut raw_batches = Vec::new();
    find_submeshes(
        &root,
        &context,
        TraversalState::default(),
        &mut HashSet::new(),
        &mut raw_batches,
    );
    deduplicate_submeshes(&mut raw_batches);
    scene::resolve_implicit_vertex_buffers(&mut raw_batches, &vertex_data);
    log_batch_diagnostics(path, &raw_batches);

    // Collect index buffers (requires batches for u16/u32 width inference).
    let mut index_data = Vec::new();
    collect_index_buffers(&root, &id_map, &raw_batches, &vertex_data, &mut index_data);
    log_vertex_buffer_diagnostics(path, &vertex_data, &index_data);

    let first_vertex_buffer = vertex_data
        .first()
        .ok_or("GeoPcDx has no supported vertex buffer")?;
    let first_index_buffer = index_data
        .first()
        .ok_or("GeoPcDx has no supported index buffer")?;

    // Batches address buffers by their declared array slot, not by decode order.
    let vertex_slots: HashMap<u32, usize> = vertex_data
        .iter()
        .enumerate()
        .map(|(position, buffer)| (buffer.source_index as u32, position))
        .collect();
    let index_slots: HashMap<u32, usize> = index_data
        .iter()
        .enumerate()
        .map(|(position, buffer)| (buffer.source_index as u32, position))
        .collect();

    let total_estimated_vertices: usize = vertex_data
        .iter()
        .map(|b| b.bytes.len().checked_div(b.stride).unwrap_or(0))
        .sum();
    let mut vertices = Vec::with_capacity(total_estimated_vertices);
    let mut vertex_buffer_offsets: Vec<Option<i32>> = vec![None; vertex_data.len()];
    let mut vertex_layouts = Vec::with_capacity(vertex_data.len());

    for (buffer_position, buffer) in vertex_data.iter().enumerate() {
        let buffer_offset = vertices.len();
        match decode_vertex_buffer(buffer) {
            Ok(decoded) => {
                vertices.extend(decoded);
                vertex_buffer_offsets[buffer_position] = i32::try_from(buffer_offset).ok();
                vertex_layouts.push(buffer.layout.as_str());
            }
            Err(reason) => warn_parse_skip(format!(
                "skipping vertex buffer #{} ({}): {}",
                buffer.source_index, buffer.layout, reason,
            )),
        }
    }

    let has_batch_ranges = !raw_batches.is_empty();
    let render_batches: Vec<&RawSubmesh> = raw_batches
        .iter()
        .filter(|batch| !batch.excluded)
        .collect();
    let primitive_type = render_batches
        .first()
        .map(|batch| batch.topology)
        .unwrap_or_else(|| find_primitive_topology(&root, &id_map));
    let batch_ranges: Vec<String> = render_batches
        .iter()
        .map(|batch| {
            format!(
                "vertex_buffer={} base_vertex={} index_buffer={} start={} count={} topology={} material={:?} texture={:?}",
                batch.vertex_buffer_index,
                batch.base_vertex,
                batch.index_buffer_index,
                batch.first_index,
                batch.index_count,
                batch.topology,
                batch.material_reference.or(batch.material_index),
                batch.texture_index,
            )
        })
        .collect();

    let mut indices = Vec::new();
    let mut submeshes = Vec::with_capacity(render_batches.len());
    if has_batch_ranges {
        let source_vertex_count = vertices.len();
        let max_texture_index = textures.len().saturating_sub(1) as u32;
        for batch in render_batches {
            let result = (|| -> Result<(), String> {
                let end = batch
                    .first_index
                    .checked_add(batch.index_count)
                    .ok_or("primitive batch index range overflows")?;
                let index_buffer = index_slots
                    .get(&batch.index_buffer_index)
                    .and_then(|&pos| index_data.get(pos))
                    .ok_or("primitive batch references a missing index buffer")?;
                let range = index_buffer
                    .indices
                    .get(batch.first_index as usize..end as usize)
                    .ok_or("primitive batch index range is outside its index buffer")?;
                let vertex_buffer_offset = vertex_buffer_offsets
                    .get(
                        vertex_slots
                            .get(&batch.vertex_buffer_index)
                            .copied()
                            .ok_or("primitive batch references a missing vertex buffer")?,
                    )
                    .and_then(|&offset| offset)
                    .ok_or("primitive batch references a vertex buffer that failed to decode")?;
                let binding = batch
                    .material_reference
                    .and_then(|reference| material_bindings.get(&reference));
                let material_index = binding
                    .map(|b| b.material_index)
                    .or(batch.material_index)
                    .unwrap_or(0);
                let texture_index = binding
                    .map(|b| b.texture_index)
                    .or(batch.texture_index)
                    .unwrap_or(material_index)
                    .min(max_texture_index);
                let render_mode = binding.map(|b| b.render_mode).unwrap_or_default();
                let alpha_cutoff = binding.map(|b| b.alpha_ref).unwrap_or(0.5);
                let triangles =
                    convert_to_triangle_list(range, batch.topology, index_buffer.element_type);
                let info = BatchInfo {
                    node_name: batch.node_name.clone(),
                    material_name: batch.material_name.clone(),
                    shader_name: batch.shader_name.clone(),
                    lod_level: batch.lod_level,
                };
                let params = SubmeshParams {
                    material_index,
                    texture_index,
                    render_mode,
                    alpha_cutoff,
                    info: &info,
                    vertex_buffer_index: batch.vertex_buffer_index,
                    base_vertex: batch.base_vertex,
                    vertex_buffer_offset,
                    vertex_count: source_vertex_count,
                };
                if batch.transform == Mat4::IDENTITY {
                    append_submesh(&mut indices, &mut submeshes, triangles, params)?;
                } else {
                    append_transformed_submesh_in_place(
                        &mut vertices,
                        &mut indices,
                        &mut submeshes,
                        triangles,
                        source_vertex_count,
                        batch.transform,
                        params,
                    )?;
                }
                Ok(())
            })();
            if let Err(reason) = result {
                warn_parse_skip(format!(
                    "skipping primitive batch vertex_buffer={} index_buffer={} start={} count={} node={:?}: {}",
                    batch.vertex_buffer_index,
                    batch.index_buffer_index,
                    batch.first_index,
                    batch.index_count,
                    batch.node_name,
                    reason,
                ));
            }
        }
    }
    if !has_batch_ranges && submeshes.is_empty() {
        let triangles = convert_to_triangle_list(
            &first_index_buffer.indices,
            primitive_type,
            first_index_buffer.element_type,
        );
        if let Err(reason) = append_submesh(
            &mut indices,
            &mut submeshes,
            triangles,
            SubmeshParams {
                material_index: 0,
                texture_index: 0,
                render_mode: GeoRenderMode::Opaque,
                alpha_cutoff: 0.5,
                info: &BatchInfo::default(),
                vertex_buffer_index: 0,
                base_vertex: 0,
                vertex_buffer_offset: 0,
                vertex_count: vertices.len(),
            },
        ) {
            warn_parse_skip(format!("skipping unnamed geometry batch: {reason}"));
        }
    }
    if indices.is_empty() {
        return Err("index buffer contains no complete, valid triangles".into());
    }

    fill_missing_normals(&mut vertices, &indices);

    let mut index_layout = String::with_capacity(index_data.len() * 64);
    for (index, buffer) in index_data.iter().enumerate() {
        if index > 0 {
            index_layout.push_str("; ");
        }
        let _ = write!(
            index_layout,
            "IndexBuffer/cHcIndexBufferDx[{index}]: indices={} bytes_per_index={}",
            buffer.indices.len(),
            if buffer.element_type == "u16" { 2 } else { 4 }
        );
    }
    let vertex_layout = vertex_layouts.join("; ");
    let diagnostics = GeoParseDiagnostics {
        vertex_stride: first_vertex_buffer.stride,
        vertex_count: vertices.len(),
        declared_vertex_count: first_vertex_buffer.declared_vertex_count,
        fvf: first_vertex_buffer.fvf,
        submesh_count: submeshes.len(),
        local_transform_count: local_transforms.len(),
        index_element_type: first_index_buffer.element_type,
        raw_index_count: index_data.iter().map(|buffer| buffer.indices.len()).sum(),
        primitive_type,
        vertex_layout,
        index_layout,
        batch_ranges,
    };
    Ok(GeoModel {
        vertices,
        indices,
        textures,
        submeshes,
        diagnostics,
    })
}

fn decode_vertex_buffer(buffer: &VertexBuffer) -> Result<Vec<GeoVertex>, String> {
    if buffer.stride == 0 {
        return Err("zero-byte stride".into());
    }
    let vertex_count = buffer.bytes.len() / buffer.stride;
    if vertex_count == 0 || !buffer.bytes.len().is_multiple_of(buffer.stride) {
        return Err("length is not a nonzero multiple of the stride".into());
    }

    let valid_normal_offset = buffer
        .normal_offset
        .filter(|offset| offset.checked_add(12).is_some_and(|end| end <= buffer.stride));
    let valid_uv_offset = buffer
        .uv_offset
        .filter(|offset| offset.checked_add(8).is_some_and(|end| end <= buffer.stride));

    let mut decoded = Vec::with_capacity(vertex_count);
    for record in buffer.bytes.chunks_exact(buffer.stride) {
        let position = read_position(record, buffer.position_offset, buffer.position_format)?;
        let normal = valid_normal_offset
            .and_then(|offset| read_vec3(record, offset).ok())
            .unwrap_or([0.0, 0.0, 0.0]);
        // GeoPcDx uses D3D9 UV convention (0,0 = top-left, V down); TgPcDx decodes top-to-bottom.
        let uv = valid_uv_offset
            .and_then(|offset| read_vec2(record, offset).ok())
            .unwrap_or([0.0, 0.0]);
        if position.iter().any(|v| !v.is_finite())
            || normal.iter().any(|v| !v.is_finite())
            || uv.iter().any(|v| !v.is_finite())
        {
            return Err("vertex buffer contains non-finite values".into());
        }
        decoded.push(GeoVertex {
            position,
            normal,
            uv,
        });
    }
    Ok(decoded)
}

pub(crate) fn convert_to_triangle_list(
    indices: &[u32],
    topology: PrimitiveTopology,
    _index_element_type: &str,
) -> Vec<u32> {
    match topology {
        PrimitiveTopology::TriangleList => {
            let complete_index_count = indices.len() / 3 * 3;
            indices[..complete_index_count].to_vec()
        }
        PrimitiveTopology::TriangleStrip => {
            let mut triangles = Vec::with_capacity(indices.len().saturating_sub(2) * 3);
            for (position, &index) in indices.iter().enumerate().skip(2) {
                let mut a = indices[position - 2];
                let mut b = indices[position - 1];
                if position % 2 == 1 {
                    std::mem::swap(&mut a, &mut b);
                }
                // Degenerate triangles are dropped, preserving DX9 winding parity.
                if a != b && b != index && a != index {
                    triangles.extend_from_slice(&[a, b, index]);
                }
            }
            triangles
        }
    }
}

fn push_submesh_record(
    submeshes: &mut Vec<GeoSubmesh>,
    first_index: u32,
    index_count: u32,
    mirrored: bool,
    params: SubmeshParams<'_>,
) {
    let info = params.info;
    submeshes.push(GeoSubmesh {
        first_index,
        index_count,
        material_index: params.material_index,
        texture_index: params.texture_index,
        render_mode: params.render_mode,
        alpha_cutoff: params.alpha_cutoff,
        vertex_buffer_index: params.vertex_buffer_index,
        base_vertex: params.base_vertex,
        vertex_buffer_offset: params.vertex_buffer_offset,
        visible: info.lod_level == 0
            && !is_auxiliary_node(&info.node_name)
            && !is_auxiliary_node(&info.material_name)
            && !is_auxiliary_node(&info.shader_name),
        node_name: info.node_name.clone(),
        material_name: info.material_name.clone(),
        shader_name: info.shader_name.clone(),
        lod_level: info.lod_level,
        triangle_count: index_count / 3,
        mirrored,
    });
}

fn transform_vertex(
    mut vertex: GeoVertex,
    transform: Mat4,
    normal_transform: Mat4,
) -> Result<GeoVertex, String> {
    vertex.position = transform
        .transform_point3(Vec3::from_array(vertex.position))
        .to_array();
    vertex.normal = normal_transform
        .transform_vector3(Vec3::from_array(vertex.normal))
        .normalize_or_zero()
        .to_array();
    if vertex.position.iter().any(|v| !v.is_finite())
        || vertex.normal.iter().any(|v| !v.is_finite())
    {
        return Err("node transform produced non-finite vertex data".into());
    }
    Ok(vertex)
}

pub(crate) fn append_submesh(
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    params: SubmeshParams<'_>,
) -> Result<(), String> {
    if triangles.is_empty() {
        return Ok(());
    }
    let first_index = u32::try_from(indices.len()).map_err(|_| "too many converted indices")?;
    let index_count = u32::try_from(triangles.len()).map_err(|_| "too many converted indices")?;
    let vertex_offset = i64::from(params.base_vertex) + i64::from(params.vertex_buffer_offset);
    let vertex_count = params.vertex_count;

    let orig_len = indices.len();
    indices.reserve(triangles.len());
    for index in triangles {
        let adjusted = i64::from(index) + vertex_offset;
        if adjusted < 0 || adjusted >= vertex_count as i64 {
            indices.truncate(orig_len);
            return Err(format!(
                "index buffer references vertex {adjusted}, but only {vertex_count} vertices were parsed"
            ));
        }
        let Ok(mapped) = u32::try_from(adjusted) else {
            indices.truncate(orig_len);
            return Err("adjusted vertex index exceeds draw limits".into());
        };
        indices.push(mapped);
    }

    push_submesh_record(submeshes, first_index, index_count, false, params);
    Ok(())
}

fn append_transformed_submesh_in_place(
    vertices: &mut Vec<GeoVertex>,
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    source_vertex_count: usize,
    transform: Mat4,
    params: SubmeshParams<'_>,
) -> Result<(), String> {
    if triangles.is_empty() {
        return Ok(());
    }
    let first_index = u32::try_from(indices.len()).map_err(|_| "too many converted indices")?;
    let index_count = u32::try_from(triangles.len()).map_err(|_| "too many converted indices")?;
    let vertex_offset = i64::from(params.base_vertex) + i64::from(params.vertex_buffer_offset);
    let det = transform.determinant();
    let normal_transform = if det.abs() > f32::EPSILON {
        transform.inverse().transpose()
    } else {
        Mat4::IDENTITY
    };

    let orig_vertices_len = vertices.len();
    let orig_indices_len = indices.len();
    let mut remapped: HashMap<u32, u32> = HashMap::with_capacity((triangles.len() / 3).min(4096));
    indices.reserve(triangles.len());

    let result: Result<(), String> = (|| {
        for index in triangles {
            let adjusted = i64::from(index) + vertex_offset;
            if adjusted < 0 || adjusted >= source_vertex_count as i64 {
                return Err(format!(
                    "index buffer references vertex {adjusted}, but only {source_vertex_count} vertices were parsed"
                ));
            }
            let source_index =
                u32::try_from(adjusted).map_err(|_| "adjusted vertex index exceeds draw limits")?;
            let transformed_index = if let Some(&mapped) = remapped.get(&source_index) {
                mapped
            } else {
                let vertex =
                    transform_vertex(vertices[source_index as usize], transform, normal_transform)?;
                let mapped =
                    u32::try_from(vertices.len()).map_err(|_| "too many transformed vertices")?;
                vertices.push(vertex);
                remapped.insert(source_index, mapped);
                mapped
            };
            indices.push(transformed_index);
        }
        Ok(())
    })();

    if let Err(error) = result {
        vertices.truncate(orig_vertices_len);
        indices.truncate(orig_indices_len);
        return Err(error);
    }

    push_submesh_record(submeshes, first_index, index_count, det < 0.0, params);
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn append_transformed_submesh(
    vertices: &mut Vec<GeoVertex>,
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    source_vertices: &[GeoVertex],
    transform: Mat4,
    params: SubmeshParams<'_>,
) -> Result<(), String> {
    if triangles.is_empty() {
        return Ok(());
    }
    let first_index = u32::try_from(indices.len()).map_err(|_| "too many converted indices")?;
    let index_count = u32::try_from(triangles.len()).map_err(|_| "too many converted indices")?;
    let vertex_offset = i64::from(params.base_vertex) + i64::from(params.vertex_buffer_offset);
    let det = transform.determinant();
    let normal_transform = if det.abs() > f32::EPSILON {
        transform.inverse().transpose()
    } else {
        Mat4::IDENTITY
    };

    let orig_vertices_len = vertices.len();
    let orig_indices_len = indices.len();
    let mut remapped: HashMap<u32, u32> = HashMap::with_capacity((triangles.len() / 3).min(4096));
    indices.reserve(triangles.len());

    let result: Result<(), String> = (|| {
        for index in triangles {
            let adjusted = i64::from(index) + vertex_offset;
            if adjusted < 0 || adjusted >= source_vertices.len() as i64 {
                return Err(format!(
                    "index buffer references vertex {adjusted}, but only {} vertices were parsed",
                    source_vertices.len()
                ));
            }
            let source_index =
                u32::try_from(adjusted).map_err(|_| "adjusted vertex index exceeds draw limits")?;
            let transformed_index = if let Some(&mapped) = remapped.get(&source_index) {
                mapped
            } else {
                let vertex = transform_vertex(
                    source_vertices[source_index as usize],
                    transform,
                    normal_transform,
                )?;
                let mapped =
                    u32::try_from(vertices.len()).map_err(|_| "too many transformed vertices")?;
                vertices.push(vertex);
                remapped.insert(source_index, mapped);
                mapped
            };
            indices.push(transformed_index);
        }
        Ok(())
    })();

    if let Err(error) = result {
        vertices.truncate(orig_vertices_len);
        indices.truncate(orig_indices_len);
        return Err(error);
    }

    push_submesh_record(submeshes, first_index, index_count, det < 0.0, params);
    Ok(())
}