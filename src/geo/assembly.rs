//! Geometry assembly: blueprint attachments, sibling parts, and model composition.
//! All items here are `#[cfg(test)]` as in the original monolithic file.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use glam::{Mat4, Vec3};
use std::sync::Arc;

use super::helpers::{
    build_id_map, references_target, matrix_from_values, parse_serz_document,
};
use crate::common::serz::SerzNode;
use crate::geo::types::{GeoAssembly, GeoAssemblyPart, GeoModel};

#[derive(Clone)]
pub(crate) struct AttachmentBlueprintRef {
    pub kind: &'static str,
    pub blueprint_reference: String,
    pub transform: Mat4,
}

pub(crate) fn load_assembly(path: &Path) -> Result<GeoAssembly, String> {
    let primary_model = Arc::new(GeoModel::load(path)?);
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
        collect_blueprint_attachments_inner(
            &blueprint_root,
            &id_map,
            false,
            false,
            &mut HashSet::new(),
            &mut attachments,
        );
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
    Ok(GeoAssembly {
        parts,
        has_blueprint_attachments,
    })
}

pub(crate) fn compose_assembly(
    assembly: &GeoAssembly,
    side_by_side: bool,
) -> Result<(GeoModel, Vec<(PathBuf, String)>), String> {
    let primary = assembly
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
    for (part_index, part) in assembly.parts.iter().enumerate() {
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
            let mut submesh = submesh.clone();
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

pub(crate) fn find_geometry_blueprint(
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

pub(crate) fn matching_geometry_blueprint(
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

pub(crate) fn asset_reference_stem(reference: &str) -> String {
    let reference = super::texture::strip_texture_slot_prefix(reference);
    Path::new(&reference.replace('\\', "/"))
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
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

pub(crate) fn direct_named_text(
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

pub(crate) fn collect_blueprint_attachments(
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

pub(crate) fn collect_blueprint_attachments_inner(
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

pub(crate) fn attachment_transform(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    is_bogey: bool,
) -> Mat4 {
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

pub(crate) fn find_named_number(
    node: &SerzNode,
    names: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Option<f32> {
    find_named_text(node, names, ids)?.parse().ok()
}

pub(crate) fn resolve_asset_path(
    source: &Path,
    reference: &str,
    extension: &str,
) -> Option<PathBuf> {
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
    let variants = variants
        .into_iter()
        .flat_map(|variant| {
            let reference = variant.to_string_lossy().replace('/', "\\");
            let stripped = super::texture::strip_texture_slot_prefix(&reference);
            if stripped == reference {
                vec![variant]
            } else {
                vec![
                    variant,
                    PathBuf::from(stripped.replace('\\', std::path::MAIN_SEPARATOR_STR)),
                ]
            }
        })
        .collect::<Vec<_>>();
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

pub(crate) fn load_cached_geometry(
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
        .and_then(|bytes| GeoModel::parse(path, &bytes));
    match result {
        Ok(model) => {
            super::diagnostics::log_parse_diagnostics(path, &model.diagnostics);
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

pub(crate) fn find_sibling_part_geometries(path: &Path) -> Vec<PathBuf> {
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

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

pub(crate) fn model_bounds(model: &GeoModel, transform: Mat4) -> (glam::Vec3, glam::Vec3) {
    let mut min = glam::Vec3::splat(f32::INFINITY);
    let mut max = glam::Vec3::splat(f32::NEG_INFINITY);
    for &index in &model.indices {
        let position = transform.transform_point3(glam::Vec3::from_array(
            model.vertices[index as usize].position,
        ));
        min = min.min(position);
        max = max.max(position);
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (glam::Vec3::ZERO, glam::Vec3::ZERO)
    }
}

pub(crate) fn transform_has_translation(transform: Mat4) -> bool {
    transform.w_axis.truncate().length_squared() > 1e-6
}

pub(crate) fn log_assembly_warning(path: &Path, message: &str) {
    use std::io::Write;

    let log_path = crate::app::settings::log_file();
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
