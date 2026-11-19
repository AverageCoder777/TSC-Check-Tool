//! Core GeoPcDx model parsing: SERZ traversal, vertex/index decoding, batch assembly.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use glam::{Mat4, Vec3};

use crate::geo::diagnostics::{
    log_batch_diagnostics, log_material_diagnostics, log_vertex_buffer_diagnostics, warn_parse_skip,
};
use crate::geo::helpers::{
    build_id_map, collect_float_values, collect_i32_values, collect_transform_names,
    compute_world_transforms, decode_indices, direct_text, find_direct_number, find_named_values,
    find_number, find_reference, find_text, find_transform_id, matrix_from_values,
    parse_float_values, parse_fvf, parse_serz_document, position_format_from_type,
    resolve_ref_node, resolve_ref_text, vertex_element_size,
};
use crate::geo::texture::{classify_shader, is_auxiliary_node, is_hidden_shader_name};
use crate::geo::types::{
    GeoModel, GeoParseDiagnostics, GeoRenderMode, GeoSubmesh, GeoVertex, PrimitiveTopology,
};
use crate::geo::vertex::{
    PositionFormat, fill_missing_normals, read_position, read_vec2, read_vec3,
};
use crate::serz::SerzNode;

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

pub(crate) fn parse_model(path: &Path, data: &[u8]) -> Result<GeoModel, String> {
    let root = parse_serz_document(data)?;
    let id_map = build_id_map(&root);
    let local_transforms = find_named_values(&root, "SourceLToPTransform")
        .into_iter()
        .flatten()
        .flat_map(collect_float_values)
        .collect::<Vec<_>>()
        .chunks_exact(16)
        .map(|values| matrix_from_values(values).unwrap_or(Mat4::IDENTITY))
        .collect::<Vec<_>>();
    let transform_dependencies = find_named_values(&root, "TransformDependency")
        .into_iter()
        .flatten()
        .flat_map(collect_i32_values)
        .collect::<Vec<_>>();
    let world_transforms = compute_world_transforms(&local_transforms, &transform_dependencies);
    let xml = crate::serz::SerzReader::to_xml(&root, 0, true);
    let xml = if data.starts_with(b"SERZ") {
        xml
    } else {
        String::from_utf8_lossy(data).into_owned()
    };
    let textures = crate::auditor::extract_geo_texture_references(&xml);
    let material_bindings = find_material_bindings(&root, &textures, &id_map);
    log_material_diagnostics(path, &material_bindings);
    let transform_names = collect_transform_names(&root);

    // Collect vertex buffers first to build the SERZ ID -> source_index map
    let mut vertex_data = Vec::new();
    let (vb_id_to_index, prim_id_to_vbuf) =
        collect_vertex_buffers(&root, &id_map, &mut vertex_data);

    // Build vertex buffer sizes array for range-based fallback
    let vb_sizes: Vec<usize> = vertex_data
        .iter()
        .map(|buffer| {
            if buffer.stride == 0 {
                0
            } else {
                buffer.bytes.len() / buffer.stride
            }
        })
        .collect();

    let context = NodeContext {
        material_bindings: &material_bindings,
        ids: &id_map,
        world_transforms: &world_transforms,
        transform_names: &transform_names,
        vb_id_to_index: &vb_id_to_index,
        prim_id_to_vbuf: &prim_id_to_vbuf,
        vb_sizes: &vb_sizes,
    };
    let mut raw_batches = Vec::new();
    find_submeshes(
        &root,
        None,
        &context,
        Mat4::IDENTITY,
        None,
        None,
        false,
        None,
        None,
        &mut HashSet::new(),
        &mut raw_batches,
    );
    deduplicate_submeshes(&mut raw_batches);
    resolve_implicit_vertex_buffers(&mut raw_batches, &vertex_data);
    log_batch_diagnostics(path, &raw_batches);

    // Now collect index buffers (requires batches for vertex count inference)
    let mut index_data = Vec::new();
    collect_index_buffers(&root, &id_map, &raw_batches, &vertex_data, &mut index_data);
    log_vertex_buffer_diagnostics(path, &vertex_data, &index_data);

    let first_vertex_buffer = vertex_data
        .first()
        .ok_or("GeoPcDx has no supported vertex buffer")?;
    let first_index_buffer = index_data
        .first()
        .ok_or("GeoPcDx has no supported index buffer")?;
    // Batches address buffers by their declared array slot, not by the order in which
    // parsable buffers ended up in these vectors. Buffers that fail to decode are
    // skipped, so a positional lookup would silently bind a batch to the wrong data.
    let vertex_slots = vertex_data
        .iter()
        .enumerate()
        .map(|(position, buffer)| (buffer.source_index as u32, position))
        .collect::<HashMap<_, _>>();
    let index_slots = index_data
        .iter()
        .enumerate()
        .map(|(position, buffer)| (buffer.source_index as u32, position))
        .collect::<HashMap<_, _>>();
    let resolve_index_buffer = |index: u32| {
        index_slots
            .get(&index)
            .and_then(|position| index_data.get(*position))
    };
    let mut vertices = Vec::new();
    // Indexed by `vertex_data` position, not by decode order: a skipped buffer keeps a
    // `None` slot so later batches still resolve to the right offset (or are skipped).
    let mut vertex_buffer_offsets: Vec<Option<i32>> = vec![None; vertex_data.len()];
    let mut vertex_layouts = Vec::with_capacity(vertex_data.len());
    for (buffer_position, buffer) in vertex_data.iter().enumerate() {
        let buffer_offset = vertices.len();
        // Decode into a scratch buffer first: a broken buffer must not leave a partial
        // vertex run behind, otherwise later batches would read half-written geometry.
        let mut decoded = Vec::new();
        let result = (|| -> Result<(), String> {
            if buffer.stride == 0 {
                return Err("zero-byte stride".into());
            }
            let vertex_count = buffer.bytes.len() / buffer.stride;
            if vertex_count == 0 || buffer.bytes.len() % buffer.stride != 0 {
                return Err("length is not a nonzero multiple of the stride".into());
            }
            for record in buffer.bytes.chunks_exact(buffer.stride) {
                let position =
                    read_position(record, buffer.position_offset, buffer.position_format)?;
                let normal = buffer
                    .normal_offset
                    .filter(|offset| {
                        offset
                            .checked_add(12)
                            .is_some_and(|end| end <= buffer.stride)
                    })
                    .and_then(|offset| read_vec3(record, offset).ok())
                    .unwrap_or([0.0, 0.0, 0.0]);
                let uv = buffer
                    .uv_offset
                    .filter(|offset| {
                        offset
                            .checked_add(8)
                            .is_some_and(|end| end <= buffer.stride)
                    })
                    .and_then(|offset| read_vec2(record, offset).ok())
                    // GeoPcDx is a DirectX 9 format: UVs use D3D convention (0,0 = top-left, V down).
                    // TgPcDx textures decode in standard DirectX orientation (top-to-bottom),
                    // so no V-flip is needed. Keep UV as-is from the vertex buffer.
                    .unwrap_or([0.0, 0.0]);
                if position.iter().any(|value| !value.is_finite())
                    || normal.iter().any(|value| !value.is_finite())
                    || uv.iter().any(|value| !value.is_finite())
                {
                    return Err("vertex buffer contains non-finite values".into());
                }
                decoded.push(GeoVertex {
                    position,
                    normal,
                    uv,
                });
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                vertices.extend(decoded);
                vertex_buffer_offsets[buffer_position] = i32::try_from(buffer_offset).ok();
                vertex_layouts.push(buffer.layout.clone());
            }
            Err(reason) => warn_parse_skip(format!(
                "skipping vertex buffer #{} ({}): {}",
                buffer.source_index, buffer.layout, reason,
            )),
        }
    }

    let has_batch_ranges = !raw_batches.is_empty();
    let render_batches = raw_batches
        .iter()
        .filter(|batch| !batch.excluded)
        .collect::<Vec<_>>();
    let primitive_type = if let Some(batch) = render_batches.first() {
        batch.topology
    } else {
        find_primitive_topology(&root, &id_map)
    };
    let batch_ranges = render_batches
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
        .collect::<Vec<_>>();
    let mut indices = Vec::new();
    let mut submeshes = Vec::new();
    if has_batch_ranges {
        let source_vertices = vertices.clone();
        for batch in render_batches {
            let result = (|| -> Result<(), String> {
                let Some(end) = batch.first_index.checked_add(batch.index_count) else {
                    return Err("primitive batch index range overflows".into());
                };
                let index_buffer = resolve_index_buffer(batch.index_buffer_index)
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
                    .and_then(|offset| *offset)
                    .ok_or("primitive batch references a vertex buffer that failed to decode")?;
                let binding = batch
                    .material_reference
                    .and_then(|reference| material_bindings.get(&reference));
                let material_index = binding
                    .map(|binding| binding.material_index)
                    .or(batch.material_index)
                    .unwrap_or(0);
                let texture_index = binding
                    .map(|binding| binding.texture_index)
                    .or(batch.texture_index)
                    .unwrap_or(material_index)
                    .min(textures.len().saturating_sub(1) as u32);
                let render_mode = binding
                    .map(|binding| binding.render_mode)
                    .unwrap_or_default();
                let alpha_cutoff = binding.map(|binding| binding.alpha_ref).unwrap_or(0.5);
                let triangles =
                    convert_to_triangle_list(range, batch.topology, index_buffer.element_type);
                let info = BatchInfo {
                    node_name: batch.node_name.clone(),
                    material_name: batch.material_name.clone(),
                    shader_name: batch.shader_name.clone(),
                    lod_level: batch.lod_level,
                };
                if batch.transform == Mat4::IDENTITY {
                    append_submesh(
                        &mut indices,
                        &mut submeshes,
                        triangles,
                        material_index,
                        texture_index,
                        render_mode,
                        alpha_cutoff,
                        &info,
                        batch.vertex_buffer_index,
                        batch.base_vertex,
                        vertex_buffer_offset,
                        source_vertices.len(),
                    )?;
                } else {
                    append_transformed_submesh(
                        &mut vertices,
                        &mut indices,
                        &mut submeshes,
                        triangles,
                        &source_vertices,
                        batch.transform,
                        material_index,
                        texture_index,
                        render_mode,
                        alpha_cutoff,
                        &info,
                        batch.vertex_buffer_index,
                        batch.base_vertex,
                        vertex_buffer_offset,
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
            0,
            0,
            GeoRenderMode::Opaque,
            0.5,
            &BatchInfo::default(),
            0,
            0,
            0,
            vertices.len(),
        ) {
            warn_parse_skip(format!("skipping unnamed geometry batch: {reason}"));
        }
    }
    if indices.is_empty() {
        return Err("index buffer contains no complete, valid triangles".into());
    }
    fill_missing_normals(&mut vertices, &indices);
    let index_layout = index_data
        .iter()
        .enumerate()
        .map(|(index, buffer)| {
            format!(
                "IndexBuffer/cHcIndexBufferDx[{index}]: indices={} bytes_per_index={}",
                buffer.indices.len(),
                if buffer.element_type == "u16" { 2 } else { 4 }
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
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

#[derive(Clone, Debug)]
pub(crate) struct NodeContext<'a> {
    pub material_bindings: &'a HashMap<u32, MaterialBinding>,
    pub ids: &'a HashMap<u32, &'a crate::serz::SerzNode>,
    pub world_transforms: &'a [Mat4],
    pub transform_names: &'a [String],
    /// SERZ ID (of cHcVertexBufferDx or cHcGeometryVertexDataDx) -> vertex buffer source_index.
    pub vb_id_to_index: &'a HashMap<u32, u32>,
    /// SERZ ID of cHcPrimitiveSetDescDx -> vertex buffer source_index (from References inside cHcGeometryVertexDataDx).
    pub prim_id_to_vbuf: &'a HashMap<u32, u32>,
    /// Vertex buffer sizes: source_index -> vertex count (for range-based fallback).
    pub vb_sizes: &'a [usize],
}

pub(crate) fn find_submeshes(
    node: &crate::serz::SerzNode,
    inherited_material: Option<u32>,
    context: &NodeContext<'_>,
    parent_transform: Mat4,
    inherited_transform_id: Option<usize>,
    inherited_lod: Option<u32>,
    inherited_excluded: bool,
    inherited_node_name: Option<&str>,
    inherited_vertex_buffer_index: Option<u32>,
    reference_stack: &mut HashSet<u32>,
    output: &mut Vec<RawSubmesh>,
) {
    let NodeContext {
        material_bindings,
        ids,
        world_transforms,
        transform_names,
        vb_id_to_index,
        prim_id_to_vbuf,
        vb_sizes,
    } = context;
    if let SerzNode::Reference { name, target_id } = node {
        if name.eq_ignore_ascii_case("material") {
            return;
        }
        if reference_stack.insert(*target_id) {
            if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                find_submeshes(
                    target,
                    inherited_material,
                    context,
                    parent_transform,
                    inherited_transform_id,
                    inherited_lod,
                    inherited_excluded,
                    inherited_node_name,
                    inherited_vertex_buffer_index,
                    reference_stack,
                    output,
                );
            }
            reference_stack.remove(target_id);
        }
        return;
    }
    let SerzNode::Element {
        name, id, children, ..
    } = node
    else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    if clean_name.contains("collisionroot") {
        return;
    }
    let node_label =
        crate::geo::helpers::direct_text(children, &["name", "nodename", "transformname"], ids)
            .unwrap_or_default();
    let has_lower_lod_name = crate::geo::texture::lod_name_is_nonzero(name)
        || ["name", "nodename", "transformname"]
            .iter()
            .filter_map(|field| crate::geo::helpers::direct_text(children, &[*field], ids))
            .any(crate::geo::texture::lod_name_is_nonzero);
    let lod = crate::geo::texture::lod_from_name(name)
        .or_else(|| crate::geo::texture::lod_from_name(node_label))
        .or_else(|| {
            clean_name
                .contains("lod")
                .then(|| find_number(children, &["lodlevel", "lodindex", "level"], ids))
                .flatten()
                .and_then(|level| u32::try_from(level).ok())
        })
        .or(inherited_lod);
    let material_reference = if clean_name.contains("sgnodematerial") {
        find_reference(children, "material").or(inherited_material)
    } else {
        inherited_material
    };
    let excluded = inherited_excluded
        || lod.is_some_and(|level| level > 0)
        || has_lower_lod_name
        || crate::geo::texture::node_is_explicitly_hidden(children, ids)
        || material_reference
            .and_then(|reference| material_bindings.get(&reference))
            .is_some_and(|binding| binding.excluded);
    let zsorter_single_child = direct_text(children, &["singlechildtransform"], ids)
        .is_some_and(|value| value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"));
    let has_zsorter_transform_id = find_transform_id(children, ids).is_some();
    let child_transform_id = if clean_name.contains("chcsgnodezsorter")
        && !zsorter_single_child
        && !has_zsorter_transform_id
    {
        None
    } else {
        find_transform_id(children, ids)
    };
    let primary_transform_is_default = direct_text(
        children,
        &["primarytransformid", "primarytransformids"],
        ids,
    )
    .and_then(|value| {
        let trimmed = value.trim();
        trimmed
            .parse::<i32>()
            .ok()
            .or_else(|| trimmed.parse::<u32>().ok().map(|v| v as i32))
            .or_else(|| trimmed.parse::<i64>().ok().map(|v| v as i32))
    }) == Some(0);
    let has_explicit_transform_id =
        direct_text(children, &["transformid", "transformids"], ids).is_some();
    let inherits_parent_transform = clean_name.contains("geometrynodesamestate")
        && inherited_transform_id.is_some()
        && primary_transform_is_default
        && !has_explicit_transform_id;
    let active_transform_id = if inherits_parent_transform {
        inherited_transform_id
    } else {
        child_transform_id.or(inherited_transform_id)
    };
    let local_transform = find_local_transform(children);
    let transform = parent_transform * local_transform;
    let binding = material_reference.and_then(|reference| material_bindings.get(&reference));
    let batch_node_name = {
        let own_label = node_label.trim();
        let inherited_label = inherited_node_name
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let transform_name = active_transform_id
            .and_then(|transform_id| transform_names.get(transform_id))
            .map(|name| name.trim())
            .filter(|name| !name.is_empty());
        own_label
            .is_empty()
            .then_some(inherited_label)
            .flatten()
            .or(transform_name)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "Node #{}",
                    active_transform_id.unwrap_or_else(|| usize::try_from(*id).unwrap_or(0))
                )
            })
    };
    let effective_node_name = if batch_node_name.starts_with("Node #") {
        inherited_node_name
    } else {
        Some(batch_node_name.as_str())
    };
    if clean_name.contains("primlist")
        || clean_name.contains("primitivelist")
        || clean_name.contains("batch")
        || clean_name.contains("primitive set desc")
        || clean_name.contains("primitivesetdescdx")
    {
        let first = find_number(
            children,
            &["firstindex", "startindex", "indexstart", "indexarraystart"],
            ids,
        );
        let topology = find_primitive_topology(node, ids);
        let is_primitive_set = clean_name.contains("primitivesetdescdx");
        // RailWorks stores `NumPrimitives` as a raw index count on primitive-set descriptors,
        // but as a primitive count on legacy list nodes. Never read past the implied range:
        // strips over-reading by a couple of indices bridge distant vertices into long webs.
        let primitives_to_indices = |value: u64| {
            if is_primitive_set {
                Some(value)
            } else {
                match topology {
                    PrimitiveTopology::TriangleList => value.checked_mul(3),
                    PrimitiveTopology::TriangleStrip => value.checked_add(2),
                }
            }
        };
        let count = match (
            find_number(children, &["indexcount", "numindices"], ids),
            find_number(children, &["primcount", "numprimitives"], ids),
        ) {
            (Some(index_count), Some(primitives)) => {
                primitives_to_indices(primitives).map(|implied| index_count.min(implied))
            }
            (Some(index_count), None) => Some(index_count),
            (None, Some(primitives)) => primitives_to_indices(primitives),
            (None, None) => None,
        };

        if let (Some(first), Some(count)) = (first, count)
            && let (Ok(first_index), Ok(index_count)) = (u32::try_from(first), u32::try_from(count))
        {
            let material_index = find_number(children, &["materialindex", "materialid"], ids)
                .and_then(|value| u32::try_from(value).ok())
                .or_else(|| {
                    material_reference
                        .and_then(|reference| material_bindings.get(&reference))
                        .map(|binding| binding.material_index)
                });
            let texture_index = material_reference
                .and_then(|reference| material_bindings.get(&reference))
                .map(|binding| binding.texture_index);
            let base_vertex = find_text(
                children,
                &["vertexindexbase", "vertexindexbases", "basevertex"],
                ids,
            )
            .and_then(|value| {
                let trimmed = value.trim();
                trimmed
                    .parse::<i32>()
                    .ok()
                    .or_else(|| trimmed.parse::<u32>().ok().map(|v| v as i32))
                    .or_else(|| trimmed.parse::<i64>().ok().map(|v| v as i32))
            })
            .unwrap_or(0);

            let vertex_index_min = find_number(children, &["vertexindexmin"], ids).unwrap_or(0);
            let vertex_index_range = find_number(children, &["vertexindexrange"], ids).unwrap_or(0);
            let max_local_vertex_ref =
                i128::from(vertex_index_min) + i128::from(vertex_index_range);
            let max_local_vertex_ref = u64::try_from(max_local_vertex_ref).ok();
            // Vertex buffer index: for cHcPrimitiveSetDescDx, use the prim_id_to_vbuf map
            // which maps the primitive set's own SERZ ID to the vertex buffer index.
            // For other batch types, fall back to the existing logic.
            let vertex_buffer_index = if is_primitive_set {
                // Look up the vertex buffer index from the primitive set's own SERZ ID
                prim_id_to_vbuf.get(id).copied()
            } else {
                None
            }
            .or_else(|| {
                find_number(
                    children,
                    &[
                        "vertexbufferindex",
                        "vertexdataindex",
                        "bufferindex",
                        "vertexbuffer",
                    ],
                    ids,
                )
                .and_then(|value| u32::try_from(value).ok())
            })
            .or_else(|| {
                // Check for SerzNode::Reference children pointing to known vertex buffer IDs
                children.iter().find_map(|child| {
                    if let SerzNode::Reference { target_id, .. } = child {
                        vb_id_to_index.get(target_id).copied()
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                find_reference(children, "vertexbuffer").and_then(|target_id| {
                    // Resolve via vb_id_to_index map first (covers both cHcVertexBufferDx and cHcGeometryVertexDataDx)
                    vb_id_to_index.get(&target_id).copied().or_else(|| {
                        // Fallback: resolve the referenced vertex buffer node and read its BufferIndex.
                        ids.get(&target_id).and_then(|vb_node| {
                            find_direct_number(
                                match vb_node {
                                    SerzNode::Element { children, .. } => children,
                                    _ => &[],
                                },
                                &["bufferindex", "vertexbufferindex"],
                                ids,
                            )
                            .and_then(|value| u32::try_from(value).ok())
                        })
                    })
                })
            })
            .or_else(|| inherited_vertex_buffer_index)
            .unwrap_or(0);
            let index_buffer_index = find_number(children, &["indexbufferindex"], ids)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);

            output.push(RawSubmesh {
                first_index,
                index_count,
                material_reference,
                material_index,
                texture_index,
                vertex_buffer_index,
                index_buffer_index,
                base_vertex,
                max_local_vertex_ref,
                topology,
                transform: active_transform_id
                    .map(|id| world_transforms.get(id).copied().unwrap_or(Mat4::IDENTITY))
                    .unwrap_or(transform),
                excluded,
                node_name: batch_node_name.clone(),
                material_name: binding
                    .map(|binding| binding.material_name.clone())
                    .unwrap_or_else(|| "default".into()),
                shader_name: binding
                    .map(|binding| binding.shader_name.clone())
                    .unwrap_or_default(),
                render_mode: binding
                    .map(|binding| binding.render_mode)
                    .unwrap_or_default(),
                lod_level: u8::try_from(lod.unwrap_or(0)).unwrap_or(u8::MAX),
            });
        }
    }
    let is_lod_container =
        clean_name.contains("lodlist") || clean_name.contains("chcsgnodelodselector");

    // Propagate vertex buffer index from parent containers.
    let mut next_inherited_vbuf = inherited_vertex_buffer_index;
    if clean_name.contains("chcgeometryvertexdata") || clean_name.contains("geometrynodesamestate")
    {
        // First try explicit numeric fields
        if let Some(vbuf_idx) = find_number(
            children,
            &["vertexbufferindex", "vertexdataindex", "bufferindex"],
            ids,
        )
        .and_then(|value| u32::try_from(value).ok())
        {
            next_inherited_vbuf = Some(vbuf_idx);
        } else {
            // Then check for References to known vertex buffer IDs
            next_inherited_vbuf =
                crate::geo::helpers::find_vbuf_ref_in_subtree(children, vb_id_to_index)
                    .or(next_inherited_vbuf);
        }
    }

    let mut next_lod = 0;
    for child in children {
        let is_lod_child = matches!(child, SerzNode::Element { .. })
            || matches!(
                child,
                SerzNode::Reference { name, .. } if !name.eq_ignore_ascii_case("material")
            );
        let child_lod = if is_lod_container && is_lod_child {
            let level = next_lod;
            next_lod += 1;
            if level > 0 {
                continue;
            }
            Some(level)
        } else {
            None
        };
        find_submeshes(
            child,
            material_reference,
            context,
            transform,
            active_transform_id,
            child_lod.or(lod),
            excluded,
            effective_node_name,
            next_inherited_vbuf,
            reference_stack,
            output,
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RawSubmeshKey {
    pub first_index: u32,
    pub index_count: u32,
    pub material_reference: Option<u32>,
    pub material_index: Option<u32>,
    pub texture_index: Option<u32>,
    pub vertex_buffer_index: u32,
    pub index_buffer_index: u32,
    pub base_vertex: i32,
    pub max_local_vertex_ref: Option<u64>,
    pub topology: u8,
    pub transform: [u32; 16],
    pub excluded: bool,
    pub lod_level: u8,
    pub node_name: String,
    pub material_name: String,
    pub shader_name: String,
    pub render_mode: GeoRenderMode,
}

pub(crate) fn deduplicate_submeshes(batches: &mut Vec<RawSubmesh>) {
    let mut seen = HashSet::with_capacity(batches.len());
    batches.retain(|batch| {
        seen.insert(RawSubmeshKey {
            first_index: batch.first_index,
            index_count: batch.index_count,
            material_reference: batch.material_reference,
            material_index: batch.material_index,
            texture_index: batch.texture_index,
            vertex_buffer_index: batch.vertex_buffer_index,
            index_buffer_index: batch.index_buffer_index,
            base_vertex: batch.base_vertex,
            max_local_vertex_ref: batch.max_local_vertex_ref,
            topology: batch.topology as u8,
            transform: batch.transform.to_cols_array().map(f32::to_bits),
            excluded: batch.excluded,
            lod_level: batch.lod_level,
            node_name: batch.node_name.clone(),
            material_name: batch.material_name.clone(),
            shader_name: batch.shader_name.clone(),
            render_mode: batch.render_mode,
        })
    });
}

pub(crate) fn find_local_transform(children: &[crate::serz::SerzNode]) -> Mat4 {
    fn parse_matrix(node: &crate::serz::SerzNode) -> Option<Mat4> {
        matrix_from_values(&collect_float_values(node))
    }

    for child in children {
        match child {
            SerzNode::Property { name, value, .. }
                if name.to_ascii_lowercase().contains("transform") =>
            {
                if let Some(matrix) = matrix_from_values(&parse_float_values(value)) {
                    return matrix;
                }
            }
            SerzNode::Element { name, .. } if name.to_ascii_lowercase().contains("transform") => {
                if let Some(matrix) = parse_matrix(child) {
                    return matrix;
                }
            }
            _ => {}
        }
    }
    Mat4::IDENTITY
}

fn resolve_implicit_vertex_buffers(batches: &mut [RawSubmesh], vertex_data: &[VertexBuffer]) {
    if vertex_data.len() <= 1 || batches.iter().any(|batch| batch.vertex_buffer_index != 0) {
        return;
    }

    let vb_sizes: Vec<usize> = vertex_data
        .iter()
        .map(|buffer| {
            if buffer.stride == 0 {
                0
            } else {
                buffer.bytes.len() / buffer.stride
            }
        })
        .collect();

    let mut cursors = vec![0usize; vertex_data.len()];
    let mut started = vec![false; vertex_data.len()];
    let mut material_to_slot: HashMap<u32, usize> = HashMap::new();
    let mut shader_to_slot: HashMap<String, usize> = HashMap::new();

    for batch in batches.iter_mut() {
        let base = usize::try_from(batch.base_vertex).unwrap_or(0);
        let range = batch
            .max_local_vertex_ref
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let end = base.saturating_add(range);
        let fits = |slot: usize| vb_sizes[slot] > 0 && end <= vb_sizes[slot];

        let mat_slot = batch
            .material_reference
            .and_then(|reference| material_to_slot.get(&reference).copied());
        let shader_key = batch.shader_name.trim().to_ascii_lowercase();
        let shader_slot = (!shader_key.is_empty())
            .then(|| shader_to_slot.get(&shader_key).copied())
            .flatten();

        let chosen_slot = if base > 0 {
            let exact: Vec<usize> = (0..vertex_data.len())
                .filter(|&slot| started[slot] && cursors[slot] == base && fits(slot))
                .collect();
            if exact.len() == 1 {
                exact[0]
            } else if exact.len() > 1 {
                mat_slot
                    .filter(|slot| exact.contains(slot))
                    .or_else(|| shader_slot.filter(|slot| exact.contains(slot)))
                    .unwrap_or(exact[0])
            } else {
                mat_slot
                    .filter(|&slot| fits(slot) && cursors[slot] <= base)
                    .or_else(|| shader_slot.filter(|&slot| fits(slot) && cursors[slot] <= base))
                    .or_else(|| {
                        (0..vertex_data.len())
                            .filter(|&slot| fits(slot) && cursors[slot] <= base)
                            .min_by_key(|&slot| (base - cursors[slot], vb_sizes[slot]))
                    })
                    .unwrap_or(0)
            }
        } else {
            mat_slot
                .filter(|&slot| fits(slot))
                .or_else(|| (0..vertex_data.len()).find(|&slot| !started[slot] && fits(slot)))
                .or_else(|| shader_slot.filter(|&slot| fits(slot)))
                .unwrap_or(0)
        };

        started[chosen_slot] = true;
        cursors[chosen_slot] = cursors[chosen_slot].max(end);
        if let Some(reference) = batch.material_reference {
            material_to_slot.entry(reference).or_insert(chosen_slot);
        }
        if !shader_key.is_empty() {
            shader_to_slot.entry(shader_key).or_insert(chosen_slot);
        }
        batch.vertex_buffer_index = vertex_data[chosen_slot].source_index as u32;
    }
}

pub(crate) fn find_index_data(
    children: &[crate::serz::SerzNode],
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
    index_buffer_index: usize,
    batches: &[RawSubmesh],
    _vertex_counts: &[usize],
) -> Option<IndexBuffer> {
    if let Some((values, type_name)) = find_index_array(children, ids) {
        let width = width_from_type_name(&type_name)?;
        return Some(IndexBuffer {
            source_index: index_buffer_index,
            indices: values,
            element_type: type_name_from_width(width),
        });
    }
    let (bytes, _) = find_blob(children, "indexdata", ids)?;
    let explicit_width = find_number(
        children,
        &["indexsize", "indexelementsize", "bytesperindex"],
        ids,
    )
    .and_then(index_width_from_number)
    .or_else(|| {
        find_text(
            children,
            &["indexformat", "indextype", "indexsize", "indexelementsize"],
            ids,
        )
        .and_then(index_width_from_type_name)
    });
    let relevant_batches = batches
        .iter()
        .filter(|batch| batch.index_buffer_index as usize == index_buffer_index)
        .collect::<Vec<_>>();
    let max_index_end = relevant_batches
        .iter()
        .filter_map(|batch| u64::from(batch.first_index).checked_add(u64::from(batch.index_count)))
        .max()
        .unwrap_or(0);
    let max_local_vertex_ref = relevant_batches
        .iter()
        .filter_map(|batch| batch.max_local_vertex_ref)
        .max()
        .unwrap_or(0);
    let inferred_u32 = max_index_end > 0 && {
        let exact_u32 = max_index_end.checked_mul(4) == Some(bytes.len() as u64)
            && max_index_end.checked_mul(2) != Some(bytes.len() as u64);
        let local_exceeds_u16 = max_local_vertex_ref > u64::from(u16::MAX);
        let enough_for_u32 = max_index_end
            .checked_mul(4)
            .is_some_and(|minimum_bytes| bytes.len() as u64 >= minimum_bytes);
        exact_u32 || (local_exceeds_u16 && enough_for_u32)
    };
    let width = explicit_width.unwrap_or(if inferred_u32 { 4 } else { 2 });
    Some(IndexBuffer {
        source_index: index_buffer_index,
        indices: decode_indices(&bytes, width).ok()?,
        element_type: type_name_from_width(width),
    })
}

pub(crate) fn find_index_array(
    children: &[crate::serz::SerzNode],
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> Option<(Vec<u32>, String)> {
    fn find(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
    ) -> Option<(Vec<u32>, String)> {
        if let SerzNode::Reference { target_id, .. } = node {
            if !reference_stack.insert(*target_id) {
                return None;
            }
            let found = ids
                .get(target_id)
                .and_then(|target| find(target, ids, reference_stack));
            reference_stack.remove(target_id);
            return found;
        }
        match node {
            SerzNode::Array {
                name,
                type_name,
                values,
            } if name.eq_ignore_ascii_case("indexdata") => {
                let parsed = values
                    .iter()
                    .map(|value| value.parse::<u32>().ok())
                    .collect::<Option<Vec<_>>>()?;
                Some((parsed, type_name.clone()))
            }
            SerzNode::Element { name, children, .. } => {
                if name.eq_ignore_ascii_case("indexdata")
                    && let Some(found) = children
                        .iter()
                        .find_map(|child| find(child, ids, reference_stack))
                {
                    return Some(found);
                }
                children
                    .iter()
                    .find_map(|child| find(child, ids, reference_stack))
            }
            _ => None,
        }
    }
    for child in children {
        if let Some(found) = find(child, ids, &mut HashSet::new()) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn width_from_type_name(type_name: &str) -> Option<usize> {
    index_width_from_type_name(type_name)
}

pub(crate) fn index_width_from_number(value: u64) -> Option<usize> {
    match value {
        2 | 16 => Some(2),
        4 | 32 => Some(4),
        _ => None,
    }
}

pub(crate) fn index_width_from_type_name(type_name: &str) -> Option<usize> {
    let type_name = type_name.to_ascii_lowercase();
    if type_name.contains("uint32") || type_name.contains("sint32") || type_name.contains("32") {
        Some(4)
    } else if type_name.contains("uint16")
        || type_name.contains("sint16")
        || type_name.contains("16")
    {
        Some(2)
    } else {
        None
    }
}

pub(crate) fn type_name_from_width(width: usize) -> &'static str {
    if width == 4 { "u32" } else { "u16" }
}

pub(crate) fn find_primitive_topology(
    node: &crate::serz::SerzNode,
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> PrimitiveTopology {
    fn find(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
    ) -> Option<PrimitiveTopology> {
        match node {
            SerzNode::Property { name, value, .. } if is_primitive_type_name(name) => {
                Some(parse_primitive_topology(value))
            }
            SerzNode::Array { name, values, .. } if is_primitive_type_name(name) => {
                values.first().map(|value| parse_primitive_topology(value))
            }
            SerzNode::Reference { name, target_id } => {
                if !reference_stack.insert(*target_id) {
                    return None;
                }
                let result = ids.get(target_id).and_then(|target| {
                    if is_primitive_type_name(name) {
                        resolve_ref_text(target, ids, reference_stack).map(parse_primitive_topology)
                    } else {
                        find(target, ids, reference_stack)
                    }
                });
                reference_stack.remove(target_id);
                result
            }
            SerzNode::Element { name, children, .. } => {
                if is_primitive_type_name(name)
                    && let Some(value) = children
                        .iter()
                        .find_map(|child| resolve_ref_text(child, ids, reference_stack))
                {
                    return Some(parse_primitive_topology(value));
                }
                if name.to_ascii_lowercase().contains("trianglestrip") {
                    return Some(PrimitiveTopology::TriangleStrip);
                }
                children
                    .iter()
                    .find_map(|child| find(child, ids, reference_stack))
            }
            _ => None,
        }
    }
    find(node, ids, &mut HashSet::new()).unwrap_or_default()
}

fn is_primitive_type_name(name: &str) -> bool {
    ["primitive", "primitivetype", "primtype", "topology"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn parse_primitive_topology(value: &str) -> PrimitiveTopology {
    if value.to_ascii_lowercase().contains("strip") {
        PrimitiveTopology::TriangleStrip
    } else {
        PrimitiveTopology::TriangleList
    }
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
            for (position, &index) in indices.iter().enumerate() {
                if position < 2 {
                    continue;
                }
                let mut a = indices[position - 2];
                let mut b = indices[position - 1];
                if position % 2 == 1 {
                    std::mem::swap(&mut a, &mut b);
                }
                // Дегенеративные треугольники отбрасываются, но
                // естественным образом сдвигают winding parity для DX9.
                if a != b && b != index && a != index {
                    triangles.extend_from_slice(&[a, b, index]);
                }
            }
            triangles
        }
    }
}

pub(crate) fn append_submesh(
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    material_index: u32,
    texture_index: u32,
    render_mode: GeoRenderMode,
    alpha_cutoff: f32,
    info: &BatchInfo,
    vertex_buffer_index: u32,
    base_vertex: i32,
    vertex_buffer_offset: i32,
    vertex_count: usize,
) -> Result<(), String> {
    let vertex_offset = i64::from(base_vertex) + i64::from(vertex_buffer_offset);
    let triangles = triangles
        .into_iter()
        .map(|index| {
            let adjusted = i64::from(index) + vertex_offset;
            if adjusted < 0 || adjusted >= vertex_count as i64 {
                return Err(format!(
                    "index buffer references vertex {adjusted}, but only {vertex_count} vertices were parsed"
                ));
            }
            u32::try_from(adjusted).map_err(|_| "adjusted vertex index exceeds draw limits".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let first_index = u32::try_from(indices.len()).map_err(|_| "too many converted indices")?;
    let index_count = u32::try_from(triangles.len()).map_err(|_| "too many converted indices")?;
    if index_count == 0 {
        return Ok(());
    }
    indices.extend(triangles);
    submeshes.push(GeoSubmesh {
        first_index,
        index_count,
        material_index,
        texture_index,
        render_mode,
        alpha_cutoff,
        vertex_buffer_index,
        base_vertex,
        vertex_buffer_offset,
        visible: info.lod_level == 0
            && !is_auxiliary_node(&info.node_name)
            && !is_auxiliary_node(&info.material_name)
            && !is_auxiliary_node(&info.shader_name),
        node_name: info.node_name.clone(),
        material_name: info.material_name.clone(),
        shader_name: info.shader_name.clone(),
        lod_level: info.lod_level,
        triangle_count: index_count / 3,
        mirrored: false,
    });
    Ok(())
}

pub(crate) fn append_transformed_submesh(
    vertices: &mut Vec<GeoVertex>,
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    source_vertices: &[GeoVertex],
    transform: Mat4,
    material_index: u32,
    texture_index: u32,
    render_mode: GeoRenderMode,
    alpha_cutoff: f32,
    info: &BatchInfo,
    vertex_buffer_index: u32,
    base_vertex: i32,
    vertex_buffer_offset: i32,
) -> Result<(), String> {
    let vertex_offset = i64::from(base_vertex) + i64::from(vertex_buffer_offset);
    let normal_transform = if transform.determinant().abs() > f32::EPSILON {
        transform.inverse().transpose()
    } else {
        Mat4::IDENTITY
    };
    let mut remapped = HashMap::new();
    let mut transformed_indices = Vec::with_capacity(triangles.len());
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
            let mut vertex = source_vertices[source_index as usize];
            vertex.position = transform
                .transform_point3(Vec3::from_array(vertex.position))
                .to_array();
            vertex.normal = normal_transform
                .transform_vector3(Vec3::from_array(vertex.normal))
                .normalize_or_zero()
                .to_array();
            if vertex.position.iter().any(|value| !value.is_finite())
                || vertex.normal.iter().any(|value| !value.is_finite())
            {
                return Err("node transform produced non-finite vertex data".into());
            }
            let mapped =
                u32::try_from(vertices.len()).map_err(|_| "too many transformed vertices")?;
            vertices.push(vertex);
            remapped.insert(source_index, mapped);
            mapped
        };
        transformed_indices.push(transformed_index);
    }

    let first_index = u32::try_from(indices.len()).map_err(|_| "too many converted indices")?;
    let index_count =
        u32::try_from(transformed_indices.len()).map_err(|_| "too many converted indices")?;
    if index_count == 0 {
        return Ok(());
    }
    indices.extend(transformed_indices);
    submeshes.push(GeoSubmesh {
        first_index,
        index_count,
        material_index,
        texture_index,
        render_mode,
        alpha_cutoff,
        vertex_buffer_index,
        base_vertex,
        vertex_buffer_offset,
        visible: info.lod_level == 0
            && !is_auxiliary_node(&info.node_name)
            && !is_auxiliary_node(&info.material_name)
            && !is_auxiliary_node(&info.shader_name),
        node_name: info.node_name.clone(),
        material_name: info.material_name.clone(),
        shader_name: info.shader_name.clone(),
        lod_level: info.lod_level,
        triangle_count: index_count / 3,
        mirrored: transform.determinant() < 0.0,
    });
    Ok(())
}

pub(crate) fn find_blob(
    children: &[crate::serz::SerzNode],
    expected: &str,
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> Option<(Vec<u8>, String)> {
    fn find_any_blob(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
    ) -> Option<Vec<u8>> {
        if let SerzNode::Reference { target_id, .. } = node {
            if !reference_stack.insert(*target_id) {
                return None;
            }
            let found = ids
                .get(target_id)
                .and_then(|target| find_any_blob(target, ids, reference_stack));
            reference_stack.remove(target_id);
            return found;
        }
        match node {
            SerzNode::Element { name, .. }
                if name.to_ascii_lowercase().contains("chcvertexbuffer") =>
            {
                None
            }
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find_any_blob(child, ids, reference_stack)),
            SerzNode::Blob { data } => Some(data.clone()),
            _ => None,
        }
    }

    fn find_named_blob(
        node: &crate::serz::SerzNode,
        expected: &str,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
    ) -> Option<(Vec<u8>, String)> {
        match node {
            SerzNode::Reference { name, target_id } => {
                if !reference_stack.insert(*target_id) {
                    return None;
                }
                let result = ids.get(target_id).and_then(|target| {
                    if name.eq_ignore_ascii_case(expected) {
                        find_any_blob(target, ids, reference_stack).map(|data| (data, name.clone()))
                    } else {
                        find_named_blob(target, expected, ids, reference_stack)
                    }
                });
                reference_stack.remove(target_id);
                result
            }
            SerzNode::Element { name, children, .. } if name.eq_ignore_ascii_case(expected) => {
                children
                    .iter()
                    .find_map(|child| find_any_blob(child, ids, reference_stack))
                    .map(|data| (data, name.clone()))
            }
            SerzNode::Element { name, .. }
                if name.to_ascii_lowercase().contains("chcvertexbuffer") =>
            {
                None
            }
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find_named_blob(child, expected, ids, reference_stack)),
            _ => None,
        }
    }

    for child in children {
        match child {
            SerzNode::Element { name, children, .. } if name.eq_ignore_ascii_case(expected) => {
                if let Some(data) = children
                    .iter()
                    .find_map(|node| find_any_blob(node, ids, &mut HashSet::new()))
                {
                    return Some((data, name.clone()));
                }
            }
            SerzNode::Reference { name, target_id } if name.eq_ignore_ascii_case(expected) => {
                if let Some(data) = ids
                    .get(target_id)
                    .and_then(|target| find_any_blob(target, ids, &mut HashSet::new()))
                {
                    return Some((data, name.clone()));
                }
            }
            _ => {}
        }
    }
    if let Some(data) = children.iter().find_map(|child| match child {
        SerzNode::Blob { data } => Some(data.clone()),
        _ => None,
    }) {
        return Some((data, String::new()));
    }
    children
        .iter()
        .find_map(|child| find_named_blob(child, expected, ids, &mut HashSet::new()))
}

pub(crate) fn collect_vertex_buffers(
    node: &crate::serz::SerzNode,
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
    vertices: &mut Vec<VertexBuffer>,
) -> (HashMap<u32, u32>, HashMap<u32, u32>) {
    let mut vertex_buffer_id_map = HashMap::new();
    let mut prim_id_to_vbuf = HashMap::new();
    let mut vertex_data_counter = 0u32;

    fn collect(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        vertices: &mut Vec<VertexBuffer>,
        reference_stack: &mut HashSet<u32>,
        visited_elements: &mut HashSet<u32>,
        next_slot: &mut usize,
        vertex_buffer_id_map: &mut HashMap<u32, u32>,
        prim_id_to_vbuf: &mut HashMap<u32, u32>,
        ancestor_ids: &mut Vec<u32>,
        vertex_data_counter: &mut u32,
    ) {
        if let SerzNode::Reference { target_id, .. } = node {
            if reference_stack.insert(*target_id) {
                if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                    collect(
                        target,
                        ids,
                        vertices,
                        reference_stack,
                        visited_elements,
                        next_slot,
                        vertex_buffer_id_map,
                        prim_id_to_vbuf,
                        ancestor_ids,
                        vertex_data_counter,
                    );
                }
                reference_stack.remove(target_id);
            }
            return;
        }
        let SerzNode::Element {
            name, id, children, ..
        } = node
        else {
            return;
        };
        if *id != 0 && !visited_elements.insert(*id) {
            return;
        }
        let clean_name = name.replace("::", "-").to_ascii_lowercase();

        // Push this element's ID to ancestor stack if it's a container we care about
        let is_vertex_data_container =
            clean_name.contains("chcgeometryvertexdata") || clean_name.contains("chcvertexbuffer");
        let pushed = if is_vertex_data_container && *id != 0 {
            ancestor_ids.push(*id);
            true
        } else {
            false
        };

        // When we encounter a cHcGeometryVertexDataDx, collect all References inside it
        // that point to cHcPrimitiveSetDescDx nodes (primitive set descriptors).
        if clean_name.contains("chcgeometryvertexdata") {
            let buf_idx = *vertex_data_counter;
            *vertex_data_counter += 1;

            // Collect all SerzNode::Reference children inside this cHcGeometryVertexDataDx
            // (excluding references to the vertex buffer itself and vertex type declaration)
            fn collect_prim_refs(
                node: &crate::serz::SerzNode,
                ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
                reference_stack: &mut HashSet<u32>,
                vb_idx: u32,
                vb_id: u32,
                vtd_id: Option<u32>,
                prim_id_to_vbuf: &mut HashMap<u32, u32>,
            ) {
                match node {
                    SerzNode::Reference { target_id, .. } => {
                        // Skip references to the vertex buffer itself and vertex type declaration
                        if *target_id == vb_id || vtd_id.is_some_and(|vtd| vtd == *target_id) {
                            return;
                        }
                        prim_id_to_vbuf.insert(*target_id, vb_idx);
                    }
                    SerzNode::Element { children, .. } => {
                        for child in children {
                            collect_prim_refs(
                                child,
                                ids,
                                reference_stack,
                                vb_idx,
                                vb_id,
                                vtd_id,
                                prim_id_to_vbuf,
                            );
                        }
                    }
                    _ => {}
                }
            }

            // Find the vertex buffer ID and vertex type declaration ID to exclude them
            let vb_id = children
                .iter()
                .find_map(|c| {
                    if let SerzNode::Element { name, id, .. } = c {
                        if name.to_ascii_lowercase().contains("chcvertexbuffer") {
                            return Some(*id);
                        }
                    }
                    None
                })
                .unwrap_or(0);

            let vtd_id = children.iter().find_map(|c| {
                if let SerzNode::Element { name, id, .. } = c {
                    if name
                        .to_ascii_lowercase()
                        .contains("chcvertextypedeclaration")
                    {
                        return Some(*id);
                    }
                }
                None
            });

            let mut ref_stack = HashSet::new();
            for child in children {
                collect_prim_refs(
                    child,
                    ids,
                    &mut ref_stack,
                    buf_idx,
                    vb_id,
                    vtd_id,
                    prim_id_to_vbuf,
                );
            }
        }

        if clean_name.contains("chcvertexbuffer") {
            let source_index =
                find_direct_number(children, &["bufferindex", "vertexbufferindex"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(*next_slot);
            *next_slot += 1;
            // Record this vertex buffer's SERZ ID -> source_index mapping
            vertex_buffer_id_map.insert(*id, source_index as u32);
            // Also record ALL ancestor IDs in the stack (intermediate containers)
            for &ancestor_id in ancestor_ids.iter() {
                vertex_buffer_id_map.insert(ancestor_id, source_index as u32);
            }
            if let Some((bytes, _fields)) = find_blob(children, "vertexdata", ids) {
                let declared_vertex_count = find_direct_number(children, &["numvertices"], ids)
                    .and_then(|value| usize::try_from(value).ok());
                let fvf = find_text(children, &["fvf"], ids)
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                let (fvf_stride, fvf_normal_offset, fvf_uv_offset) = if fvf != 0 {
                    parse_fvf(fvf)
                } else {
                    (0, None, None)
                };
                let declaration = find_vertex_attribute_offsets(children, ids);
                // Вычисляем реальный шаг, просто отбрасывая мусорный паддинг в конце буфера
                let declared_stride = declared_vertex_count
                    .filter(|count| *count > 0 && bytes.len() >= *count)
                    .map(|count| bytes.len() / count);

                let explicit_stride = find_direct_number(
                    children,
                    &["vertexstride", "stride", "vertexsize", "vertexstridebytes"],
                    ids,
                )
                .and_then(|value| usize::try_from(value).ok());

                // Жесткий приоритет: 1. Явный размер из файла, 2. Вычисленный по кол-ву вершин, 3. Декларация
                let stride = explicit_stride
                    .filter(|s| *s > 0)
                    .or(declared_stride.filter(|s| *s > 0))
                    .or(Some(declaration.declaration_stride).filter(|s| *s > 0))
                    .or(Some(fvf_stride).filter(|s| *s > 0))
                    .unwrap_or(32);
                // An explicit `PositionOffset` wins when present; otherwise fall back to the
                // declaration's POSITION offset, then to zero.
                let position = find_direct_number(children, &["positionoffset"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or(declaration.position)
                    .unwrap_or(0);
                let position_format = declaration.position_format;
                let normal = find_direct_number(children, &["normaloffset"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or(declaration.normal)
                    .or(fvf_normal_offset)
                    .or_else(|| (stride >= 24).then_some(12));
                let uv = find_direct_number(children, &["texcoord0offset", "uvoffset"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or(declaration.uv)
                    .or(fvf_uv_offset)
                    .or_else(|| (stride >= 32).then_some(24));
                let layout = format!(
                    "VertexData/cHcGeometryVertexDataDx/VertexBuffer/cHcVertexBufferDx: vertices={} stride={} position={}({:?}) normal={:?} texcoord0={:?} declaration=[{}]",
                    declared_vertex_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "?".into()),
                    stride,
                    position,
                    position_format,
                    normal,
                    uv,
                    declaration.declaration,
                );
                vertices.push(VertexBuffer {
                    source_index,
                    bytes,
                    stride,
                    declared_vertex_count: declared_vertex_count.unwrap_or(0),
                    fvf,
                    position_offset: position,
                    position_format,
                    normal_offset: normal,
                    uv_offset: uv,
                    layout,
                });
            }
        }
        for child in children {
            collect(
                child,
                ids,
                vertices,
                reference_stack,
                visited_elements,
                next_slot,
                vertex_buffer_id_map,
                prim_id_to_vbuf,
                ancestor_ids,
                vertex_data_counter,
            );
        }
        // Pop ancestor ID when backtracking
        if pushed {
            ancestor_ids.pop();
        }
    }

    collect(
        node,
        ids,
        vertices,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut 0,
        &mut vertex_buffer_id_map,
        &mut prim_id_to_vbuf,
        &mut Vec::new(),
        &mut vertex_data_counter,
    );

    (vertex_buffer_id_map, prim_id_to_vbuf)
}

/// Collect index buffers (requires batches for vertex count inference).
pub(crate) fn collect_index_buffers(
    node: &crate::serz::SerzNode,
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
    batches: &[RawSubmesh],
    vertices: &[VertexBuffer],
    indices: &mut Vec<IndexBuffer>,
) {
    fn collect(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        batches: &[RawSubmesh],
        vertices: &[VertexBuffer],
        indices: &mut Vec<IndexBuffer>,
        reference_stack: &mut HashSet<u32>,
        visited_elements: &mut HashSet<u32>,
        next_slot: &mut usize,
    ) {
        if let SerzNode::Reference { target_id, .. } = node {
            if reference_stack.insert(*target_id) {
                if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                    collect(
                        target,
                        ids,
                        batches,
                        vertices,
                        indices,
                        reference_stack,
                        visited_elements,
                        next_slot,
                    );
                }
                reference_stack.remove(target_id);
            }
            return;
        }
        let SerzNode::Element {
            name, id, children, ..
        } = node
        else {
            return;
        };
        if *id != 0 && !visited_elements.insert(*id) {
            return;
        }
        let clean_name = name.replace("::", "-").to_ascii_lowercase();
        if clean_name.contains("chcindexbuffer") {
            let buffer_index =
                find_direct_number(children, &["bufferindex", "indexbufferindex"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(*next_slot);
            *next_slot += 1;
            let vertex_counts = vertices
                .iter()
                .map(|buffer| {
                    if buffer.stride == 0 {
                        0
                    } else {
                        buffer.bytes.len() / buffer.stride
                    }
                })
                .collect::<Vec<_>>();
            if let Some(index_buffer) =
                find_index_data(children, ids, buffer_index, batches, &vertex_counts)
            {
                indices.push(index_buffer);
            }
        }
        for child in children {
            collect(
                child,
                ids,
                batches,
                vertices,
                indices,
                reference_stack,
                visited_elements,
                next_slot,
            );
        }
    }

    collect(
        node,
        ids,
        batches,
        vertices,
        indices,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut 0,
    );
}

fn find_vertex_attribute_offsets(
    children: &[crate::serz::SerzNode],
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> VertexAttributeOffsets {
    let mut attributes = VertexAttributeOffsets {
        position: None,
        position_format: PositionFormat::default(),
        normal: None,
        tangent: None,
        uv: None,
        declaration_stride: 0,
        declaration: String::new(),
    };
    let mut stream_offsets = HashMap::new();
    let mut stream_sizes = HashMap::new();
    let mut declaration = Vec::new();

    // Recursively search for cHcVertexTypeDeclarationDx, resolving References via ids.
    // Stop recursion when hitting another cHcVertexBufferDx.
    fn process_declaration_node(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
        attributes: &mut crate::geo::parse::VertexAttributeOffsets,
        stream_offsets: &mut HashMap<u32, usize>,
        stream_sizes: &mut HashMap<u32, usize>,
        declaration: &mut Vec<String>,
    ) {
        match node {
            SerzNode::Reference { target_id, .. } => {
                if reference_stack.insert(*target_id) {
                    if let Some(target) = ids.get(target_id) {
                        process_declaration_node(
                            target,
                            ids,
                            reference_stack,
                            attributes,
                            stream_offsets,
                            stream_sizes,
                            declaration,
                        );
                    }
                    reference_stack.remove(target_id);
                }
            }
            SerzNode::Element { name, children, .. } => {
                let clean_name = name.replace("::", "-").to_ascii_lowercase();
                // Stop if we hit another vertex buffer (sibling or nested)
                if clean_name.contains("chcvertexbuffer") {
                    return;
                }
                // If this is the declaration node, process its ElementDecl children
                if clean_name.contains("chcvertextypedeclaration") {
                    for decl_child in children {
                        let SerzNode::Element {
                            name: decl_name,
                            children: element_children,
                            ..
                        } = decl_child
                        else {
                            continue;
                        };
                        if !decl_name.to_ascii_lowercase().contains("elementdecl") {
                            continue;
                        }
                        let usage =
                            direct_text(element_children, &["usage"], ids).unwrap_or_default();
                        let type_name =
                            direct_text(element_children, &["type"], ids).unwrap_or_default();
                        let stream_index =
                            find_direct_number(element_children, &["streamindex"], ids)
                                .and_then(|v| u32::try_from(v).ok())
                                .unwrap_or(0);
                        let explicit_offset =
                            find_direct_number(element_children, &["offset", "byteoffset"], ids)
                                .and_then(|value| usize::try_from(value).ok());
                        let offset = explicit_offset
                            .unwrap_or_else(|| *stream_offsets.entry(stream_index).or_default());
                        let size = vertex_element_size(&type_name);
                        if let Some(size) = size {
                            stream_offsets.insert(stream_index, offset.saturating_add(size));
                            let stream_size: &mut usize =
                                stream_sizes.entry(stream_index).or_default();
                            *stream_size = stream_size.saturating_add(size);
                        }
                        if stream_index == 0 {
                            let usage_lower = usage.to_ascii_lowercase();
                            if usage_lower.contains("position") && attributes.position.is_none() {
                                attributes.position = Some(offset);
                                attributes.position_format = position_format_from_type(&type_name);
                            } else if usage_lower.contains("normal") && attributes.normal.is_none()
                            {
                                attributes.normal = Some(offset);
                            } else if usage_lower.contains("tangent")
                                && attributes.tangent.is_none()
                            {
                                attributes.tangent = Some(offset);
                            } else if usage_lower.contains("texcoord")
                                && find_direct_number(element_children, &["usageindex"], ids)
                                    .unwrap_or(0)
                                    == 0
                                && attributes.uv.is_none()
                            {
                                attributes.uv = Some(offset);
                            }
                        }
                        declaration.push(format!(
                            "{}:{}@stream{}+{}",
                            usage, type_name, stream_index, offset
                        ));
                    }
                    // Found declaration, don't recurse further into this branch
                    return;
                }
                // Otherwise, recurse into children (e.g., VertexType, Declaration containers)
                for child in children {
                    process_declaration_node(
                        child,
                        ids,
                        reference_stack,
                        attributes,
                        stream_offsets,
                        stream_sizes,
                        declaration,
                    );
                }
            }
            _ => {}
        }
    }

    let mut reference_stack = HashSet::new();
    for child in children {
        process_declaration_node(
            child,
            ids,
            &mut reference_stack,
            &mut attributes,
            &mut stream_offsets,
            &mut stream_sizes,
            &mut declaration,
        );
    }

    attributes.declaration_stride = stream_sizes.get(&0).copied().unwrap_or(0);
    attributes.declaration = declaration.join(", ");
    attributes
}

#[derive(Clone, Debug)]
pub(crate) struct VertexAttributeOffsets {
    pub position: Option<usize>,
    pub position_format: PositionFormat,
    pub normal: Option<usize>,
    pub tangent: Option<usize>,
    pub uv: Option<usize>,
    pub declaration_stride: usize,
    pub declaration: String,
}

pub(crate) fn find_material_bindings(
    node: &crate::serz::SerzNode,
    textures: &[String],
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> std::collections::HashMap<u32, MaterialBinding> {
    fn collect_delta_strings(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
        output: &mut Vec<String>,
    ) {
        match node {
            SerzNode::Reference { target_id, .. } => {
                if reference_stack.insert(*target_id) {
                    if let Some(target) = ids.get(target_id) {
                        collect_delta_strings(target, ids, reference_stack, output);
                    }
                    reference_stack.remove(target_id);
                }
            }
            SerzNode::Property {
                name,
                type_name,
                value,
                ..
            } if type_name.eq_ignore_ascii_case("cDeltaString")
                && !["shadername", "effectname"]
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                output.push(value.trim().to_owned());
            }
            SerzNode::Array {
                name,
                type_name,
                values,
            } if type_name.eq_ignore_ascii_case("cDeltaString")
                && !["shadername", "effectname"]
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                output.extend(values.iter().map(|value| value.trim().to_owned()));
            }
            SerzNode::Element { name, children, .. }
                if !["shadername", "effectname"]
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                for child in children {
                    collect_delta_strings(child, ids, reference_stack, output);
                }
            }
            _ => {}
        }
    }

    fn collect(
        node: &crate::serz::SerzNode,
        textures: &[String],
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        next_index: &mut u32,
        bindings: &mut std::collections::HashMap<u32, MaterialBinding>,
        visited_elements: &mut HashSet<u32>,
        reference_stack: &mut HashSet<u32>,
    ) {
        if let SerzNode::Reference { target_id, .. } = node {
            if reference_stack.insert(*target_id) {
                if let Some(target) = ids.get(target_id) {
                    collect(
                        target,
                        textures,
                        ids,
                        next_index,
                        bindings,
                        visited_elements,
                        reference_stack,
                    );
                }
                reference_stack.remove(target_id);
            }
            return;
        }
        let SerzNode::Element {
            name, id, children, ..
        } = node
        else {
            return;
        };
        if *id != 0 && !visited_elements.insert(*id) {
            return;
        }
        if name.eq_ignore_ascii_case("cHcEffectMaterialDx")
            || name.eq_ignore_ascii_case("cHcExplicitStateMaterial")
        {
            let mut texture_references = collect_text_values(children, &["texturebyname"], ids);
            let mut delta_strings = Vec::new();
            let mut reference_stack = HashSet::new();
            for child in children {
                collect_delta_strings(child, ids, &mut reference_stack, &mut delta_strings);
            }
            texture_references.extend(delta_strings.into_iter().filter(|value| {
                value.contains('\\')
                    || value.contains('/')
                    || value.to_ascii_lowercase().contains(".tgpcdx")
            }));
            let shader_name =
                find_text(children, &["shadername", "effectname"], ids).unwrap_or_default();
            let texture_is_shadow = texture_references
                .iter()
                .any(|texture| texture.to_ascii_lowercase().contains("shadow"));
            let frame_alpha_mode = find_text(children, &["framealphamode"], ids)
                .unwrap_or_default()
                .trim()
                .to_ascii_uppercase();
            let subtract_alpha = frame_alpha_mode == "ALPHA_MODE_SUBTRACT";
            // Invisible/shadow/collision materials are stripped from the texture table by
            // `auditor::filter_material_blocks`, so they must not consume a `material_index`
            // slot here, otherwise `material_index` desynchronizes from `textures` and
            // visible batches fall back to the wrong texture index.
            let excluded =
                is_hidden_shader_name(&shader_name) || texture_is_shadow || subtract_alpha;
            let material_index = *next_index;
            // Always advance the index counter so excluded materials (shadows,
            // subtractive, hidden) still consume a slot and material_index stays
            // synchronized with the full material table (0..N). The `excluded`
            // flag only marks the batch as non-renderable.
            *next_index = (*next_index).saturating_add(1);
            let texture_index = texture_references
                .iter()
                .filter_map(|reference| {
                    let reference = crate::auditor::normalize_texture_reference(reference);
                    textures
                        .iter()
                        .position(|texture| texture.eq_ignore_ascii_case(&reference))
                        .map(|index| (index, crate::geo::texture::texture_priority(&reference)))
                })
                .min_by_key(|(_, priority)| *priority)
                .map(|(index, _)| index)
                .unwrap_or(0) as u32;
            let material_name =
                find_text(children, &["name", "materialname"], ids).unwrap_or_default();
            let display_material_name = {
                let trimmed = material_name.trim();
                if trimmed.is_empty() {
                    "default".to_owned()
                } else {
                    trimmed.to_owned()
                }
            };
            let frame_alpha_mode = find_text(children, &["framealphamode"], ids)
                .unwrap_or_default()
                .trim()
                .to_ascii_uppercase();
            let alpha_test_type = find_text(children, &["alphatesttype"], ids)
                .unwrap_or_default()
                .trim()
                .to_ascii_uppercase();
            // In Train Simulator, the diffuse alpha channel is a specular/environment mask,
            // NOT transparency. Alpha test is ONLY enabled when AlphaTestType explicitly
            // requests a comparison (GREATER, GREATEREQUAL, LESS, LESSEQUAL, NOTEQUAL).
            // HC_ALPHATEST_ALWAYS means "always pass" (test disabled). ALPHA_MODE_NONE
            // means no alpha processing. Both force Opaque regardless of AlphaRef.
            let alpha_test_enabled = matches!(
                alpha_test_type.as_str(),
                "HC_ALPHATEST_GREATER"
                    | "HC_ALPHATEST_GREATEREQUAL"
                    | "HC_ALPHATEST_LESS"
                    | "HC_ALPHATEST_LESSEQUAL"
                    | "HC_ALPHATEST_NOTEQUAL"
            );
            let alpha_ref = find_text(children, &["alpharef"], ids)
                .and_then(|value| value.trim().parse::<f32>().ok())
                .map(|value| (value / 255.0).clamp(0.0, 1.0))
                .unwrap_or(0.5);
            let mut render_mode = classify_shader(&shader_name);
            // Shader classification gives a baseline. Material settings can only
            // *upgrade* Opaque -> Cutout/Blended, never downgrade Cutout/Blended -> Opaque.
            if render_mode == GeoRenderMode::Opaque {
                render_mode = match frame_alpha_mode.as_str() {
                    "ALPHA_MODE_BLEND" | "ALPHA_MODE_ADD" => GeoRenderMode::Blended,
                    "ALPHA_MODE_TEST" => GeoRenderMode::Cutout,
                    _ if alpha_test_enabled => GeoRenderMode::Cutout,
                    _ => GeoRenderMode::Opaque,
                };
            }
            // sunblend is a semi-transparent sun visor material in the cab. In TS it's defined
            // by material Name="sunblend" with shader TrainBasicObjectDiffuse.fx, so we must
            // check material_name, not shader_name.
            if display_material_name
                .to_ascii_lowercase()
                .contains("sunblend")
            {
                render_mode = GeoRenderMode::Blended;
            }
            bindings.insert(
                *id,
                MaterialBinding {
                    material_index,
                    texture_index,
                    render_mode,
                    excluded,
                    material_name: display_material_name,
                    shader_name: shader_name.trim().to_owned(),
                    alpha_ref,
                },
            );
        }
        for child in children {
            collect(
                child,
                textures,
                ids,
                next_index,
                bindings,
                visited_elements,
                reference_stack,
            );
        }
    }

    let mut bindings = HashMap::new();
    let mut next_index = 0;
    collect(
        node,
        textures,
        ids,
        &mut next_index,
        &mut bindings,
        &mut HashSet::new(),
        &mut HashSet::new(),
    );
    bindings
}

fn collect_text_values(
    children: &[crate::serz::SerzNode],
    expected: &[&str],
    ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
) -> Vec<String> {
    fn collect(
        node: &crate::serz::SerzNode,
        expected: &[&str],
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
        output: &mut Vec<String>,
    ) {
        match node {
            SerzNode::Property { name, value, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                output.push(value.trim().to_owned());
            }
            SerzNode::Array { name, values, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                output.extend(values.iter().map(|value| value.trim().to_owned()));
            }
            SerzNode::Reference { name, target_id } => {
                if reference_stack.insert(*target_id) {
                    if let Some(target) = ids.get(target_id) {
                        if expected
                            .iter()
                            .any(|candidate| name.eq_ignore_ascii_case(candidate))
                        {
                            collect_descendant_text(target, ids, reference_stack, output);
                        } else {
                            collect(target, expected, ids, reference_stack, output);
                        }
                    }
                    reference_stack.remove(target_id);
                }
            }
            SerzNode::Element { name, children, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                for child in children {
                    collect_descendant_text(child, ids, reference_stack, output);
                }
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect(child, expected, ids, reference_stack, output);
                }
            }
            _ => {}
        }
    }

    fn collect_descendant_text(
        node: &crate::serz::SerzNode,
        ids: &std::collections::HashMap<u32, &crate::serz::SerzNode>,
        reference_stack: &mut HashSet<u32>,
        output: &mut Vec<String>,
    ) {
        match node {
            SerzNode::Property { value, .. } => output.push(value.trim().to_owned()),
            SerzNode::Array { values, .. } => {
                output.extend(values.iter().map(|value| value.trim().to_owned()));
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_descendant_text(child, ids, reference_stack, output);
                }
            }
            SerzNode::Reference { target_id, .. } => {
                if reference_stack.insert(*target_id) {
                    if let Some(target) = ids.get(target_id) {
                        collect_descendant_text(target, ids, reference_stack, output);
                    }
                    reference_stack.remove(target_id);
                }
            }
            _ => {}
        }
    }

    let mut values = Vec::new();
    for child in children {
        collect(child, expected, ids, &mut HashSet::new(), &mut values);
    }
    values
}
