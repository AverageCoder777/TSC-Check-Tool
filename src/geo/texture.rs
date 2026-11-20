//! Texture reference resolution and material classification.

use crate::common::archive::normalize_member;
use crate::common::serz::SerzNode;
use std::path::{Path, PathBuf};

/// Explicit shader-level signals for geometry that must never be rendered as visible
/// surface. Node and material *names* are deliberately not inspected: in rolling stock
/// they carry variant/numbering suffixes (`body_b`, `cab_c`, `number_plate`, `decal_*`)
/// that are real geometry, so substring heuristics erase half of a locomotive.
pub(crate) fn is_hidden_shader_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["stencil", "shadow", "invisible", "occluder", "occlusion"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// A node opts out only through the explicit `Visible` flag (0/false), not through its
/// name. Collision subtrees are pruned separately by their `collisionroot` ancestor.
pub(crate) fn node_is_explicitly_hidden(
    children: &[SerzNode],
    ids: &std::collections::HashMap<u32, &SerzNode>,
) -> bool {
    ["visible", "isvisible", "visibleflag"]
        .iter()
        .find_map(|field| super::helpers::direct_text(children, &[*field], ids))
        .is_some_and(|value| {
            let value = value.trim();
            value == "0" || value.eq_ignore_ascii_case("false")
        })
}

pub(crate) fn lod_from_name(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    if let Some(start) = lower.find("lod") {
        let digits = lower[start + 3..].trim_start_matches(|character: char| {
            character == '_' || character == '-' || character.is_ascii_whitespace()
        });
        let end = digits
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(digits.len());
        if end > 0 {
            if let Ok(level) = digits[..end].parse::<u32>() {
                return Some(level);
            }
        }
    }
    // RailWorks LOD distance prefixes: "1_0500_", "1_1000_", "2_...", etc.
    // Prefix 1 => LOD0, 2 => LOD1, ...
    let bytes = lower.as_bytes();
    for index in 0..bytes.len() {
        if (b'1'..=b'4').contains(&bytes[index])
            && index + 1 < bytes.len()
            && bytes[index + 1] == b'_'
        {
            let mut end = index + 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 2 && end < bytes.len() && bytes[end] == b'_' {
                return Some((bytes[index] - b'1') as u32);
            }
        }
    }
    None
}

pub(crate) fn lod_name_is_nonzero(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let mut remaining = lower.as_str();
    while let Some(index) = remaining.find("lod") {
        let after_lod = &remaining[index + 3..];
        let digits = after_lod.trim_start_matches(|character: char| {
            character == '_' || character == '-' || character.is_ascii_whitespace()
        });
        let digit_end = digits
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(digits.len());
        if digit_end > 0
            && digits[..digit_end]
                .parse::<u32>()
                .is_ok_and(|level| level > 0)
        {
            return true;
        }
        remaining = after_lod;
    }
    // RailWorks LOD distance prefixes: prefix 2/3/4 => higher LOD.
    let bytes = lower.as_bytes();
    for index in 0..bytes.len() {
        if (b'2'..=b'4').contains(&bytes[index])
            && index + 1 < bytes.len()
            && bytes[index + 1] == b'_'
        {
            let mut end = index + 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 2 && end < bytes.len() && bytes[end] == b'_' {
                return true;
            }
        }
    }
    false
}

/// Auxiliary nodes (shadows, stencil, occlusion) start hidden in the inspector.
/// Numbering, decals and `_b`/`_c` suffixed nodes are real rolling-stock geometry
/// and must NOT be filtered out by name.
pub(crate) fn is_auxiliary_node(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "schatten",
        "shadow",
        "stencil",
        "invisible",
        "occluder",
        "occlusion",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub(crate) fn texture_priority(reference: &str) -> u8 {
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

pub(crate) fn classify_shader(shader_name: &str) -> super::types::GeoRenderMode {
    let shader = shader_name.to_ascii_lowercase();
    // Explicit cutout shaders (alpha-tested foliage, decals, flora). "mask" is NOT a cutout trigger:
    // TrainBumpSpecEnvMask.fx, TrainBumpSpec.fx, TrainSpecEnvMask.fx, TrainBasicObjectDiffuse.fx
    // use the diffuse alpha channel as a specular/env mask, NOT as transparency.
    if [
        "flora",
        "veg",
        "fence",
        "1bit",
        "blendatex",
        "alphatest",
        "cutout",
        "trainflora",
        "lofttexdifftrans",
        "traindecal",
    ]
    .iter()
    .any(|marker| shader.contains(marker))
    {
        super::types::GeoRenderMode::Cutout
    } else if ["glass", "trans", "alpha", "blend", "decal"]
        .iter()
        .any(|marker| shader.contains(marker))
    {
        super::types::GeoRenderMode::Blended
    } else {
        super::types::GeoRenderMode::Opaque
    }
}

pub(crate) fn strip_texture_slot_prefix(reference: &str) -> String {
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

pub(crate) fn resolve_texture_data(
    geo_path: &Path,
    reference: &str,
    archive_toc_cache: &mut std::collections::HashMap<PathBuf, Vec<String>>,
) -> Option<(String, Vec<u8>)> {
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
        .map(|relative| normalize_member(relative))
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
            // Cache the archive table of contents so the central directory is only
            // read once per archive during an entire model load.
            let member_names = archive_toc_cache
                .entry(archive_path.clone())
                .or_insert_with(|| {
                    std::fs::File::open(&archive_path)
                        .ok()
                        .and_then(|file| zip::ZipArchive::new(file).ok())
                        .map(|mut archive| {
                            let mut names = Vec::with_capacity(archive.len());
                            for index in 0..archive.len() {
                                if let Ok(entry) = archive.by_index(index) {
                                    if let Ok(name) = entry.name() {
                                        names.push(name.to_string().replace('/', "\\"));
                                    }
                                }
                            }
                            names
                        })
                        .unwrap_or_default()
                });
            let found = member_names.iter().find_map(|name| {
                let normalized = normalize_member(name);
                targets
                    .iter()
                    .any(|target| {
                        normalized == *target || normalized.ends_with(&format!("\\{target}"))
                    })
                    .then_some(name.to_owned())
            });
            let Some(member_name) = found else {
                continue;
            };
            let Ok(file) = std::fs::File::open(&archive_path) else {
                continue;
            };
            let Ok(mut archive) = zip::ZipArchive::new(file) else {
                continue;
            };
            let index = member_names.iter().position(|n| n == &member_name)?;
            let Ok(mut member) = archive.by_index(index) else {
                continue;
            };
            use std::io::Read;
            let mut bytes = Vec::new();
            if member.read_to_end(&mut bytes).is_ok() {
                return Some((format!("{}!{}", archive_path.display(), member_name), bytes));
            }
        }
    }
    None
}
