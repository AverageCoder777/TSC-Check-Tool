//! SERZ node traversal helpers shared by parsing, assembly, and texture modules.

use std::collections::{HashMap, HashSet};

use glam::Mat4;

use crate::common::serz::{SerzNode, SerzReader};

use super::vertex::PositionFormat;

pub(crate) fn parse_serz_document(data: &[u8]) -> Result<SerzNode, String> {
    if data.starts_with(b"SERZ") {
        SerzReader::new(data).parse()
    } else {
        let xml = std::str::from_utf8(data).map_err(|error| error.to_string())?;
        SerzNode::from_xml_str(xml).map(|(root, _)| root)
    }
}

pub(crate) fn build_id_map(root: &SerzNode) -> HashMap<u32, &SerzNode> {
    fn collect<'a>(node: &'a SerzNode, ids: &mut HashMap<u32, &'a SerzNode>) {
        if let SerzNode::Element { id, children, .. } = node {
            if *id != 0 {
                ids.insert(*id, node);
            }
            for child in children {
                collect(child, ids);
            }
        }
    }
    let mut ids = HashMap::new();
    collect(root, &mut ids);
    ids
}

pub(crate) fn resolve_ref_node<'a>(
    node: &'a SerzNode,
    ids: &HashMap<u32, &'a SerzNode>,
    reference_stack: &mut HashSet<u32>,
) -> Option<&'a SerzNode> {
    match node {
        SerzNode::Reference { target_id, .. } => {
            if !reference_stack.insert(*target_id) {
                return None;
            }
            let target = ids
                .get(target_id)
                .and_then(|target| resolve_ref_node(target, ids, reference_stack));
            reference_stack.remove(target_id);
            target
        }
        _ => Some(node),
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn matches_any_ignore_ascii_case(name: &str, expected: &[&str]) -> bool {
    expected
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn split_numeric_tokens(value: &str) -> impl Iterator<Item = &str> {
    value.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']'))
}

fn parse_i32_token(part: &str) -> Option<i32> {
    part.parse::<i32>()
        .ok()
        .or_else(|| part.parse::<u32>().ok().map(|v| v as i32))
        .or_else(|| part.parse::<i64>().ok().map(|v| v as i32))
}

pub(crate) fn parse_float_values(value: &str) -> Vec<f32> {
    split_numeric_tokens(value)
        .filter_map(|part| part.parse::<f32>().ok())
        .collect()
}

pub(crate) fn collect_float_values(node: &SerzNode) -> Vec<f32> {
    fn collect_into(node: &SerzNode, out: &mut Vec<f32>) {
        match node {
            SerzNode::Property { value, .. } => {
                out.extend(split_numeric_tokens(value).filter_map(|p| p.parse::<f32>().ok()));
            }
            SerzNode::Array { values, .. } => {
                for value in values {
                    out.extend(split_numeric_tokens(value).filter_map(|p| p.parse::<f32>().ok()));
                }
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_into(child, out);
                }
            }
            SerzNode::Blob { data } => {
                let chunks = data.as_chunks::<4>().0;
                out.reserve(chunks.len());
                out.extend(chunks.iter().copied().map(f32::from_le_bytes));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    collect_into(node, &mut out);
    out
}

pub(crate) fn parse_i32_values(value: &str) -> Vec<i32> {
    split_numeric_tokens(value)
        .filter_map(parse_i32_token)
        .collect()
}

pub(crate) fn collect_i32_values(node: &SerzNode) -> Vec<i32> {
    fn collect_into(node: &SerzNode, out: &mut Vec<i32>) {
        match node {
            SerzNode::Property { value, .. } => {
                out.extend(split_numeric_tokens(value).filter_map(parse_i32_token));
            }
            SerzNode::Array { values, .. } => {
                for value in values {
                    out.extend(split_numeric_tokens(value).filter_map(parse_i32_token));
                }
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_into(child, out);
                }
            }
            SerzNode::Blob { data } => {
                let chunks = data.as_chunks::<4>().0;
                out.reserve(chunks.len());
                out.extend(chunks.iter().copied().map(i32::from_le_bytes));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    collect_into(node, &mut out);
    out
}

pub(crate) fn find_named_values<'a>(
    node: &'a SerzNode,
    expected: &str,
) -> Option<Vec<&'a SerzNode>> {
    fn collect_values<'a>(node: &'a SerzNode, values: &mut Vec<&'a SerzNode>) {
        match node {
            SerzNode::Property { .. } | SerzNode::Array { .. } | SerzNode::Blob { .. } => {
                values.push(node);
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_values(child, values);
                }
            }
            _ => {}
        }
    }
    match node {
        SerzNode::Property { name, .. } | SerzNode::Array { name, .. }
            if name.eq_ignore_ascii_case(expected) =>
        {
            Some(vec![node])
        }
        SerzNode::Element { name, children, .. } if name.eq_ignore_ascii_case(expected) => {
            let mut values = Vec::new();
            for child in children {
                collect_values(child, &mut values);
            }
            Some(values)
        }
        SerzNode::Element { children, .. } => children
            .iter()
            .find_map(|child| find_named_values(child, expected)),
        _ => None,
    }
}

pub(crate) fn collect_transform_names(root: &SerzNode) -> Vec<String> {
    fn push_name(output: &mut Vec<String>, value: &str) {
        let name = value.trim();
        if !name.is_empty() {
            output.push(name.to_owned());
        }
    }
    fn push_strings(node: &SerzNode, output: &mut Vec<String>) {
        match node {
            SerzNode::Property { value, .. } => push_name(output, value),
            SerzNode::Array { values, .. } => {
                for value in values {
                    push_name(output, value);
                }
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    push_strings(child, output);
                }
            }
            _ => {}
        }
    }
    fn collect(node: &SerzNode, output: &mut Vec<String>) {
        match node {
            SerzNode::Property { name, value, .. }
                if name.eq_ignore_ascii_case("transformname") =>
            {
                push_name(output, value);
            }
            SerzNode::Array { name, values, .. } if name.eq_ignore_ascii_case("transformname") => {
                for value in values {
                    push_name(output, value);
                }
            }
            SerzNode::Element { name, children, .. } => {
                if name.eq_ignore_ascii_case("transformname") {
                    for child in children {
                        push_strings(child, output);
                    }
                } else {
                    for child in children {
                        collect(child, output);
                    }
                }
            }
            _ => {}
        }
    }
    let mut names = Vec::new();
    collect(root, &mut names);
    names
}

pub(crate) fn compute_world_transforms(local: &[Mat4], parents: &[i32]) -> Vec<Mat4> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Unvisited,
        Visiting,
        Invalid,
        Done,
    }

    fn resolve(
        index: usize,
        local: &[Mat4],
        parents: &[i32],
        states: &mut [VisitState],
        world: &mut [Mat4],
        stack: &mut Vec<usize>,
    ) -> Mat4 {
        match states[index] {
            VisitState::Done => return world[index],
            VisitState::Visiting | VisitState::Invalid => {
                if let Some(cycle_start) = stack.iter().position(|&item| item == index) {
                    for &cycle_index in &stack[cycle_start..] {
                        states[cycle_index] = VisitState::Invalid;
                    }
                }
                return Mat4::IDENTITY;
            }
            VisitState::Unvisited => {}
        }

        states[index] = VisitState::Visiting;
        stack.push(index);
        let parent = parents.get(index).copied().unwrap_or(-1);
        let resolved = if parent == -1 {
            local[index]
        } else if let Ok(parent_index) = usize::try_from(parent)
            && parent_index < local.len()
        {
            resolve(parent_index, local, parents, states, world, stack) * local[index]
        } else {
            states[index] = VisitState::Invalid;
            Mat4::IDENTITY
        };
        stack.pop();

        world[index] = if states[index] == VisitState::Invalid
            || resolved.to_cols_array().iter().any(|v| !v.is_finite())
        {
            Mat4::IDENTITY
        } else {
            resolved
        };
        states[index] = VisitState::Done;
        world[index]
    }

    let mut states = vec![VisitState::Unvisited; local.len()];
    let mut world = vec![Mat4::IDENTITY; local.len()];
    let mut stack = Vec::new();
    for index in 0..local.len() {
        resolve(index, local, parents, &mut states, &mut world, &mut stack);
    }
    world
}

pub(crate) fn matrix_from_values(values: &[f32]) -> Option<Mat4> {
    let matrix = <[f32; 16]>::try_from(values).ok()?;
    if matrix.iter().any(|value| !value.is_finite()) {
        return None;
    }
    // Train Simulator stores DX9 matrices. `Mat4::from_cols_array` interprets
    // each group of four floats as a column, which matches the layout produced
    // by the SERZ→XML round-trip used by the parser.
    let parsed = Mat4::from_cols_array(&matrix);
    if parsed.x_axis.w.abs() > 0.01
        || parsed.y_axis.w.abs() > 0.01
        || parsed.z_axis.w.abs() > 0.01
        || (parsed.w_axis.w - 1.0).abs() > 0.01
    {
        return Some(Mat4::IDENTITY);
    }
    let basis = [
        parsed.x_axis.truncate(),
        parsed.y_axis.truncate(),
        parsed.z_axis.truncate(),
    ];
    if basis.iter().any(|axis| axis.length_squared() < 1.0e-10) {
        return Some(Mat4::IDENTITY);
    }
    Some(parsed)
}

pub(crate) fn find_transform_id(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<usize> {
    const ALIASES: &[&str] = &[
        "transformid",
        "transformids",
        "primarytransformid",
        "primarytransformids",
        "childtransformid",
        "childtransformids",
    ];

    fn first_non_negative_in_str(value: &str) -> Option<usize> {
        split_numeric_tokens(value)
            .filter_map(parse_i32_token)
            .find(|&v| v >= 0)
            .map(|v| v as usize)
    }

    fn first_non_negative_in_node(node: &SerzNode) -> Option<usize> {
        match node {
            SerzNode::Property { value, .. } => first_non_negative_in_str(value),
            SerzNode::Array { values, .. } => {
                values.iter().find_map(|v| first_non_negative_in_str(v))
            }
            SerzNode::Element { children, .. } => {
                children.iter().find_map(first_non_negative_in_node)
            }
            SerzNode::Blob { data } => data
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .map(i32::from_le_bytes)
                .find(|&v| v >= 0)
                .map(|v| v as usize),
            _ => None,
        }
    }

    for child in children {
        let found = match child {
            SerzNode::Property { name, value, .. }
                if matches_any_ignore_ascii_case(name, ALIASES) =>
            {
                first_non_negative_in_str(value)
            }
            SerzNode::Array { name, values, .. }
                if matches_any_ignore_ascii_case(name, ALIASES) =>
            {
                values.iter().find_map(|v| first_non_negative_in_str(v))
            }
            SerzNode::Reference { name, target_id }
                if matches_any_ignore_ascii_case(name, ALIASES) =>
            {
                ids.get(target_id).and_then(|t| first_non_negative_in_node(t))
            }
            SerzNode::Element { name, children, .. }
                if matches_any_ignore_ascii_case(name, ALIASES) =>
            {
                children.iter().find_map(first_non_negative_in_node)
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

pub(crate) fn position_format_from_type(type_name: &str) -> PositionFormat {
    if contains_ignore_ascii_case(type_name, "float16")
        || contains_ignore_ascii_case(type_name, "half")
    {
        return PositionFormat::Float16_4;
    }
    if contains_ignore_ascii_case(type_name, "short4n") {
        return PositionFormat::Short4N;
    }
    PositionFormat::Float3
}

pub(crate) fn parse_fvf(fvf: u32) -> (usize, Option<usize>, Option<usize>) {
    let mut offset = 0usize;
    let mut normal_offset = None;
    let mut uv_offset = None;
    if fvf & 0x004 != 0 {
        offset += 16;
    } else if fvf & 0x002 != 0 {
        offset += 12;
        let pos_mask = fvf & 0x00E;
        if pos_mask >= 0x006 {
            offset += (((pos_mask >> 1) - 2) * 4) as usize;
        }
    }
    if fvf & 0x010 != 0 {
        normal_offset = Some(offset);
        offset += 12;
    }
    if fvf & 0x020 != 0 {
        offset += 4;
    }
    if fvf & 0x040 != 0 {
        offset += 4;
    }
    if fvf & 0x080 != 0 {
        offset += 4;
    }
    let tex_count = ((fvf >> 8) & 0x0F) as usize;
    for i in 0..tex_count {
        let coord_size = match (fvf >> (16 + i * 2)) & 0x3 {
            0 => 8,
            1 => 12,
            2 => 16,
            _ => 4,
        };
        if i == 0 {
            uv_offset = Some(offset);
        }
        offset += coord_size;
    }
    (offset, normal_offset, uv_offset)
}

/// Picks a stride that actually divides the buffer. An explicit `VertexStride` from the
/// file is authoritative when present and valid; the parsed declaration is the fallback.
#[cfg(test)]
pub(crate) fn select_vertex_stride(
    bytes_len: usize,
    explicit: Option<usize>,
    declared: Option<usize>,
    declaration: usize,
    fvf_stride: usize,
) -> usize {
    let valid = |stride: usize| stride > 0 && bytes_len % stride == 0;
    Some(declaration)
        .filter(|&stride| valid(stride))
        .or(declared.filter(|&stride| valid(stride)))
        .or(explicit.filter(|&stride| valid(stride)))
        .or(Some(fvf_stride).filter(|&stride| valid(stride)))
        .or(explicit)
        .or(declared)
        .or(Some(declaration).filter(|&stride| stride > 0))
        .or(Some(fvf_stride).filter(|&stride| stride > 0))
        .unwrap_or(32)
}

pub(crate) fn vertex_element_size(type_name: &str) -> Option<usize> {
    if contains_ignore_ascii_case(type_name, "float16")
        || contains_ignore_ascii_case(type_name, "half")
    {
        return Some(if type_name.contains('4') { 8 } else { 4 });
    }
    for (needle, size) in [("float1", 4), ("float2", 8), ("float3", 12), ("float4", 16)] {
        if contains_ignore_ascii_case(type_name, needle) {
            return Some(size);
        }
    }
    if contains_ignore_ascii_case(type_name, "color")
        || contains_ignore_ascii_case(type_name, "ubyte4")
        || contains_ignore_ascii_case(type_name, "short2")
    {
        Some(4)
    } else if contains_ignore_ascii_case(type_name, "short4") {
        Some(8)
    } else {
        None
    }
}

pub(crate) fn direct_text<'a>(
    children: &'a [SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &'a SerzNode>,
) -> Option<&'a str> {
    let mut reference_stack = HashSet::new();
    children.iter().find_map(|child| match child {
        SerzNode::Property { name, value, .. }
            if matches_any_ignore_ascii_case(name, expected) =>
        {
            Some(value.as_str())
        }
        SerzNode::Array { name, values, .. } if matches_any_ignore_ascii_case(name, expected) => {
            values.first().map(String::as_str)
        }
        SerzNode::Reference { name, target_id }
            if matches_any_ignore_ascii_case(name, expected) =>
        {
            ids.get(target_id)
                .and_then(|target| resolve_ref_text(target, ids, &mut reference_stack))
        }
        SerzNode::Element { name, children, .. }
            if matches_any_ignore_ascii_case(name, expected) =>
        {
            children
                .iter()
                .find_map(|property| resolve_ref_text(property, ids, &mut reference_stack))
        }
        _ => None,
    })
}

pub(crate) fn find_text<'a>(
    children: &'a [SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &'a SerzNode>,
) -> Option<&'a str> {
    fn find<'a>(
        node: &'a SerzNode,
        expected: &[&str],
        ids: &HashMap<u32, &'a SerzNode>,
        reference_stack: &mut HashSet<u32>,
    ) -> Option<&'a str> {
        match node {
            SerzNode::Property { name, value, .. }
                if matches_any_ignore_ascii_case(name, expected) =>
            {
                Some(value)
            }
            SerzNode::Array { name, values, .. }
                if matches_any_ignore_ascii_case(name, expected) =>
            {
                values.first().map(String::as_str)
            }
            SerzNode::Reference { name, target_id } => {
                if !reference_stack.insert(*target_id) {
                    return None;
                }
                let result = ids.get(target_id).and_then(|target| {
                    if matches_any_ignore_ascii_case(name, expected) {
                        resolve_ref_text(target, ids, reference_stack)
                    } else {
                        find(target, expected, ids, reference_stack)
                    }
                });
                reference_stack.remove(target_id);
                result
            }
            SerzNode::Element { name, children, .. }
                if matches_any_ignore_ascii_case(name, expected) =>
            {
                children
                    .iter()
                    .find_map(|child| resolve_ref_text(child, ids, reference_stack))
            }
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find(child, expected, ids, reference_stack)),
            _ => None,
        }
    }
    let mut reference_stack = HashSet::new();
    children
        .iter()
        .find_map(|child| find(child, expected, ids, &mut reference_stack))
}

pub(crate) fn find_number(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<u64> {
    find_text(children, expected, ids)?.trim().parse().ok()
}

pub(crate) fn find_direct_number(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<u64> {
    direct_text(children, expected, ids)?.trim().parse().ok()
}

pub(crate) fn find_reference(children: &[SerzNode], expected: &str) -> Option<u32> {
    for child in children {
        match child {
            SerzNode::Reference { name, target_id } if name.eq_ignore_ascii_case(expected) => {
                return Some(*target_id);
            }
            SerzNode::Element { children, .. } => {
                if let Some(found) = find_reference(children, expected) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn resolve_ref_text<'a>(
    node: &'a SerzNode,
    ids: &HashMap<u32, &'a SerzNode>,
    reference_stack: &mut HashSet<u32>,
) -> Option<&'a str> {
    match node {
        SerzNode::Property { value, .. } => Some(value),
        SerzNode::Array { values, .. } => values.first().map(String::as_str),
        SerzNode::Element { children, .. } => children
            .iter()
            .find_map(|child| resolve_ref_text(child, ids, reference_stack)),
        SerzNode::Reference { target_id, .. } => {
            if !reference_stack.insert(*target_id) {
                return None;
            }
            let value = ids
                .get(target_id)
                .and_then(|target| resolve_ref_text(target, ids, reference_stack));
            reference_stack.remove(target_id);
            value
        }
        _ => None,
    }
}

pub(crate) fn decode_indices(data: &[u8], width: usize) -> Result<Vec<u32>, String> {
    match width {
        2 => Ok(data
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(|bytes| u32::from(u16::from_le_bytes(bytes)))
            .collect()),
        4 => Ok(data
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(u32::from_le_bytes)
            .collect()),
        _ => Err("invalid index buffer byte length".into()),
    }
}

/// Recursively search for a SerzNode::Reference in the subtree that points to a known vertex buffer ID.
pub(crate) fn find_vbuf_ref_in_subtree(
    children: &[SerzNode],
    vb_id_to_index: &HashMap<u32, u32>,
) -> Option<u32> {
    fn search(node: &SerzNode, vb_id_to_index: &HashMap<u32, u32>, depth: u32) -> Option<u32> {
        if depth > 10 {
            return None;
        }
        match node {
            SerzNode::Reference { target_id, .. } => vb_id_to_index.get(target_id).copied(),
            SerzNode::Element { name, children, .. } => {
                if contains_ignore_ascii_case(name, "geometrynodesamestate")
                    || contains_ignore_ascii_case(name, "primitivesetdescdx")
                    || contains_ignore_ascii_case(name, "primlist")
                    || contains_ignore_ascii_case(name, "primitivelist")
                {
                    return None;
                }
                children
                    .iter()
                    .find_map(|child| search(child, vb_id_to_index, depth + 1))
            }
            _ => None,
        }
    }
    children
        .iter()
        .find_map(|child| search(child, vb_id_to_index, 0))
}