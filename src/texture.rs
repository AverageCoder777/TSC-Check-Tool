//! TgPcDx texture decoding, DDS export/import.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

pub fn decode_tgpcdx_to_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    decode_tgpcdx_with_max_dimension(data, None)
}

pub fn decode_tgpcdx_for_viewer(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    decode_tgpcdx_with_max_dimension(data, Some(1024))
}

fn decode_tgpcdx_with_max_dimension(
    data: &[u8],
    max_dimension: Option<u32>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let xml = if data.starts_with(b"SERZ") {
        serz_bytes_to_xml(data)?
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?
    };

    let image_blocks = all_image_blocks(&xml);
    let image_block = max_dimension
        .and_then(|max_dimension| {
            image_blocks.iter().copied().find(|block| {
                image_dimensions(block).is_some_and(|(width, height)| {
                    width <= max_dimension && height <= max_dimension
                })
            })
        })
        .or_else(|| image_blocks.first().copied())
        .ok_or("no image block")?;
    let format = extract_format(image_block);
    let (width, height) = image_dimensions(image_block).ok_or("invalid dimensions")?;
    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    let blob = extract_blob(image_block).ok_or("no blob")?;
    let bytes = hex_decode(&blob);

    let pixels = width
        .checked_mul(height)
        .ok_or("texture dimensions overflow")? as usize;
    let rgba_bytes = pixels.checked_mul(4).ok_or("texture dimensions overflow")?;
    let format_upper = format.to_uppercase();
    let is_bc3 = format_upper.contains("INTERP_ALPHA")
        || format_upper.contains("BC3")
        || format_upper.contains("DXT5");
    let is_bc1 = format_upper.contains("COMPRESSED") && !is_bc3;
    let is_uncompressed = format_upper.contains("RGBA")
        || format_upper.contains("BGRA")
        || format_upper.contains("ARGB")
        || format_upper.contains("UNCOMPRESSED");

    let mut rgba = Vec::with_capacity(rgba_bytes);

    if is_uncompressed || (!is_bc3 && !is_bc1 && bytes.len() >= pixels * 4) {
        // Uncompressed 32-bit (RGBA/BGRA)
        let bgra = format_upper.contains("BGRA");
        for i in 0..pixels {
            let off = i * 4;
            if off + 4 > bytes.len() {
                break;
            }
            let (r, g, b, a) = if bgra {
                (bytes[off + 2], bytes[off + 1], bytes[off], bytes[off + 3])
            } else {
                (bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3])
            };
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(a);
        }
    } else if is_bc3 || bytes.len() >= pixels {
        // BC3 / DXT5 (16 bytes per 4x4 block)
        let mut image_u32 = vec![0u32; pixels];
        texture2ddecoder::decode_bc3(&bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
        for px in image_u32 {
            rgba.push(((px >> 16) & 0xff) as u8);
            rgba.push(((px >> 8) & 0xff) as u8);
            rgba.push((px & 0xff) as u8);
            rgba.push(((px >> 24) & 0xff) as u8);
        }
    } else {
        // BC1 / DXT1 (8 bytes per 4x4 block)
        let mut image_u32 = vec![0u32; pixels];
        texture2ddecoder::decode_bc1(&bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
        for px in image_u32 {
            rgba.push(((px >> 16) & 0xff) as u8);
            rgba.push(((px >> 8) & 0xff) as u8);
            rgba.push((px & 0xff) as u8);
            rgba.push(((px >> 24) & 0xff) as u8);
        }
    }

    if rgba.len() < rgba_bytes {
        return Err("decoded data too small".into());
    }
    Ok((width, height, rgba))
}

fn image_dimensions(block: &str) -> Option<(u32, u32)> {
    let width = extract_tag(block, "Width", "sUInt16")
        .or_else(|| extract_tag(block, "Width", "sUInt32"))?
        .parse()
        .ok()?;
    let height = extract_tag(block, "Height", "sUInt16")
        .or_else(|| extract_tag(block, "Height", "sUInt32"))?
        .parse()
        .ok()?;
    Some((width, height))
}

pub fn rgba_to_slint_image(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        bytemuck::cast_slice(rgba),
        width,
        height,
    );
    slint::Image::from_rgba8(pixel_buffer)
}

// ---------- метаданные .TgPcDx для информационной панели просмотрщика ----------

/// Сжатие DDS, которое движок TS Classic ожидает от импортируемого файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdsCompression {
    Dxt1,
    Dxt3,
    Dxt5,
    Rgba8888,
    Bgra8888,
    L8,
    A8,
    Unknown,
}

impl DdsCompression {
    pub fn as_dds_name(self) -> &'static str {
        match self {
            DdsCompression::Dxt1 => "DXT1",
            DdsCompression::Dxt3 => "DXT3",
            DdsCompression::Dxt5 => "DXT5",
            DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => "RGBA8888",
            DdsCompression::L8 => "L8",
            DdsCompression::A8 => "A8",
            DdsCompression::Unknown => "неизвестный формат",
        }
    }
}

/// Распознаёт формат сжатия по заголовку DDS_HEADER файла .dds.
pub fn dds_compression_of(dds: &ddsfile::Dds) -> DdsCompression {
    if let Some(dxgi) = dds.get_dxgi_format() {
        let s = format!("{dxgi:?}");
        if s.contains("BC3") {
            return DdsCompression::Dxt5;
        }
        if s.contains("BC2") {
            return DdsCompression::Dxt3;
        }
        if s.contains("BC1") {
            return DdsCompression::Dxt1;
        }
        if s.contains("B8G8R8A8") {
            return DdsCompression::Bgra8888;
        }
        if s.contains("R8G8B8A8") || s.contains("R8G8B8X8") {
            return DdsCompression::Rgba8888;
        }
        if s.contains("A8_UNORM") {
            return DdsCompression::A8;
        }
        if s.contains("R8_UNORM") || s.contains("L8_UNORM") {
            return DdsCompression::L8;
        }
        return DdsCompression::Unknown;
    }
    if let Some(d3d) = dds.get_d3d_format() {
        return match d3d {
            ddsfile::D3DFormat::DXT5 => DdsCompression::Dxt5,
            ddsfile::D3DFormat::DXT3 => DdsCompression::Dxt3,
            ddsfile::D3DFormat::DXT1 | ddsfile::D3DFormat::DXT2 | ddsfile::D3DFormat::DXT4 => {
                DdsCompression::Dxt1
            }
            ddsfile::D3DFormat::A8R8G8B8
            | ddsfile::D3DFormat::A8B8G8R8
            | ddsfile::D3DFormat::X8R8G8B8
            | ddsfile::D3DFormat::X8B8G8R8 => DdsCompression::Rgba8888,
            ddsfile::D3DFormat::L8 => DdsCompression::L8,
            ddsfile::D3DFormat::A8 => DdsCompression::A8,
            _ => DdsCompression::Unknown,
        };
    }
    if let Some(ref fourcc) = dds.header.spf.fourcc {
        let s = format!("{fourcc:?}");
        if s.contains("DXT5") {
            return DdsCompression::Dxt5;
        }
        if s.contains("DXT3") {
            return DdsCompression::Dxt3;
        }
        if s.contains("DXT1") {
            return DdsCompression::Dxt1;
        }
    }
    if dds.header.spf.fourcc.is_none()
        && let Some(bpp) = dds.header.spf.rgb_bit_count
        && bpp == 32
    {
        return DdsCompression::Rgba8888;
    }
    DdsCompression::Unknown
}

/// Имя формата .dds, требуемое для импорта в текстуру с данным тегом `Format`.
pub fn tgpcdx_format_to_dds_name(fmt: &str) -> Option<&'static str> {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return Some("DXT5");
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return Some("DXT3");
    }
    if u.contains("DXT1") || u.contains("BC1") || u.contains("COMPRESSED") {
        return Some("DXT1");
    }
    if u.contains("L8") {
        return Some("L8");
    }
    if u.contains("A8") {
        return Some("A8");
    }
    if u.contains("8888")
        || u.contains("ARGB")
        || u.contains("RGBA")
        || u.contains("BGRA")
        || u.contains("UNCOMPRESSED")
    {
        return Some("RGBA8888");
    }
    None
}

/// (человекочитаемая метка, эквивалент в DDS) для тега `Format` из .TgPcDx.
fn format_label_and_dds_name(fmt: &str) -> (String, Option<&'static str>) {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return ("DXT5 (BC3, плавная альфа)".into(), Some("DXT5"));
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return ("DXT3 (BC2, явная альфа)".into(), Some("DXT3"));
    }
    if u.contains("DXT1") || u.contains("BC1") || u.contains("COMPRESSED") {
        return ("DXT1 (BC1, без альфы / 1-bit альфа)".into(), Some("DXT1"));
    }
    if u.contains("L8") || u.contains("A8") {
        return ("L8 / A8 (8-bit градации серого)".into(), {
            if u.contains("L8") {
                Some("L8")
            } else {
                Some("A8")
            }
        });
    }

    if u.contains("8888")
        || u.contains("ARGB")
        || u.contains("RGBA")
        || u.contains("BGRA")
        || u.contains("UNCOMPRESSED")
    {
        return ("RGBA8888 (без сжатия)".into(), Some("RGBA8888"));
    }
    (fmt.to_owned(), None)
}

/// Экстрагированные метаданные тектуры .TgPcDx для панели информации.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TextureInfo {
    pub name: String,
    pub is_swizzled: bool,
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub format: String,
    pub raw_format: String,
    pub format_label: String,
    pub dds_format: Option<&'static str>,
    pub texture_count: u32,
    pub mip_storage_mode: String,
    pub flags_u16: u16,
    pub flags: u16,
    pub unnamed_u16: u16,
    pub image_flags: u16,
    pub pre_blob_u16: u16,
    pub file_size_bytes: u64,
    pub total_blob_bytes: u64,
    pub header_bytes: u64,
    pub name_matches_filename: bool,
    pub has_bak: bool,
    pub bak_size_bytes: u64,
    pub backup_size_bytes: u64,
}

/// Алиасы для совместимости с ожидаемыми именами структуры.
#[allow(dead_code)]
pub type TgpcdxInfo = TextureInfo;
#[allow(dead_code)]
pub type TextureInspectReport = TextureInfo;

/// Извлекает метаданные текстуры из файла .TgPcDx (SERZ или простой XML).
/// Извлекает метаданные текстуры из файла .TgPcDx (SERZ или простой XML).
pub fn inspect_tgpcdx(data: &[u8]) -> Result<TextureInfo, String> {
    inspect_tgpcdx_with_path(data, None)
}

/// Вариант с указанием пути на диске: позволяет сверить имя и проверить .bak.
pub fn inspect_tgpcdx_with_path(data: &[u8], path: Option<&Path>) -> Result<TextureInfo, String> {
    let xml = if data.starts_with(b"SERZ") {
        serz_bytes_to_xml(data)?
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
    let total_blob_bytes: u64 = blocks
        .iter()
        .map(|b| {
            extract_blob(b)
                .map(|s| hex_decode(&s).len() as u64)
                .unwrap_or(0)
        })
        .sum();
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
    let format = raw_format.clone();

    // Путь / файл.
    let (file_size_bytes, has_bak, bak_size_bytes, name_matches_filename) = match path {
        Some(p) => {
            let fs_size = std::fs::metadata(p)
                .map(|m| m.len())
                .unwrap_or(data.len() as u64);
            let bak_path = PathBuf::from(format!("{}.bak", p.display()));
            let (has, bak_sz) = match std::fs::metadata(&bak_path) {
                Ok(m) => (true, m.len()),
                Err(_) => (false, 0u64),
            };
            let file_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let internal_tail = name.rsplit(['\\', '/']).next().unwrap_or(&name);
            let matches = !file_stem.is_empty() && file_stem.eq_ignore_ascii_case(internal_tail);
            (fs_size, has, bak_sz, matches)
        }
        None => (data.len() as u64, false, 0u64, false),
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
        format,
        raw_format,
        format_label,
        dds_format,
        texture_count,
        mip_storage_mode,
        flags_u16,
        flags: flags_u16,
        unnamed_u16: flags_u16,
        image_flags: flags_u16,
        pre_blob_u16: flags_u16,
        file_size_bytes,
        total_blob_bytes,
        header_bytes,
        name_matches_filename,
        has_bak,
        bak_size_bytes,
        backup_size_bytes: bak_size_bytes,
    })
}

#[allow(dead_code)]
pub fn inspect_tgpcdx_file(path: &Path) -> Result<TextureInfo, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    inspect_tgpcdx_with_path(&data, Some(path))
}

/// Обновляет внутреннее имя текстуры (узел `Name` / `cDeltaString`) в `.TgPcDx`.
#[allow(dead_code)]
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

    // Парсим SERZ с сохранением преамбулы (или XML).
    let is_serz = data.starts_with(b"SERZ");
    let (mut root, preamble) = if is_serz {
        let mut reader = crate::serz::SerzReader::new(&data);
        let node = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        (node, reader.preamble.clone())
    } else {
        let text = String::from_utf8(data.clone()).map_err(|e| format!("invalid text: {e}"))?;
        crate::serz::SerzNode::from_xml_str(&text).map_err(|e| format!("xml parse: {e}"))?
    };

    // Обновляем все cHcTexture/Name.
    let updated = update_internal_name_in_node(&mut root, &normalized);
    if !updated {
        return Err("В файле не найден узел cHcTexture с дочерним Name (cDeltaString)".into());
    }

    let out_bytes = if is_serz {
        crate::serz::encode_to_vec(&root, preamble.as_ref())
            .map_err(|e| format!("serz encode: {e}"))?
    } else {
        crate::serz::SerzReader::to_xml(&root, 0, true).into_bytes()
    };

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

/// Вариант для работы с байтами.
#[allow(dead_code)]
pub fn update_tgpcdx_internal_name_bytes(data: &[u8], new_name: &str) -> Result<Vec<u8>, String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err("Внутреннее имя текстуры не может быть пустым".into());
    }
    let normalized = trimmed.replace('/', "\\");

    let is_serz = data.starts_with(b"SERZ");
    let (mut root, preamble) = if is_serz {
        let mut reader = crate::serz::SerzReader::new(data);
        let node = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        (node, reader.preamble.clone())
    } else {
        let text = String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?;
        crate::serz::SerzNode::from_xml_str(&text).map_err(|e| format!("xml parse: {e}"))?
    };

    if !update_internal_name_in_node(&mut root, &normalized) {
        return Err("В файле не найден узел cHcTexture с дочерним Name (cDeltaString)".into());
    }

    let out_bytes = if is_serz {
        crate::serz::encode_to_vec(&root, preamble.as_ref())
            .map_err(|e| format!("serz encode: {e}"))?
    } else {
        crate::serz::SerzReader::to_xml(&root, 0, true).into_bytes()
    };
    if out_bytes.is_empty() {
        return Err("Пустой результат сериализации".into());
    }
    Ok(out_bytes)
}

#[allow(dead_code)]
fn update_internal_name_in_node(node: &mut crate::serz::SerzNode, new_name: &str) -> bool {
    let mut changed = false;
    if let crate::serz::SerzNode::Element { name, children, .. } = node {
        if name == "cHcTexture" {
            for child in children.iter_mut() {
                if let crate::serz::SerzNode::Property {
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

fn append_tgpcdx_log(message: &str) {
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

#[allow(dead_code)]
pub fn inspect_texture(data: &[u8]) -> Result<TextureInfo, String> {
    inspect_tgpcdx(data)
}

#[allow(dead_code)]
pub fn inspect_texture_file(path: &Path) -> Result<TextureInfo, String> {
    inspect_tgpcdx_file(path)
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

/// Ожидаемый размер одного уровня мипмапа в байтах.
pub fn mip_level_bytes(w: u32, h: u32, fmt: DdsCompression) -> Option<usize> {
    let bw = w.div_ceil(4).max(1) as usize;
    let bh = h.div_ceil(4).max(1) as usize;
    match fmt {
        DdsCompression::Dxt1 => Some(bw * bh * 8),
        DdsCompression::Dxt3 | DdsCompression::Dxt5 => Some(bw * bh * 16),
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => {
            Some((w as usize) * (h as usize) * 4)
        }
        DdsCompression::L8 | DdsCompression::A8 => Some((w as usize) * (h as usize)),
        DdsCompression::Unknown => None,
    }
}

/// Результат проверки цепочки мипмапов .dds.
#[derive(Debug, Clone)]
pub struct MipChainInfo {
    pub levels: u32,
    pub total_bytes: usize,
}

/// Проверяет, что буфер данных .dds содержит ровно столько байт, сколько
/// требуется заявленным уровням мипмапов (сумма размеров уровней строго
/// равна длине буфера — без обрезанных хвостов и лишнего мусора).
pub fn validate_dds_mip_chain(
    width: u32,
    height: u32,
    fmt: DdsCompression,
    declared_levels: u32,
    data_len: usize,
    expected_levels: Option<u32>,
) -> Result<MipChainInfo, String> {
    if fmt == DdsCompression::Unknown {
        return Err("неизвестный формат сжатия DDS".into());
    }
    if declared_levels == 0 {
        return Err("в DDS-файле не объявлено ни одного уровня мипмапов".into());
    }
    if let Some(expected) = expected_levels
        && declared_levels < expected
    {
        return Err(format!(
            "В DDS-файле {} уровень{} мипмапов, ожидается {}. Включите генерацию MipMaps при сохранении DDS",
            declared_levels,
            if declared_levels == 1 { "" } else { "ей" },
            expected
        ));
    }

    let mut total = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..declared_levels {
        let size =
            mip_level_bytes(mw, mh, fmt).ok_or("невозможно вычислить размер уровня мипмапа")?;
        total = total
            .checked_add(size)
            .ok_or("переполнение при суммировании уровней мипмапов")?;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }

    if total != data_len {
        return Err(format!(
            "DDS-файл обрезан или содержит лишние данные: ожидается {total} байт мипмапов, найдено {data_len}"
        ));
    }
    Ok(MipChainInfo {
        levels: declared_levels,
        total_bytes: total,
    })
}

/// Итоговый отчёт об успешном импорте DDS -> .TgPcDx.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub total_bytes: usize,
    pub dds_format: String,
}

// ---------- import DDS -> TgPcDx ----------

pub fn convert_dds_to_tgpcdx_report(input: &Path, output: &Path) -> Result<ImportReport, String> {
    let file = fs::File::open(input).map_err(|e| e.to_string())?;
    let dds = ddsfile::Dds::read(file).map_err(|e| e.to_string())?;

    let width = dds.get_width();
    let height = dds.get_height();
    if width == 0 || height == 0 {
        return Err("некорректные размеры DDS".into());
    }
    // 1. Проверка степеней двойки.
    if !width.is_power_of_two() || !height.is_power_of_two() {
        return Err(format!(
            "Размеры DDS не являются степенью двойки: {width}×{height}"
        ));
    }

    let compression = dds_compression_of(&dds);
    let all = dds.get_data(0).map_err(|e| e.to_string())?;
    let declared = dds.get_num_mipmap_levels();

    // Целевой .TgPcDx — источник ожидаемого формата и длины цепочки мипмапов.
    let target_bytes = fs::read(output).ok();
    let expected_meta = target_bytes.as_deref().and_then(|b| inspect_tgpcdx(b).ok());

    // 2. Проверка соответствия формата сжатия оригиналу.
    if let Some(meta) = &expected_meta {
        let expected_name = tgpcdx_format_to_dds_name(&meta.format);
        let actual_name = compression.as_dds_name();
        if let Some(expected) = expected_name
            && expected != actual_name
        {
            return Err(format!(
                "Ожидался формат {expected}, но выбранный DDS имеет формат {actual_name}"
            ));
        }
    }

    // 3. Математическая проверка цепочки мипмапов.
    let chain = validate_dds_mip_chain(
        width,
        height,
        compression,
        declared,
        all.len(),
        expected_meta.as_ref().map(|m| m.mip_count),
    )?;

    let report = ImportReport {
        width,
        height,
        mip_count: chain.levels,
        total_bytes: chain.total_bytes,
        dds_format: compression.as_dds_name().to_string(),
    };

    let fmt_str = dds_format_to_tgpcdx(&dds);
    let format = dds.get_format().ok_or("unsupported format")?;
    let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut off = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..declared {
        let pitch = format.get_pitch(mw).ok_or("pitch")?;
        let ph = format.get_pitch_height();
        let row_h = mh.div_ceil(ph);
        let size = (pitch * row_h) as usize;
        let end = (off + size).min(all.len());
        mips.push((mw, mh, all[off..end].to_vec()));
        off = end;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }

    if let Some(target_bytes) = target_bytes {
        if let Some(meta) = &expected_meta
            && meta.width == width
            && meta.height == height
        {
            let packed_mips: Vec<(u32, u32, Vec<u8>)> = if meta.mip_count == 1 {
                vec![(width, height, all.to_vec())]
            } else {
                mips.clone()
            };

            if let Ok(patched) = crate::serz::patch_tgpcdx_blobs(&target_bytes, &packed_mips) {
                return write_output_bytes(output, &patched).map(|()| report);
            }
        }
        // Fallback на полную пересборку только если изменилось разрешение (upscale/downscale)
        let mut reader = crate::serz::SerzReader::new(&target_bytes);
        if let Ok(mut root) = reader.parse() {
            let preamble = reader.preamble.clone();
            if modify_tgpcdx_tree(&mut root, width, height, &fmt_str, &mips).is_ok() {
                let bytes = crate::serz::encode_to_vec(&root, preamble.as_ref())?;
                if !bytes.is_empty() {
                    return write_output_bytes(output, &bytes).map(|()| report);
                }
            }
        }
    }

    // Fallback: build from scratch (used when no target file exists).
    let mut s = String::with_capacity(all.len() * 3 + 4096);
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    s.push_str("<cHcTextureGroup xmlns:d=\"http://www.kuju.com/TnT/2003/Delta\" d:version=\"1.0\" d:id=\"1\">\n");
    s.push_str("\t<Texture>\n");
    s.push_str("\t\t<cHcTexture d:id=\"2\">\n");
    let _ = writeln!(s, "\t\t\t<Width d:type=\"sUInt32\">{width}</Width>");
    let _ = writeln!(s, "\t\t\t<Height d:type=\"sUInt32\">{height}</Height>");
    s.push_str("\t\t\t<Name d:type=\"cDeltaString\">texture</Name>\n");
    s.push_str("\t\t\t<Data>\n");
    s.push_str("\t\t\t\t<cHcTextureData d:id=\"3\">\n");
    s.push_str("\t\t\t\t\t<Mip>\n");

    for (w, h, bytes) in &mips {
        s.push_str("\t\t\t\t\t\t<cHcImageDx d:id=\"4\">\n");
        let _ = writeln!(
            s,
            "\t\t\t\t\t\t\t<Format d:type=\"cDeltaString\">{fmt_str}</Format>"
        );
        s.push_str("\t\t\t\t\t\t\t<IsSwizzled d:type=\"bool\">0</IsSwizzled>\n");
        let _ = writeln!(s, "\t\t\t\t\t\t\t<Width d:type=\"sUInt16\">{w}</Width>");
        let _ = writeln!(s, "\t\t\t\t\t\t\t<Height d:type=\"sUInt16\">{h}</Height>");
        let _ = write!(s, "\t\t\t\t\t\t\t<d:blob d:size=\"{}\">", bytes.len());

        for (i, byte) in bytes.iter().enumerate() {
            let _ = write!(s, "{:02X}", byte);
            if (i + 1) % 8 == 0 && i + 1 < bytes.len() {
                s.push(' ');
            }
        }
        s.push_str("</d:blob>\n");
        s.push_str("\t\t\t\t\t\t</cHcImageDx>\n");
    }

    s.push_str("\t\t\t\t\t</Mip>\n");
    s.push_str("\t\t\t\t</cHcTextureData>\n");
    s.push_str("\t\t\t</Data>\n");
    s.push_str("\t\t</cHcTexture>\n");
    s.push_str("\t</Texture>\n");
    s.push_str("</cHcTextureGroup>\n");

    let (node, preamble) = crate::serz::SerzNode::from_xml_str(&s)?;
    let bytes = crate::serz::encode_to_vec(&node, preamble.as_ref())?;
    if bytes.is_empty() {
        return Err("empty output".into());
    }

    write_output_bytes(output, &bytes).map(|()| report)
}

#[allow(dead_code)]
pub fn convert_dds_to_tgpcdx(input: &Path, output: &Path) -> Result<(), String> {
    convert_dds_to_tgpcdx_report(input, output).map(|_| ())
}

// ---------- export TgPcDx -> DDS ----------

pub fn export_tgpcdx_to_dds(input: &Path, output: &Path) -> Result<(), String> {
    let data = fs::read(input).map_err(|e| e.to_string())?;
    export_tgpcdx_bytes_to_dds(&data, output)
}

pub fn export_tgpcdx_bytes_to_dds(data: &[u8], output: &Path) -> Result<(), String> {
    let xml = if data.starts_with(b"SERZ") {
        serz_bytes_to_xml(data)?
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| e.to_string())?
    };

    let blocks = all_image_blocks(&xml);
    if blocks.is_empty() {
        return Err("no image blocks".into());
    }

    // Собираем мипмапы с размерами.
    let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::with_capacity(blocks.len());
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
            return Err("invalid mip dimensions".into());
        }
        let blob = extract_blob(block).ok_or("no blob")?;
        let bytes = hex_decode(&blob);
        if bytes.is_empty() {
            return Err("empty mip blob".into());
        }
        mips.push((w, h, bytes));
    }

    let fmt_str = extract_format(blocks[0]);
    let d3d_fmt = tgpcdx_format_to_d3d(&fmt_str);

    // Сортируем от самого крупного уровня к самому мелкому (стандарт DirectX).
    mips.sort_by_key(|a| std::cmp::Reverse(a.0 * a.1));

    // Проверка размеров мипмапов по формуле DXT/несжатого формата.
    for (w, h, bytes) in &mips {
        let expected = expected_mip_size(*w, *h, d3d_fmt);
        if expected > 0 && bytes.len() != expected {
            // Допускаем расхождение только если не удалось вычислить точно; иначе ошибка.
            // Для несжатых форматов expected может быть 0 (неизвестно) — пропускаем.
            if expected != usize::MAX {
                return Err(format!(
                    "mip {}x{} size mismatch: got {}, expected {}",
                    w,
                    h,
                    bytes.len(),
                    expected
                ));
            }
        }
    }

    let width = mips[0].0;
    let height = mips[0].1;
    let mip_count = mips.len() as u32;

    let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height,
        width,
        depth: None,
        format: d3d_fmt,
        mipmap_levels: Some(mip_count),
        caps2: None,
    })
    .map_err(|e| e.to_string())?;

    let mut offset = 0usize;
    for (_, _, bytes) in &mips {
        let sz = bytes.len();
        if offset + sz > dds.data.len() {
            return Err("mip data exceeds dds buffer".into());
        }
        dds.data[offset..offset + sz].copy_from_slice(bytes);
        offset += sz;
    }

    let mut file = fs::File::create(output).map_err(|e| e.to_string())?;
    dds.write(&mut file).map_err(|e| e.to_string())?;
    Ok(())
}

fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = output.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    let _ = fs::remove_file(output);
    fs::rename(&tmp, output).map_err(|e| e.to_string())?;
    Ok(())
}

fn find_element_mut<'a>(
    node: &'a mut crate::serz::SerzNode,
    name: &str,
) -> Option<&'a mut crate::serz::SerzNode> {
    if let crate::serz::SerzNode::Element { name: n, .. } = &*node
        && n == name
    {
        return Some(node);
    }
    if let crate::serz::SerzNode::Element { children, .. } = node {
        for child in children.iter_mut() {
            if let Some(found) = find_element_mut(child, name) {
                return Some(found);
            }
        }
    }
    None
}

fn set_property_value(elem: &mut crate::serz::SerzNode, name: &str, value: &str) -> bool {
    if let crate::serz::SerzNode::Element { children, .. } = elem {
        for child in children.iter_mut() {
            if let crate::serz::SerzNode::Property {
                name: n, value: v, ..
            } = child
                && n == name
            {
                *v = value.to_string();
                return true;
            }
        }
    }
    false
}

fn set_blob_data(elem: &mut crate::serz::SerzNode, data: &[u8]) {
    if let crate::serz::SerzNode::Element { children, .. } = elem {
        for child in children.iter_mut() {
            if let crate::serz::SerzNode::Blob { data: d } = child {
                *d = data.to_vec();
                return;
            }
        }
        children.push(crate::serz::SerzNode::Blob {
            data: data.to_vec(),
        });
    }
}

fn find_all_elements<'a>(
    node: &'a crate::serz::SerzNode,
    name: &str,
) -> Vec<&'a crate::serz::SerzNode> {
    let mut out = Vec::new();
    if let crate::serz::SerzNode::Element {
        name: n, children, ..
    } = node
    {
        if n == name {
            out.push(node);
        }
        for child in children {
            out.extend(find_all_elements(child, name));
        }
    }
    out
}

fn modify_tgpcdx_tree(
    root: &mut crate::serz::SerzNode,
    width: u32,
    height: u32,
    fmt_str: &str,
    mips: &[(u32, u32, Vec<u8>)],
) -> Result<(), String> {
    {
        let tex = find_element_mut(root, "cHcTexture").ok_or("no texture node")?;
        set_property_value(tex, "Width", &width.to_string());
        set_property_value(tex, "Height", &height.to_string());
    }

    let templates: Vec<crate::serz::SerzNode> = find_all_elements(root, "cHcImageDx")
        .into_iter()
        .cloned()
        .collect();

    let mip_container = find_element_mut(root, "Mip").ok_or("no Mip node")?;
    if let crate::serz::SerzNode::Element { children, .. } = mip_container {
        children.clear();
    } else {
        return Err("invalid Mip node".into());
    }

    for (idx, (w, h, bytes)) in mips.iter().enumerate() {
        let template = templates.get(idx).or_else(|| templates.first());
        let mut img = if let Some(t) = template {
            t.clone()
        } else {
            crate::serz::SerzNode::Element {
                name: "cHcImageDx".to_string(),
                id: 4,
                version: 1,
                children: vec![
                    crate::serz::SerzNode::Property {
                        name: "Format".to_string(),
                        type_name: "cDeltaString".to_string(),
                        value: fmt_str.to_string(),
                        alt_encoding: None,
                    },
                    crate::serz::SerzNode::Property {
                        name: "IsSwizzled".to_string(),
                        type_name: "bool".to_string(),
                        value: "0".to_string(),
                        alt_encoding: None,
                    },
                    crate::serz::SerzNode::Property {
                        name: "Width".to_string(),
                        type_name: "sUInt16".to_string(),
                        value: w.to_string(),
                        alt_encoding: None,
                    },
                    crate::serz::SerzNode::Property {
                        name: "Height".to_string(),
                        type_name: "sUInt16".to_string(),
                        value: h.to_string(),
                        alt_encoding: None,
                    },
                    crate::serz::SerzNode::Blob {
                        data: bytes.clone(),
                    },
                ],
            }
        };
        set_property_value(&mut img, "Format", fmt_str);
        set_property_value(&mut img, "Width", &w.to_string());
        set_property_value(&mut img, "Height", &h.to_string());
        set_blob_data(&mut img, bytes);
        if let crate::serz::SerzNode::Element { name, .. } = &mut img {
            *name = "cHcImageDx".to_string();
        }

        let mip_container = find_element_mut(root, "Mip").ok_or("no Mip node")?;
        if let crate::serz::SerzNode::Element { children, .. } = mip_container {
            children.push(img);
        }
    }
    Ok(())
}
fn serz_bytes_to_xml(data: &[u8]) -> Result<String, String> {
    if data.starts_with(b"SERZ") {
        let mut reader = crate::serz::SerzReader::new(data);
        let node = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        Ok(crate::serz::SerzReader::to_xml(&node, 0, true))
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))
    }
}

fn all_image_blocks(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut search = xml;
    while let Some(start) = search.find("<cHcImageDx") {
        let end_marker = "</cHcImageDx>";
        if let Some(rel_end) = search[start..].find(end_marker) {
            let end = start + rel_end + end_marker.len();
            out.push(&search[start..end]);
            search = &search[end..];
        } else {
            break;
        }
    }
    out
}

fn extract_format(block: &str) -> String {
    if let Some(start) = block.find("<Format") {
        let rest = &block[start..];
        if let Some(gt) = rest.find('>') {
            let content_start = start + gt + 1;
            if let Some(end) = block[content_start..].find("</Format>") {
                let val = block[content_start..content_start + end].trim();
                if !val.is_empty() {
                    return val.to_string();
                }
            }
        }
    }
    // Fallback: scan block for known markers
    if block.contains("INTERP_ALPHA") {
        return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".to_string();
    }
    if block.contains("DXT5") || block.contains("BC3") {
        return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".to_string();
    }
    if block.contains("DXT1") || block.contains("BC1") {
        return "HC_IMAGE_FORMAT_COMPRESSED".to_string();
    }
    String::new()
}

fn extract_tag(block: &str, tag: &str, dtype: &str) -> Option<String> {
    let pattern = format!("<{} d:type=\"{}\">", tag, dtype);
    let start = block.find(&pattern)?;
    let rest = &block[start + pattern.len()..];
    let end = rest.find("</")?;
    Some(rest[..end].trim().to_string())
}
fn extract_blob(block: &str) -> Option<String> {
    let marker = "<d:blob d:size=\"";
    let start = block.find(marker)?;
    let rest = &block[start..];
    let gt = rest.find('>')?;
    let content_start = start + gt + 1;
    let end_marker = "</d:blob>";
    let end = block[content_start..].find(end_marker)? + content_start;
    Some(block[content_start..end].to_string())
}

fn count_c_hc_textures(xml: &str) -> u32 {
    let mut count = 0usize;
    let mut search = xml;
    while let Some(pos) = search.find("<cHcTexture") {
        let after = &search[pos + "<cHcTexture".len()..];
        let next = after.chars().next();
        if next == Some(' ') || next == Some('>') || next == Some('\t') || next == Some('\n') {
            count += 1;
        }
        // advance past this occurrence
        if pos + 1 >= search.len() {
            break;
        }
        search = &search[pos + 1..];
    }
    count as u32
}

fn parse_bool_tag(block: &str, tag: &str) -> bool {
    for dtype in ["bool", "sUInt32", "sUInt8", "sInt32", "sUInt16"] {
        if let Some(v) = extract_tag(block, tag, dtype) {
            let t = v.trim().to_lowercase();
            if t == "1" || t == "true" || t == "yes" {
                return true;
            }
            if t == "0" || t == "false" || t == "no" {
                return false;
            }
            if let Ok(n) = t.parse::<u32>() {
                return n != 0;
            }
        }
    }
    false
}

fn extract_unnamed_u16(block: &str) -> u16 {
    if let Some(blob_pos) = block.find("<d:blob") {
        let pre = &block[..blob_pos];
        if let Some(start) = pre.rfind("<e d:type=\"sUInt16\">") {
            let rest = &pre[start + "<e d:type=\"sUInt16\">".len()..];
            if let Some(end) = rest.find("</e>")
                && let Ok(v) = rest[..end].trim().parse::<u16>()
            {
                return v;
            }
        }
    }
    // Fallback: any <e d:type="sUInt16"> in block.
    if let Some(start) = block.find("<e d:type=\"sUInt16\">") {
        let rest = &block[start + "<e d:type=\"sUInt16\">".len()..];
        if let Some(end) = rest.find("</e>")
            && let Ok(v) = rest[..end].trim().parse::<u16>()
        {
            return v;
        }
    }
    0
}

fn extract_blob_size(block: &str) -> u64 {
    let marker = "<d:blob d:size=\"";
    if let Some(start) = block.find(marker) {
        let rest = &block[start + marker.len()..];
        if let Some(end) = rest.find('"')
            && let Ok(n) = rest[..end].parse::<u64>()
        {
            return n;
        }
    }
    0
}

fn tgpcdx_dds_compression(fmt: &str) -> DdsCompression {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return DdsCompression::Dxt5;
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return DdsCompression::Dxt3;
    }
    if u.contains("COMPRESSED") || u.contains("DXT1") || u.contains("BC1") {
        return DdsCompression::Dxt1;
    }
    if u.contains("8888") || u.contains("RGBA") || u.contains("BGRA") || u.contains("ARGB") {
        return DdsCompression::Rgba8888;
    }
    if u.contains("L8") {
        return DdsCompression::L8;
    }
    if u.contains("A8") {
        return DdsCompression::A8;
    }
    DdsCompression::Unknown
}

/// Потоковое прямое декодирование Hex без промежуточных строк и подстрок.
fn hex_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut hi: Option<u8> = None;
    for &b in s.as_bytes() {
        let val = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        if let Some(h) = hi.take() {
            out.push((h << 4) | val);
        } else {
            hi = Some(val);
        }
    }
    out
}

fn tgpcdx_format_to_d3d(fmt: &str) -> ddsfile::D3DFormat {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return ddsfile::D3DFormat::DXT5;
    }
    if u.contains("DXT3") || u.contains("BC2") {
        return ddsfile::D3DFormat::DXT3;
    }
    if u.contains("DXT1") || u.contains("BC1") || u.contains("COMPRESSED") {
        return ddsfile::D3DFormat::DXT1;
    }
    if u.contains("BGRA") {
        return ddsfile::D3DFormat::A8R8G8B8;
    }
    if u.contains("RGBA") || u.contains("ARGB") {
        return ddsfile::D3DFormat::A8B8G8R8;
    }
    if u.contains("L8") {
        return ddsfile::D3DFormat::L8;
    }
    if u.contains("A8") {
        return ddsfile::D3DFormat::A8;
    }
    if u.contains("8888") {
        return ddsfile::D3DFormat::A8B8G8R8;
    }
    ddsfile::D3DFormat::DXT1
}

fn expected_mip_size(w: u32, h: u32, fmt: ddsfile::D3DFormat) -> usize {
    match fmt {
        ddsfile::D3DFormat::DXT1
        | ddsfile::D3DFormat::DXT2
        | ddsfile::D3DFormat::DXT3
        | ddsfile::D3DFormat::DXT4
        | ddsfile::D3DFormat::DXT5 => {
            let bw = (w as usize).div_ceil(4);
            let bh = (h as usize).div_ceil(4);
            let block = if matches!(fmt, ddsfile::D3DFormat::DXT1) {
                8
            } else {
                16
            };
            bw.max(1) * bh.max(1) * block
        }
        ddsfile::D3DFormat::A8 | ddsfile::D3DFormat::L8 | ddsfile::D3DFormat::A4L4 => {
            (w as usize) * (h as usize)
        }
        ddsfile::D3DFormat::R5G6B5
        | ddsfile::D3DFormat::A1R5G5B5
        | ddsfile::D3DFormat::X1R5G5B5
        | ddsfile::D3DFormat::A4R4G4B4
        | ddsfile::D3DFormat::X4R4G4B4
        | ddsfile::D3DFormat::A8L8
        | ddsfile::D3DFormat::L16 => (w as usize) * (h as usize) * 2,
        _ => (w as usize) * (h as usize) * 4,
    }
}

fn dds_format_to_tgpcdx(dds: &ddsfile::Dds) -> String {
    if let Some(ref fourcc) = dds.header.spf.fourcc {
        let s = format!("{:?}", fourcc);
        if s.contains("DXT5") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("DXT3") {
            return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into();
        }
        if s.contains("DXT1") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
    }
    if let Some(d3d) = dds.get_d3d_format() {
        match d3d {
            ddsfile::D3DFormat::DXT5 => return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into(),
            ddsfile::D3DFormat::DXT3 => return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into(),
            ddsfile::D3DFormat::DXT1 => return "HC_IMAGE_FORMAT_COMPRESSED".into(),
            ddsfile::D3DFormat::A8R8G8B8
            | ddsfile::D3DFormat::A8B8G8R8
            | ddsfile::D3DFormat::X8R8G8B8
            | ddsfile::D3DFormat::X8B8G8R8 => return "HC_IMAGE_FORMAT_8888".into(),
            _ => {}
        }
    }
    if let Some(dxgi) = dds.get_dxgi_format() {
        let s = format!("{:?}", dxgi);
        if s.contains("BC3") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("BC2") {
            return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into();
        }
        if s.contains("BC1") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
        if s.contains("R8G8B8A8") || s.contains("B8G8R8A8") || s.contains("R8G8B8X8") {
            return "HC_IMAGE_FORMAT_8888".into();
        }
    }
    // Uncompressed heuristic: if no fourcc/DXT and pitch is 4 bytes per pixel.
    if dds.header.spf.fourcc.is_none() {
        let bpp = dds.header.spf.rgb_bit_count;
        if bpp == Some(32) {
            return "HC_IMAGE_FORMAT_8888".into();
        }
    }
    "HC_IMAGE_FORMAT_COMPRESSED".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;

    #[test]
    fn viewer_decoder_selects_the_first_mipmap_at_or_below_1024() {
        let pixels = vec![0x7f; 2 * 2 * 4];
        let hex = pixels
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root>
                <cHcImageDx>
                    <Format d:type="cDeltaString">RGBA</Format>
                    <Width d:type="sUInt32">2048</Width>
                    <Height d:type="sUInt32">2048</Height>
                </cHcImageDx>
                <cHcImageDx>
                    <Format d:type="cDeltaString">RGBA</Format>
                    <Width d:type="sUInt32">2</Width>
                    <Height d:type="sUInt32">2</Height>
                    <d:blob d:size="{}">{hex}</d:blob>
                </cHcImageDx>
            </Root>"#,
            hex.len() / 2
        );

        let (width, height, decoded) = decode_tgpcdx_for_viewer(xml.as_bytes()).unwrap();

        assert_eq!((width, height), (2, 2));
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn round_trip_tgpcdx_dds() {
        let dir = tempfile::tempdir().unwrap();
        let dds_path = dir.path().join("src.dds");
        let dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
            height: 8,
            width: 8,
            depth: None,
            format: ddsfile::D3DFormat::DXT5,
            mipmap_levels: Some(3),
            caps2: None,
        })
        .unwrap();
        let mut f = fs::File::create(&dds_path).unwrap();
        dds.write(&mut f).unwrap();

        let tg_path = dir.path().join("src.TgPcDx");
        convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
        assert!(tg_path.is_file());

        // Preserve a custom Name in the target before re-importing.
        let data = fs::read(&tg_path).unwrap();
        let mut reader = crate::serz::SerzReader::new(&data);
        let mut root = reader.parse().unwrap();
        let preamble = reader.preamble.clone();
        let tex = find_element_mut(&mut root, "cHcTexture").unwrap();
        assert!(set_property_value(
            tex,
            "Name",
            "TrainSimContent\\Textures\\Sample"
        ));
        let patched = crate::serz::encode_to_vec(&root, preamble.as_ref()).unwrap();
        fs::write(&tg_path, &patched).unwrap();

        let out_dds = dir.path().join("out.dds");
        export_tgpcdx_to_dds(&tg_path, &out_dds).unwrap();
        let data = fs::read(&out_dds).unwrap();
        assert_eq!(&data[0..4], b"DDS ");

        let dds2 = ddsfile::Dds::read(Cursor::new(&data)).unwrap();
        assert_eq!(dds2.get_width(), 8);
        assert_eq!(dds2.get_height(), 8);
        assert_eq!(dds2.get_num_mipmap_levels(), 3);

        // Import back into the same target file, preserving metadata.
        convert_dds_to_tgpcdx(&out_dds, &tg_path).unwrap();
        assert!(tg_path.is_file());

        let data = fs::read(&tg_path).unwrap();
        let xml = serz_bytes_to_xml(&data).unwrap();
        assert!(
            xml.contains(r#"<Name d:type="cDeltaString">TrainSimContent\Textures\Sample</Name>"#)
        );
        assert!(xml.contains("HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA"));

        let blocks = all_image_blocks(&xml);
        assert_eq!(blocks.len(), 3);
        let ws: Vec<u32> = blocks
            .iter()
            .map(|b| extract_tag(b, "Width", "sUInt16").unwrap().parse().unwrap())
            .collect();
        assert_eq!(ws, vec![8, 4, 2]);
    }

    #[test]
    fn test_mip_chain_sizes_dxt1_dxt5_1024_to_1() {
        const DXT1_TOTAL: usize = 699_064;
        const DXT5_TOTAL: usize = 1_398_128;

        for (fmt, block_bytes, expected_total) in [
            (DdsCompression::Dxt1, 8usize, DXT1_TOTAL),
            (DdsCompression::Dxt5, 16usize, DXT5_TOTAL),
        ] {
            let mut total = 0usize;
            let mut levels = 0u32;
            let (mut w, mut h) = (1024u32, 1024u32);
            loop {
                let size = mip_level_bytes(w, h, fmt).unwrap();
                total += size;
                levels += 1;
                if w == 1 && h == 1 {
                    break;
                }
                w = (w / 2).max(1);
                h = (h / 2).max(1);
            }
            assert_eq!(
                levels, 11,
                "{fmt:?} должен содержать 11 уровней для 1024x1024"
            );
            assert_eq!(
                total, expected_total,
                "{fmt:?} неверный суммарный размер цепочки"
            );
            assert_eq!(
                mip_level_bytes(1024, 1024, fmt),
                Some(256 * 256 * block_bytes)
            );
            assert_eq!(mip_level_bytes(2, 2, fmt), Some(block_bytes));
            assert_eq!(mip_level_bytes(1, 1, fmt), Some(block_bytes));

            // Валидация принимает точную цепочку мипмапов.
            let info = validate_dds_mip_chain(1024, 1024, fmt, 11, total, Some(11)).unwrap();
            assert_eq!(info.levels, 11);
            assert_eq!(info.total_bytes, total);
        }
    }

    #[test]
    fn test_validate_dds_rejects_invalid_chains() {
        // Обрезанный буфер данных.
        assert!(
            validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 11, 100, Some(11)).is_err()
        );
        // Лишние данные после цепочки мипмапов.
        assert!(
            validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 11, 699_064 + 1, Some(11),)
                .is_err()
        );
        // Мипмапов меньше, чем в исходном .TgPcDx.
        assert!(
            validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 1, 699_064, Some(11)).is_err()
        );
        // Без ожидания от оригинала один уровень допустим.
        let one = mip_level_bytes(1024, 1024, DdsCompression::Dxt1).unwrap();
        assert!(validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 1, one, None).is_ok());
        // Неизвестный формат отвергается.
        assert!(validate_dds_mip_chain(1024, 1024, DdsCompression::Unknown, 1, 0, None).is_err());
    }

    #[test]
    fn test_dds_format_mismatch_messages() {
        let original = TextureInfo {
            name: "tex".into(),
            is_swizzled: false,
            width: 1024,
            height: 1024,
            mip_count: 11,
            min_width: 1,
            min_height: 1,
            format: "HC_IMAGE_FORMAT_COMPRESSED".into(),
            raw_format: "HC_IMAGE_FORMAT_COMPRESSED".into(),
            format_label: "DXT1 (BC1, без альфы / 1-bit альфа)".into(),
            dds_format: Some("DXT1"),
            texture_count: 1,
            mip_storage_mode: "Единый упакованный блоб (1× cHcImageDx)".into(),
            flags_u16: 0,
            flags: 0,
            unnamed_u16: 0,
            image_flags: 0,
            pre_blob_u16: 0,
            file_size_bytes: 0,
            total_blob_bytes: 0,
            header_bytes: 0,
            name_matches_filename: false,
            has_bak: false,
            bak_size_bytes: 0,
            backup_size_bytes: 0,
        };
        let expected = tgpcdx_format_to_dds_name(&original.format).unwrap();
        let actual = DdsCompression::Dxt5.as_dds_name();
        assert_ne!(expected, actual);
        assert_eq!(
            format!("Ожидался формат {expected}, но выбранный DDS имеет формат {actual}"),
            "Ожидался формат DXT1, но выбранный DDS имеет формат DXT5"
        );

        // Нестепень двойки отвергается.
        assert!(!700u32.is_power_of_two());
        assert!(1024u32.is_power_of_two());
    }

    #[test]
    fn test_texture_info_text_renders_chain() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<cHcTextureGroup xmlns:d="http://www.kuju.com/TnT/2003/Delta" d:version="1.0" d:id="1">
<Texture>
<cHcTexture d:id="2">
<Width d:type="sUInt32">1024</Width>
<Height d:type="sUInt32">1024</Height>
<Name d:type="cDeltaString">TrainSimContent\Textures\Sample</Name>
<Data>
<cHcTextureData d:id="3">
<Mip>
<cHcImageDx d:id="4">
<Format d:type="cDeltaString">HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA</Format>
<IsSwizzled d:type="bool">0</IsSwizzled>
<Width d:type="sUInt16">1024</Width>
<Height d:type="sUInt16">1024</Height>
<d:blob d:size="4">0000</d:blob>
</cHcImageDx>
</Mip>
</cHcTextureData>
</Data>
</cHcTexture>
</Texture>
</cHcTextureGroup>"#;
        let info = inspect_tgpcdx(xml.as_bytes()).unwrap();
        assert_eq!(info.name, r#"TrainSimContent\Textures\Sample"#);
        assert_eq!(info.width, 1024);
        assert_eq!(info.height, 1024);
        assert_eq!(info.mip_count, 1);
        assert_eq!(info.format_label, "DXT5 (BC3, плавная альфа)");
        assert_eq!(info.dds_format, Some("DXT5"));

        let text = texture_info_text(&info, Path::new("x\\y\\tex.TgPcDx"), false);
        assert!(text.contains("Внутреннее имя (Name): TrainSimContent\\Textures\\Sample"));
        assert!(text.contains("Разрешение: 1024×1024 px | Текстур в группе: 1 | Мипмапов: 1"));
        assert!(text.contains("Формат: DXT5 (BC3, плавная альфа) [HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA] | DDS: DXT5"));
        assert!(text.contains("IsSwizzled: 0"));
        assert!(text.contains("Flags(u16): 0"));
        assert!(text.contains(
            "Для импорта требуется: DDS в формате DXT5, размеры степень двойки, с мипмапами"
        ));
        assert!(text.contains("TGPCDX  x\\y\\tex.TgPcDx"));
    }
    #[test]
    fn preserves_serz_prefix_and_tail_on_reimport() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let dds_path = dir.path().join("src.dds");
        let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
            height: 16,
            width: 16,
            depth: None,
            format: ddsfile::D3DFormat::DXT1,
            mipmap_levels: Some(5),
            caps2: None,
        })
        .unwrap();
        for (i, b) in dds.data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut f = fs::File::create(&dds_path).unwrap();
        dds.write(&mut f).unwrap();

        let tg_path = dir.path().join("src.TgPcDx");
        convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
        let orig = fs::read(&tg_path).unwrap();

        // Modify only the first mip in an exported DDS, then reimport.
        let out_dds = dir.path().join("out.dds");
        export_tgpcdx_to_dds(&tg_path, &out_dds).unwrap();
        let mut dds2 = fs::read(&out_dds).unwrap();
        for b in &mut dds2[128..256] {
            *b = b.wrapping_add(1);
        }
        fs::write(&out_dds, &dds2).unwrap();

        let tg2_path = dir.path().join("dst.TgPcDx");
        fs::copy(&tg_path, &tg2_path).unwrap();
        convert_dds_to_tgpcdx(&out_dds, &tg2_path).unwrap();
        let new = fs::read(&tg2_path).unwrap();

        assert_eq!(new.len(), orig.len());

        let marker = [0xFFu8, 0x42];
        let pos = orig.windows(2).position(|w| w == marker).expect("marker");
        let elem_count = u32::from_le_bytes(orig[pos + 2..pos + 6].try_into().unwrap());
        let blob_start = pos;
        let blob_end = pos + 6 + (elem_count as usize) * 2;

        assert_eq!(&new[..blob_start], &orig[..blob_start]);
        assert_eq!(&new[blob_end..], &orig[blob_end..]);
        assert_ne!(&new[pos + 6..blob_end], &orig[pos + 6..blob_end]);
    }

    #[test]
    fn update_tgpcdx_internal_name_preserves_metadata_and_blob() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let dds_path = dir.path().join("src.dds");
        let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
            height: 16,
            width: 16,
            depth: None,
            format: ddsfile::D3DFormat::DXT1,
            mipmap_levels: Some(5),
            caps2: None,
        })
        .unwrap();
        for (i, b) in dds.data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut f = fs::File::create(&dds_path).unwrap();
        dds.write(&mut f).unwrap();

        let tg_path = dir.path().join("src.TgPcDx");
        convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
        let orig_bytes = fs::read(&tg_path).unwrap();
        let orig_info = inspect_tgpcdx_with_path(&orig_bytes, Some(&tg_path)).unwrap();

        fn first_blob(data: &[u8]) -> Vec<u8> {
            let xml = serz_bytes_to_xml(data).unwrap();
            let block = all_image_blocks(&xml)[0];
            hex_decode(&extract_blob(block).unwrap())
        }
        let orig_blob = first_blob(&orig_bytes);

        // Длиннее.
        update_tgpcdx_internal_name(&tg_path, "Provider\\Product\\Folder\\LongerTextureName")
            .unwrap();
        let long_bytes = fs::read(&tg_path).unwrap();
        let long_info = inspect_tgpcdx_with_path(&long_bytes, Some(&tg_path)).unwrap();
        assert_eq!(
            long_info.name,
            "Provider\\Product\\Folder\\LongerTextureName"
        );
        assert_eq!(long_info.width, orig_info.width);
        assert_eq!(long_info.height, orig_info.height);
        assert_eq!(long_info.format, orig_info.format);
        assert_eq!(long_info.is_swizzled, orig_info.is_swizzled);
        assert_eq!(long_info.total_blob_bytes, orig_info.total_blob_bytes);
        assert_eq!(first_blob(&long_bytes), orig_blob);

        // Короче.
        update_tgpcdx_internal_name(&tg_path, "X").unwrap();
        let short_bytes = fs::read(&tg_path).unwrap();
        let short_info = inspect_tgpcdx_with_path(&short_bytes, Some(&tg_path)).unwrap();
        assert_eq!(short_info.name, "X");
        assert_eq!(short_info.width, orig_info.width);
        assert_eq!(short_info.height, orig_info.height);
        assert_eq!(short_info.format, orig_info.format);
        assert_eq!(short_info.is_swizzled, orig_info.is_swizzled);
        assert_eq!(short_info.total_blob_bytes, orig_info.total_blob_bytes);
        assert_eq!(first_blob(&short_bytes), orig_blob);
    }

    #[test]
    fn update_tgpcdx_internal_name_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tg_path = dir.path().join("x.TgPcDx");
        std::fs::write(&tg_path, b"").unwrap();
        assert!(update_tgpcdx_internal_name(&tg_path, "   ").is_err());
    }
}
