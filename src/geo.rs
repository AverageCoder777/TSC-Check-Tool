use crate::serz::{SerzNode, SerzReader};
use glam::{Mat4, Vec3};
use std::collections::HashMap;
use std::path::Path;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeoSubmesh {
    pub first_index: u32,
    pub index_count: u32,
    pub material_index: u32,
    pub texture_index: u32,
    pub vertex_buffer_index: u32,
    pub base_vertex: i32,
    pub vertex_buffer_offset: i32,
}

#[derive(Clone, Debug)]
pub struct GeoParseDiagnostics {
    pub vertex_stride: usize,
    pub vertex_count: usize,
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

#[derive(Clone, Copy, Debug)]
struct RawSubmesh {
    first_index: u32,
    index_count: u32,
    material_reference: Option<u32>,
    material_index: Option<u32>,
    texture_index: Option<u32>,
    vertex_buffer_index: u32,
    index_buffer_index: u32,
    base_vertex: i32,
    topology: PrimitiveTopology,
    transform: Mat4,
    excluded: bool,
}

impl GeoModel {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        let root = if data.starts_with(b"SERZ") {
            SerzReader::new(data).parse()?
        } else {
            let xml = std::str::from_utf8(data).map_err(|error| error.to_string())?;
            SerzNode::from_xml_str(xml)?.0
        };
        let mut vertex_data = Vec::new();
        let mut index_data = Vec::new();
        let mut xml = String::new();
        find_geometry_buffers(&root, &mut vertex_data, &mut index_data);
        xml.push_str(&SerzReader::to_xml(&root, 0, true));

        let first_vertex_buffer = vertex_data
            .first()
            .ok_or("GeoPcDx has no supported vertex buffer")?;
        let first_index_buffer = index_data
            .first()
            .ok_or("GeoPcDx has no supported index buffer")?;
        let mut vertices = Vec::new();
        let mut vertex_buffer_offsets = Vec::with_capacity(vertex_data.len());
        let mut vertex_layouts = Vec::with_capacity(vertex_data.len());
        for buffer in &vertex_data {
            vertex_buffer_offsets.push(vertices.len());
            vertex_layouts.push(buffer.layout.clone());
            if buffer.stride == 0 {
                return Err("vertex buffer has a zero-byte stride".into());
            }
            let vertex_count = buffer.bytes.len() / buffer.stride;
            if vertex_count == 0 || buffer.bytes.len() % buffer.stride != 0 {
                return Err("vertex buffer length is not a nonzero multiple of the stride".into());
            }
            for record in buffer.bytes.chunks_exact(buffer.stride) {
                let position = read_vec3(record, buffer.position_offset)?;
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
                    .unwrap_or([0.0, 0.0]);
                if position.iter().any(|value| !value.is_finite())
                    || normal.iter().any(|value| !value.is_finite())
                    || uv.iter().any(|value| !value.is_finite())
                {
                    return Err("vertex buffer contains non-finite values".into());
                }
                vertices.push(GeoVertex {
                    position,
                    normal,
                    uv,
                });
            }
        }

        let xml = if data.starts_with(b"SERZ") {
            xml
        } else {
            String::from_utf8_lossy(data).into_owned()
        };
        let textures = crate::auditor::extract_geo_texture_references(&xml);
        let material_bindings = find_material_bindings(&root, &textures);
        let mut raw_batches = Vec::new();
        find_submeshes(
            &root,
            None,
            &material_bindings,
            Mat4::IDENTITY,
            None,
            false,
            &mut raw_batches,
        );
        let has_batch_ranges = !raw_batches.is_empty();
        let render_batches = raw_batches
            .iter()
            .filter(|batch| !batch.excluded)
            .collect::<Vec<_>>();
        let primitive_type = if let Some(batch) = render_batches.first() {
            batch.topology
        } else {
            find_primitive_topology(&root)
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
                let Some(end) = batch.first_index.checked_add(batch.index_count) else {
                    return Err("primitive batch index range overflows".into());
                };
                let index_buffer = index_data
                    .get(batch.index_buffer_index as usize)
                    .ok_or("primitive batch references a missing index buffer")?;
                let range = index_buffer
                    .indices
                    .get(batch.first_index as usize..end as usize)
                    .ok_or("primitive batch index range is outside its index buffer")?;
                let vertex_buffer_offset = *vertex_buffer_offsets
                    .get(batch.vertex_buffer_index as usize)
                    .ok_or("primitive batch references a missing vertex buffer")?;
                let vertex_buffer_offset = i32::try_from(vertex_buffer_offset)
                    .map_err(|_| "vertex buffer offset exceeds draw limits")?;
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
                let triangles =
                    convert_to_triangle_list(range, batch.topology, index_buffer.element_type);
                if batch.transform == Mat4::IDENTITY {
                    append_submesh(
                        &mut indices,
                        &mut submeshes,
                        triangles,
                        material_index,
                        texture_index,
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
                        batch.vertex_buffer_index,
                        batch.base_vertex,
                        vertex_buffer_offset,
                    )?;
                }
            }
        }
        if submeshes.is_empty() {
            let triangles = convert_to_triangle_list(
                &first_index_buffer.indices,
                primitive_type,
                first_index_buffer.element_type,
            );
            append_submesh(
                &mut indices,
                &mut submeshes,
                triangles,
                0,
                0,
                0,
                0,
                0,
                vertices.len(),
            )?;
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
            index_element_type: first_index_buffer.element_type,
            raw_index_count: index_data.iter().map(|buffer| buffer.indices.len()).sum(),
            primitive_type,
            vertex_layout,
            index_layout,
            batch_ranges,
        };
        Ok(Self {
            vertices,
            indices,
            textures,
            submeshes,
            diagnostics,
        })
    }
}

#[derive(Clone, Debug)]
struct VertexBuffer {
    bytes: Vec<u8>,
    stride: usize,
    position_offset: usize,
    normal_offset: Option<usize>,
    uv_offset: Option<usize>,
    layout: String,
}

#[derive(Clone, Debug)]
struct IndexBuffer {
    indices: Vec<u32>,
    element_type: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct MaterialBinding {
    material_index: u32,
    texture_index: u32,
}

#[derive(Clone, Debug)]
struct VertexAttributeOffsets {
    position: Option<usize>,
    normal: Option<usize>,
    uv: Option<usize>,
    declaration: String,
}

fn find_geometry_buffers(
    node: &SerzNode,
    vertices: &mut Vec<VertexBuffer>,
    indices: &mut Vec<IndexBuffer>,
) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    if clean_name.contains("chcvertexbuffer") {
        if let Some((bytes, _fields)) = find_blob(children, "vertexdata") {
            let declared_vertex_count = find_number(children, &["numvertices"])
                .and_then(|value| usize::try_from(value).ok());
            let stride = find_number(children, &["vertexstride", "stride"])
                .and_then(|value| usize::try_from(value).ok())
                .or_else(|| {
                    declared_vertex_count
                        .filter(|count| *count > 0 && bytes.len() % count == 0)
                        .map(|count| bytes.len() / count)
                })
                .unwrap_or(32);
            let declaration = find_vertex_attribute_offsets(children);
            let position = find_number(children, &["positionoffset"])
                .and_then(|value| usize::try_from(value).ok())
                .or(declaration.position)
                .unwrap_or(0);
            let normal = find_number(children, &["normaloffset"])
                .and_then(|value| usize::try_from(value).ok())
                .or(declaration.normal)
                .or_else(|| (stride >= 24).then_some(12));
            let uv = find_number(children, &["texcoord0offset", "uvoffset"])
                .and_then(|value| usize::try_from(value).ok())
                .or(declaration.uv)
                .or_else(|| (stride >= 32).then_some(24));
            let layout = format!(
                "VertexData/cHcGeometryVertexDataDx/VertexBuffer/cHcVertexBufferDx: vertices={} stride={} position={} normal={:?} texcoord0={:?} declaration=[{}]",
                declared_vertex_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".into()),
                stride,
                position,
                normal,
                uv,
                declaration.declaration,
            );
            vertices.push(VertexBuffer {
                bytes,
                stride,
                position_offset: position,
                normal_offset: normal,
                uv_offset: uv,
                layout,
            });
        }
    }
    if clean_name.contains("chcindexbuffer") {
        if let Some(index_buffer) = find_index_data(children) {
            indices.push(index_buffer);
        }
    }
    for child in children {
        find_geometry_buffers(child, vertices, indices);
    }
}

fn find_vertex_attribute_offsets(children: &[SerzNode]) -> VertexAttributeOffsets {
    fn collect(
        node: &SerzNode,
        stream_offsets: &mut HashMap<u64, usize>,
        attributes: &mut VertexAttributeOffsets,
        declaration: &mut Vec<String>,
    ) {
        let SerzNode::Element { name, children, .. } = node else {
            return;
        };
        let clean_name = name.to_ascii_lowercase();
        if clean_name.contains("elementdecl") {
            let usage = find_text(children, &["usage"]).unwrap_or_default();
            let type_name = find_text(children, &["type"]).unwrap_or_default();
            let stream_index = find_number(children, &["streamindex"]).unwrap_or(0);
            let explicit_offset = find_number(children, &["offset", "byteoffset"])
                .and_then(|value| usize::try_from(value).ok());
            let offset =
                explicit_offset.unwrap_or_else(|| *stream_offsets.entry(stream_index).or_default());
            let size = vertex_element_size(type_name);
            if let Some(size) = size {
                stream_offsets.insert(stream_index, offset.saturating_add(size));
            }
            if stream_index == 0 {
                let usage_lower = usage.to_ascii_lowercase();
                if usage_lower.contains("position") && attributes.position.is_none() {
                    attributes.position = Some(offset);
                } else if usage_lower.contains("normal") && attributes.normal.is_none() {
                    attributes.normal = Some(offset);
                } else if usage_lower.contains("texcoord")
                    && find_number(children, &["usageindex"]).unwrap_or(0) == 0
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
        for child in children {
            collect(child, stream_offsets, attributes, declaration);
        }
    }

    let mut attributes = VertexAttributeOffsets {
        position: None,
        normal: None,
        uv: None,
        declaration: String::new(),
    };
    let mut stream_offsets = HashMap::new();
    let mut declaration = Vec::new();
    for child in children {
        collect(
            child,
            &mut stream_offsets,
            &mut attributes,
            &mut declaration,
        );
    }
    attributes.declaration = declaration.join(", ");
    attributes
}

fn vertex_element_size(type_name: &str) -> Option<usize> {
    let type_name = type_name.to_ascii_lowercase();
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

fn find_material_bindings(node: &SerzNode, textures: &[String]) -> HashMap<u32, MaterialBinding> {
    fn collect(
        node: &SerzNode,
        textures: &[String],
        next_index: &mut u32,
        bindings: &mut HashMap<u32, MaterialBinding>,
    ) {
        let SerzNode::Element {
            name, id, children, ..
        } = node
        else {
            return;
        };
        if name.eq_ignore_ascii_case("cHcEffectMaterialDx") {
            let material_index = *next_index;
            *next_index = (*next_index).saturating_add(1);
            if let Some(reference) = find_text(children, &["texturebyname"]) {
                let normalized = crate::auditor::normalize_texture_reference(reference);
                let texture_index = textures
                    .iter()
                    .position(|texture| texture.eq_ignore_ascii_case(&normalized))
                    .unwrap_or(0) as u32;
                bindings.insert(
                    *id,
                    MaterialBinding {
                        material_index,
                        texture_index,
                    },
                );
            }
        }
        for child in children {
            collect(child, textures, next_index, bindings);
        }
    }

    let mut bindings = HashMap::new();
    let mut next_index = 0;
    collect(node, textures, &mut next_index, &mut bindings);
    bindings
}

fn find_reference(children: &[SerzNode], expected: &str) -> Option<u32> {
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

fn find_submeshes(
    node: &SerzNode,
    inherited_material: Option<u32>,
    material_bindings: &HashMap<u32, MaterialBinding>,
    parent_transform: Mat4,
    inherited_lod: Option<u32>,
    inherited_excluded: bool,
    output: &mut Vec<RawSubmesh>,
) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    let node_label = direct_text(children, &["name", "nodename"]).unwrap_or_default();
    let lod = lod_from_name(name)
        .or_else(|| lod_from_name(node_label))
        .or_else(|| {
            clean_name
                .contains("lod")
                .then(|| find_number(children, &["lodlevel", "lodindex", "level"]))
                .flatten()
                .and_then(|level| u32::try_from(level).ok())
        })
        .or(inherited_lod);
    let excluded = inherited_excluded
        || lod.is_some_and(|level| level > 0)
        || is_helper_name(name)
        || is_helper_name(node_label);
    let transform = parent_transform * find_local_transform(children);
    let material_reference = if clean_name.contains("sgnodematerial") {
        find_reference(children, "material").or(inherited_material)
    } else {
        inherited_material
    };
    if clean_name.contains("primlist")
        || clean_name.contains("primitivelist")
        || clean_name.contains("batch")
        || clean_name.contains("primitive set desc")
        || clean_name.contains("primitivesetdescdx")
    {
        let is_primitive_set = clean_name.contains("primitivesetdescdx");
        let first = find_number(
            children,
            &["firstindex", "startindex", "indexstart", "indexarraystart"],
        );
        let topology = find_primitive_topology(node);
        let count = find_number(children, &["indexcount", "numindices"]).or_else(|| {
            find_number(children, &["primcount", "numprimitives"]).and_then(|value| {
                if is_primitive_set {
                    Some(value)
                } else {
                    match topology {
                        PrimitiveTopology::TriangleList => value.checked_mul(3),
                        PrimitiveTopology::TriangleStrip => value.checked_add(2),
                    }
                }
            })
        });
        if let (Some(first), Some(count)) = (first, count)
            && let (Ok(first_index), Ok(index_count)) = (u32::try_from(first), u32::try_from(count))
        {
            let material_index = find_number(children, &["materialindex", "materialid"])
                .and_then(|value| u32::try_from(value).ok())
                .or_else(|| {
                    material_reference
                        .and_then(|reference| material_bindings.get(&reference))
                        .map(|binding| binding.material_index)
                });
            let texture_index = material_reference
                .and_then(|reference| material_bindings.get(&reference))
                .map(|binding| binding.texture_index);
            let base_vertex = find_number(children, &["vertexindexbase", "basevertex"])
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or(0);
            let vertex_buffer_index = find_number(children, &["vertexbufferindex"])
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            let index_buffer_index = find_number(children, &["indexbufferindex"])
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
                topology,
                transform,
                excluded,
            });
        }
    }
    let mut next_lod = 0;
    for child in children {
        let child_lod =
            if clean_name.contains("lodlist") && matches!(child, SerzNode::Element { .. }) {
                let level = next_lod;
                next_lod += 1;
                Some(level)
            } else {
                None
            };
        find_submeshes(
            child,
            material_reference,
            material_bindings,
            transform,
            child_lod.or(lod),
            excluded,
            output,
        );
    }
}

fn direct_text<'a>(children: &'a [SerzNode], expected: &[&str]) -> Option<&'a str> {
    children.iter().find_map(|child| match child {
        SerzNode::Property { name, value, .. }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            Some(value.as_str())
        }
        SerzNode::Element { name, children, .. }
            if expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            children.iter().find_map(|property| match property {
                SerzNode::Property { value, .. } => Some(value.as_str()),
                _ => None,
            })
        }
        _ => None,
    })
}

fn lod_from_name(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let start = lower.find("lod")? + 3;
    let digits = lower[start..].trim_start_matches(|character: char| !character.is_ascii_digit());
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    (end > 0).then(|| digits[..end].parse().ok()).flatten()
}

fn is_helper_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["shadow", "volume", "collision", "occlusion"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn find_local_transform(children: &[SerzNode]) -> Mat4 {
    fn matrix_values(node: &SerzNode, values: &mut Vec<f32>) {
        match node {
            SerzNode::Property { value, .. } => {
                values.extend(
                    value
                        .split(|character: char| {
                            character.is_whitespace() || matches!(character, ',' | ';')
                        })
                        .filter_map(|part| part.parse::<f32>().ok()),
                );
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    matrix_values(child, values);
                }
            }
            _ => {}
        }
    }

    fn parse_matrix(node: &SerzNode) -> Option<Mat4> {
        let mut values = Vec::new();
        matrix_values(node, &mut values);
        let matrix = <[f32; 16]>::try_from(values).ok()?;
        if matrix.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let serialized = Mat4::from_cols_array(&matrix);
        let translation_is_last_column =
            matrix[3].abs() > f32::EPSILON
                || matrix[7].abs() > f32::EPSILON
                || matrix[11].abs() > f32::EPSILON;
        let translation_is_last_row =
            matrix[12].abs() > f32::EPSILON
                || matrix[13].abs() > f32::EPSILON
                || matrix[14].abs() > f32::EPSILON;
        Some(if translation_is_last_column && !translation_is_last_row {
            serialized.transpose()
        } else {
            serialized
        })
    }

    for child in children {
        match child {
            SerzNode::Property { name, .. } if name.to_ascii_lowercase().contains("transform") => {
                if let Some(matrix) = parse_matrix(child) {
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

fn find_index_data(children: &[SerzNode]) -> Option<IndexBuffer> {
    if let Some((values, type_name)) = find_index_array(children) {
        let width = width_from_type_name(&type_name)?;
        return Some(IndexBuffer {
            indices: values,
            element_type: type_name_from_width(width),
        });
    }
    let (bytes, _) = find_blob(children, "indexdata")?;
    let width = find_number(children, &["indexsize", "indexelementsize"])
        .and_then(index_width_from_number)
        .or_else(|| {
            find_text(children, &["indexformat", "indextype"]).and_then(index_width_from_type_name)
        })
        .unwrap_or(2);
    Some(IndexBuffer {
        indices: decode_indices(&bytes, width).ok()?,
        element_type: type_name_from_width(width),
    })
}

fn find_index_array(children: &[SerzNode]) -> Option<(Vec<u32>, String)> {
    for child in children {
        match child {
            SerzNode::Array {
                name,
                type_name,
                values,
            } if name.eq_ignore_ascii_case("indexdata") => {
                let parsed = values
                    .iter()
                    .map(|value| value.parse::<u32>().ok())
                    .collect::<Option<Vec<_>>>()?;
                return Some((parsed, type_name.clone()));
            }
            SerzNode::Element { name, children, .. } => {
                if name.eq_ignore_ascii_case("indexdata")
                    && let Some(found) = find_index_array(children)
                {
                    return Some(found);
                }
                if let Some(found) = find_index_array(children) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn width_from_type_name(type_name: &str) -> Option<usize> {
    index_width_from_type_name(type_name)
}

fn index_width_from_number(value: u64) -> Option<usize> {
    match value {
        2 | 16 => Some(2),
        4 | 32 => Some(4),
        _ => None,
    }
}

fn index_width_from_type_name(type_name: &str) -> Option<usize> {
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

fn type_name_from_width(width: usize) -> &'static str {
    if width == 4 { "u32" } else { "u16" }
}

fn find_primitive_topology(node: &SerzNode) -> PrimitiveTopology {
    match node {
        SerzNode::Property { name, value, .. }
            if name.eq_ignore_ascii_case("primitive")
                || name.eq_ignore_ascii_case("primitivetype")
                || name.eq_ignore_ascii_case("primtype")
                || name.eq_ignore_ascii_case("topology") =>
        {
            parse_primitive_topology(value)
        }
        SerzNode::Element { name, children, .. } => {
            if name.to_ascii_lowercase().contains("trianglestrip") {
                return PrimitiveTopology::TriangleStrip;
            }
            for child in children {
                let topology = find_primitive_topology(child);
                if topology == PrimitiveTopology::TriangleStrip {
                    return topology;
                }
            }
            PrimitiveTopology::TriangleList
        }
        _ => PrimitiveTopology::TriangleList,
    }
}

fn parse_primitive_topology(value: &str) -> PrimitiveTopology {
    if value.to_ascii_lowercase().contains("strip") {
        PrimitiveTopology::TriangleStrip
    } else {
        PrimitiveTopology::TriangleList
    }
}

fn convert_to_triangle_list(
    indices: &[u32],
    topology: PrimitiveTopology,
    index_element_type: &str,
) -> Vec<u32> {
    match topology {
        PrimitiveTopology::TriangleList => {
            let complete_index_count = indices.len() / 3 * 3;
            indices[..complete_index_count].to_vec()
        }
        PrimitiveTopology::TriangleStrip => {
            let restart_index = if index_element_type == "u16" {
                u16::MAX as u32
            } else {
                u32::MAX
            };
            let mut triangles = Vec::with_capacity(indices.len().saturating_sub(2) * 3);
            let mut strip = Vec::new();
            for &index in indices.iter().chain(std::iter::once(&restart_index)) {
                if index == restart_index {
                    append_strip_triangles(&strip, &mut triangles);
                    strip.clear();
                } else {
                    strip.push(index);
                }
            }
            triangles
        }
    }
}

fn append_strip_triangles(strip: &[u32], output: &mut Vec<u32>) {
    for (triangle_index, window) in strip.windows(3).enumerate() {
        let (a, b, c) = if triangle_index % 2 == 0 {
            (window[0], window[1], window[2])
        } else {
            (window[1], window[0], window[2])
        };
        if a != b && b != c && a != c {
            output.extend_from_slice(&[a, b, c]);
        }
    }
}

fn append_submesh(
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    material_index: u32,
    texture_index: u32,
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
        vertex_buffer_index,
        base_vertex,
        vertex_buffer_offset,
    });
    Ok(())
}

fn append_transformed_submesh(
    vertices: &mut Vec<GeoVertex>,
    indices: &mut Vec<u32>,
    submeshes: &mut Vec<GeoSubmesh>,
    triangles: Vec<u32>,
    source_vertices: &[GeoVertex],
    transform: Mat4,
    material_index: u32,
    texture_index: u32,
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
        vertex_buffer_index,
        base_vertex,
        vertex_buffer_offset,
    });
    Ok(())
}

fn find_blob(children: &[SerzNode], expected: &str) -> Option<(Vec<u8>, String)> {
    for child in children {
        match child {
            SerzNode::Element { name, children, .. } => {
                if name.eq_ignore_ascii_case(expected)
                    && let Some(data) = children.iter().find_map(|node| match node {
                        SerzNode::Blob { data } => Some(data.clone()),
                        _ => None,
                    })
                {
                    return Some((data, name.clone()));
                }
                if let Some(found) = find_blob(children, expected) {
                    return Some(found);
                }
            }
            SerzNode::Blob { data } if expected == "vertexdata" || expected == "indexdata" => {
                return Some((data.clone(), String::new()));
            }
            _ => {}
        }
    }
    None
}

fn find_number(children: &[SerzNode], expected: &[&str]) -> Option<u64> {
    find_text(children, expected)?.parse().ok()
}

fn find_text<'a>(children: &'a [SerzNode], expected: &[&str]) -> Option<&'a str> {
    for child in children {
        match child {
            SerzNode::Property { name, value, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                return Some(value);
            }
            SerzNode::Element { name, children, .. }
                if expected
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
            {
                if let Some(SerzNode::Property { value, .. }) = children.first() {
                    return Some(value);
                }
            }
            SerzNode::Element { children, .. } => {
                if let Some(value) = find_text(children, expected) {
                    return Some(value);
                }
            }
            _ => {}
        }
    }
    None
}

fn decode_indices(data: &[u8], width: usize) -> Result<Vec<u32>, String> {
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

pub(crate) fn log_parse_diagnostics(path: &Path, diagnostics: &GeoParseDiagnostics) {
    use std::io::Write;

    let ranges = if diagnostics.batch_ranges.is_empty() {
        "none".to_owned()
    } else {
        diagnostics.batch_ranges.join("; ")
    };
    let message = format!(
        "[GeoPcDx] {}: vertex_stride={} vertex_count={} vertex_layout=[{}] index_type={} raw_index_count={} index_layout=[{}] primitive_type={} batches=[{}]",
        path.display(),
        diagnostics.vertex_stride,
        diagnostics.vertex_count,
        diagnostics.vertex_layout,
        diagnostics.index_element_type,
        diagnostics.raw_index_count,
        diagnostics.index_layout,
        diagnostics.primitive_type,
        ranges,
    );
    let log_path = crate::settings::log_file();
    let result = (|| {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        )
    })();
    if let Err(error) = result {
        eprintln!(
            "Failed to write GeoPcDx diagnostics to {}: {error}",
            log_path.display()
        );
    }
}

fn read_vec3(data: &[u8], offset: usize) -> Result<[f32; 3], String> {
    let end = offset.checked_add(12).ok_or("vertex offset overflow")?;
    let bytes = data
        .get(offset..end)
        .ok_or("position outside vertex record")?;
    Ok([
        f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        f32::from_le_bytes(bytes[8..12].try_into().unwrap()),
    ])
}

fn read_vec2(data: &[u8], offset: usize) -> Result<[f32; 2], String> {
    let end = offset.checked_add(8).ok_or("vertex offset overflow")?;
    let bytes = data.get(offset..end).ok_or("UV outside vertex record")?;
    Ok([
        f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
    ])
}

fn fill_missing_normals(vertices: &mut [GeoVertex], indices: &[u32]) {
    let mut generated = vec![[0.0; 3]; vertices.len()];
    for triangle in indices.chunks_exact(3) {
        let a = vertices[triangle[0] as usize].position;
        let b = vertices[triangle[1] as usize].position;
        let c = vertices[triangle[2] as usize].position;
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &index in triangle {
            let vertex_index = index as usize;
            let vertex = &mut vertices[vertex_index];
            let normal_length = vertex.normal.iter().map(|value| value * value).sum::<f32>();
            if normal_length >= 1e-10 {
                continue;
            }
            for axis in 0..3 {
                generated[vertex_index][axis] += normal[axis];
            }
        }
    }
    for (vertex, generated_normal) in vertices.iter_mut().zip(generated) {
        let length = vertex
            .normal
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if length >= 1e-5 {
            for component in &mut vertex.normal {
                *component /= length;
            }
        } else {
            let generated_length = generated_normal
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            vertex.normal = if generated_length >= 1e-5 {
                generated_normal.map(|value| value / generated_length)
            } else {
                [0.0, 1.0, 0.0]
            };
        }
    }
}

pub fn resolve_texture_data(geo_path: &Path, reference: &str) -> Option<(String, Vec<u8>)> {
    let reference = reference
        .replace('/', "\\")
        .trim_start_matches('\\')
        .to_owned();
    let stem = Path::new(&reference);
    let relative = if stem.extension().is_some() {
        reference.clone()
    } else {
        format!("{reference}.TgPcDx")
    };
    let parent = geo_path.parent()?;
    let mut ancestors = parent.ancestors().take(6).collect::<Vec<_>>();
    if ancestors.is_empty() {
        ancestors.push(parent);
    }
    for ancestor in &ancestors {
        let candidate = ancestor.join(relative.replace('\\', std::path::MAIN_SEPARATOR_STR));
        if let Ok(bytes) = std::fs::read(&candidate) {
            return Some((candidate.display().to_string(), bytes));
        }
    }
    for ancestor in ancestors {
        let Ok(entries) = std::fs::read_dir(ancestor) else {
            continue;
        };
        for entry in entries.flatten() {
            let archive_path = entry.path();
            if !archive_path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
            {
                continue;
            }
            let Ok(file) = std::fs::File::open(&archive_path) else {
                continue;
            };
            let Ok(mut archive) = zip::ZipArchive::new(file) else {
                continue;
            };
            let target = crate::archive::normalize_member(&relative);
            let found = (0..archive.len()).find_map(|index| {
                let entry = archive.by_index(index).ok()?;
                let name = entry.name().ok()?.replace('/', "\\");
                let normalized = crate::archive::normalize_member(&name);
                (normalized == target || normalized.ends_with(&format!("\\{target}")))
                    .then_some((index, name))
            });
            if let Some((index, name)) = found
                && let Ok(mut member) = archive.by_index(index)
            {
                use std::io::Read;
                let mut bytes = Vec::new();
                if member.read_to_end(&mut bytes).is_ok() {
                    return Some((format!("{}!{name}", archive_path.display()), bytes));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle_vertices(count: usize) -> Vec<u8> {
        let mut vertices = Vec::new();
        for index in 0..count {
            let position = [index as f32, (index % 2) as f32, 0.0];
            for value in position
                .into_iter()
                .chain([0.0, 0.0, 1.0])
                .chain([0.0, 0.0])
            {
                vertices.extend_from_slice(&value.to_le_bytes());
            }
        }
        vertices
    }

    fn xml(vertices: &[u8], indices: &[u8], stride: usize) -> String {
        let vertex_hex = vertices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let index_hex = indices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer><VertexStride d:type="sUInt32">{stride}</VertexStride>
                    <VertexData><blob>{vertex_hex}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer><IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        )
    }

    #[test]
    fn parses_mesh_and_texture_reference() {
        let vertices = triangle_vertices(3);
        let model = GeoModel::parse(&xml(&vertices, &[0, 0, 1, 0, 2, 0], 32).into_bytes()).unwrap();
        assert_eq!(model.vertices.len(), 3);
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.textures, [r"Textures\sample.TgPcDx"]);
    }

    #[test]
    fn rejects_indices_outside_vertex_buffer() {
        let mut vertices = Vec::new();
        for _ in 0..3 {
            vertices.extend_from_slice(&[0u8; 32]);
        }
        assert!(GeoModel::parse(&xml(&vertices, &[0, 0, 1, 0, 3, 0], 32).into_bytes()).is_err());
    }

    #[test]
    fn rejects_truncated_vertex_records() {
        assert!(GeoModel::parse(&xml(&[0; 31], &[0, 0, 1, 0, 2, 0], 32).into_bytes()).is_err());
    }

    #[test]
    fn truncates_incomplete_triangle_list_indices_and_odd_byte_padding() {
        let vertices = triangle_vertices(4);
        let raw_indices = [0, 0, 1, 0, 2, 0, 3, 0, 0xff];
        let model = GeoModel::parse(&xml(&vertices, &raw_indices, 32).into_bytes()).unwrap();
        assert_eq!(model.diagnostics.index_element_type, "u16");
        assert_eq!(model.diagnostics.raw_index_count, 4);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn converts_triangle_strips_with_alternating_winding_and_batch_ranges() {
        let vertices = triangle_vertices(6);
        let batch = r#"
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">1</StartIndex>
                <IndexCount d:type="sUInt32">4</IndexCount>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
                <MaterialIndex d:type="sUInt32">2</MaterialIndex>
            </cHcPrimitiveList>
        "#;
        let source = xml(&vertices, &[5, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0], 32)
            .replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3]);
        assert_eq!(
            model.submeshes,
            [GeoSubmesh {
                first_index: 0,
                index_count: 6,
                material_index: 2,
                texture_index: 0,
                vertex_buffer_index: 0,
                base_vertex: 0,
                vertex_buffer_offset: 0,
            }]
        );
        assert_eq!(
            model.diagnostics.primitive_type,
            PrimitiveTopology::TriangleStrip
        );
        assert!(model.diagnostics.batch_ranges[0].contains("start=1 count=4"));
    }

    #[test]
    fn preserves_primitive_set_materials_and_base_vertex_offsets() {
        let batches_and_materials = r#"
            <cHcSGNodeMaterial>
                <Material d:type="ref">7</Material>
                <cHcPrimitiveSetDescDx>
                    <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                    <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                    <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
                </cHcPrimitiveSetDescDx>
            </cHcSGNodeMaterial>
            <cHcSGNodeMaterial>
                <Material d:type="ref">8</Material>
                <cHcPrimitiveSetDescDx>
                    <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                    <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                    <VertexIndexBase d:type="sUInt32">3</VertexIndexBase>
                    <IndexArrayStart d:type="sUInt32">3</IndexArrayStart>
                </cHcPrimitiveSetDescDx>
            </cHcSGNodeMaterial>
            <cHcEffectMaterialDx d:id="7">
                <TextureByName><e d:type="cDeltaString">Textures\first.TgPcDx</e></TextureByName>
            </cHcEffectMaterialDx>
            <cHcEffectMaterialDx d:id="8">
                <TextureByName><e d:type="cDeltaString">Textures\second.TgPcDx</e></TextureByName>
            </cHcEffectMaterialDx>
        "#;
        let source = xml(
            &triangle_vertices(9),
            &[0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0],
            32,
        )
        .replace("</Root>", &format!("{batches_and_materials}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 2);
        assert_eq!(model.indices, [0, 1, 2, 6, 7, 8]);
        let first_texture = model
            .textures
            .iter()
            .position(|texture| texture.eq_ignore_ascii_case(r"Textures\first.TgPcDx"))
            .unwrap() as u32;
        let second_texture = model
            .textures
            .iter()
            .position(|texture| texture.eq_ignore_ascii_case(r"Textures\second.TgPcDx"))
            .unwrap() as u32;
        assert_eq!(model.submeshes[0].texture_index, first_texture);
        assert_eq!(model.submeshes[1].texture_index, second_texture);
        assert_eq!(model.submeshes[1].base_vertex, 3);
    }

    #[test]
    fn keeps_only_lod_zero_and_excludes_helper_batches() {
        let batches = r#"
            <LOD0><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></LOD0>
            <LOD1><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></LOD1>
            <softshadow><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></softshadow>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.diagnostics.batch_ranges.len(), 1);
    }

    #[test]
    fn applies_parent_transform_to_batch_vertices() {
        let transform = r#"
            <cHcTransformNode>
                <Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 3 -2 4 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcTransformNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{transform}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        let transformed_position = model.vertices[model.indices[0] as usize].position;

        assert_eq!(transformed_position, [3.0, -2.0, 4.0]);

        let row_major = transform.replace(
            "1 0 0 0 0 1 0 0 0 0 1 0 3 -2 4 1",
            "1 0 0 3 0 1 0 -2 0 0 1 4 0 0 0 1",
        );
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{row_major}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(
            model.vertices[model.indices[0] as usize].position,
            [3.0, -2.0, 4.0]
        );
    }

    #[test]
    fn skips_degenerate_triangles_in_triangle_strips() {
        assert_eq!(
            convert_to_triangle_list(&[0, 1, 2, 2, 3, 4], PrimitiveTopology::TriangleStrip, "u16"),
            [0, 1, 2, 3, 2, 4]
        );
    }

    #[test]
    fn decodes_u32_index_blobs_and_primitive_count_strip_ranges() {
        let index_data = [0u32, 1, 2, 3]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut source = xml(&triangle_vertices(4), &index_data, 32).replace(
            "<IndexSize d:type=\"sUInt32\">2</IndexSize>",
            "<IndexSize d:type=\"sUInt32\">32</IndexSize>",
        );
        let batch = r#"
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <PrimCount d:type="sUInt32">2</PrimCount>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
            </cHcPrimitiveList>
        "#;
        source = source.replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(model.diagnostics.index_element_type, "u32");
        assert_eq!(model.diagnostics.raw_index_count, 4);
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3]);
    }

    #[test]
    fn reads_index_data_arrays_from_serz_ast() {
        let mut source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32);
        source = source.replace(
            "<IndexData><blob>000001000200</blob></IndexData>",
            "<IndexData d:numElements=\"3\" d:elementType=\"sUInt16\">0 1 2</IndexData>",
        );
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.diagnostics.index_element_type, "u16");
    }

    #[test]
    fn resolves_texture_from_neighboring_ap_archive() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let product = directory
            .path()
            .join("Assets")
            .join("Provider")
            .join("Product");
        let geo_dir = product.join("Models");
        std::fs::create_dir_all(&geo_dir).unwrap();
        let geo_path = geo_dir.join("model.GeoPcDx");
        std::fs::write(&geo_path, b"").unwrap();

        let archive_path = product.join("MainContent.ap");
        let mut archive = zip::ZipWriter::new(std::fs::File::create(archive_path).unwrap());
        archive
            .start_file(
                "Provider/Product/Textures/body.TgPcDx",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"texture data").unwrap();
        archive.finish().unwrap();

        let (resolved_path, bytes) =
            resolve_texture_data(&geo_path, r"Textures\body.TgPcDx").unwrap();
        assert!(resolved_path.contains("!Provider\\Product\\Textures\\body.TgPcDx"));
        assert_eq!(bytes, b"texture data");
    }
}
