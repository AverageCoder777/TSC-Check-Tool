//! Texture metadata, inspection, and internal name editing.

use std::fs;
use std::path::{Path, PathBuf};

use crate::serz::SerzNode;
use super::format::{
    format_label_and_dds_name, mip_level_bytes, tgpcdx_dds_compression, DdsCompression,
};
use super::serz_parse::{
    all_image_blocks, count_c_hc_textures, extract_blob,
    extract_blob_size, extract_format, extract_tag, extract_unnamed_u16, hex_decode,
    image_mips_from_bytes, parse_bool_tag, serz_bytes_to_metadata_xml,
};

/// Экстрагированные метаданные тектуры .TgPcDx для панели информации.
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
    let xml = if data.starts_with(b"SERZ") {
        serz_bytes_to_metadata_xml(data)?
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?
    };

    let blocks = all_image_blocks(&xml);
    if blocks.is_empty() {
        return Err("no image blocks".into());
    }

    // Количество текстур в группе cHcTextureGroup.
    let texture_count = count_c_hc_textures(&xml).max(1);

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
        if width == 0 {
            width = w;
            height = h;
        }
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

    // Сырой тег формата (fallback).
    if raw_format.is_empty() {
        raw_format = extract_format(blocks[0]);
    }

    // Размеры blob-данных.
    let total_blob_bytes: u64 = if data.starts_with(b"SERZ") {
        image_mips_from_bytes(data)?
            .iter()
            .map(|mip| mip.data.len() as u64)
            .sum()
    } else {
        blocks
            .iter()
            .map(|b| {
                extract_blob(b)
                    .map(|s| hex_decode(&s).len() as u64)
                    .unwrap_or(0)
            })
            .sum()
    };
    // Если blob-строки не распознались, используем d:size.
    let total_blob_bytes = if total_blob_bytes == 0 {
        blocks.iter().map(|b| extract_blob_size(b)).sum()
    } else {
        total_blob_bytes
    };

    // Режим хранения мипмапов.
    let (mip_count, min_width, min_height, mip_storage_mode) = if blocks.len() <= 1 {
        // Единый упакованный блоб: вычисляем реальное число уровней.
        let fmt = tgpcdx_dds_compression(&raw_format);
        let mut levels = 1u32;
        let mut mw = width;
        let mut mh = height;
        let mut sum = mip_level_bytes(width, height, fmt).unwrap_or(0) as u64;
        if fmt != DdsCompression::Unknown && total_blob_bytes > sum {
            let mut w = (width / 2).max(1);
            let mut h = (height / 2).max(1);
            while sum < total_blob_bytes {
                let sz = mip_level_bytes(w, h, fmt).unwrap_or(0) as u64;
                if sz == 0 || sum + sz > total_blob_bytes {
                    break;
                }
                sum += sz;
                levels += 1;
                mw = w;
                mh = h;
                if w == 1 && h == 1 {
                    break;
                }
                w = (w / 2).max(1);
                h = (h / 2).max(1);
            }
        }
        if sum != total_blob_bytes && total_blob_bytes > 0 {
            // Не совпало — оставляем хотя бы 1.
            levels = levels.max(1);
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

    // Путь / файл.
    let (file_size_bytes, has_bak, name_matches_filename) = match path {
        Some(p) => {
            let fs_size = std::fs::metadata(p)
                .map(|m| m.len())
                .unwrap_or(data.len() as u64);
            let bak_path = PathBuf::from(format!("{}.bak", p.display()));
            let has = match std::fs::metadata(&bak_path) {
                Ok(_) => true,
                Err(_) => false,
            };
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

/// Inspects a TgPcDx file from disk.
pub fn inspect_tgpcdx_file(path: &Path) -> Result<TextureInfo, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    inspect_tgpcdx_with_path(&data, Some(path))
}

/// Обновляет внутреннее имя текстуры (узел `Name` / `cDeltaString`) в `.TgPcDx`.
pub fn update_tgpcdx_internal_name(tgpcdx_path: &Path, new_name: &str) -> Result<(), String> {
    append_tgpcdx_log(&format!(
        "update_tgpcdx_internal_name: path={}, new_name={}",
        tgpcdx_path.display(),
        new_name
    ));
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err("Внутреннее имя текстуры не может быть пустым".into());
    }
    // Нормализация путей TS Classic: прямые слэши -> обратные.
    let normalized = trimmed.replace('/', "\\");

    // Файл внутри .ap-архива доступен только для чтения — редактирование невозможно.
    let meta = std::fs::metadata(tgpcdx_path).map_err(|e| e.to_string())?;
    if meta.permissions().readonly() {
        return Err(format!(
            "Файл доступен только для чтения (возможно, внутри .ap-архива): {}. Распакуйте файл перед редактированием.",
            tgpcdx_path.display()
        ));
    }

    let data = std::fs::read(tgpcdx_path).map_err(|e| e.to_string())?;
    if !data.starts_with(b"SERZ") {
        return Err("TgPcDx save requires a binary SERZ file".into());
    }

    // Резервная копия <путь>.TgPcDx.bak, если её ещё нет.
    let bak_path = PathBuf::from(format!("{}.bak", tgpcdx_path.display()));
    if !bak_path.exists() {
        std::fs::copy(tgpcdx_path, &bak_path).map_err(|e| {
            format!(
                "Не удалось создать резервную копию {}: {e}",
                bak_path.display()
            )
        })?;
        append_tgpcdx_log(&format!("Создана резервная копия: {}", bak_path.display()));
    }

    let mut reader = crate::serz::SerzReader::new(&data);
    let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
    let preamble = reader.preamble.clone();

    // Обновляем все cHcTexture/Name.
    let updated = update_internal_name_in_node(&mut root, &normalized);
    if !updated {
        return Err("В файле не найден узел cHcTexture с дочерним Name (cDeltaString)".into());
    }

    let out_bytes = crate::serz::encode_tgpcdx_to_vec(&root, preamble.as_ref())
        .map_err(|e| format!("serz encode: {e}"))?;

    if out_bytes.is_empty() {
        return Err("Пустой результат сериализации".into());
    }

    write_output_bytes(tgpcdx_path, &out_bytes)?;

    append_tgpcdx_log(&format!(
        "Обновлено внутреннее имя текстуры {}: {}",
        tgpcdx_path.display(),
        normalized
    ));
    Ok(())
}

pub fn update_internal_name_in_node(node: &mut SerzNode, new_name: &str) -> bool {
    let mut changed = false;
    if let SerzNode::Element { name, children, .. } = node {
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
                    *value = new_name.to_string();
                    changed = true;
                }
            }
        }
        for child in children.iter_mut() {
            if update_internal_name_in_node(child, new_name) {
                changed = true;
            }
        }
    }
    changed
}

/// Writes output bytes atomically.
pub fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"SERZ") {
        crate::serz::validate_tgpcdx_binary(bytes, None)?;
    }
    let tmp = output.with_extension(format!("{}.tmp", output.extension().and_then(|e| e.to_str()).unwrap_or("tmp")));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    let _ = fs::remove_file(output);
    fs::rename(&tmp, output).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(())
}

/// Appends a log message to the TgPcDx log file.
pub fn append_tgpcdx_log(message: &str) {
    use std::io::Write;
    let log_path = crate::settings::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{}", message);
    }
}

/// Собирает многострочный текст информационной панели окна просмотра текстур.
pub fn texture_info_text(info: &TextureInfo, path: &Path, show_alpha: bool) -> String {
    let dds_name = info.dds_format.map_or("неизвестный", |n| n);
    let mut s = String::new();
    if !info.name.is_empty() {
        let mark = if info.name_matches_filename {
            "✓"
        } else {
            "⚠ имя файла отличается"
        };
        s.push_str(&format!("Внутреннее имя (Name): {} {}\n", info.name, mark));
    }
    s.push_str(&format!(
        "Разрешение: {}×{} px | Текстур в группе: {} | Мипмапов: {} ({}×{} → {}×{}) [{}]\n",
        info.width,
        info.height,
        info.texture_count,
        info.mip_count,
        info.width,
        info.height,
        info.min_width,
        info.min_height,
        info.mip_storage_mode,
    ));
    s.push_str(&format!(
        "Формат: {} [{}] | DDS: {} | IsSwizzled: {} | Flags(u16): {}\n",
        info.format_label,
        info.raw_format,
        dds_name,
        if info.is_swizzled { 1 } else { 0 },
        info.flags_u16,
    ));
    let bak = if info.has_bak { "есть" } else { "нет" };
    s.push_str(&format!(
        "Размер файла: {} ({} Б) | Пиксели (blob): {} Б | Заголовок: {} Б | Бэкап (.bak): {}\n",
        format_size_bytes(info.file_size_bytes),
        format_bytes_spaces(info.file_size_bytes),
        format_bytes_spaces(info.total_blob_bytes),
        format_bytes_spaces(info.header_bytes),
        bak,
    ));
    s.push_str(&format!(
        "Для импорта требуется: DDS в формате {}, размеры степень двойки, с мипмапами\n",
        dds_name
    ));
    if show_alpha {
        s.push_str("Режим просмотра: альфа-канал\n");
    }
    s.push_str(&format!("TGPCDX  {}", path.display()));
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
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.insert(0, ' ');
        }
        out.insert(0, c);
    }
    out
}