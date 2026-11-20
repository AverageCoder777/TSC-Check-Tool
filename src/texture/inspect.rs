//! Texture metadata, inspection, and internal name editing.

use std::fs;
use std::path::{Path, PathBuf};

use super::format::{
    DdsCompression, format_label_and_dds_name, mip_level_bytes, tgpcdx_dds_compression,
};
use super::serz_parse::{
    ImageMip, all_image_blocks, count_c_hc_textures, extract_blob, extract_blob_size,
    extract_format, extract_tag, extract_unnamed_u16, hex_decode, parse_bool_tag,
};
use crate::common::serz::SerzNode;

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
    let (xml, total_blob_bytes) = if data.starts_with(b"SERZ") {
        // Single SERZ parse: get XML for metadata and blob sizes before clearing.
        let mut reader = crate::common::serz::SerzReader::new(data);
        let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;

        // Collect blob sizes BEFORE clearing blobs for XML generation.
        let total_blob_bytes = collect_blob_bytes_and_clear(&mut root);

        let xml = crate::common::serz::SerzReader::to_xml(&root, 0, true);
        (xml, total_blob_bytes)
    } else {
        let xml = String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?;
        let blocks = all_image_blocks(&xml);
        let mips = blocks
            .into_iter()
            .map(|block| -> Result<ImageMip, String> {
                let (width, height) =
                    super::serz_parse::image_dimensions(block).ok_or("invalid mip dimensions")?;
                let bytes = extract_blob(block)
                    .map(|blob| hex_decode(&blob))
                    .unwrap_or_default();
                Ok(ImageMip {
                    format: extract_format(block),
                    width,
                    height,
                    data: bytes,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let total_blob_bytes: u64 = mips.iter().map(|mip| mip.data.len() as u64).sum();
        (xml, total_blob_bytes)
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

    // total_blob_bytes already computed above (SERZ: before clear_blobs, XML: from mips).
    let total_blob_bytes = if total_blob_bytes == 0 {
        blocks.iter().map(|b| extract_blob_size(b)).sum()
    } else {
        total_blob_bytes
    };

    let (mip_count, min_width, min_height, mip_storage_mode) = if blocks.len() <= 1 {
        let fmt = tgpcdx_dds_compression(&raw_format);
        let mut levels = 1u32;
        let mut mw = width;
        let mut mh = height;
        let mut sum = mip_level_bytes(width, height, fmt).unwrap_or(0) as u64;
        if fmt != DdsCompression::Unknown && total_blob_bytes > sum {
            let mut w = (width / 2).max(1);
            let mut h = (height / 2).max(1);
            // Не заходим в цикл если уже на 1×1 — избегаем лишнего уровня для битой 1×1 текстуры.
            while sum < total_blob_bytes && !(w == 1 && h == 1) {
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
            let has = std::fs::metadata(&bak_path).is_ok();
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

pub fn inspect_tgpcdx_file(path: &Path) -> Result<TextureInfo, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    inspect_tgpcdx_with_path(&data, Some(path))
}

/// Валидация и нормализация внутреннего имени текстуры.
/// Возвращает нормализованное имя или ошибку.
pub fn validate_and_normalize_internal_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Внутреннее имя текстуры не может быть пустым".into());
    }

    // Запрещаем расширения на конце.
    let lower = trimmed.to_lowercase();
    if lower.ends_with(".tgpcdx")
        || lower.ends_with(".dds")
        || lower.ends_with(".ace")
        || lower.ends_with(".tgpc")
    {
        return Err(format!(
            "Внутреннее имя не должно содержать расширение файла (.tgpcdx, .dds, .ace): {}",
            trimmed
        ));
    }

    // Нормализация: прямые слэши -> обратные.
    let normalized = trimmed.replace('/', "\\");

    // Проверка на не-ASCII символы (TS Classic не поддерживает Unicode в SERZ).
    if !normalized.is_ascii() {
        return Err("Внутреннее имя должно содержать только ASCII символы (без кириллицы)".into());
    }

    // Проверка на спецсимволы, которые могут сломать парсинг.
    if normalized
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '<' || c == '>')
    {
        return Err("Внутреннее имя содержит недопустимые символы".into());
    }

    Ok(normalized)
}

pub fn update_tgpcdx_internal_name(tgpcdx_path: &Path, new_name: &str) -> Result<(), String> {
    append_tgpcdx_log(&format!(
        "update_tgpcdx_internal_name: path={}, new_name={}",
        tgpcdx_path.display(),
        new_name
    ));

    // Валидация и нормализация имени.
    let normalized = validate_and_normalize_internal_name(new_name)?;

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

    let mut reader = crate::common::serz::SerzReader::new(&data);
    let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
    let preamble = reader.preamble.clone();

    // Обновляем имена с учётом групп текстур.
    update_tgpcdx_internal_names(&mut root, &normalized)?;

    let out_bytes = crate::common::serz::encode_tgpcdx_to_vec(&root, preamble.as_ref())
        .map_err(|e| format!("serz encode: {e}"))?;

    if out_bytes.is_empty() {
        return Err("Пустой результат сериализации".into());
    }

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

    write_output_bytes(tgpcdx_path, &out_bytes)?;

    append_tgpcdx_log(&format!(
        "Обновлено внутреннее имя текстуры {}: {}",
        tgpcdx_path.display(),
        normalized
    ));
    Ok(())
}

/// Обновляет имена cHcTexture/Name с учётом групп текстур.
/// Для одиночных текстур (texture_count == 1) — просто ставит новое имя.
/// Для групп — меняет общий префикс, сохраняя уникальные суффиксы каждого узла.
pub fn update_tgpcdx_internal_names(root: &mut SerzNode, new_name: &str) -> Result<(), String> {
    // Первый проход: собираем текущие имена всех cHcTexture.
    let mut current_names = Vec::new();
    collect_c_hc_texture_names(root, &mut current_names);

    if current_names.is_empty() {
        return Err("В файле не найден узел cHcTexture с дочерним Name (cDeltaString)".into());
    }

    if current_names.len() == 1 {
        // Одиночная текстура — просто обновляем Name.
        update_first_c_hc_texture_name(root, new_name);
        return Ok(());
    }

    // Группа текстур: определяем общий префикс.
    let common_prefix = find_common_prefix(&current_names);

    // Второй проход: обновляем каждый Name, заменяя префикс на новый, сохраняя суффикс.
    let new_prefix = new_name.trim_end_matches('\\');
    update_c_hc_texture_names_with_prefix(root, &current_names, &common_prefix, new_prefix)?;

    Ok(())
}

fn collect_c_hc_texture_names(node: &SerzNode, out: &mut Vec<String>) {
    if let SerzNode::Element { name, .. } = node
        && name == "cHcTexture"
        && let Some(prop) = find_name_property(node)
    {
        out.push(prop.clone());
    }
    if let SerzNode::Element { children, .. } = node {
        for child in children {
            collect_c_hc_texture_names(child, out);
        }
    }
}

fn update_first_c_hc_texture_name(node: &mut SerzNode, new_name: &str) {
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
                    return;
                }
            }
        }
        for child in children.iter_mut() {
            update_first_c_hc_texture_name(child, new_name);
        }
    }
}

fn update_c_hc_texture_names_with_prefix(
    node: &mut SerzNode,
    current_names: &[String],
    common_prefix: &str,
    new_prefix: &str,
) -> Result<(), String> {
    if let SerzNode::Element { name, children, .. } = node {
        if name == "cHcTexture" {
            // Ищем Name в children.
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
                    // Ищем индекс этого имени в current_names.
                    if let Some(idx) = current_names.iter().position(|n| n == value) {
                        let old_name = &current_names[idx];
                        let suffix = if old_name.len() > common_prefix.len() {
                            &old_name[common_prefix.len()..]
                        } else {
                            ""
                        };
                        // Склеиваем через \ (если оба непустые), чтобы не потерять разделитель.
                        *value = if new_prefix.is_empty() {
                            suffix.to_string()
                        } else if suffix.is_empty() {
                            new_prefix.to_string()
                        } else {
                            format!("{}\\{}", new_prefix, suffix.trim_start_matches('\\'))
                        };
                    }
                    break;
                }
            }
        }
        for child in children.iter_mut() {
            update_c_hc_texture_names_with_prefix(child, current_names, common_prefix, new_prefix)?;
        }
    }
    Ok(())
}

fn find_name_property(node: &SerzNode) -> Option<&String> {
    if let SerzNode::Element { children, .. } = node {
        for child in children {
            if let SerzNode::Property {
                name: prop_name,
                type_name,
                value,
                ..
            } = child
                && prop_name == "Name"
                && type_name == "cDeltaString"
            {
                return Some(value);
            }
        }
    }
    None
}

fn find_common_prefix(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    if names.len() == 1 {
        // Для одного имени: префикс — всё до последнего \ включительно.
        if let Some(pos) = names[0].rfind('\\') {
            return names[0][..=pos].to_string();
        }
        return String::new();
    }

    // Находим общий префикс среди всех имён.
    let mut prefix = names[0].clone();
    for name in &names[1..] {
        let mut new_len = 0;
        for (a, b) in prefix.chars().zip(name.chars()) {
            if a == b {
                new_len += a.len_utf8();
            } else {
                break;
            }
        }
        prefix.truncate(new_len);
        // Обрезаем до последнего \ включительно.
        if let Some(pos) = prefix.rfind('\\') {
            prefix.truncate(pos + 1);
        } else {
            prefix.clear();
        }
    }
    prefix
}

/// Writes output bytes atomically.
pub fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"SERZ") {
        crate::common::serz::validate_tgpcdx_binary(bytes, None)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", output.display()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    let old = PathBuf::from(format!("{}.old", output.display()));
    if output.exists() {
        let _ = fs::remove_file(&old);
        fs::rename(output, &old).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, output).map_err(|e| {
        let _ = fs::rename(&old, output);
        e.to_string()
    })?;
    let _ = fs::remove_file(&old);
    Ok(())
}

/// Собирает суммарный размер всех Blob и очищает их (для генерации XML без блобов).
fn collect_blob_bytes_and_clear(node: &mut crate::common::serz::SerzNode) -> u64 {
    let mut total = 0u64;
    if let crate::common::serz::SerzNode::Element { children, .. } = node {
        for child in children {
            if let crate::common::serz::SerzNode::Blob { data } = child {
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
    use std::io::Write;
    let log_path = crate::app::settings::log_file();
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
