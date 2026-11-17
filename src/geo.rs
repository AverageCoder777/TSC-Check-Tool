use crate::serz::{SerzNode, SerzReader};
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
}

#[derive(Clone, Debug)]
pub struct GeoParseDiagnostics {
    pub vertex_stride: usize,
    pub vertex_count: usize,
    pub index_element_type: &'static str,
    pub raw_index_count: usize,
    pub primitive_type: PrimitiveTopology,
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
    material_index: u32,
    topology: PrimitiveTopology,
}

impl GeoModel {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        let root = if data.starts_with(b"SERZ") {
            SerzReader::new(data).parse()?
        } else {
            let xml = std::str::from_utf8(data).map_err(|error| error.to_string())?;
            SerzNode::from_xml_str(xml)?.0
        };
        let mut vertex_data = None;
        let mut index_data = None;
        let mut xml = String::new();
        find_geometry_buffers(&root, &mut vertex_data, &mut index_data);
        xml.push_str(&SerzReader::to_xml(&root, 0, true));

        let (vertex_bytes, stride, position_offset, normal_offset, uv_offset) =
            vertex_data.ok_or("GeoPcDx has no supported vertex buffer")?;
        let (raw_indices, index_element_type) =
            index_data.ok_or("GeoPcDx has no supported index buffer")?;
        if stride < 12
            || position_offset
                .checked_add(12)
                .is_none_or(|end| end > stride)
        {
            return Err("invalid vertex stride or position offset".into());
        }
        let vertex_count = vertex_bytes.len() / stride;
        if vertex_count == 0 || vertex_bytes.len() % stride != 0 {
            return Err("vertex buffer length is not a nonzero multiple of the stride".into());
        }

        let mut vertices = Vec::with_capacity(vertex_count);
        for record in vertex_bytes.chunks_exact(stride) {
            let position = read_vec3(record, position_offset)?;
            let normal = normal_offset
                .filter(|offset| offset.checked_add(12).is_some_and(|end| end <= stride))
                .and_then(|offset| read_vec3(record, offset).ok())
                .unwrap_or([0.0, 0.0, 0.0]);
            let uv = uv_offset
                .filter(|offset| offset.checked_add(8).is_some_and(|end| end <= stride))
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

        let xml = if data.starts_with(b"SERZ") {
            xml
        } else {
            String::from_utf8_lossy(data).into_owned()
        };
        let textures = crate::auditor::extract_geo_texture_references(&xml);
        let mut raw_batches = Vec::new();
        find_submeshes(&root, &mut raw_batches);
        let has_batch_ranges = !raw_batches.is_empty();
        let primitive_type = if has_batch_ranges {
            raw_batches[0].topology
        } else {
            find_primitive_topology(&root)
        };
        let batch_ranges = raw_batches
            .iter()
            .map(|batch| {
                format!(
                    "start={} count={} topology={} material={}",
                    batch.first_index, batch.index_count, batch.topology, batch.material_index
                )
            })
            .collect::<Vec<_>>();
        let mut indices = Vec::new();
        let mut submeshes = Vec::new();
        if has_batch_ranges {
            for batch in &raw_batches {
                let Some(end) = batch.first_index.checked_add(batch.index_count) else {
                    continue;
                };
                let Some(range) = raw_indices.get(batch.first_index as usize..end as usize) else {
                    continue;
                };
                let triangles = convert_to_triangle_list(range, batch.topology, index_element_type);
                append_submesh(
                    &mut indices,
                    &mut submeshes,
                    triangles,
                    batch.material_index,
                    vertices.len(),
                )?;
            }
        }
        if submeshes.is_empty() {
            let triangles =
                convert_to_triangle_list(&raw_indices, primitive_type, index_element_type);
            append_submesh(&mut indices, &mut submeshes, triangles, 0, vertices.len())?;
        }
        if indices.is_empty() {
            return Err("index buffer contains no complete, valid triangles".into());
        }
        fill_missing_normals(&mut vertices, &indices);
        let diagnostics = GeoParseDiagnostics {
            vertex_stride: stride,
            vertex_count,
            index_element_type,
            raw_index_count: raw_indices.len(),
            primitive_type,
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

type VertexBuffer = (Vec<u8>, usize, usize, Option<usize>, Option<usize>);
type IndexBuffer = (Vec<u32>, &'static str);

fn find_geometry_buffers(
    node: &SerzNode,
    vertices: &mut Option<VertexBuffer>,
    indices: &mut Option<IndexBuffer>,
) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    if vertices.is_none() && clean_name.contains("vertexbuffer") {
        if let Some((bytes, fields)) = find_blob(children, "vertexdata") {
            let stride = find_number(children, &["vertexstride", "stride"])
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(32);
            let position = find_number(children, &["positionoffset"])
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            let normal = find_number(children, &["normaloffset"])
                .and_then(|value| usize::try_from(value).ok())
                .or_else(|| (stride >= 24).then_some(12));
            let uv = find_number(children, &["texcoord0offset", "uvoffset"])
                .and_then(|value| usize::try_from(value).ok())
                .or_else(|| (stride >= 32).then_some(24));
            let _ = fields;
            *vertices = Some((bytes, stride, position, normal, uv));
        }
    }
    if indices.is_none() && clean_name.contains("indexbuffer") {
        if let Some((values, element_type)) = find_index_data(children) {
            *indices = Some((values, element_type));
        }
    }
    for child in children {
        find_geometry_buffers(child, vertices, indices);
    }
}

fn find_submeshes(node: &SerzNode, output: &mut Vec<RawSubmesh>) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    if clean_name.contains("primlist")
        || clean_name.contains("primitivelist")
        || clean_name.contains("batch")
    {
        let first = find_number(children, &["firstindex", "startindex", "indexstart"]);
        let topology = find_primitive_topology(node);
        let count = find_number(children, &["indexcount", "numindices"]).or_else(|| {
            find_number(children, &["primcount", "numprimitives"]).and_then(
                |value| match topology {
                    PrimitiveTopology::TriangleList => value.checked_mul(3),
                    PrimitiveTopology::TriangleStrip => value.checked_add(2),
                },
            )
        });
        if let (Some(first), Some(count)) = (first, count)
            && let (Ok(first_index), Ok(index_count)) = (u32::try_from(first), u32::try_from(count))
        {
            let material_index = find_number(children, &["materialindex", "materialid"])
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            output.push(RawSubmesh {
                first_index,
                index_count,
                material_index,
                topology,
            });
        }
    }
    for child in children {
        find_submeshes(child, output);
    }
}

fn find_index_data(children: &[SerzNode]) -> Option<IndexBuffer> {
    if let Some((values, type_name)) = find_index_array(children) {
        let width = width_from_type_name(&type_name)?;
        return Some((values, type_name_from_width(width)));
    }
    let (bytes, _) = find_blob(children, "indexdata")?;
    let width = find_number(children, &["indexsize", "indexelementsize"])
        .and_then(index_width_from_number)
        .or_else(|| {
            find_text(children, &["indexformat", "indextype"]).and_then(index_width_from_type_name)
        })
        .unwrap_or(2);
    Some((
        decode_indices(&bytes, width).ok()?,
        type_name_from_width(width),
    ))
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
    vertex_count: usize,
) -> Result<(), String> {
    if let Some(index) = triangles.iter().find(|&&index| index as usize >= vertex_count) {
        return Err(format!(
            "index buffer references vertex {index}, but only {vertex_count} vertices were parsed"
        ));
    }
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
        "[GeoPcDx] {}: vertex_stride={} vertex_count={} index_type={} raw_index_count={} primitive_type={} batches=[{}]",
        path.display(),
        diagnostics.vertex_stride,
        diagnostics.vertex_count,
        diagnostics.index_element_type,
        diagnostics.raw_index_count,
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
    if vertices
        .iter()
        .any(|vertex| vertex.normal.iter().any(|component| *component != 0.0))
    {
        return;
    }
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
            let vertex = &mut vertices[index as usize];
            for axis in 0..3 {
                vertex.normal[axis] += normal[axis];
            }
        }
    }
    for vertex in vertices {
        let length = vertex
            .normal
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if length > f32::EPSILON {
            for component in &mut vertex.normal {
                *component /= length;
            }
        } else {
            vertex.normal = [0.0, 1.0, 0.0];
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
        let source = xml(
            &vertices,
            &[5, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0],
            32,
        )
        .replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3]);
        assert_eq!(
            model.submeshes,
            [GeoSubmesh {
                first_index: 0,
                index_count: 6,
                material_index: 2,
            }]
        );
        assert_eq!(
            model.diagnostics.primitive_type,
            PrimitiveTopology::TriangleStrip
        );
        assert!(model.diagnostics.batch_ranges[0].contains("start=1 count=4"));
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
