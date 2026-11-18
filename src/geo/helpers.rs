//! SERZ node traversal helpers shared by parsing, assembly, and texture modules.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::serz::SerzNode;
use glam::Mat4;

pub(crate) fn parse_serz_document(data: &[u8]) -> Result<SerzNode, String> {
    if data.starts_with(b"SERZ") {
        crate::serz::SerzReader::new(data).parse()
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

pub(crate) fn collect_float_values(node: &SerzNode) -> Vec<f32> {
    match node {
        SerzNode::Property { value, .. } => parse_float_values(value),
        SerzNode::Array { values, .. } => values
            .iter()
            .flat_map(|value| parse_float_values(value))
            .collect(),
        SerzNode::Element { children, .. } => {
            children.iter().flat_map(collect_float_values).collect()
        }
        SerzNode::Blob { data } => data
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("4-byte float chunk")))
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn parse_float_values(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ';' | '[' | ']')
        })
        .filter_map(|part| part.parse::<f32>().ok())
        .collect()
}

pub(crate) fn parse_i32_values(value: &str) -> Vec<i32> {
    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ';' | '[' | ']')
        })
        .filter_map(|part| {
            part.parse::<i32>()
                .ok()
                .or_else(|| part.parse::<u32>().ok().map(|value| value as i32))
                .or_else(|| part.parse::<i64>().ok().map(|value| value as i32))
        })
        .collect()
}

pub(crate) fn collect_i32_values(node: &SerzNode) -> Vec<i32> {
    match node {
        SerzNode::Property { value, .. } => parse_i32_values(value),
        SerzNode::Array { values, .. } => values
            .iter()
            .flat_map(|value| parse_i32_values(value))
            .collect(),
        SerzNode::Element { children, .. } => {
            children.iter().flat_map(collect_i32_values).collect()
        }
        SerzNode::Blob { data } => data
            .chunks_exact(4)
            .map(|bytes| i32::from_le_bytes(bytes.try_into().expect("4-byte integer chunk")))
            .collect(),
        _ => Vec::new(),
    }
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
    fn resolve(
        index: usize,
        local: &[Mat4],
        parents: &[i32],
        states: &mut [u8],
        invalid: &mut [bool],
        world: &mut [Mat4],
        stack: &mut Vec<usize>,
    ) -> Mat4 {
        if states[index] == 2 {
            return world[index];
        }
        if states[index] == 1 {
            if let Some(cycle_start) = stack.iter().position(|&item| item == index) {
                for &cycle_index in &stack[cycle_start..] {
                    invalid[cycle_index] = true;
                }
            }
            return Mat4::IDENTITY;
        }
        states[index] = 1;
        stack.push(index);
        let parent = parents.get(index).copied().unwrap_or(-1);
        let resolved = if parent == -1 {
            local[index]
        } else if let Ok(parent_index) = usize::try_from(parent)
            && parent_index < local.len()
        {
            resolve(parent_index, local, parents, states, invalid, world, stack) * local[index]
        } else {
            invalid[index] = true;
            Mat4::IDENTITY
        };
        stack.pop();
        world[index] = if invalid[index]
            || resolved
                .to_cols_array()
                .iter()
                .any(|value| !value.is_finite())
        {
            Mat4::IDENTITY
        } else {
            resolved
        };
        states[index] = 2;
        world[index]
    }
    let mut states = vec![0; local.len()];
    let mut invalid = vec![false; local.len()];
    let mut world = vec![Mat4::IDENTITY; local.len()];
    let mut stack = Vec::new();
    for index in 0..local.len() {
        resolve(
            index,
            local,
            parents,
            &mut states,
            &mut invalid,
            &mut world,
            &mut stack,
        );
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
    if basis.iter().any(|axis| axis.length() < 1.0e-5) {
        return Some(Mat4::IDENTITY);
    }
    Some(parsed)
}

pub(crate) fn find_transform_id(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<usize> {
    let aliases = [
        "transformid",
        "transformids",
        "primarytransformid",
        "primarytransformids",
        "childtransformid",
        "childtransformids",
    ];
    let mut values = Vec::new();
    for child in children {
        match child {
            SerzNode::Property { name, value, .. }
                if aliases.iter().any(|a| name.eq_ignore_ascii_case(a)) =>
            {
                values.extend(parse_i32_values(value));
            }
            SerzNode::Array {
                name, values: vals, ..
            } if aliases.iter().any(|a| name.eq_ignore_ascii_case(a)) => {
                for value in vals {
                    values.extend(parse_i32_values(value));
                }
            }
            SerzNode::Reference { name, target_id }
                if aliases.iter().any(|a| name.eq_ignore_ascii_case(a)) =>
            {
                if let Some(target) = ids.get(target_id) {
                    values.extend(collect_i32_values(target));
                }
            }
            SerzNode::Element { name, children, .. }
                if aliases.iter().any(|a| name.eq_ignore_ascii_case(a)) =>
            {
                values.extend(children.iter().flat_map(collect_i32_values));
            }
            _ => {}
        }
    }
    values
        .into_iter()
        .find(|&value| value >= 0)
        .map(|value| value as usize)
}

pub(crate) fn position_format_from_type(type_name: &str) -> crate::geo::vertex::PositionFormat {
    let type_name = type_name.to_ascii_lowercase();
    if type_name.contains("float16") || type_name.contains("half") {
        return crate::geo::vertex::PositionFormat::Float16_4;
    }
    if type_name.contains("short4n") {
        return crate::geo::vertex::PositionFormat::Short4N;
    }
    crate::geo::vertex::PositionFormat::Float3
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
        let size_code = (fvf >> (16 + i * 2)) & 0x3;
        let coord_size = match size_code {
            0 => 8,
            1 => 12,
            2 => 16,
            3 => 4,
            _ => 8,
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
pub(crate) fn select_vertex_stride(
    bytes_len: usize,
    explicit: Option<usize>,
    declared: Option<usize>,
    declaration: usize,
    fvf_stride: usize,
) -> usize {
    let valid = |stride: usize| stride > 0 && bytes_len % stride == 0;
    Some(declaration)
        .filter(|stride| valid(*stride))
        .or(declared.filter(|stride| valid(*stride)))
        .or(explicit.filter(|stride| valid(*stride)))
        .or(Some(fvf_stride).filter(|stride| valid(*stride)))
        .or(explicit)
        .or(declared)
        .or(Some(declaration).filter(|stride| *stride > 0))
        .or(Some(fvf_stride).filter(|stride| *stride > 0))
        .unwrap_or(32)
}

pub(crate) fn vertex_element_size(type_name: &str) -> Option<usize> {
    let type_name = type_name.to_ascii_lowercase();
    if type_name.contains("float16") || type_name.contains("half") {
        if type_name.contains("4") {
            return Some(8);
        }
        if type_name.contains("2") {
            return Some(4);
        }
        return Some(4);
    }
    for components in 1..=4 {
        if type_name.contains(&format!("float{components}")) {
            return Some(components * 4);
        }
    }
    if type_name.contains("color") || type_name.contains("ubyte4") {
        Some(4)
    } else if type_name.contains("short2") {
        Some(4)
    } else if type_name.contains("short4") {
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
    children.iter().find_map(|child| match child {
        SerzNode::Property { name, value, .. }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(value.as_str())
        }
        SerzNode::Array { name, values, .. }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            values.first().map(String::as_str)
        }
        SerzNode::Reference { name, target_id }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            ids.get(target_id)
                .and_then(|target| resolve_ref_text(target, ids, &mut HashSet::new()))
        }
        SerzNode::Element { name, children, .. }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            children
                .iter()
                .find_map(|property| resolve_ref_text(property, ids, &mut HashSet::new()))
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
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                Some(value)
            }
            SerzNode::Array { name, values, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                values.first().map(String::as_str)
            }
            SerzNode::Reference { name, target_id } => {
                if !reference_stack.insert(*target_id) {
                    return None;
                }
                let result = ids.get(target_id).and_then(|target| {
                    if expected
                        .iter()
                        .any(|candidate| name.eq_ignore_ascii_case(candidate))
                    {
                        resolve_ref_text(target, ids, reference_stack)
                    } else {
                        find(target, expected, ids, reference_stack)
                    }
                });
                reference_stack.remove(target_id);
                result
            }
            SerzNode::Element { name, children, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
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
    for child in children {
        if let Some(value) = find(child, expected, ids, &mut HashSet::new()) {
            return Some(value);
        }
    }
    None
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
    if !matches!(width, 2 | 4) {
        return Err("invalid index buffer byte length".into());
    }
    let complete_bytes = data.len() / width * width;
    Ok(data
        .get(..complete_bytes)
        .ok_or("invalid index buffer byte length")?
        .chunks_exact(width)
        .map(|bytes| {
            if width == 2 {
                u16::from_le_bytes([bytes[0], bytes[1]]) as u32
            } else {
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
        })
        .collect())
}

pub(crate) fn find_parent_geometry_vertex_data<'a>(
    node: &'a SerzNode,
    ids: &HashMap<u32, &'a SerzNode>,
) -> &'a SerzNode {
    for (_, candidate) in ids {
        if let SerzNode::Element { name, children, .. } = candidate {
            if name.to_ascii_lowercase().contains("chcgeometryvertexdata") {
                for child in children {
                    if references_target(child, node) {
                        return candidate;
                    }
                }
            }
        }
    }
    node
}

pub(crate) fn find_parent_vertex_data_element<'a>(
    node: &'a SerzNode,
    ids: &HashMap<u32, &'a SerzNode>,
) -> &'a SerzNode {
    for (_, candidate) in ids {
        if let SerzNode::Element {
            name, children, id, ..
        } = candidate
        {
            let clean_name = name.replace("::", "-").to_ascii_lowercase();
            if clean_name.contains("vertexdata") && *id != 0 {
                for child in children {
                    if references_target(child, node) {
                        return candidate;
                    }
                }
            }
        }
    }
    node
}

pub(crate) fn references_target<'a>(node: &'a SerzNode, target: &'a SerzNode) -> bool {
    let target_id = match target {
        SerzNode::Element { id, .. } => *id,
        _ => return false,
    };
    if target_id == 0 {
        return false;
    }
    match node {
        SerzNode::Reference {
            target_id: ref_id, ..
        } => *ref_id == target_id,
        SerzNode::Element { children, .. } => children.iter().any(|c| references_target(c, target)),
        _ => false,
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
                let clean_name = name.replace("::", "-").to_ascii_lowercase();
                if clean_name.contains("geometrynodesamestate")
                    || clean_name.contains("primitivesetdescdx")
                    || clean_name.contains("primlist")
                    || clean_name.contains("primitivelist")
                {
                    return None;
                }
                for child in children {
                    if let Some(idx) = search(child, vb_id_to_index, depth + 1) {
                        return Some(idx);
                    }
                }
                None
            }
            _ => None,
        }
    }
    for child in children {
        if let Some(idx) = search(child, vb_id_to_index, 0) {
            return Some(idx);
        }
    }
    None
}

/// Named text lookup used by assembly helpers (kept outside #[cfg(test)] to match
/// original visibility; only invoked from test code paths).
pub(crate) fn find_named_text(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<String> {
    match node {
        SerzNode::Property { name, value, .. }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(value.trim().to_owned())
        }
        SerzNode::Array { name, values, .. }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(values.join(" "))
        }
        SerzNode::Reference { name, target_id }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            ids.get(target_id)
                .and_then(|target| find_asset_path_text(target))
        }
        SerzNode::Element { name, children, .. } => {
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                let value = children
                    .iter()
                    .find_map(|child| find_named_text(child, names, ids))
                    .or_else(|| {
                        names
                            .iter()
                            .any(|candidate| {
                                candidate.eq_ignore_ascii_case("geometryid")
                                    || candidate.eq_ignore_ascii_case("collisiongeometryid")
                            })
                            .then(|| {
                                children
                                    .iter()
                                    .find_map(|child| find_named_text(child, &["blueprintid"], ids))
                            })
                            .flatten()
                    })
                    .or_else(|| match children.as_slice() {
                        [SerzNode::Property { value, .. }] => Some(value.trim().to_owned()),
                        _ => None,
                    });
                if let Some(value) = value {
                    return Some(value);
                }
            }
            children
                .iter()
                .find_map(|child| find_named_text(child, names, ids))
        }
        _ => None,
    }
}

pub(crate) fn find_asset_path_text(node: &SerzNode) -> Option<String> {
    match node {
        SerzNode::Property { value, .. } => {
            let value = value.trim();
            (!value.is_empty()
                && (value.contains('\\')
                    || value.contains('/')
                    || value.to_ascii_lowercase().ends_with(".xml")
                    || value.to_ascii_lowercase().ends_with(".bin")
                    || value.to_ascii_lowercase().ends_with(".geopcdx")))
            .then(|| value.to_owned())
        }
        SerzNode::Array { values, .. } => values.iter().find_map(|value| {
            let lower = value.to_ascii_lowercase();
            (value.contains('\\')
                || value.contains('/')
                || lower.ends_with(".xml")
                || lower.ends_with(".bin")
                || lower.ends_with(".geopcdx"))
            .then(|| value.clone())
        }),
        SerzNode::Element { children, .. } => children.iter().find_map(find_asset_path_text),
        _ => None,
    }
}

pub(crate) fn find_named_number(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<f32> {
    find_named_text(node, names, ids)?.parse().ok()
}

pub(crate) fn find_named_numbers(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<Vec<f32>> {
    match node {
        SerzNode::Property { name, value, .. }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(parse_float_values(value))
        }
        SerzNode::Array { name, values, .. }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(
                values
                    .iter()
                    .flat_map(|value| parse_float_values(value))
                    .collect(),
            )
        }
        SerzNode::Reference { name, target_id }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            ids.get(target_id)
                .and_then(|target| find_named_numbers(target, names, ids))
        }
        SerzNode::Element { name, children, .. } => {
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                let values = children
                    .iter()
                    .flat_map(collect_float_values)
                    .collect::<Vec<_>>();
                if !values.is_empty() {
                    return Some(values);
                }
            }
            children
                .iter()
                .find_map(|child| find_named_numbers(child, names, ids))
        }
        _ => None,
    }
}
