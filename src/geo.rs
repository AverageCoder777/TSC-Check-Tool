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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeoSubmesh {
    pub first_index: u32,
    pub index_count: u32,
    pub material_index: u32,
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
        let indices = index_data.ok_or("GeoPcDx has no supported index buffer")?;
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

        if indices.is_empty() || indices.len() % 3 != 0 {
            return Err("index buffer does not contain complete triangles".into());
        }
        if indices
            .iter()
            .any(|&index| index as usize >= vertices.len())
        {
            return Err("index buffer references a vertex outside the vertex buffer".into());
        }
        fill_missing_normals(&mut vertices, &indices);

        let xml = if data.starts_with(b"SERZ") {
            xml
        } else {
            String::from_utf8_lossy(data).into_owned()
        };
        let textures = crate::auditor::extract_geo_texture_references(&xml);
        let mut submeshes = Vec::new();
        find_submeshes(&root, &mut submeshes);
        submeshes.retain(|submesh| {
            submesh.index_count > 0
                && submesh.index_count % 3 == 0
                && submesh.first_index % 3 == 0
                && submesh
                    .first_index
                    .checked_add(submesh.index_count)
                    .is_some_and(|end| end as usize <= indices.len())
        });
        if submeshes.is_empty() {
            submeshes.push(GeoSubmesh {
                first_index: 0,
                index_count: indices.len() as u32,
                material_index: 0,
            });
        }
        Ok(Self {
            vertices,
            indices,
            textures,
            submeshes,
        })
    }
}

type VertexBuffer = (Vec<u8>, usize, usize, Option<usize>, Option<usize>);

fn find_geometry_buffers(
    node: &SerzNode,
    vertices: &mut Option<VertexBuffer>,
    indices: &mut Option<Vec<u32>>,
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
        if let Some((bytes, _)) = find_blob(children, "indexdata") {
            let index_width = find_number(children, &["indexsize", "indexelementsize"])
                .and_then(|value| usize::try_from(value).ok())
                .filter(|&value| matches!(value, 2 | 4))
                .or_else(|| {
                    find_text(children, &["indexformat"]).map(|text| {
                        if text.to_ascii_lowercase().contains("32") {
                            4
                        } else {
                            2
                        }
                    })
                })
                .unwrap_or(2);
            *indices = decode_indices(&bytes, index_width).ok();
        }
    }
    for child in children {
        find_geometry_buffers(child, vertices, indices);
    }
}

fn find_submeshes(node: &SerzNode, output: &mut Vec<GeoSubmesh>) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    if clean_name.contains("primlist") || clean_name.contains("batch") {
        let first = find_number(children, &["firstindex", "startindex", "indexstart"]);
        let count = find_number(children, &["indexcount", "numindices"]).or_else(|| {
            find_number(children, &["numprimitives"]).and_then(|value| value.checked_mul(3))
        });
        if let (Some(first), Some(count)) = (first, count)
            && let (Ok(first_index), Ok(index_count)) = (u32::try_from(first), u32::try_from(count))
        {
            let material_index = find_number(children, &["materialindex", "materialid"])
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            output.push(GeoSubmesh {
                first_index,
                index_count,
                material_index,
            });
        }
    }
    for child in children {
        find_submeshes(child, output);
    }
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
    if !matches!(width, 2 | 4) || data.len() % width != 0 {
        return Err("invalid index buffer byte length".into());
    }
    Ok(data
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
        let mut vertices = Vec::new();
        for (position, uv) in [
            ([0.0f32, 0.0, 0.0], [0.0f32, 0.0]),
            ([1.0, 0.0, 0.0], [1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 1.0]),
        ] {
            for value in position.into_iter().chain([0.0, 0.0, 1.0]).chain(uv) {
                vertices.extend_from_slice(&value.to_le_bytes());
            }
        }
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
