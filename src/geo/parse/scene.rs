//! Scene graph traversal, batch extraction, deduplication, and implicit vertex buffer resolution.

use std::collections::{HashMap, HashSet};

use glam::Mat4;

use crate::common::serz::SerzNode;
use crate::geo::helpers::{
    collect_float_values, direct_text, find_direct_number, find_number, find_reference, find_text,
    find_transform_id, find_vbuf_ref_in_subtree, matrix_from_values, parse_float_values,
    resolve_ref_node, resolve_ref_text,
};
use crate::geo::texture::{lod_from_name, lod_name_is_nonzero, node_is_explicitly_hidden};
use crate::geo::types::{GeoRenderMode, PrimitiveTopology};

use super::{MaterialBinding, RawSubmesh, VertexBuffer, contains_ignore_ascii_case};

#[derive(Clone, Debug)]
pub(crate) struct NodeContext<'a> {
    pub material_bindings: &'a HashMap<u32, MaterialBinding>,
    pub ids: &'a HashMap<u32, &'a SerzNode>,
    pub world_transforms: &'a [Mat4],
    pub transform_names: &'a [String],
    pub vb_id_to_index: &'a HashMap<u32, u32>,
    pub prim_id_to_vbuf: &'a HashMap<u32, u32>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TraversalState<'a> {
    pub(crate) material: Option<u32>,
    pub(crate) transform: Mat4,
    pub(crate) transform_id: Option<usize>,
    pub(crate) lod: Option<u32>,
    pub(crate) excluded: bool,
    pub(crate) node_name: Option<&'a str>,
    pub(crate) vertex_buffer_index: Option<u32>,
}

fn parse_i32_trimmed(value: &str) -> Option<i32> {
    let trimmed = value.trim();
    trimmed
        .parse::<i32>()
        .ok()
        .or_else(|| trimmed.parse::<u32>().ok().map(|v| v as i32))
        .or_else(|| trimmed.parse::<i64>().ok().map(|v| v as i32))
}

pub(crate) fn find_submeshes(
    node: &SerzNode,
    context: &NodeContext<'_>,
    state: TraversalState<'_>,
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
    } = context;
    let TraversalState {
        material: inherited_material,
        transform: parent_transform,
        transform_id: inherited_transform_id,
        lod: inherited_lod,
        excluded: inherited_excluded,
        node_name: inherited_node_name,
        vertex_buffer_index: inherited_vertex_buffer_index,
    } = state;

    if let SerzNode::Reference { name, target_id } = node {
        if name.eq_ignore_ascii_case("material") {
            return;
        }
        if reference_stack.insert(*target_id) {
            if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                find_submeshes(target, context, state, reference_stack, output);
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
    if contains_ignore_ascii_case(name, "collisionroot") {
        return;
    }

    let node_label =
        direct_text(children, &["name", "nodename", "transformname"], ids).unwrap_or_default();
    let has_lower_lod_name = lod_name_is_nonzero(name)
        || ["name", "nodename", "transformname"]
            .iter()
            .filter_map(|field| direct_text(children, &[*field], ids))
            .any(lod_name_is_nonzero);
    let lod = lod_from_name(name)
        .or_else(|| lod_from_name(node_label))
        .or_else(|| {
            contains_ignore_ascii_case(name, "lod")
                .then(|| find_number(children, &["lodlevel", "lodindex", "level"], ids))
                .flatten()
                .and_then(|level| u32::try_from(level).ok())
        })
        .or(inherited_lod);
    let material_reference = if contains_ignore_ascii_case(name, "sgnodematerial") {
        find_reference(children, "material").or(inherited_material)
    } else {
        inherited_material
    };
    let binding = material_reference.and_then(|reference| material_bindings.get(&reference));
    let excluded = inherited_excluded
        || lod.is_some_and(|level| level > 0)
        || has_lower_lod_name
        || node_is_explicitly_hidden(children, ids)
        || binding.is_some_and(|b| b.excluded);

    let child_transform_id = find_transform_id(children, ids);
    let is_same_state = contains_ignore_ascii_case(name, "geometrynodesamestate");
    let inherits_parent_transform = is_same_state
        && inherited_transform_id.is_some()
        && direct_text(children, &["primarytransformid", "primarytransformids"], ids)
            .and_then(parse_i32_trimmed)
            == Some(0)
        && direct_text(children, &["transformid", "transformids"], ids).is_none();
    let active_transform_id = if inherits_parent_transform {
        inherited_transform_id
    } else {
        child_transform_id.or(inherited_transform_id)
    };
    let transform = parent_transform * find_local_transform(children);

    let own_label = node_label.trim();
    let inherited_label = inherited_node_name
        .map(str::trim)
        .filter(|label| !label.is_empty());
    let transform_name = active_transform_id
        .and_then(|transform_id| transform_names.get(transform_id))
        .map(|label| label.trim())
        .filter(|label| !label.is_empty());
    let resolved_name = own_label
        .is_empty()
        .then_some(inherited_label)
        .flatten()
        .or(transform_name);
    let effective_node_name = match resolved_name {
        Some(label) if !label.starts_with("Node #") => Some(label),
        _ => inherited_node_name,
    };

    let is_primitive_set = contains_ignore_ascii_case(name, "primitivesetdescdx");
    if is_primitive_set
        || contains_ignore_ascii_case(name, "primlist")
        || contains_ignore_ascii_case(name, "primitivelist")
        || contains_ignore_ascii_case(name, "batch")
        || contains_ignore_ascii_case(name, "primitive set desc")
    {
        let first = find_number(
            children,
            &["firstindex", "startindex", "indexstart", "indexarraystart"],
            ids,
        );
        let topology = find_primitive_topology(node, ids);
        // RailWorks stores `NumPrimitives` as a raw index count on primitive-set descriptors,
        // but as a primitive count on legacy list nodes.
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
                .or_else(|| binding.map(|b| b.material_index));
            let texture_index = binding.map(|b| b.texture_index);
            let base_vertex = find_text(
                children,
                &["vertexindexbase", "vertexindexbases", "basevertex"],
                ids,
            )
            .and_then(parse_i32_trimmed)
            .unwrap_or(0);

            let vertex_index_min = find_number(children, &["vertexindexmin"], ids).unwrap_or(0);
            let vertex_index_range = find_number(children, &["vertexindexrange"], ids).unwrap_or(0);
            let max_local_vertex_ref = vertex_index_min.checked_add(vertex_index_range);

            let vertex_buffer_index = if is_primitive_set {
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
                children.iter().find_map(|child| match child {
                    SerzNode::Reference { target_id, .. } => vb_id_to_index.get(target_id).copied(),
                    _ => None,
                })
            })
            .or_else(|| {
                find_reference(children, "vertexbuffer").and_then(|target_id| {
                    vb_id_to_index.get(&target_id).copied().or_else(|| {
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
            .or(inherited_vertex_buffer_index)
            .unwrap_or(0);
            let index_buffer_index = find_number(children, &["indexbufferindex"], ids)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);

            let batch_node_name = resolved_name.map(str::to_owned).unwrap_or_else(|| {
                format!(
                    "Node #{}",
                    active_transform_id.unwrap_or_else(|| usize::try_from(*id).unwrap_or(0))
                )
            });

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
                    .map(|tid| world_transforms.get(tid).copied().unwrap_or(Mat4::IDENTITY))
                    .unwrap_or(transform),
                excluded,
                node_name: batch_node_name,
                material_name: binding
                    .map(|b| b.material_name.clone())
                    .unwrap_or_else(|| "default".into()),
                shader_name: binding.map(|b| b.shader_name.clone()).unwrap_or_default(),
                render_mode: binding.map(|b| b.render_mode).unwrap_or_default(),
                lod_level: u8::try_from(lod.unwrap_or(0)).unwrap_or(u8::MAX),
            });
        }
    }

    let is_lod_container = contains_ignore_ascii_case(name, "lodlist")
        || contains_ignore_ascii_case(name, "chcsgnodelodselector");

    // Propagate vertex buffer index from parent containers.
    let mut next_inherited_vbuf = inherited_vertex_buffer_index;
    if is_same_state || contains_ignore_ascii_case(name, "chcgeometryvertexdata") {
        if let Some(vbuf_idx) = find_number(
            children,
            &["vertexbufferindex", "vertexdataindex", "bufferindex"],
            ids,
        )
        .and_then(|value| u32::try_from(value).ok())
        {
            next_inherited_vbuf = Some(vbuf_idx);
        } else {
            next_inherited_vbuf =
                find_vbuf_ref_in_subtree(children, vb_id_to_index).or(next_inherited_vbuf);
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
            context,
            TraversalState {
                material: material_reference,
                transform,
                transform_id: active_transform_id,
                lod: child_lod.or(lod),
                excluded,
                node_name: effective_node_name,
                vertex_buffer_index: next_inherited_vbuf,
            },
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

pub(crate) fn find_local_transform(children: &[SerzNode]) -> Mat4 {
    for child in children {
        match child {
            SerzNode::Property { name, value, .. }
                if contains_ignore_ascii_case(name, "transform") =>
            {
                if let Some(matrix) = matrix_from_values(&parse_float_values(value)) {
                    return matrix;
                }
            }
            SerzNode::Element { name, .. } if contains_ignore_ascii_case(name, "transform") => {
                if let Some(matrix) = matrix_from_values(&collect_float_values(child)) {
                    return matrix;
                }
            }
            _ => {}
        }
    }
    Mat4::IDENTITY
}

pub(super) fn resolve_implicit_vertex_buffers(
    batches: &mut [RawSubmesh],
    vertex_data: &[VertexBuffer],
) {
    if vertex_data.len() <= 1 || batches.iter().any(|batch| batch.vertex_buffer_index != 0) {
        return;
    }

    let vb_sizes: Vec<usize> = vertex_data
        .iter()
        .map(|buffer| buffer.bytes.len().checked_div(buffer.stride).unwrap_or(0))
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
        let shader_trimmed = batch.shader_name.trim();
        let shader_key = (!shader_trimmed.is_empty()).then(|| shader_trimmed.to_ascii_lowercase());
        let shader_slot = shader_key
            .as_deref()
            .and_then(|key| shader_to_slot.get(key).copied());

        let chosen_slot = if base > 0 {
            let mut first_exact = None;
            let mut exact_count = 0usize;
            let mut mat_exact = false;
            let mut shader_exact = false;
            for slot in 0..vertex_data.len() {
                if started[slot] && cursors[slot] == base && fits(slot) {
                    if first_exact.is_none() {
                        first_exact = Some(slot);
                    }
                    exact_count += 1;
                    if mat_slot == Some(slot) {
                        mat_exact = true;
                    }
                    if shader_slot == Some(slot) {
                        shader_exact = true;
                    }
                }
            }

            if exact_count == 1 {
                first_exact.unwrap_or(0)
            } else if exact_count > 1 {
                if mat_exact {
                    mat_slot.unwrap_or(0)
                } else if shader_exact {
                    shader_slot.unwrap_or(0)
                } else {
                    first_exact.unwrap_or(0)
                }
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
        if let Some(key) = shader_key {
            shader_to_slot.entry(key).or_insert(chosen_slot);
        }
        batch.vertex_buffer_index = vertex_data[chosen_slot].source_index as u32;
    }
}

pub(crate) fn find_primitive_topology(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
) -> PrimitiveTopology {
    fn find(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
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
                if contains_ignore_ascii_case(name, "trianglestrip") {
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
    if contains_ignore_ascii_case(value, "strip") {
        PrimitiveTopology::TriangleStrip
    } else {
        PrimitiveTopology::TriangleList
    }
}