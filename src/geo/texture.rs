//! Texture reference resolution and material classification.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use super::helpers::direct_text;
use super::types::GeoRenderMode;
use crate::common::archive::normalize_member;
use crate::common::serz::SerzNode;

/// Explicit shader-level signals for geometry that must never be rendered as visible
/// surface. Node and material *names* are deliberately not inspected: in rolling stock
/// they carry variant/numbering suffixes (`body_b`, `cab_c`, `number_plate`, `decal_*`)
/// that are real geometry, so substring heuristics erase half of a locomotive.
pub(crate) fn is_hidden_shader_name(name: &str) -> bool {
    contains_any_ignore_ascii_case(
        name,
        &["stencil", "shadow", "invisible", "occluder", "occlusion"],
    )
}

/// A node opts out only through the explicit `Visible` flag (0/false), not through its
/// name. Collision subtrees are pruned separately by their `collisionroot` ancestor.
pub(crate) fn node_is_explicitly_hidden(
    children: &[SerzNode],
    ids: &HashMap<u32, &SerzNode>,
) -> bool {
    direct_text(children, &["visible", "isvisible", "visibleflag"], ids).is_some_and(|value| {
        let value = value.trim();
        value == "0" || value.eq_ignore_ascii_case("false")
    })
}

pub(crate) fn lod_from_name(name: &str) -> Option<u32> {
    let mut remaining = name;
    while let Some(start) = find_ignore_ascii_case(remaining, b"lod") {
        let after_lod = &remaining[start + 3..];
        if let Some(level) = parse_leading_lod_number(after_lod) {
            return Some(level);
        }
        remaining = after_lod;
    }
    // RailWorks LOD distance prefixes: "1_0500_", "1_1000_", "2_...", etc.
    // Prefix 1 => LOD0, 2 => LOD1, ...
    railworks_lod_prefix(name.as_bytes(), b'1')
}

pub(crate) fn lod_name_is_nonzero(name: &str) -> bool {
    let mut remaining = name;
    while let Some(start) = find_ignore_ascii_case(remaining, b"lod") {
        let after_lod = &remaining[start + 3..];
        if parse_leading_lod_number(after_lod).is_some_and(|level| level > 0) {
            return true;
        }
        remaining = after_lod;
    }
    // RailWorks LOD distance prefixes: prefix 2/3/4 => higher LOD.
    railworks_lod_prefix(name.as_bytes(), b'2').is_some()
}

/// Auxiliary nodes (shadows, stencil, occlusion) start hidden in the inspector.
/// Numbering, decals and `_b`/`_c` suffixed nodes are real rolling-stock geometry
/// and must NOT be filtered out by name.
pub(crate) fn is_auxiliary_node(name: &str) -> bool {
    contains_any_ignore_ascii_case(
        name,
        &[
            "schatten",
            "shadow",
            "stencil",
            "invisible",
            "occluder",
            "occlusion",
        ],
    )
}

pub(crate) fn texture_priority(reference: &str) -> u8 {
    let file_name = reference
        .rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let stem = match file_name.rfind('.') {
        Some(dot) if dot > 0 => &file_name[..dot],
        _ => file_name,
    };
    if ends_with_any_ignore_ascii_case(stem, &["_diffuse", "_base_color"]) {
        0
    } else if ends_with_any_ignore_ascii_case(stem, &["_nm", "_emiss"]) {
        2
    } else {
        1
    }
}

pub(crate) fn classify_shader(shader_name: &str) -> GeoRenderMode {
    // Explicit cutout shaders (alpha-tested foliage, decals, flora). "mask" is NOT a cutout trigger:
    // TrainBumpSpecEnvMask.fx, TrainBumpSpec.fx, TrainSpecEnvMask.fx, TrainBasicObjectDiffuse.fx
    // use the diffuse alpha channel as a specular/env mask, NOT as transparency.
    if contains_any_ignore_ascii_case(
        shader_name,
        &[
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
        ],
    ) {
        GeoRenderMode::Cutout
    } else if contains_any_ignore_ascii_case(
        shader_name,
        &["glass", "trans", "alpha", "blend", "decal"],
    ) {
        GeoRenderMode::Blended
    } else {
        GeoRenderMode::Opaque
    }
}

pub(crate) fn strip_texture_slot_prefix(reference: &str) -> String {
    let (parent, file) = reference
        .rsplit_once('\\')
        .map_or((None, reference), |(parent, file)| (Some(parent), file));
    if let [b'[', a, b, b']', ..] = file.as_bytes()
        && a.is_ascii_hexdigit()
        && b.is_ascii_hexdigit()
    {
        let stripped = &file[4..];
        return match parent {
            Some(parent) => format!("{parent}\\{stripped}"),
            None => stripped.to_owned(),
        };
    }
    reference.to_owned()
}

pub(crate) fn resolve_texture_data(
    geo_path: &Path,
    reference: &str,
    archive_toc_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Option<(String, Vec<u8>)> {
    let normalized_ref = reference.replace('/', "\\");
    let trimmed_ref = normalized_ref.trim_start_matches('\\');
    let stripped = strip_texture_slot_prefix(trimmed_ref);

    let mut relative_paths = Vec::with_capacity(2);
    let same_as_original = stripped == trimmed_ref;
    relative_paths.push(ensure_tgpcdx_extension(stripped));
    if !same_as_original {
        relative_paths.push(ensure_tgpcdx_extension(trimmed_ref.to_owned()));
    }

    let parent = geo_path.parent()?;
    for ancestor in parent.ancestors().take(6) {
        for relative in &relative_paths {
            let candidate = if std::path::MAIN_SEPARATOR == '\\' {
                ancestor.join(relative)
            } else {
                ancestor.join(relative.replace('\\', std::path::MAIN_SEPARATOR_STR))
            };
            if let Ok(bytes) = fs::read(&candidate) {
                return Some((candidate.display().to_string(), bytes));
            }
        }
    }

    let targets: Vec<String> = relative_paths
        .iter()
        .map(|relative| normalize_member(relative))
        .collect();

    for ancestor in parent.ancestors().take(6) {
        let Ok(entries) = fs::read_dir(ancestor) else {
            continue;
        };
        for archive_path in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_ap_archive(path))
        {
            if let Some(found) = read_archive_texture(&archive_path, &targets, archive_toc_cache) {
                return Some(found);
            }
        }
    }

    None
}

fn find_ignore_ascii_case(haystack: &str, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn contains_any_ignore_ascii_case(haystack: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| find_ignore_ascii_case(haystack, needle.as_bytes()).is_some())
}

fn ends_with_any_ignore_ascii_case(haystack: &str, suffixes: &[&str]) -> bool {
    let bytes = haystack.as_bytes();
    suffixes.iter().any(|suffix| {
        bytes
            .len()
            .checked_sub(suffix.len())
            .is_some_and(|start| bytes[start..].eq_ignore_ascii_case(suffix.as_bytes()))
    })
}

fn parse_leading_lod_number(after_lod: &str) -> Option<u32> {
    let digits =
        after_lod.trim_start_matches(|c: char| c == '_' || c == '-' || c.is_ascii_whitespace());
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    (end > 0).then(|| digits[..end].parse().ok()).flatten()
}

fn railworks_lod_prefix(bytes: &[u8], min_digit: u8) -> Option<u32> {
    let mut index = 0;
    while index + 3 < bytes.len() {
        if (min_digit..=b'4').contains(&bytes[index]) && bytes[index + 1] == b'_' {
            let mut end = index + 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 2 && bytes.get(end) == Some(&b'_') {
                return Some(u32::from(bytes[index] - b'1'));
            }
        }
        index += 1;
    }
    None
}

fn ensure_tgpcdx_extension(mut reference: String) -> String {
    if Path::new(&reference).extension().is_none() {
        reference.push_str(".TgPcDx");
    }
    reference
}

fn is_ap_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
}

fn member_matches_target(normalized: &str, target: &str) -> bool {
    normalized
        .strip_suffix(target)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('\\'))
}

/// Cache the archive table of contents so the central directory is only
/// read once per archive during an entire model load.
fn get_or_load_archive_toc<'a>(
    archive_path: &Path,
    cache: &'a mut HashMap<PathBuf, Vec<String>>,
) -> &'a [String] {
    if !cache.contains_key(archive_path) {
        let names = File::open(archive_path)
            .ok()
            .and_then(|file| ZipArchive::new(file).ok())
            .map(|mut archive| {
                let len = archive.len();
                let mut names = Vec::with_capacity(len);
                for index in 0..len {
                    let name = archive
                        .by_index(index)
                        .ok()
                        .and_then(|entry| entry.name().ok().map(|n| n.replace('/', "\\")))
                        .unwrap_or_default();
                    names.push(name);
                }
                names
            })
            .unwrap_or_default();
        cache.insert(archive_path.to_path_buf(), names);
    }
    &cache[archive_path]
}

fn read_archive_texture(
    archive_path: &Path,
    targets: &[String],
    archive_toc_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Option<(String, Vec<u8>)> {
    let member_names = get_or_load_archive_toc(archive_path, archive_toc_cache);
    let (index, member_name) = member_names.iter().enumerate().find(|(_, name)| {
        if name.is_empty() {
            return false;
        }
        let normalized = normalize_member(name);
        targets
            .iter()
            .any(|target| member_matches_target(&normalized, target))
    })?;

    let file = File::open(archive_path).ok()?;
    let mut archive = ZipArchive::new(file).ok()?;
    let mut member = archive.by_index(index).ok()?;
    let mut bytes = Vec::new();
    member.read_to_end(&mut bytes).ok()?;
    Some((format!("{}!{member_name}", archive_path.display()), bytes))
}
