//! Texture metadata, inspection, and internal name editing.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::decode::{ChannelMode, expand_and_sort_mips};
use super::format::{
    DdsCompression, format_label_and_dds_name, mip_level_bytes, tgpcdx_dds_compression,
};
use super::serz_parse::{
    ImageMip, all_image_blocks, collect_serz_image_mips_owned, count_c_hc_textures,
    extract_blob_size, extract_format, extract_tag, extract_unnamed_u16, image_mips_from_bytes,
    parse_bool_tag,
};
use crate::app::settings::log_file;
use crate::common::serz::{SerzNode, SerzReader, encode_tgpcdx_to_vec, validate_tgpcdx_binary};

/// Экстрагированные метаданные текстуры .TgPcDx для панели информации.
#[derive(Debug, Clone)]
pub struct TextureInfo {
    pub name: String,
    pub is_swizzled: bool,
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub raw_format: String,
    pub format_label: String,
    pub dds_format: Option<&'static str>,
    pub texture_count: u32,
    pub mip_storage_mode: String,
    pub flags_u16: u16,
    pub file_size_bytes: u64,
    pub total_blob_bytes: u64,
    pub header_bytes: u64,
    pub name_matches_filename: bool,
    pub has_bak: bool,
}

#[derive(Default)]
struct RawInspectStats {
    name: String,
    texture_count: u32,
    block_count: usize,
    width: u32,
    height: u32,
    min_width: u32,
    min_height: u32,
    raw_format: String,
    is_swizzled: bool,
    flags_u16: u16,
    total_blob_bytes: u64,
}

/// Однопроходная загрузка мипмапов и метаданных (1 парсинг SERZ вместо 2, без XML).
pub(super) fn load_tgpcdx_bundle(
    data: &[u8],
    path: Option<&Path>,
) -> Result<(Vec<ImageMip>, Option<TextureInfo>), String> {
    if data.starts_with(b"SERZ") {
        let mut reader = SerzReader::new(data);
        let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        let info = inspect_serz_node(&root, data.len() as u64, path).ok();
        let mut mips = Vec::new();
        collect_serz_image_mips_owned(&mut root, &mut mips)?;
        return Ok((expand_and_sort_mips(mips), info));
    }

    let info = inspect_tgpcdx_with_path(data, path).ok();
    let mips = expand_and_sort_mips(image_mips_from_bytes(data)?);
    Ok((mips, info))
}

/// Вариант с указанием пути на диске: позволяет сверить имя и проверить .bak.
pub fn inspect_tgpcdx_with_path(data: &[u8], path: Option<&Path>) -> Result<TextureInfo, String> {
    if data.starts_with(b"SERZ") {
        let mut reader = SerzReader::new(data);
        let root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        return inspect_serz_node(&root, data.len() as u64, path);
    }

    let xml = std::str::from_utf8(data).map_err(|e| format!("invalid text: {e}"))?;
    let blocks = all_image_blocks(xml);
    if blocks.is_empty() {
        return Err("no image blocks".into());
    }

    let mut stats = RawInspectStats {
        name: extract_tag(xml, "Name", "cDeltaString").unwrap_or_default(),
        texture_count: count_c_hc_textures(xml).max(1),
        block_count: blocks.len(),
        min_width: u32::MAX,
        min_height: u32::MAX,
        ..Default::default()
    };

    for block in &blocks {
        let w: u32 = extract_tag(block, "Width", "sUInt16")
            .or_else(|| extract_tag(block, "Width", "sUInt32"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let h: u32 = extract_tag(block, "Height", "sUInt16")
            .or_else(|| extract_tag(block, "Height", "sUInt32"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        stats.total_blob_bytes += extract_blob_size(block);
        if w == 0 || h == 0 {
            continue;
        }
        stats.width = stats.width.max(w);
        stats.height = stats.height.max(h);
        stats.min_width = stats.min_width.min(w);
        stats.min_height = stats.min_height.min(h);
        if stats.raw_format.is_empty() {
            stats.raw_format = extract_tag(block, "Format", "cDeltaString")
                .unwrap_or_else(|| extract_format(block));
        }
        if !stats.is_swizzled {
            stats.is_swizzled = parse_bool_tag(block, "IsSwizzled");
        }
        if stats.flags_u16 == 0 {
            stats.flags_u16 = extract_unnamed_u16(block);
        }
    }
    if stats.raw_format.is_empty() {
        stats.raw_format = extract_format(blocks[0]);
    }

    finalize_texture_info(stats, data.len() as u64, path)
}

fn inspect_serz_node(
    root: &SerzNode,
    data_len: u64,
    path: Option<&Path>,
) -> Result<TextureInfo, String> {
    let mut stats = RawInspectStats {
        min_width: u32::MAX,
        min_height: u32::MAX,
        ..Default::default()
    };
    collect_serz_stats(root, &mut stats);
    if stats.block_count == 0 {
        return Err("no image blocks".into());
    }
    stats.texture_count = stats.texture_count.max(1);
    finalize_texture_info(stats, data_len, path)
}

fn collect_serz_stats(node: &SerzNode, stats: &mut RawInspectStats) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };

    if name == "cHcTexture" {
        stats.texture_count += 1;
        if stats.name.is_empty()
            && let Some(prop) = find_name_property(children)
        {
            stats.name = prop.to_owned();
        }
    } else if name == "cHcImageDx" {
        stats.block_count += 1;
        let mut w = 0u32;
        let mut h = 0u32;
        let mut fmt: Option<&str> = None;
        let mut swizzled = false;
        let mut flags = 0u16;
        let mut seen_blob = false;

        for child in children {
            match child {
                SerzNode::Property {
                    name,
                    type_name,
                    value,
                    ..
                } => match name.as_str() {
                    "Width" => w = value.trim().parse().unwrap_or(0),
                    "Height" => h = value.trim().parse().unwrap_or(0),
                    "Format" if type_name == "cDeltaString" => fmt = Some(value.trim()),
                    "IsSwizzled" => {
                        let v = value.trim();
                        swizzled = v == "1"
                            || v.eq_ignore_ascii_case("true")
                            || v.eq_ignore_ascii_case("yes")
                            || v.parse::<u32>().is_ok_and(|n| n != 0);
                    }
                    "e" if type_name == "sUInt16" && (!seen_blob || flags == 0) => {
                        if let Ok(v) = value.trim().parse::<u16>() {
                            flags = v;
                        }
                    }
                    _ => {}
                },
                SerzNode::Blob { data } => {
                    seen_blob = true;
                    stats.total_blob_bytes += data.len() as u64;
                }
                _ => {}
            }
        }

        if w > 0 && h > 0 {
            stats.width = stats.width.max(w);
            stats.height = stats.height.max(h);
            stats.min_width = stats.min_width.min(w);
            stats.min_height = stats.min_height.min(h);
        }
        if stats.raw_format.is_empty()
            && let Some(f) = fmt
            && !f.is_empty()
        {
            stats.raw_format = f.to_owned();
        }
        if !stats.is_swizzled {
            stats.is_swizzled = swizzled;
        }
        if stats.flags_u16 == 0 {
            stats.flags_u16 = flags;
        }
        return;
    }

    for child in children {
        collect_serz_stats(child, stats);
    }
}

fn finalize_texture_info(
    mut stats: RawInspectStats,
    data_len: u64,
    path: Option<&Path>,
) -> Result<TextureInfo, String> {
    if stats.width == 0 || stats.height == 0 {
        return Err("invalid dimensions".into());
    }
    if stats.min_width == u32::MAX {
        stats.min_width = stats.width;
        stats.min_height = stats.height;
    }

    let (mip_count, min_width, min_height, mip_storage_mode) = if stats.block_count <= 1 {
        let fmt = tgpcdx_dds_compression(&stats.raw_format);
        let mut levels = 1u32;
        let mut mw = stats.width;
        let mut mh = stats.height;
        let mut sum = mip_level_bytes(stats.width, stats.height, fmt).unwrap_or(0) as u64;
        if fmt != DdsCompression::Unknown && stats.total_blob_bytes > sum {
            while sum < stats.total_blob_bytes && !(mw == 1 && mh == 1) {
                let w = (mw / 2).max(1);
                let h = (mh / 2).max(1);
                let sz = mip_level_bytes(w, h, fmt).unwrap_or(0) as u64;
                if sz == 0 || sz > stats.total_blob_bytes - sum {
                    break;
                }
                sum += sz;
                levels += 1;
                mw = w;
                mh = h;
            }
        }
        (levels, mw, mh, "Blob (1× cHcImageDx)".to_owned())
    } else {
        (
            stats.block_count as u32,
            stats.min_width,
            stats.min_height,
            format!("Separate nodes ({}× cHcImageDx)", stats.block_count),
        )
    };

    let (format_label, dds_format) = format_label_and_dds_name(&stats.raw_format);
    let (file_size_bytes, has_bak, name_matches_filename) = match path {
        Some(p) => {
            let fs_size = fs::metadata(p).map(|m| m.len()).unwrap_or(data_len);
            let has = fs::metadata(append_path_suffix(p, ".bak")).is_ok();
            let file_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let internal_tail = stats.name.rsplit(['\\', '/']).next().unwrap_or(&stats.name);
            let matches = !file_stem.is_empty() && file_stem.eq_ignore_ascii_case(internal_tail);
            (fs_size, has, matches)
        }
        None => (data_len, false, false),
    };

    let header_bytes = file_size_bytes.saturating_sub(stats.total_blob_bytes);

    Ok(TextureInfo {
        name: stats.name,
        is_swizzled: stats.is_swizzled,
        width: stats.width,
        height: stats.height,
        mip_count,
        min_width,
        min_height,
        raw_format: stats.raw_format,
        format_label,
        dds_format,
        texture_count: stats.texture_count,
        mip_storage_mode,
        flags_u16: stats.flags_u16,
        file_size_bytes,
        total_blob_bytes: stats.total_blob_bytes,
        header_bytes,
        name_matches_filename,
        has_bak,
    })
}

/// Валидация и нормализация внутреннего имени текстуры.
pub fn validate_and_normalize_internal_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Internal texture name cannot be empty".into());
    }
    if !trimmed.is_ascii() {
        return Err("Internal name must contain only ASCII symbols (without Cyrillic)".into());
    }

    const FORBIDDEN_EXTS: [&str; 4] = [".tgpcdx", ".dds", ".ace", ".tgpc"];
    if FORBIDDEN_EXTS.iter().any(|ext| {
        trimmed.len() >= ext.len()
            && trimmed[trimmed.len() - ext.len()..].eq_ignore_ascii_case(ext)
    }) {
        return Err(format!(
            "Internal name must not contain a file extension (.tgpcdx, .dds, .ace): {trimmed}"
        ));
    }

    if trimmed
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'"' | b'<' | b'>'))
    {
        return Err("Internal name contains invalid characters.".into());
    }
    Ok(trimmed.replace('/', "\\"))
}

pub fn update_tgpcdx_internal_name(tgpcdx_path: &Path, new_name: &str) -> Result<(), String> {
    append_tgpcdx_log(&format!(
        "update_tgpcdx_internal_name: path={}, new_name={new_name}",
        tgpcdx_path.display()
    ));

    let normalized = validate_and_normalize_internal_name(new_name)?;

    let meta = fs::metadata(tgpcdx_path).map_err(|e| e.to_string())?;
    if meta.permissions().readonly() {
        return Err(format!(
            "File is read-only (possibly inside a .ap archive): {}. Unpack it before editing.",
            tgpcdx_path.display()
        ));
    }

    let data = fs::read(tgpcdx_path).map_err(|e| e.to_string())?;
    if !data.starts_with(b"SERZ") {
        return Err("TgPcDx save requires a binary SERZ file".into());
    }

    let mut reader = SerzReader::new(&data);
    let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
    let preamble = reader.preamble.clone();

    update_tgpcdx_internal_names(&mut root, &normalized)?;

    let out_bytes =
        encode_tgpcdx_to_vec(&root, preamble.as_ref()).map_err(|e| format!("serz encode: {e}"))?;
    if out_bytes.is_empty() {
        return Err("Serialization result is empty".into());
    }

    let bak_path = append_path_suffix(tgpcdx_path, ".bak");
    if !bak_path.exists() {
        fs::copy(tgpcdx_path, &bak_path)
            .map_err(|e| format!("Failed to create backup {}: {e}", bak_path.display()))?;
        append_tgpcdx_log(&format!("Backup created: {}", bak_path.display()));
    }

    write_output_bytes(tgpcdx_path, &out_bytes)?;

    append_tgpcdx_log(&format!(
        "Internal texture name updated {}: {normalized}",
        tgpcdx_path.display()
    ));
    Ok(())
}

/// Восстанавливает `.TgPcDx` из соседнего `.bak` (атомарно).
pub(crate) fn restore_tgpcdx_from_backup(tgpcdx_path: &Path) -> Result<(), String> {
    let bak_path = append_path_suffix(tgpcdx_path, ".bak");
    if !bak_path.is_file() {
        return Err(format!("Backup file not found: {}", bak_path.display()));
    }
    let bytes = fs::read(&bak_path).map_err(|e| format!("Failed to read backup: {e}"))?;
    write_output_bytes(tgpcdx_path, &bytes)?;
    append_tgpcdx_log(&format!("Restored from backup: {}", tgpcdx_path.display()));
    Ok(())
}

/// Обновляет имена cHcTexture/Name с учётом групп текстур.
pub fn update_tgpcdx_internal_names(root: &mut SerzNode, new_name: &str) -> Result<(), String> {
    let mut current_names = Vec::new();
    collect_c_hc_texture_names(root, &mut current_names);

    if current_names.is_empty() {
        return Err(
            "cHcTexture node with a child Name (cDeltaString) was not found in file.".into(),
        );
    }

    if current_names.len() == 1 {
        update_first_c_hc_texture_name(root, new_name);
        return Ok(());
    }

    let common_prefix = find_common_prefix(&current_names);
    let new_prefix = new_name.trim_end_matches('\\');
    update_c_hc_texture_names_with_prefix(root, &common_prefix, new_prefix);
    Ok(())
}

fn collect_c_hc_texture_names(node: &SerzNode, out: &mut Vec<String>) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    if name == "cHcTexture"
        && let Some(prop) = find_name_property(children)
    {
        out.push(prop.to_owned());
    }
    for child in children {
        collect_c_hc_texture_names(child, out);
    }
}

fn update_first_c_hc_texture_name(node: &mut SerzNode, new_name: &str) -> bool {
    let SerzNode::Element { name, children, .. } = node else {
        return false;
    };
    if name == "cHcTexture" {
        for child in children.iter_mut() {
            if let SerzNode::Property {
                name: prop_name,
                type_name,
                value,
                ..
            } = child
                && prop_name == "Name"
                && type_name == "cDeltaString"
            {
                *value = new_name.to_owned();
                return true;
            }
        }
    }
    children
        .iter_mut()
        .any(|child| update_first_c_hc_texture_name(child, new_name))
}

fn update_c_hc_texture_names_with_prefix(
    node: &mut SerzNode,
    common_prefix: &str,
    new_prefix: &str,
) {
    let SerzNode::Element { name, children, .. } = node else {
        return;
    };
    if name == "cHcTexture" {
        for child in children.iter_mut() {
            if let SerzNode::Property {
                name: prop_name,
                type_name,
                value,
                ..
            } = child
                && prop_name == "Name"
                && type_name == "cDeltaString"
            {
                let suffix = value
                    .strip_prefix(common_prefix)
                    .unwrap_or(value.as_str())
                    .trim_start_matches('\\');
                *value = if new_prefix.is_empty() {
                    suffix.to_owned()
                } else if suffix.is_empty() {
                    new_prefix.to_owned()
                } else {
                    format!("{new_prefix}\\{suffix}")
                };
                break;
            }
        }
    }
    for child in children.iter_mut() {
        update_c_hc_texture_names_with_prefix(child, common_prefix, new_prefix);
    }
}

fn find_name_property(children: &[SerzNode]) -> Option<&str> {
    children.iter().find_map(|child| match child {
        SerzNode::Property {
            name,
            type_name,
            value,
            ..
        } if name == "Name" && type_name == "cDeltaString" => Some(value.as_str()),
        _ => None,
    })
}

fn find_common_prefix(names: &[String]) -> String {
    let Some((first, rest)) = names.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        return first
            .rfind('\\')
            .map(|pos| first[..=pos].to_owned())
            .unwrap_or_default();
    }

    let mut prefix = first.as_str();
    for name in rest {
        let common_len = prefix
            .char_indices()
            .zip(name.chars())
            .take_while(|&((_, a), b)| a == b)
            .map(|((idx, a), _)| idx + a.len_utf8())
            .last()
            .unwrap_or(0);
        prefix = &prefix[..common_len];
        match prefix.rfind('\\') {
            Some(pos) => prefix = &prefix[..=pos],
            None => return String::new(),
        }
    }
    prefix.to_owned()
}

/// Writes output bytes atomically with `.old` rollback on failure (Windows-safe).
pub fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"SERZ") {
        validate_tgpcdx_binary(bytes, None)?;
    }
    let tmp = append_path_suffix(output, ".tmp");
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    let old = append_path_suffix(output, ".old");
    if output.exists() {
        let _ = fs::remove_file(&old);
        fs::rename(output, &old).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })?;
    }
    fs::rename(&tmp, output).map_err(|e| {
        let _ = fs::rename(&old, output);
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    let _ = fs::remove_file(&old);
    Ok(())
}

pub(super) fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Appends a log message to the TgPcDx log file.
pub fn append_tgpcdx_log(message: &str) {
    let log_path = log_file();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = writeln!(f, "{message}");
    }
}

/// Расширенная карточка текстуры с учётом режима каналов и текущего уровня мипмапа.
pub(crate) fn texture_info_text_with_view(
    info: &TextureInfo,
    path: &Path,
    mode: ChannelMode,
    mip_index: usize,
    mip_w: u32,
    mip_h: u32,
) -> String {
    let dds_name = info.dds_format.unwrap_or("unknown");
    let mut s = String::with_capacity(512);
    if !info.name.is_empty() {
        let mark = if info.name_matches_filename {
            "✓"
        } else {
            "⚠ file name is different"
        };
        let _ = writeln!(s, "Internal name (Name): {} {mark}", info.name);
    }
    let _ = writeln!(
        s,
        "Resolution: {}×{} px | Textures in group: {} | Mipmaps: {} ({}×{} → {}×{}) [{}]",
        info.width,
        info.height,
        info.texture_count,
        info.mip_count,
        info.width,
        info.height,
        info.min_width,
        info.min_height,
        info.mip_storage_mode,
    );
    let _ = writeln!(
        s,
        "Format: {} [{}] | DDS: {dds_name} | IsSwizzled: {} | Flags(u16): {}",
        info.format_label,
        info.raw_format,
        u8::from(info.is_swizzled),
        info.flags_u16,
    );
    let bak = if info.has_bak { "yes" } else { "no" };
    let _ = writeln!(
        s,
        "File size: {} ({} B) | Pixels (blob): {} B | Header: {} B | Backup (.bak): {bak}",
        format_size_bytes(info.file_size_bytes),
        format_bytes_spaces(info.file_size_bytes),
        format_bytes_spaces(info.total_blob_bytes),
        format_bytes_spaces(info.header_bytes),
    );
    let _ = writeln!(
        s,
        "View: {} | Active mip: #{mip_index} ({mip_w}×{mip_h} px) | Import requires: DDS {dds_name} (power of two, with mipmaps)",
        mode.label(),
    );
    let _ = write!(s, "TGPCDX  {}", path.display());
    s
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

fn format_bytes_spaces(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}
