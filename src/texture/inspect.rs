//! Texture metadata, inspection, and internal name editing.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::format::{
    DdsCompression, format_label_and_dds_name, mip_level_bytes, tgpcdx_dds_compression,
};
use super::serz_parse::{
    all_image_blocks, count_c_hc_textures, extract_blob_size, extract_format, extract_tag,
    extract_unnamed_u16, parse_bool_tag,
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

/// Вариант с указанием пути на диске: позволяет сверить имя и проверить .bak.
pub fn inspect_tgpcdx_with_path(data: &[u8], path: Option<&Path>) -> Result<TextureInfo, String> {
    let (xml, serz_blob_bytes): (Cow<'_, str>, u64) = if data.starts_with(b"SERZ") {
        // Single SERZ parse: get XML for metadata and blob sizes before clearing.
        let mut reader = SerzReader::new(data);
        let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;

        // Collect blob sizes BEFORE clearing blobs for XML generation.
        let total = collect_blob_bytes_and_clear(&mut root);
        let xml = SerzReader::to_xml(&root, 0, true);
        (Cow::Owned(xml), total)
    } else {
        let xml = std::str::from_utf8(data).map_err(|e| format!("invalid text: {e}"))?;
        (Cow::Borrowed(xml), 0)
    };

    let blocks = all_image_blocks(&xml);
    if blocks.is_empty() {
        return Err("no image blocks".into());
    }

    let texture_count = count_c_hc_textures(&xml).max(1);

    // width/height = максимальные (базовый уровень), min_width/min_height = минимальные.
    let mut width = 0u32;
    let mut height = 0u32;
    let mut min_width = u32::MAX;
    let mut min_height = u32::MAX;
    let mut raw_format = String::new();
    let mut is_swizzled = false;
    let mut flags_u16 = 0u16;

    for block in &blocks {
        let w: u32 = extract_tag(block, "Width", "sUInt16")
            .or_else(|| extract_tag(block, "Width", "sUInt32"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let h: u32 = extract_tag(block, "Height", "sUInt16")
            .or_else(|| extract_tag(block, "Height", "sUInt32"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if w == 0 || h == 0 {
            continue;
        }
        width = width.max(w);
        height = height.max(h);
        min_width = min_width.min(w);
        min_height = min_height.min(h);
        if raw_format.is_empty() {
            raw_format = extract_tag(block, "Format", "cDeltaString")
                .unwrap_or_else(|| extract_format(block));
        }
        if !is_swizzled {
            is_swizzled = parse_bool_tag(block, "IsSwizzled");
        }
        if flags_u16 == 0 {
            flags_u16 = extract_unnamed_u16(block);
        }
    }

    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    if min_width == u32::MAX {
        min_width = width;
        min_height = height;
    }

    if raw_format.is_empty() {
        raw_format = extract_format(blocks[0]);
    }

    let total_blob_bytes = if serz_blob_bytes > 0 {
        serz_blob_bytes
    } else {
        blocks.iter().map(|b| extract_blob_size(b)).sum()
    };

    let (mip_count, min_width, min_height, mip_storage_mode) = if blocks.len() <= 1 {
        let fmt = tgpcdx_dds_compression(&raw_format);
        let mut levels = 1u32;
        let mut mw = width;
        let mut mh = height;
        let mut sum = mip_level_bytes(width, height, fmt).unwrap_or(0) as u64;
        if fmt != DdsCompression::Unknown && total_blob_bytes > sum {
            while sum < total_blob_bytes && !(mw == 1 && mh == 1) {
                let w = (mw / 2).max(1);
                let h = (mh / 2).max(1);
                let sz = mip_level_bytes(w, h, fmt).unwrap_or(0) as u64;
                if sz == 0 || sz > total_blob_bytes - sum {
                    break;
                }
                sum += sz;
                levels += 1;
                mw = w;
                mh = h;
            }
        }
        (
            levels,
            mw,
            mh,
            "Единый упакованный блоб (1× cHcImageDx)".to_string(),
        )
    } else {
        (
            blocks.len() as u32,
            min_width,
            min_height,
            format!("Раздельные узлы ({}× cHcImageDx)", blocks.len()),
        )
    };

    let name = extract_tag(&xml, "Name", "cDeltaString").unwrap_or_default();
    let (format_label, dds_format) = format_label_and_dds_name(&raw_format);

    let (file_size_bytes, has_bak, name_matches_filename) = match path {
        Some(p) => {
            let fs_size = fs::metadata(p)
                .map(|m| m.len())
                .unwrap_or(data.len() as u64);
            let has = fs::metadata(append_path_suffix(p, ".bak")).is_ok();
            let file_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let internal_tail = name.rsplit(['\\', '/']).next().unwrap_or(&name);
            let matches = !file_stem.is_empty() && file_stem.eq_ignore_ascii_case(internal_tail);
            (fs_size, has, matches)
        }
        None => (data.len() as u64, false, false),
    };

    let header_bytes = file_size_bytes.saturating_sub(total_blob_bytes);

    Ok(TextureInfo {
        name,
        is_swizzled,
        width,
        height,
        mip_count,
        min_width,
        min_height,
        raw_format,
        format_label,
        dds_format,
        texture_count,
        mip_storage_mode,
        flags_u16,
        file_size_bytes,
        total_blob_bytes,
        header_bytes,
        name_matches_filename,
        has_bak,
    })
}

/// Валидация и нормализация внутреннего имени текстуры.
/// Возвращает нормализованное имя или ошибку.
pub fn validate_and_normalize_internal_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Внутреннее имя текстуры не может быть пустым".into());
    }

    if !trimmed.is_ascii() {
        return Err("Внутреннее имя должно содержать только ASCII символы (без кириллицы)".into());
    }

    const FORBIDDEN_EXTS: [&str; 4] = [".tgpcdx", ".dds", ".ace", ".tgpc"];
    if FORBIDDEN_EXTS.iter().any(|ext| {
        trimmed.len() >= ext.len() && trimmed[trimmed.len() - ext.len()..].eq_ignore_ascii_case(ext)
    }) {
        return Err(format!(
            "Внутреннее имя не должно содержать расширение файла (.tgpcdx, .dds, .ace): {trimmed}"
        ));
    }

    if trimmed
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'"' | b'<' | b'>'))
    {
        return Err("Внутреннее имя содержит недопустимые символы".into());
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
            "Файл доступен только для чтения (возможно, внутри .ap-архива): {}. Распакуйте файл перед редактированием.",
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
        return Err("Пустой результат сериализации".into());
    }

    let bak_path = append_path_suffix(tgpcdx_path, ".bak");
    if !bak_path.exists() {
        fs::copy(tgpcdx_path, &bak_path).map_err(|e| {
            format!(
                "Не удалось создать резервную копию {}: {e}",
                bak_path.display()
            )
        })?;
        append_tgpcdx_log(&format!("Создана резервная копия: {}", bak_path.display()));
    }

    write_output_bytes(tgpcdx_path, &out_bytes)?;

    append_tgpcdx_log(&format!(
        "Обновлено внутреннее имя текстуры {}: {normalized}",
        tgpcdx_path.display()
    ));
    Ok(())
}

/// Обновляет имена cHcTexture/Name с учётом групп текстур.
/// Для одиночных текстур (texture_count == 1) — просто ставит новое имя.
/// Для групп — меняет общий префикс, сохраняя уникальные суффиксы каждого узла.
pub fn update_tgpcdx_internal_names(root: &mut SerzNode, new_name: &str) -> Result<(), String> {
    let mut current_names = Vec::new();
    collect_c_hc_texture_names(root, &mut current_names);

    if current_names.is_empty() {
        return Err("В файле не найден узел cHcTexture с дочерним Name (cDeltaString)".into());
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

/// Writes output bytes atomically with rollback on failure.
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

fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Собирает суммарный размер всех Blob и очищает их (для генерации XML без блобов).
fn collect_blob_bytes_and_clear(node: &mut SerzNode) -> u64 {
    let mut total = 0u64;
    if let SerzNode::Element { children, .. } = node {
        for child in children {
            if let SerzNode::Blob { data } = child {
                total += data.len() as u64;
                data.clear();
            } else {
                total += collect_blob_bytes_and_clear(child);
            }
        }
    }
    total
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

/// Собирает многострочный текст информационной панели окна просмотра текстур.
pub fn texture_info_text(info: &TextureInfo, path: &Path, show_alpha: bool) -> String {
    let dds_name = info.dds_format.unwrap_or("неизвестный");
    let mut s = String::with_capacity(512);
    if !info.name.is_empty() {
        let mark = if info.name_matches_filename {
            "✓"
        } else {
            "⚠ имя файла отличается"
        };
        let _ = writeln!(s, "Внутреннее имя (Name): {} {mark}", info.name);
    }
    let _ = writeln!(
        s,
        "Разрешение: {}×{} px | Текстур в группе: {} | Мипмапов: {} ({}×{} → {}×{}) [{}]",
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
        "Формат: {} [{}] | DDS: {dds_name} | IsSwizzled: {} | Flags(u16): {}",
        info.format_label,
        info.raw_format,
        u8::from(info.is_swizzled),
        info.flags_u16,
    );
    let bak = if info.has_bak { "есть" } else { "нет" };
    let _ = writeln!(
        s,
        "Размер файла: {} ({} Б) | Пиксели (blob): {} Б | Заголовок: {} Б | Бэкап (.bak): {bak}",
        format_size_bytes(info.file_size_bytes),
        format_bytes_spaces(info.file_size_bytes),
        format_bytes_spaces(info.total_blob_bytes),
        format_bytes_spaces(info.header_bytes),
    );
    let _ = writeln!(
        s,
        "Для импорта требуется: DDS в формате {dds_name}, размеры степень двойки, с мипмапами"
    );
    if show_alpha {
        s.push_str("Режим просмотра: альфа-канал\n");
    }
    let _ = write!(s, "TGPCDX  {}", path.display());
    s
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} КБ", bytes as f64 / 1024.0)
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
