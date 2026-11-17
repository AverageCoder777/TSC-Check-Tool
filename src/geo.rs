use crate::serz::{SerzNode, SerzReader};
use glam::{Mat4, Vec3};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

#[derive(Clone)]
pub struct GeoAssemblyPart {
    pub path: PathBuf,
    pub model: Arc<GeoModel>,
    pub transform: Mat4,
    pub blueprint_attached: bool,
}

pub struct GeoAssembly {
    pub parts: Vec<GeoAssemblyPart>,
    pub has_blueprint_attachments: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeoSubmesh {
    pub first_index: u32,
    pub index_count: u32,
    pub material_index: u32,
    pub texture_index: u32,
    pub render_mode: GeoRenderMode,
    pub vertex_buffer_index: u32,
    pub base_vertex: i32,
    pub vertex_buffer_offset: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
        let root = parse_serz_document(data)?;
        let id_map = build_id_map(&root);
        let local_transforms = find_named_values(&root, "SourceLToPTransform")
            .into_iter()
            .flatten()
            .flat_map(collect_float_values)
            .collect::<Vec<_>>()
            .chunks_exact(16)
            .map(|values| {
                if values.iter().all(|value| value.is_finite()) {
                    Mat4::from_cols_array(values.try_into().expect("16-value matrix chunk"))
                } else {
                    Mat4::IDENTITY
                }
            })
            .collect::<Vec<_>>();
        let transform_dependencies = find_named_values(&root, "TransformDependency")
            .into_iter()
            .flatten()
            .flat_map(collect_i32_values)
            .collect::<Vec<_>>();
        let world_transforms = compute_world_transforms(&local_transforms, &transform_dependencies);
        let mut vertex_data = Vec::new();
        let mut index_data = Vec::new();
        let mut xml = String::new();
        find_geometry_buffers(&root, &id_map, &mut vertex_data, &mut index_data);
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
        let material_bindings = find_material_bindings(&root, &textures, &id_map);
        let mut raw_batches = Vec::new();
        let mut visited_submesh_refs = HashSet::new();
        find_submeshes(
            &root,
            None,
            &material_bindings,
            &id_map,
            Mat4::IDENTITY,
            &world_transforms,
            None,
            None,
            false,
            &mut visited_submesh_refs,
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
                let render_mode = binding
                    .map(|binding| binding.render_mode)
                    .unwrap_or_default();
                let triangles =
                    convert_to_triangle_list(range, batch.topology, index_buffer.element_type);
                if batch.transform == Mat4::IDENTITY {
                    append_submesh(
                        &mut indices,
                        &mut submeshes,
                        triangles,
                        material_index,
                        texture_index,
                        render_mode,
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
                        batch.vertex_buffer_index,
                        batch.base_vertex,
                        vertex_buffer_offset,
                    )?;
                }
            }
        }
        if !has_batch_ranges && submeshes.is_empty() {
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
                GeoRenderMode::Opaque,
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
            local_transform_count: local_transforms.len(),
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

impl GeoAssembly {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let primary_model = Arc::new(GeoModel::parse(&bytes)?);
        log_parse_diagnostics(path, &primary_model.diagnostics);
        let mut parts = vec![GeoAssemblyPart {
            path: path.to_path_buf(),
            model: primary_model,
            transform: Mat4::IDENTITY,
            blueprint_attached: false,
        }];
        let mut geometry_cache = HashMap::new();
        geometry_cache.insert(
            path.to_string_lossy().to_ascii_lowercase(),
            Arc::clone(&parts[0].model),
        );
        let mut linked_geometry = Vec::new();
        if let Some((blueprint_path, blueprint_document, blueprint_root)) = find_geometry_blueprint(
            path,
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
        ) {
            let id_map = build_id_map(&blueprint_document);
            let mut attachments = Vec::new();
            collect_blueprint_attachments(&blueprint_root, &id_map, false, false, &mut attachments);
            for attachment in attachments {
                let Some(blueprint_path) =
                    resolve_asset_path(&blueprint_path, &attachment.blueprint_reference, "bin")
                else {
                    log_assembly_warning(
                        path,
                        &format!(
                            "Unable to resolve {} blueprint '{}'",
                            attachment.kind, attachment.blueprint_reference
                        ),
                    );
                    continue;
                };
                let blueprint_bytes = match std::fs::read(&blueprint_path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        log_assembly_warning(
                            path,
                            &format!(
                                "Unable to read attachment blueprint {}: {error}",
                                blueprint_path.display()
                            ),
                        );
                        continue;
                    }
                };
                let child_root = match parse_serz_document(&blueprint_bytes) {
                    Ok(root) => root,
                    Err(error) => {
                        log_assembly_warning(
                            path,
                            &format!(
                                "Unable to parse attachment blueprint {}: {error}",
                                blueprint_path.display()
                            ),
                        );
                        continue;
                    }
                };
                let child_ids = build_id_map(&child_root);
                let Some(geometry_reference) =
                    find_named_text(&child_root, &["geometryid"], &child_ids)
                else {
                    log_assembly_warning(
                        path,
                        &format!(
                            "Attachment blueprint {} has no GeometryID",
                            blueprint_path.display()
                        ),
                    );
                    continue;
                };
                let Some(geometry_path) =
                    resolve_asset_path(&blueprint_path, &geometry_reference, "GeoPcDx")
                else {
                    log_assembly_warning(
                        path,
                        &format!(
                            "Unable to resolve attachment geometry '{}' from {}",
                            geometry_reference,
                            blueprint_path.display()
                        ),
                    );
                    continue;
                };
                let Some(model) = load_cached_geometry(&geometry_path, path, &mut geometry_cache)
                else {
                    continue;
                };
                linked_geometry.push(geometry_path.clone());
                parts.push(GeoAssemblyPart {
                    path: geometry_path,
                    model,
                    transform: attachment.transform,
                    blueprint_attached: true,
                });
            }
        }

        for sibling in find_sibling_part_geometries(path) {
            if linked_geometry
                .iter()
                .any(|linked| same_path(linked, &sibling))
            {
                continue;
            }
            if let Some(model) = load_cached_geometry(&sibling, path, &mut geometry_cache) {
                parts.push(GeoAssemblyPart {
                    path: sibling,
                    model,
                    transform: Mat4::IDENTITY,
                    blueprint_attached: false,
                });
            }
        }
        let has_blueprint_attachments = parts.iter().any(|part| part.blueprint_attached);
        Ok(Self {
            parts,
            has_blueprint_attachments,
        })
    }

    pub fn compose(
        &self,
        side_by_side: bool,
    ) -> Result<(GeoModel, Vec<(PathBuf, String)>), String> {
        let primary = self
            .parts
            .first()
            .ok_or("assembly has no primary geometry")?;
        let (primary_min, primary_max) = model_bounds(&primary.model, Mat4::IDENTITY);
        let mut next_x = primary_max.x;
        let mut diagnostics = primary.model.diagnostics.clone();
        diagnostics.vertex_count = 0;
        diagnostics.raw_index_count = 0;
        diagnostics.batch_ranges.clear();
        let mut result = GeoModel {
            vertices: Vec::new(),
            indices: Vec::new(),
            textures: Vec::new(),
            submeshes: Vec::new(),
            diagnostics,
        };
        let mut texture_sources = Vec::new();
        for (part_index, part) in self.parts.iter().enumerate() {
            if part_index > 0 && !side_by_side && !part.blueprint_attached {
                continue;
            }
            let mut transform = if part_index == 0 {
                Mat4::IDENTITY
            } else if side_by_side || !transform_has_translation(part.transform) {
                let (part_min, _) = model_bounds(&part.model, Mat4::IDENTITY);
                Mat4::from_translation(Vec3::new(
                    next_x + 1.5 - part_min.x,
                    primary_min.y - part_min.y,
                    0.0,
                ))
            } else {
                part.transform
            };
            if !transform.is_finite() {
                transform = Mat4::IDENTITY;
            }
            let (_, part_max) = model_bounds(&part.model, transform);
            let vertex_offset =
                u32::try_from(result.vertices.len()).map_err(|_| "too many assembly vertices")?;
            let index_offset =
                u32::try_from(result.indices.len()).map_err(|_| "too many assembly indices")?;
            let texture_offset =
                u32::try_from(result.textures.len()).map_err(|_| "too many assembly textures")?;
            let normal_transform = if transform.determinant().abs() > f32::EPSILON {
                transform.inverse().transpose()
            } else {
                Mat4::IDENTITY
            };
            let mut transformed_vertices = Vec::with_capacity(part.model.vertices.len());
            let mut invalid_position = false;
            for vertex in &part.model.vertices {
                let mut transformed = *vertex;
                transformed.position = transform
                    .transform_point3(Vec3::from_array(vertex.position))
                    .to_array();
                if transformed.position.iter().any(|value| !value.is_finite()) {
                    invalid_position = true;
                    break;
                }
                transformed.normal = normal_transform
                    .transform_vector3(Vec3::from_array(vertex.normal))
                    .normalize_or_zero()
                    .to_array();
                transformed_vertices.push(transformed);
            }
            if invalid_position {
                if part_index == 0 {
                    return Err("primary geometry transform produced invalid positions".into());
                }
                log_assembly_warning(
                    &part.path,
                    "Skipping attachment whose transform produced invalid positions",
                );
                continue;
            }
            next_x = next_x.max(part_max.x);
            result.vertices.extend(transformed_vertices);
            result.indices.extend(
                part.model
                    .indices
                    .iter()
                    .map(|index| {
                        index
                            .checked_add(vertex_offset)
                            .ok_or("assembly index overflow")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
            for submesh in &part.model.submeshes {
                let mut submesh = *submesh;
                submesh.first_index = submesh
                    .first_index
                    .checked_add(index_offset)
                    .ok_or("assembly index overflow")?;
                submesh.texture_index = submesh
                    .texture_index
                    .checked_add(texture_offset)
                    .ok_or("assembly texture index overflow")?;
                result.submeshes.push(submesh);
            }
            result.textures.extend(part.model.textures.iter().cloned());
            texture_sources.extend(
                part.model
                    .textures
                    .iter()
                    .map(|reference| (part.path.clone(), reference.clone())),
            );
            result.diagnostics.batch_ranges.extend(
                part.model
                    .diagnostics
                    .batch_ranges
                    .iter()
                    .map(|range| format!("{}: {range}", part.path.display())),
            );
            result.diagnostics.raw_index_count = result
                .diagnostics
                .raw_index_count
                .saturating_add(part.model.diagnostics.raw_index_count);
        }
        result.diagnostics.vertex_count = result.vertices.len();
        Ok((result, texture_sources))
    }
}

fn parse_serz_document(data: &[u8]) -> Result<SerzNode, String> {
    if data.starts_with(b"SERZ") {
        SerzReader::new(data).parse()
    } else {
        let xml = std::str::from_utf8(data).map_err(|error| error.to_string())?;
        SerzNode::from_xml_str(xml).map(|(root, _)| root)
    }
}

#[derive(Clone)]
struct AttachmentBlueprintRef {
    kind: &'static str,
    blueprint_reference: String,
    transform: Mat4,
}

fn find_geometry_blueprint(
    path: &Path,
    geometry_stem: &str,
) -> Option<(PathBuf, SerzNode, SerzNode)> {
    let mut candidates = std::fs::read_dir(path.parent()?)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    for candidate in candidates {
        let Ok(bytes) = std::fs::read(&candidate) else {
            continue;
        };
        let Ok(root) = parse_serz_document(&bytes) else {
            continue;
        };
        let id_map = build_id_map(&root);
        if let Some(blueprint) = matching_geometry_blueprint(&root, geometry_stem, &id_map) {
            return Some((candidate, root, blueprint));
        }
    }
    None
}

fn build_id_map(root: &SerzNode) -> HashMap<u32, &SerzNode> {
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

fn resolve_ref_node<'a>(
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

fn matching_geometry_blueprint(
    node: &SerzNode,
    geometry_stem: &str,
    ids: &HashMap<u32, &SerzNode>,
) -> Option<SerzNode> {
    let SerzNode::Element { name, children, .. } = node else {
        return None;
    };
    let name = name.to_ascii_lowercase();
    if ["cengineblueprint", "cwagonblueprint", "ctenderblueprint"]
        .iter()
        .any(|blueprint_type| name.contains(blueprint_type))
        && ["geometryid", "collisiongeometryid"]
            .iter()
            .any(|property| {
                find_named_text(node, &[*property], ids).is_some_and(|reference| {
                    asset_reference_stem(&reference).eq_ignore_ascii_case(geometry_stem)
                })
            })
    {
        return Some(node.clone());
    }
    children
        .iter()
        .find_map(|child| matching_geometry_blueprint(child, geometry_stem, ids))
}

fn asset_reference_stem(reference: &str) -> String {
    Path::new(&reference.replace('\\', "/"))
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn find_named_text(
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

fn find_asset_path_text(node: &SerzNode) -> Option<String> {
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

fn direct_named_text(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<String> {
    let SerzNode::Element { children, .. } = node else {
        return None;
    };
    children.iter().find_map(|child| match child {
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
        SerzNode::Element { name, .. }
            if names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate)) =>
        {
            find_named_text(child, names, ids)
        }
        _ => None,
    })
}

fn collect_blueprint_attachments(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    in_bogey: bool,
    in_children: bool,
    output: &mut Vec<AttachmentBlueprintRef>,
) {
    collect_blueprint_attachments_inner(
        node,
        ids,
        in_bogey,
        in_children,
        &mut HashSet::new(),
        output,
    );
}

fn collect_blueprint_attachments_inner(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    in_bogey: bool,
    in_children: bool,
    reference_stack: &mut HashSet<u32>,
    output: &mut Vec<AttachmentBlueprintRef>,
) {
    if let SerzNode::Reference { name, target_id } = node {
        let name = name.to_ascii_lowercase();
        let in_bogey = in_bogey || name.contains("bogey") || name.contains("bogie");
        let in_children = in_children
            || name.contains("schild")
            || name.contains("childentity")
            || name == "children";
        if reference_stack.insert(*target_id) {
            if let Some(target) = ids.get(target_id) {
                collect_blueprint_attachments_inner(
                    target,
                    ids,
                    in_bogey,
                    in_children,
                    reference_stack,
                    output,
                );
            }
            reference_stack.remove(target_id);
        }
        return;
    }
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let name = name.to_ascii_lowercase();
    let in_bogey = in_bogey || name.contains("bogey") || name.contains("bogie");
    let in_children = in_children
        || name.contains("schild")
        || name.contains("childentity")
        || name == "children";
    if in_bogey || in_children {
        let names = if in_bogey {
            &[
                "bogeyblueprint",
                "bogieblueprint",
                "bogeyblueprintid",
                "bogieblueprintid",
                "blueprintid",
                "blueprint",
            ][..]
        } else {
            &["blueprintid", "childblueprint", "blueprint"][..]
        };
        if let Some(blueprint_reference) = direct_named_text(node, names, ids)
            && !blueprint_reference.is_empty()
        {
            output.push(AttachmentBlueprintRef {
                kind: if in_bogey { "bogey" } else { "child" },
                blueprint_reference,
                transform: attachment_transform(node, ids, in_bogey),
            });
            return;
        }
    }
    for child in children {
        collect_blueprint_attachments_inner(
            child,
            ids,
            in_bogey,
            in_children,
            reference_stack,
            output,
        );
    }
}

fn attachment_transform(node: &SerzNode, ids: &HashMap<u32, &SerzNode>, is_bogey: bool) -> Mat4 {
    if let Some(values) = find_named_numbers(node, &["matrix"], ids)
        && let Some(matrix) = matrix_from_values(&values)
    {
        return matrix;
    }
    if let Some(values) = find_named_numbers(node, &["offset"], ids) {
        if let Some(matrix) = matrix_from_values(&values) {
            return matrix;
        }
        if values.len() >= 3 {
            return Mat4::from_translation(Vec3::new(values[0], values[1], values[2]));
        }
    }
    if is_bogey {
        let x = find_named_number(node, &["bogeypivotx", "bogiepivotx", "pivotx"], ids);
        let y = find_named_number(node, &["bogeypivoty", "bogiepivoty", "pivoty"], ids);
        let z = find_named_number(node, &["bogeypivotz", "bogiepivotz", "pivotz"], ids);
        if x.is_some() || y.is_some() || z.is_some() {
            return Mat4::from_translation(Vec3::new(
                x.unwrap_or(0.0),
                y.unwrap_or(0.0),
                z.unwrap_or(0.0),
            ));
        }
    }
    Mat4::IDENTITY
}

fn find_named_number(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<f32> {
    find_named_text(node, names, ids)?.parse().ok()
}

fn find_named_numbers(
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
                    .flat_map(|child| collect_float_values(child))
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

fn collect_float_values(node: &SerzNode) -> Vec<f32> {
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

fn parse_float_values(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ';' | '[' | ']')
        })
        .filter_map(|part| part.parse::<f32>().ok())
        .collect()
}

fn parse_i32_values(value: &str) -> Vec<i32> {
    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ';' | '[' | ']')
        })
        .filter_map(|part| part.parse::<i32>().ok())
        .collect()
}

fn collect_i32_values(node: &SerzNode) -> Vec<i32> {
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

fn find_named_values<'a>(node: &'a SerzNode, expected: &str) -> Option<Vec<&'a SerzNode>> {
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

fn compute_world_transforms(local: &[Mat4], parents: &[i32]) -> Vec<Mat4> {
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

fn find_transform_id(children: &[SerzNode], ids: &HashMap<u32, &SerzNode>) -> Option<usize> {
    direct_text(
        children,
        &["transformid", "primarytransformid", "childtransformid"],
        ids,
    )
    .and_then(|value| value.trim().parse::<i64>().ok())
    .filter(|&value| value >= 0)
    .and_then(|value| usize::try_from(value).ok())
}

fn matrix_from_values(values: &[f32]) -> Option<Mat4> {
    let matrix = <[f32; 16]>::try_from(values).ok()?;
    if matrix.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let mut parsed = Mat4::from_cols_array(&matrix);
    if (parsed.x_axis.w.abs() > 0.01)
        || (parsed.y_axis.w.abs() > 0.01)
        || (parsed.z_axis.w.abs() > 0.01)
        || (parsed.w_axis.w - 1.0).abs() > 0.01
    {
        return Some(Mat4::IDENTITY);
    }
    let mut basis = [
        parsed.x_axis.truncate(),
        parsed.y_axis.truncate(),
        parsed.z_axis.truncate(),
    ];
    let lengths = basis.map(Vec3::length);
    if lengths.iter().any(|length| !(0.8..=1.25).contains(length))
        || (basis[0].dot(basis[1]).abs() > 0.2)
        || (basis[0].dot(basis[2]).abs() > 0.2)
        || (basis[1].dot(basis[2]).abs() > 0.2)
    {
        return Some(Mat4::IDENTITY);
    }
    for (axis, length) in basis.iter_mut().zip(lengths) {
        *axis /= length;
    }
    parsed.x_axis = basis[0].extend(0.0);
    parsed.y_axis = basis[1].extend(0.0);
    parsed.z_axis = basis[2].extend(0.0);
    Some(parsed)
}

fn resolve_asset_path(source: &Path, reference: &str, extension: &str) -> Option<PathBuf> {
    let normalized = reference
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .trim_start_matches('\\')
        .to_owned();
    if normalized.is_empty() {
        return None;
    }
    let mut relative = PathBuf::from(&normalized);
    if relative.components().next().is_some_and(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("Assets")
    }) {
        relative = relative.components().skip(1).collect();
    }
    let variants = if relative
        .extension()
        .is_some_and(|current| current.eq_ignore_ascii_case(extension))
    {
        vec![relative]
    } else {
        vec![relative.with_extension(extension)]
    };
    let mut bases = Vec::new();
    if let Some(parent) = source.parent() {
        bases.extend(parent.ancestors().take(8).map(Path::to_path_buf));
    }
    for base in bases {
        for variant in &variants {
            let candidate = base.join(variant);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn load_cached_geometry(
    path: &Path,
    main_path: &Path,
    cache: &mut HashMap<String, Arc<GeoModel>>,
) -> Option<Arc<GeoModel>> {
    let key = path.to_string_lossy().to_ascii_lowercase();
    if let Some(model) = cache.get(&key) {
        return Some(Arc::clone(model));
    }
    let result = std::fs::read(path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| GeoModel::parse(&bytes));
    match result {
        Ok(model) => {
            log_parse_diagnostics(path, &model.diagnostics);
            let model = Arc::new(model);
            cache.insert(key, Arc::clone(&model));
            Some(model)
        }
        Err(error) => {
            log_assembly_warning(
                main_path,
                &format!(
                    "Unable to load attached geometry {}: {error}",
                    path.display()
                ),
            );
            None
        }
    }
}

fn find_sibling_part_geometries(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let part_names = [
        "bogey",
        "bogie",
        "coupler",
        "pantograph",
        "underframe",
        "detail",
    ];
    let mut siblings = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            !same_path(candidate, path)
                && candidate
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("GeoPcDx"))
                && candidate.file_stem().is_some_and(|stem| {
                    let stem = stem.to_string_lossy().to_ascii_lowercase();
                    part_names.iter().any(|part_name| stem.contains(part_name))
                })
        })
        .collect::<Vec<_>>();
    siblings.sort();
    siblings
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn model_bounds(model: &GeoModel, transform: Mat4) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for &index in &model.indices {
        let position =
            transform.transform_point3(Vec3::from_array(model.vertices[index as usize].position));
        min = min.min(position);
        max = max.max(position);
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (Vec3::ZERO, Vec3::ZERO)
    }
}

fn transform_has_translation(transform: Mat4) -> bool {
    transform.w_axis.truncate().length_squared() > 1e-6
}

pub(crate) fn log_assembly_warning(path: &Path, message: &str) {
    use std::io::Write;

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
            "{} [GeoPcDx assembly] {}: {message}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            path.display()
        )
    })();
    if let Err(error) = result {
        eprintln!(
            "Failed to write assembly warning to {}: {error}",
            log_path.display()
        );
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
    render_mode: GeoRenderMode,
    excluded: bool,
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
    ids: &HashMap<u32, &SerzNode>,
    vertices: &mut Vec<VertexBuffer>,
    indices: &mut Vec<IndexBuffer>,
) {
    fn collect(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
        vertices: &mut Vec<VertexBuffer>,
        indices: &mut Vec<IndexBuffer>,
        reference_stack: &mut HashSet<u32>,
        visited_elements: &mut HashSet<u32>,
    ) {
        if let SerzNode::Reference { target_id, .. } = node {
            if reference_stack.insert(*target_id) {
                if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                    collect(
                        target,
                        ids,
                        vertices,
                        indices,
                        reference_stack,
                        visited_elements,
                    );
                }
                reference_stack.remove(target_id);
            }
            return;
        }
        let SerzNode::Element {
            name,
            id,
            children,
            ..
        } = node
        else {
            return;
        };
        if *id != 0 && !visited_elements.insert(*id) {
            return;
        }
        let clean_name = name.replace("::", "-").to_ascii_lowercase();
        if clean_name.contains("chcvertexbuffer") {
            if let Some((bytes, _fields)) = find_blob(children, "vertexdata", ids) {
                let declared_vertex_count =
                    find_direct_number(children, &["numvertices"], ids)
                        .and_then(|value| usize::try_from(value).ok());
                let stride = find_direct_number(children, &["vertexstride", "stride"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or_else(|| {
                        declared_vertex_count
                            .filter(|count| *count > 0 && bytes.len() % count == 0)
                            .map(|count| bytes.len() / count)
                    })
                    .unwrap_or(32);
                let declaration = find_vertex_attribute_offsets(children, ids);
                let position = find_direct_number(children, &["positionoffset"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or(declaration.position)
                    .unwrap_or(0);
                let normal = find_direct_number(children, &["normaloffset"], ids)
                    .and_then(|value| usize::try_from(value).ok())
                    .or(declaration.normal)
                    .or_else(|| (stride >= 24).then_some(12));
                let uv = find_direct_number(children, &["texcoord0offset", "uvoffset"], ids)
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
            if let Some(index_buffer) = find_index_data(children, ids) {
                indices.push(index_buffer);
            }
        }
        for child in children {
            collect(
                child,
                ids,
                vertices,
                indices,
                reference_stack,
                visited_elements,
            );
        }
    }

    collect(
        node,
        ids,
        vertices,
        indices,
        &mut HashSet::new(),
        &mut HashSet::new(),
    );
}

fn find_vertex_attribute_offsets(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> VertexAttributeOffsets {
    fn collect(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
        reference_stack: &mut HashSet<u32>,
        stream_offsets: &mut HashMap<u64, usize>,
        attributes: &mut VertexAttributeOffsets,
        declaration: &mut Vec<String>,
    ) {
        if let SerzNode::Reference { target_id, .. } = node {
            if reference_stack.insert(*target_id) {
                if let Some(target) = ids.get(target_id) {
                    collect(
                        target,
                        ids,
                        reference_stack,
                        stream_offsets,
                        attributes,
                        declaration,
                    );
                }
                reference_stack.remove(target_id);
            }
            return;
        }
        let SerzNode::Element { name, children, .. } = node else {
            return;
        };
        let clean_name = name.to_ascii_lowercase();
        if clean_name.contains("elementdecl") {
            let usage = direct_text(children, &["usage"], ids).unwrap_or_default();
            let type_name = direct_text(children, &["type"], ids).unwrap_or_default();
            let stream_index = find_direct_number(children, &["streamindex"], ids).unwrap_or(0);
            let explicit_offset = find_direct_number(children, &["offset", "byteoffset"], ids)
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
                    && find_direct_number(children, &["usageindex"], ids).unwrap_or(0) == 0
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
            collect(
                child,
                ids,
                reference_stack,
                stream_offsets,
                attributes,
                declaration,
            );
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
            ids,
            &mut HashSet::new(),
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

fn find_material_bindings(
    node: &SerzNode,
    textures: &[String],
    ids: &HashMap<u32, &SerzNode>,
) -> HashMap<u32, MaterialBinding> {
    fn collect_delta_strings(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
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
        node: &SerzNode,
        textures: &[String],
        ids: &HashMap<u32, &SerzNode>,
        next_index: &mut u32,
        bindings: &mut HashMap<u32, MaterialBinding>,
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
            let material_index = *next_index;
            *next_index = (*next_index).saturating_add(1);
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
            let texture_index = texture_references
                .iter()
                .filter_map(|reference| {
                    let reference = crate::auditor::normalize_texture_reference(reference);
                    textures
                        .iter()
                        .position(|texture| texture.eq_ignore_ascii_case(&reference))
                        .map(|index| (index, texture_priority(&reference)))
                })
                .min_by_key(|(_, priority)| *priority)
                .map(|(index, _)| index)
                .unwrap_or(0) as u32;
            let shader_name =
                find_text(children, &["shadername", "effectname"], ids).unwrap_or_default();
            let material_name =
                find_text(children, &["name", "materialname"], ids).unwrap_or_default();
            let texture_is_shadow = texture_references
                .iter()
                .any(|texture| texture.to_ascii_lowercase().contains("shadow"));
            let subtract_alpha = find_text(children, &["framealphamode"], ids)
                .is_some_and(|mode| mode.eq_ignore_ascii_case("ALPHA_MODE_SUBTRACT"));
            bindings.insert(
                *id,
                MaterialBinding {
                    material_index,
                    texture_index,
                    render_mode: classify_shader(shader_name),
                    excluded: is_helper_name(shader_name)
                        || is_helper_name(material_name)
                        || texture_is_shadow
                        || subtract_alpha,
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
    ids: &HashMap<u32, &SerzNode>,
    parent_transform: Mat4,
    world_transforms: &[Mat4],
    inherited_transform_id: Option<usize>,
    inherited_lod: Option<u32>,
    inherited_excluded: bool,
    reference_stack: &mut HashSet<u32>,
    output: &mut Vec<RawSubmesh>,
) {
    if let SerzNode::Reference { name, target_id } = node {
        if name.eq_ignore_ascii_case("material") {
            return;
        }
        if reference_stack.insert(*target_id) {
            if let Some(target) = resolve_ref_node(node, ids, &mut HashSet::new()) {
                find_submeshes(
                    target,
                    inherited_material,
                    material_bindings,
                    ids,
                    parent_transform,
                    world_transforms,
                    inherited_transform_id,
                    inherited_lod,
                    inherited_excluded,
                    reference_stack,
                    output,
                );
            }
            reference_stack.remove(target_id);
        }
        return;
    }
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    let clean_name = name.replace("::", "-").to_ascii_lowercase();
    let node_label =
        direct_text(children, &["name", "nodename", "transformname"], ids).unwrap_or_default();
    let lod = lod_from_name(name)
        .or_else(|| lod_from_name(node_label))
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
        || is_helper_name(name)
        || is_helper_name(node_label)
        || material_reference
            .and_then(|reference| material_bindings.get(&reference))
            .is_some_and(|binding| binding.excluded);
    let active_transform_id = find_transform_id(children, ids).or(inherited_transform_id);
    let local_transform = find_local_transform(children);
    let transform = parent_transform * local_transform;
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
        let count = find_number(children, &["indexcount", "numindices"], ids).or_else(|| {
            find_number(children, &["primcount", "numprimitives"], ids).and_then(|value| {
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
            let base_vertex = find_number(children, &["vertexindexbase", "basevertex"], ids)
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or(0);
            let vertex_buffer_index = find_number(children, &["vertexbufferindex"], ids)
                .and_then(|value| u32::try_from(value).ok())
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
                topology,
                transform: active_transform_id
                    .map(|id| world_transforms.get(id).copied().unwrap_or(Mat4::IDENTITY))
                    .unwrap_or(transform),
                excluded,
            });
        }
    }
    let is_lod_container =
        clean_name.contains("lodlist") || clean_name.contains("chcsgnodelodselector");
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
            material_bindings,
            ids,
            transform,
            world_transforms,
            active_transform_id,
            child_lod.or(lod),
            excluded,
            reference_stack,
            output,
        );
    }
}

fn direct_text<'a>(
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
    [
        "schatten",
        "shadow",
        "stencil",
        "occluder",
        "volume",
        "collision",
        "occlusion",
        "dummy",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn collect_text_values(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Vec<String> {
    fn collect(
        node: &SerzNode,
        expected: &[&str],
        ids: &HashMap<u32, &SerzNode>,
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
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
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

fn texture_priority(reference: &str) -> u8 {
    let stem = Path::new(&reference.replace('\\', "/"))
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if ["_diffuse", "_base_color"]
        .iter()
        .any(|suffix| stem.ends_with(suffix))
    {
        0
    } else if ["_nm", "_emiss"]
        .iter()
        .any(|suffix| stem.ends_with(suffix))
    {
        2
    } else {
        1
    }
}

fn classify_shader(shader_name: &str) -> GeoRenderMode {
    let shader = shader_name.to_ascii_lowercase();
    if ["glass", "trans", "alpha", "blend", "decal"]
        .iter()
        .any(|marker| shader.contains(marker))
    {
        GeoRenderMode::Blended
    } else if ["flora", "veg", "fence", "mask", "1bit"]
        .iter()
        .any(|marker| shader.contains(marker))
    {
        GeoRenderMode::Cutout
    } else {
        GeoRenderMode::Opaque
    }
}

fn find_local_transform(children: &[SerzNode]) -> Mat4 {
    fn parse_matrix(node: &SerzNode) -> Option<Mat4> {
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

fn find_index_data(children: &[SerzNode], ids: &HashMap<u32, &SerzNode>) -> Option<IndexBuffer> {
    if let Some((values, type_name)) = find_index_array(children, ids) {
        let width = width_from_type_name(&type_name)?;
        return Some(IndexBuffer {
            indices: values,
            element_type: type_name_from_width(width),
        });
    }
    let (bytes, _) = find_blob(children, "indexdata", ids)?;
    let width = find_number(children, &["indexsize", "indexelementsize"], ids)
        .and_then(index_width_from_number)
        .or_else(|| {
            find_text(children, &["indexformat", "indextype"], ids)
                .and_then(index_width_from_type_name)
        })
        .unwrap_or(2);
    Some(IndexBuffer {
        indices: decode_indices(&bytes, width).ok()?,
        element_type: type_name_from_width(width),
    })
}

fn find_index_array(
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

fn find_primitive_topology(node: &SerzNode, ids: &HashMap<u32, &SerzNode>) -> PrimitiveTopology {
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
    render_mode: GeoRenderMode,
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
    render_mode: GeoRenderMode,
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
        vertex_buffer_index,
        base_vertex,
        vertex_buffer_offset,
    });
    Ok(())
}

fn find_blob(
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
            let found = ids.get(target_id).and_then(|target| {
                find_any_blob(target, ids, reference_stack)
            });
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
            SerzNode::Blob { data } => {
                Some(data.clone())
            }
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
                        find_any_blob(target, ids, reference_stack)
                            .map(|data| (data, name.clone()))
                    } else {
                        find_named_blob(target, expected, ids, reference_stack)
                    }
                });
                reference_stack.remove(target_id);
                result
            }
            SerzNode::Element {
                name, children, ..
            } if name.eq_ignore_ascii_case(expected) => children
                .iter()
                .find_map(|child| find_any_blob(child, ids, reference_stack))
                .map(|data| (data, name.clone())),
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
            SerzNode::Element {
                name, children, ..
            } if name.eq_ignore_ascii_case(expected) => {
                if let Some(data) = children
                    .iter()
                    .find_map(|node| find_any_blob(node, ids, &mut HashSet::new()))
                {
                    return Some((data, name.clone()));
                }
            }
            SerzNode::Reference { name, target_id }
                if name.eq_ignore_ascii_case(expected) =>
            {
                if let Some(data) = ids.get(target_id).and_then(|target| {
                    find_any_blob(target, ids, &mut HashSet::new())
                }) {
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
    children.iter().find_map(|child| {
        find_named_blob(child, expected, ids, &mut HashSet::new())
    })
}

fn find_number(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<u64> {
    find_text(children, expected, ids)?.trim().parse().ok()
}

fn find_direct_number(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<u64> {
    direct_text(children, expected, ids)?.trim().parse().ok()
}

fn find_text<'a>(
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

fn resolve_ref_text<'a>(
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
        "[GeoPcDx] {}: vertex_stride={} vertex_count={} local_transforms={} vertex_layout=[{}] index_type={} raw_index_count={} index_layout=[{}] primitive_type={} batches=[{}]",
        path.display(),
        diagnostics.vertex_stride,
        diagnostics.vertex_count,
        diagnostics.local_transform_count,
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
    let stripped_reference = strip_texture_slot_prefix(&reference);
    let relative_paths = [stripped_reference, reference]
        .into_iter()
        .map(|reference| {
            if Path::new(&reference).extension().is_some() {
                reference
            } else {
                format!("{reference}.TgPcDx")
            }
        })
        .collect::<Vec<_>>();
    let parent = geo_path.parent()?;
    let mut ancestors = parent.ancestors().take(6).collect::<Vec<_>>();
    if ancestors.is_empty() {
        ancestors.push(parent);
    }
    for ancestor in &ancestors {
        for relative in &relative_paths {
            let candidate = ancestor.join(relative.replace('\\', std::path::MAIN_SEPARATOR_STR));
            if let Ok(bytes) = std::fs::read(&candidate) {
                return Some((candidate.display().to_string(), bytes));
            }
        }
    }
    let targets = relative_paths
        .iter()
        .map(|relative| crate::archive::normalize_member(relative))
        .collect::<Vec<_>>();
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
            let found = (0..archive.len()).find_map(|index| {
                let entry = archive.by_index(index).ok()?;
                let name = entry.name().ok()?.replace('/', "\\");
                let normalized = crate::archive::normalize_member(&name);
                targets
                    .iter()
                    .any(|target| {
                        normalized == *target || normalized.ends_with(&format!("\\{target}"))
                    })
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

fn strip_texture_slot_prefix(reference: &str) -> String {
    let (parent, file) = reference
        .rsplit_once('\\')
        .map(|(parent, file)| (Some(parent), file))
        .unwrap_or((None, reference));
    let Some(prefix) = file.get(..4) else {
        return reference.to_owned();
    };
    if prefix.starts_with('[')
        && prefix.ends_with(']')
        && prefix[1..3]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        let stripped = &file[4..];
        if let Some(parent) = parent {
            format!("{parent}\\{stripped}")
        } else {
            stripped.to_owned()
        }
    } else {
        reference.to_owned()
    }
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
    fn reads_blueprint_geometry_and_attachment_transforms() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cWagonBlueprint d:id="1">
                    <GeometryID d:type="cDeltaString">Models\Main.GeoPcDx</GeometryID>
                    <Bogey>
                        <BogeyBlueprint d:type="cDeltaString">Bogies\front.xml</BogeyBlueprint>
                        <BogeyPivotX d:type="sFloat32">3</BogeyPivotX>
                        <BogeyPivotY d:type="sFloat32">-1</BogeyPivotY>
                        <BogiePivotZ d:type="sFloat32">7</BogiePivotZ>
                    </Bogey>
                    <Bogey d:type="ref">2</Bogey>
                    <ContainerComponent>
                        <Children>
                            <cEntityContainerBlueprint-sChild>
                                <BlueprintID d:type="cDeltaString">Parts\pantograph.xml</BlueprintID>
                                <Matrix d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 5 6 7 1</Matrix>
                            </cEntityContainerBlueprint-sChild>
                        </Children>
                    </ContainerComponent>
                </cWagonBlueprint>
                <BogeyEntry d:id="2">
                    <BogeyBlueprint d:type="cDeltaString">Bogies\rear.xml</BogeyBlueprint>
                    <BogeyPivotX d:type="sFloat32">-3</BogeyPivotX>
                </BogeyEntry>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let ids = build_id_map(&root);

        let blueprint = matching_geometry_blueprint(&root, "main", &ids).unwrap();
        let mut attachments = Vec::new();
        collect_blueprint_attachments(&blueprint, &ids, false, false, &mut attachments);
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| attachment.blueprint_reference.as_str())
                .collect::<Vec<_>>(),
            [
                r"Bogies\front.xml",
                r"Bogies\rear.xml",
                r"Parts\pantograph.xml"
            ]
        );
        assert_eq!(attachments[0].kind, "bogey");
        assert_eq!(attachments[0].blueprint_reference, r"Bogies\front.xml");
        assert_eq!(
            attachments[0].transform.w_axis.truncate(),
            Vec3::new(3.0, -1.0, 7.0)
        );
        assert_eq!(attachments[1].kind, "bogey");
        assert_eq!(
            attachments[1].transform.w_axis.truncate(),
            Vec3::new(-3.0, 0.0, 0.0)
        );
        assert_eq!(attachments[2].kind, "child");
        assert_eq!(
            attachments[2].transform.w_axis.truncate(),
            Vec3::new(5.0, 6.0, 7.0)
        );
    }

    #[test]
    fn discovers_sibling_blueprint_and_resolves_asset_relative_links() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("Assets");
        let model_dir = assets.join("Provider").join("Product").join("Models");
        let bogey_dir = assets.join("Provider").join("Product").join("Bogies");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::create_dir_all(&bogey_dir).unwrap();
        let geometry_path = model_dir.join("Main.GeoPcDx");
        let blueprint_path = model_dir.join("Main.bin");
        let bogey_path = bogey_dir.join("Front.bin");
        std::fs::write(&geometry_path, b"").unwrap();
        std::fs::write(
            &blueprint_path,
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cWagonBlueprint>
                    <GeometryID d:type="cDeltaString">Provider\Product\Models\Main.GeoPcDx</GeometryID>
                </cWagonBlueprint>
            </Root>"#,
        )
        .unwrap();
        std::fs::write(&bogey_path, b"").unwrap();

        let (found_path, _, _) = find_geometry_blueprint(&geometry_path, "main").unwrap();
        let resolved =
            resolve_asset_path(&found_path, r"Provider\Product\Bogies\Front.xml", "bin").unwrap();

        assert_eq!(found_path, blueprint_path);
        assert_eq!(resolved, bogey_path);
    }

    #[test]
    fn composes_attached_parts_and_side_by_side_layouts() {
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32);
        let model = Arc::new(GeoModel::parse(source.as_bytes()).unwrap());
        let assembly = GeoAssembly {
            parts: vec![
                GeoAssemblyPart {
                    path: PathBuf::from("Main.GeoPcDx"),
                    model: Arc::clone(&model),
                    transform: Mat4::IDENTITY,
                    blueprint_attached: false,
                },
                GeoAssemblyPart {
                    path: PathBuf::from("Bogey.GeoPcDx"),
                    model,
                    transform: Mat4::from_translation(Vec3::new(10.0, -1.0, 2.0)),
                    blueprint_attached: true,
                },
            ],
            has_blueprint_attachments: true,
        };

        let (assembled, sources) = assembly.compose(false).unwrap();
        let bogey_position = assembled.vertices[assembled.indices[3] as usize].position;
        assert_eq!(bogey_position, [10.0, -1.0, 2.0]);
        assert_eq!(sources.len(), assembled.textures.len());

        let (side_by_side, _) = assembly.compose(true).unwrap();
        let bogey_position = side_by_side.vertices[side_by_side.indices[3] as usize].position;
        assert_eq!(bogey_position, [3.5, 0.0, 0.0]);
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
    fn keeps_vertex_data_and_count_scoped_to_each_vertex_buffer() {
        let narrow_vertices = [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let narrow_blob = narrow_vertices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let unrelated_blob = "0000000000000000";
        let second_buffer = format!(
            r#"
                <cHcVertexBuffer>
                    <OtherData>
                        <NumVertices d:type="sUInt32">5</NumVertices>
                        <IndexData><blob>{unrelated_blob}</blob></IndexData>
                    </OtherData>
                    <NumVertices d:type="sUInt32">3</NumVertices>
                    <blob>{narrow_blob}</blob>
                </cHcVertexBuffer>
            "#
        );
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{second_buffer}</Root>"));

        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.vertices.len(), 6);
        assert_eq!(model.indices, [0, 1, 2]);
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
                render_mode: GeoRenderMode::Opaque,
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
    fn primitive_set_counts_follow_triangle_topology() {
        let batches = r#"
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
            </cHcPrimitiveSetDescDx>
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="sUInt32">4</NumPrimitives>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">3</IndexArrayStart>
            </cHcPrimitiveSetDescDx>
        "#;
        let source = xml(
            &triangle_vertices(4),
            &[0, 0, 1, 0, 2, 0, 0, 0, 1, 0, 2, 0, 3, 0],
            32,
        )
        .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.indices, [0, 1, 2, 0, 1, 2, 2, 1, 3]);
        assert!(model.diagnostics.batch_ranges[0].contains("start=0 count=3"));
        assert!(model.diagnostics.batch_ranges[1].contains("start=3 count=4"));
    }

    #[test]
    fn resolves_referenced_primitive_types_for_triangle_strips() {
        let batches = r#"
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:id="43">
                    <Value d:type="sUInt32">4</Value>
                </NumPrimitives>
                <PrimitiveType d:id="42">
                    <Value d:type="cDeltaString">PRIMTYPE_TRIANGLESTRIP</Value>
                </PrimitiveType>
                <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
                <VertexIndexBase d:id="44">
                    <Value d:type="sUInt32">0</Value>
                </VertexIndexBase>
                <VertexIndexMin d:id="45">
                    <Value d:type="sUInt32">0</Value>
                </VertexIndexMin>
                <VertexIndexRange d:id="46">
                    <Value d:type="sUInt32">4</Value>
                </VertexIndexRange>
            </cHcPrimitiveSetDescDx>
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="ref">43</NumPrimitives>
                <PrimitiveType d:type="ref">42</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">4</IndexArrayStart>
                <VertexIndexBase d:type="ref">44</VertexIndexBase>
                <VertexIndexMin d:type="ref">45</VertexIndexMin>
                <VertexIndexRange d:type="ref">46</VertexIndexRange>
            </cHcPrimitiveSetDescDx>
        "#;
        let source = xml(
            &triangle_vertices(4),
            &[0, 0, 1, 0, 2, 0, 3, 0, 0, 0, 1, 0, 2, 0, 3, 0],
            32,
        )
        .replace("</Root>", &format!("{batches}</Root>"));
        let parsed_root = parse_serz_document(source.as_bytes()).unwrap();
        let ids = build_id_map(&parsed_root);
        let SerzNode::Element { children, .. } = &parsed_root else {
            panic!("expected root element");
        };
        assert_eq!(find_number(children, &["numprimitives"], &ids), Some(4));
        assert_eq!(find_number(children, &["vertexindexbase"], &ids), Some(0));
        assert_eq!(find_number(children, &["vertexindexmin"], &ids), Some(0));
        assert_eq!(find_number(children, &["vertexindexrange"], &ids), Some(4));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(
            model.diagnostics.primitive_type,
            PrimitiveTopology::TriangleStrip
        );
        assert_eq!(model.diagnostics.batch_ranges.len(), 2);
        assert!(
            model
                .diagnostics
                .batch_ranges
                .iter()
                .all(|range| range.contains("topology=TriangleStrip"))
        );
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3, 0, 1, 2, 2, 1, 3]);
    }

    #[test]
    fn reads_float_matrices_and_integer_dependencies_from_blobs() {
        let matrix_values = [
            1.0_f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 3.0, 4.0, 0.0, 1.0,
        ];
        let matrix_bytes = matrix_values
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let dependency_bytes = [-1_i32, 0].into_iter().flat_map(i32::to_le_bytes).collect();
        let root = SerzNode::Element {
            name: "Root".into(),
            id: 0,
            version: 0,
            children: vec![
                SerzNode::Element {
                    name: "SourceLToPTransform".into(),
                    id: 0,
                    version: 0,
                    children: vec![SerzNode::Blob { data: matrix_bytes }],
                },
                SerzNode::Element {
                    name: "TransformDependency".into(),
                    id: 0,
                    version: 0,
                    children: vec![SerzNode::Blob {
                        data: dependency_bytes,
                    }],
                },
            ],
        };
        let transforms = find_named_values(&root, "SourceLToPTransform")
            .unwrap()
            .into_iter()
            .flat_map(collect_float_values)
            .collect::<Vec<_>>();
        let dependencies = find_named_values(&root, "TransformDependency")
            .unwrap()
            .into_iter()
            .flat_map(collect_i32_values)
            .collect::<Vec<_>>();

        assert_eq!(transforms.len(), 16);
        assert_eq!(
            matrix_from_values(&transforms).unwrap().w_axis.truncate(),
            Vec3::new(3.0, 4.0, 0.0)
        );
        assert_eq!(dependencies, [-1, 0]);
    }

    #[test]
    fn applies_root_transform_dependency_hierarchy_to_scenegraph_batches() {
        let transforms = r#"
            <SourceLToPTransform d:type="sFloat32">
                1 0 0 0 0 1 0 0 0 0 1 0 3 0 0 1
                1 0 0 0 0 1 0 0 0 0 1 0 0 4 0 1
            </SourceLToPTransform>
            <TransformDependency d:type="sInt32">-1 0</TransformDependency>
            <cHcTransformNode>
                <Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 100 0 0 1</Transform>
                <cHcGeometryNodeSameState>
                    <PrimaryTransformID d:type="sInt32">1</PrimaryTransformID>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcGeometryNodeSameState>
            </cHcTransformNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{transforms}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.diagnostics.local_transform_count, 2);
        assert_eq!(
            model.vertices[model.indices[0] as usize].position,
            [3.0, 4.0, 0.0]
        );
    }

    #[test]
    fn invalid_transform_dependencies_fall_back_to_identity() {
        let local = [
            Mat4::from_translation(Vec3::X),
            Mat4::from_translation(Vec3::Y),
            Mat4::from_translation(Vec3::Z),
        ];
        let cyclic = compute_world_transforms(&local[..2], &[1, 0]);
        assert_eq!(cyclic, [Mat4::IDENTITY, Mat4::IDENTITY]);

        let invalid_parent = compute_world_transforms(&local[2..], &[9]);
        assert_eq!(invalid_parent, [Mat4::IDENTITY]);
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
            <StencilShadow><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></StencilShadow>
            <DummyNode><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></DummyNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.diagnostics.batch_ranges.len(), 1);
    }

    #[test]
    fn traverses_only_the_first_lod_selector_child() {
        let selector = r#"
            <cHcSGNodeLODSelector>
                <Child>
                    <LOD0><cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList></LOD0>
                </Child>
                <Child>
                    <LOD0><cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList></LOD0>
                </Child>
            </cHcSGNodeLODSelector>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{selector}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
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

        let skeleton = transform.replace("cHcTransformNode", "cHcSkeletonNode");
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{skeleton}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(
            model.vertices[model.indices[0] as usize].position,
            [3.0, -2.0, 4.0]
        );

        let matrix = matrix_from_values(&[
            1.0, 0.0, 0.0, 0.25, 0.0, 1.0, 0.0, 0.5, 0.0, 0.0, 1.0, 0.75, 3.0, -2.0, 4.0, 1.0,
        ])
        .unwrap();
        assert_eq!(matrix, Mat4::IDENTITY);

        assert_eq!(
            matrix_from_values(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 13.2, 0.0, 0.0, 0.0, 0.0, 1.0,
            ]),
            Some(Mat4::IDENTITY)
        );
        assert_eq!(
            matrix_from_values(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ]),
            Some(Mat4::IDENTITY)
        );
        assert_eq!(
            matrix_from_values(&[
                1.1, 0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 3.0, -2.0, 4.0, 1.0,
            ])
            .unwrap()
            .x_axis
            .truncate()
            .length(),
            1.0
        );
        assert_eq!(
            matrix_from_values(&[
                1.0, 0.4, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ]),
            Some(Mat4::IDENTITY)
        );
    }

    #[test]
    fn classifies_shader_alpha_and_helper_batches() {
        assert_eq!(classify_shader("TrainGlass"), GeoRenderMode::Blended);
        assert_eq!(classify_shader("RailVehicleFlora"), GeoRenderMode::Cutout);
        assert_eq!(classify_shader("TrainStandard"), GeoRenderMode::Opaque);
        assert!(is_helper_name("TrainShadowOnly"));
        assert!(is_helper_name("Schatten"));
        assert!(is_helper_name("StencilShadow"));
        assert!(is_helper_name("DummyNode"));

        let batches = r#"
            <cHcSGNodeMaterial>
                <Material d:type="ref">7</Material>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcSGNodeMaterial>
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList>
            <cHcEffectMaterialDx d:id="7">
                <ShaderName d:type="cDeltaString">TrainShadowOnly</ShaderName>
                <TextureByName d:type="cDeltaString">Textures\sample.TgPcDx</TextureByName>
            </cHcEffectMaterialDx>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(source.as_bytes()).unwrap();
        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn excludes_subtractive_and_shadow_materials_and_prefers_diffuse_textures() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcExplicitStateMaterial d:id="7">
                    <TextureByName d:type="cDeltaString">Textures\body_nm.TgPcDx</TextureByName>
                    <TextureByName d:type="cDeltaString">Textures\body_diffuse.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
                <cHcExplicitStateMaterial d:id="8">
                    <FrameAlphaMode d:type="cDeltaString">ALPHA_MODE_SUBTRACT</FrameAlphaMode>
                    <ShaderName d:type="cDeltaString">Lit3DirSingle</ShaderName>
                    <TextureByName d:type="cDeltaString">Textures\shadow_mask.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
                <cHcExplicitStateMaterial d:id="9">
                    <Name d:type="cDeltaString">ShadowMaterial</Name>
                    <TextureByName d:type="cDeltaString">Textures\shadow.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let textures = [
            r"Textures\body_nm.TgPcDx".to_owned(),
            r"Textures\body_diffuse.TgPcDx".to_owned(),
            r"Textures\shadow.TgPcDx".to_owned(),
        ];
        let bindings = find_material_bindings(&root, &textures, &build_id_map(&root));

        assert_eq!(bindings[&7].texture_index, 1);
        assert!(!bindings[&7].excluded);
        assert!(bindings[&8].excluded);
        assert!(bindings[&9].excluded);
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
            resolve_texture_data(&geo_path, r"Textures\[00]body.TgPcDx").unwrap();
        assert!(resolved_path.contains("!Provider\\Product\\Textures\\body.TgPcDx"));
        assert_eq!(bytes, b"texture data");
    }
}
