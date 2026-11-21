//! Vertex and index buffer extraction and vertex attribute declaration parsing.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use crate::common::serz::SerzNode;
use crate::geo::helpers::{
    decode_indices, direct_text, find_direct_number, find_number, find_text, parse_fvf,
    position_format_from_type, resolve_ref_node, vertex_element_size,
};
use crate::geo::vertex::PositionFormat;

use super::{IndexBuffer, RawSubmesh, VertexBuffer, contains_ignore_ascii_case};

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

pub(crate) fn collect_vertex_buffers(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    vertices: &mut Vec<VertexBuffer>,
) -> (HashMap<u32, u32>, HashMap<u32, u32>) {
    struct Collector<'a> {
        ids: &'a HashMap<u32, &'a SerzNode>,
        vertices: &'a mut Vec<VertexBuffer>,
        reference_stack: HashSet<u32>,
        resolve_stack: HashSet<u32>,
        visited_elements: HashSet<u32>,
        next_slot: usize,
        vertex_buffer_id_map: HashMap<u32, u32>,
        prim_id_to_vbuf: HashMap<u32, u32>,
        ancestor_ids: Vec<u32>,
        vertex_data_counter: u32,
    }

    impl<'a> Collector<'a> {
        fn collect_prim_refs(
            &mut self,
            node: &SerzNode,
            vb_idx: u32,
            vb_id: u32,
            vtd_id: Option<u32>,
        ) {
            match node {
                SerzNode::Reference { target_id, .. } => {
                    if *target_id == vb_id || vtd_id.is_some_and(|vtd| vtd == *target_id) {
                        return;
                    }
                    self.prim_id_to_vbuf.insert(*target_id, vb_idx);
                }
                SerzNode::Element { children, .. } => {
                    for child in children {
                        self.collect_prim_refs(child, vb_idx, vb_id, vtd_id);
                    }
                }
                _ => {}
            }
        }

        fn collect(&mut self, node: &SerzNode) {
            if let SerzNode::Reference { target_id, .. } = node {
                if self.reference_stack.insert(*target_id) {
                    self.resolve_stack.clear();
                    if let Some(target) = resolve_ref_node(node, self.ids, &mut self.resolve_stack)
                    {
                        self.collect(target);
                    }
                    self.reference_stack.remove(target_id);
                }
                return;
            }
            let SerzNode::Element {
                name, id, children, ..
            } = node
            else {
                return;
            };
            if *id != 0 && !self.visited_elements.insert(*id) {
                return;
            }

            let is_geometry_vertex_data =
                contains_ignore_ascii_case(name, "chcgeometryvertexdata");
            let is_vertex_buffer = contains_ignore_ascii_case(name, "chcvertexbuffer");
            let pushed = if (is_geometry_vertex_data || is_vertex_buffer) && *id != 0 {
                self.ancestor_ids.push(*id);
                true
            } else {
                false
            };

            if is_geometry_vertex_data {
                let buf_idx = self.vertex_data_counter;
                self.vertex_data_counter += 1;

                let vb_id = children
                    .iter()
                    .find_map(|c| match c {
                        SerzNode::Element { name, id, .. }
                            if contains_ignore_ascii_case(name, "chcvertexbuffer") =>
                        {
                            Some(*id)
                        }
                        _ => None,
                    })
                    .unwrap_or(0);

                let vtd_id = children.iter().find_map(|c| match c {
                    SerzNode::Element { name, id, .. }
                        if contains_ignore_ascii_case(name, "chcvertextypedeclaration") =>
                    {
                        Some(*id)
                    }
                    _ => None,
                });

                for child in children {
                    self.collect_prim_refs(child, buf_idx, vb_id, vtd_id);
                }
            }

            if is_vertex_buffer {
                let source_index =
                    find_direct_number(children, &["bufferindex", "vertexbufferindex"], self.ids)
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(self.next_slot);
                self.next_slot += 1;
                self.vertex_buffer_id_map.insert(*id, source_index as u32);
                for &ancestor_id in &self.ancestor_ids {
                    self.vertex_buffer_id_map
                        .insert(ancestor_id, source_index as u32);
                }
                if let Some((bytes, _)) = find_blob(children, "vertexdata", self.ids) {
                    let declared_vertex_count =
                        find_direct_number(children, &["numvertices"], self.ids)
                            .and_then(|value| usize::try_from(value).ok());
                    let fvf = find_text(children, &["fvf"], self.ids)
                        .and_then(|value| value.trim().parse::<u32>().ok())
                        .unwrap_or(0);
                    let (fvf_stride, fvf_normal_offset, fvf_uv_offset) = if fvf != 0 {
                        parse_fvf(fvf)
                    } else {
                        (0, None, None)
                    };
                    let declaration = find_vertex_attribute_offsets(children, self.ids);
                    let declared_stride = declared_vertex_count
                        .filter(|&count| count > 0 && bytes.len() >= count)
                        .map(|count| bytes.len() / count);
                    let explicit_stride = find_direct_number(
                        children,
                        &["vertexstride", "stride", "vertexsize", "vertexstridebytes"],
                        self.ids,
                    )
                    .and_then(|value| usize::try_from(value).ok());

                    let stride = explicit_stride
                        .filter(|&s| s > 0)
                        .or(declared_stride.filter(|&s| s > 0))
                        .or(Some(declaration.declaration_stride).filter(|&s| s > 0))
                        .or(Some(fvf_stride).filter(|&s| s > 0))
                        .unwrap_or(32);
                    let position = find_direct_number(children, &["positionoffset"], self.ids)
                        .and_then(|value| usize::try_from(value).ok())
                        .or(declaration.position)
                        .unwrap_or(0);
                    let position_format = declaration.position_format;
                    let normal = find_direct_number(children, &["normaloffset"], self.ids)
                        .and_then(|value| usize::try_from(value).ok())
                        .or(declaration.normal)
                        .or(fvf_normal_offset)
                        .or_else(|| (stride >= 24).then_some(12));
                    let uv =
                        find_direct_number(children, &["texcoord0offset", "uvoffset"], self.ids)
                            .and_then(|value| usize::try_from(value).ok())
                            .or(declaration.uv)
                            .or(fvf_uv_offset)
                            .or_else(|| (stride >= 32).then_some(24));

                    let mut layout = String::with_capacity(128 + declaration.declaration.len());
                    let _ = write!(
                        layout,
                        "VertexData/cHcGeometryVertexDataDx/VertexBuffer/cHcVertexBufferDx: vertices="
                    );
                    match declared_vertex_count {
                        Some(count) => {
                            let _ = write!(layout, "{count}");
                        }
                        None => layout.push('?'),
                    }
                    let _ = write!(
                        layout,
                        " stride={stride} position={position}({position_format:?}) normal={normal:?} texcoord0={uv:?} declaration=[{}]",
                        declaration.declaration,
                    );

                    self.vertices.push(VertexBuffer {
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
                self.collect(child);
            }
            if pushed {
                self.ancestor_ids.pop();
            }
        }
    }

    let mut collector = Collector {
        ids,
        vertices,
        reference_stack: HashSet::new(),
        resolve_stack: HashSet::new(),
        visited_elements: HashSet::new(),
        next_slot: 0,
        vertex_buffer_id_map: HashMap::new(),
        prim_id_to_vbuf: HashMap::new(),
        ancestor_ids: Vec::new(),
        vertex_data_counter: 0,
    };
    collector.collect(node);

    (collector.vertex_buffer_id_map, collector.prim_id_to_vbuf)
}

/// Collect index buffers (requires batches for u16/u32 width inference).
pub(crate) fn collect_index_buffers(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    batches: &[RawSubmesh],
    _vertices: &[VertexBuffer],
    indices: &mut Vec<IndexBuffer>,
) {
    struct Collector<'a> {
        ids: &'a HashMap<u32, &'a SerzNode>,
        batches: &'a [RawSubmesh],
        indices: &'a mut Vec<IndexBuffer>,
        reference_stack: HashSet<u32>,
        resolve_stack: HashSet<u32>,
        visited_elements: HashSet<u32>,
        next_slot: usize,
    }

    impl<'a> Collector<'a> {
        fn collect(&mut self, node: &SerzNode) {
            if let SerzNode::Reference { target_id, .. } = node {
                if self.reference_stack.insert(*target_id) {
                    self.resolve_stack.clear();
                    if let Some(target) = resolve_ref_node(node, self.ids, &mut self.resolve_stack)
                    {
                        self.collect(target);
                    }
                    self.reference_stack.remove(target_id);
                }
                return;
            }
            let SerzNode::Element {
                name, id, children, ..
            } = node
            else {
                return;
            };
            if *id != 0 && !self.visited_elements.insert(*id) {
                return;
            }
            if contains_ignore_ascii_case(name, "chcindexbuffer") {
                let buffer_index =
                    find_direct_number(children, &["bufferindex", "indexbufferindex"], self.ids)
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(self.next_slot);
                self.next_slot += 1;
                if let Some(index_buffer) =
                    find_index_data(children, self.ids, buffer_index, self.batches, &[])
                {
                    self.indices.push(index_buffer);
                }
            }
            for child in children {
                self.collect(child);
            }
        }
    }

    let mut collector = Collector {
        ids,
        batches,
        indices,
        reference_stack: HashSet::new(),
        resolve_stack: HashSet::new(),
        visited_elements: HashSet::new(),
        next_slot: 0,
    };
    collector.collect(node);
}

pub(crate) fn find_index_data(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
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

    let mut max_index_end = 0u64;
    let mut max_local_vertex_ref = 0u64;
    for batch in batches {
        if batch.index_buffer_index as usize != index_buffer_index {
            continue;
        }
        if let Some(end) = u64::from(batch.first_index).checked_add(u64::from(batch.index_count)) {
            max_index_end = max_index_end.max(end);
        }
        if let Some(local_ref) = batch.max_local_vertex_ref {
            max_local_vertex_ref = max_local_vertex_ref.max(local_ref);
        }
    }

    let inferred_u32 = max_index_end > 0 && {
        let byte_len = bytes.len() as u64;
        let exact_u32 =
            max_index_end.checked_mul(4) == Some(byte_len) && max_index_end.checked_mul(2) != Some(byte_len);
        let local_exceeds_u16 = max_local_vertex_ref > u64::from(u16::MAX);
        let enough_for_u32 = max_index_end
            .checked_mul(4)
            .is_some_and(|minimum_bytes| byte_len >= minimum_bytes);
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
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<(Vec<u32>, String)> {
    fn find(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
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
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find(child, ids, reference_stack)),
            _ => None,
        }
    }

    let mut reference_stack = HashSet::new();
    children
        .iter()
        .find_map(|child| find(child, ids, &mut reference_stack))
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
    if contains_ignore_ascii_case(type_name, "32") {
        Some(4)
    } else if contains_ignore_ascii_case(type_name, "16") {
        Some(2)
    } else {
        None
    }
}

pub(crate) fn type_name_from_width(width: usize) -> &'static str {
    if width == 4 { "u32" } else { "u16" }
}

pub(crate) fn find_blob(
    children: &[SerzNode],
    expected: &str,
    ids: &HashMap<u32, &SerzNode>,
) -> Option<(Vec<u8>, String)> {
    fn find_any_blob(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
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
            SerzNode::Element { name, .. } if contains_ignore_ascii_case(name, "chcvertexbuffer") => {
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
        node: &SerzNode,
        expected: &str,
        ids: &HashMap<u32, &SerzNode>,
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
            SerzNode::Element { name, .. } if contains_ignore_ascii_case(name, "chcvertexbuffer") => {
                None
            }
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find_named_blob(child, expected, ids, reference_stack)),
            _ => None,
        }
    }

    let mut reference_stack = HashSet::new();
    for child in children {
        match child {
            SerzNode::Element { name, children, .. } if name.eq_ignore_ascii_case(expected) => {
                if let Some(data) = children
                    .iter()
                    .find_map(|node| find_any_blob(node, ids, &mut reference_stack))
                {
                    return Some((data, name.clone()));
                }
            }
            SerzNode::Reference { name, target_id } if name.eq_ignore_ascii_case(expected) => {
                if let Some(data) = ids
                    .get(target_id)
                    .and_then(|target| find_any_blob(target, ids, &mut reference_stack))
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
        .find_map(|child| find_named_blob(child, expected, ids, &mut reference_stack))
}

fn find_vertex_attribute_offsets(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> VertexAttributeOffsets {
    struct DeclCollector<'a> {
        ids: &'a HashMap<u32, &'a SerzNode>,
        reference_stack: HashSet<u32>,
        attributes: VertexAttributeOffsets,
        stream_offsets: HashMap<u32, usize>,
        stream_sizes: HashMap<u32, usize>,
    }

    impl<'a> DeclCollector<'a> {
        fn process(&mut self, node: &SerzNode) {
            match node {
                SerzNode::Reference { target_id, .. } => {
                    if self.reference_stack.insert(*target_id) {
                        if let Some(target) = self.ids.get(target_id) {
                            self.process(target);
                        }
                        self.reference_stack.remove(target_id);
                    }
                }
                SerzNode::Element { name, children, .. } => {
                    if contains_ignore_ascii_case(name, "chcvertexbuffer") {
                        return;
                    }
                    if contains_ignore_ascii_case(name, "chcvertextypedeclaration") {
                        for decl_child in children {
                            let SerzNode::Element {
                                name: decl_name,
                                children: element_children,
                                ..
                            } = decl_child
                            else {
                                continue;
                            };
                            if !contains_ignore_ascii_case(decl_name, "elementdecl") {
                                continue;
                            }
                            let usage =
                                direct_text(element_children, &["usage"], self.ids).unwrap_or_default();
                            let type_name =
                                direct_text(element_children, &["type"], self.ids).unwrap_or_default();
                            let stream_index =
                                find_direct_number(element_children, &["streamindex"], self.ids)
                                    .and_then(|v| u32::try_from(v).ok())
                                    .unwrap_or(0);
                            let explicit_offset = find_direct_number(
                                element_children,
                                &["offset", "byteoffset"],
                                self.ids,
                            )
                            .and_then(|value| usize::try_from(value).ok());
                            let offset = explicit_offset
                                .unwrap_or_else(|| *self.stream_offsets.entry(stream_index).or_default());
                            if let Some(size) = vertex_element_size(type_name) {
                                self.stream_offsets
                                    .insert(stream_index, offset.saturating_add(size));
                                let stream_size = self.stream_sizes.entry(stream_index).or_default();
                                *stream_size = stream_size.saturating_add(size);
                            }
                            if stream_index == 0 {
                                if contains_ignore_ascii_case(usage, "position")
                                    && self.attributes.position.is_none()
                                {
                                    self.attributes.position = Some(offset);
                                    self.attributes.position_format =
                                        position_format_from_type(type_name);
                                } else if contains_ignore_ascii_case(usage, "normal")
                                    && self.attributes.normal.is_none()
                                {
                                    self.attributes.normal = Some(offset);
                                } else if contains_ignore_ascii_case(usage, "tangent")
                                    && self.attributes.tangent.is_none()
                                {
                                    self.attributes.tangent = Some(offset);
                                } else if contains_ignore_ascii_case(usage, "texcoord")
                                    && find_direct_number(element_children, &["usageindex"], self.ids)
                                        .unwrap_or(0)
                                        == 0
                                    && self.attributes.uv.is_none()
                                {
                                    self.attributes.uv = Some(offset);
                                }
                            }
                            if !self.attributes.declaration.is_empty() {
                                self.attributes.declaration.push_str(", ");
                            }
                            let _ = write!(
                                self.attributes.declaration,
                                "{usage}:{type_name}@stream{stream_index}+{offset}"
                            );
                        }
                        return;
                    }
                    for child in children {
                        self.process(child);
                    }
                }
                _ => {}
            }
        }
    }

    let mut collector = DeclCollector {
        ids,
        reference_stack: HashSet::new(),
        attributes: VertexAttributeOffsets {
            position: None,
            position_format: PositionFormat::default(),
            normal: None,
            tangent: None,
            uv: None,
            declaration_stride: 0,
            declaration: String::new(),
        },
        stream_offsets: HashMap::new(),
        stream_sizes: HashMap::new(),
    };
    for child in children {
        collector.process(child);
    }
    collector.attributes.declaration_stride = collector.stream_sizes.get(&0).copied().unwrap_or(0);
    collector.attributes
}