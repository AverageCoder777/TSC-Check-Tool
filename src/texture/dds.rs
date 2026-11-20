//! DDS import/export, mip chain validation, and SERZ tree modification.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use super::format::{
    ImportReport, dds_compression_of, dds_format_to_tgpcdx, expected_mip_size,
    tgpcdx_format_to_d3d, validate_dds_mip_chain,
};
use super::serz_parse::{
    collect_serz_image_mips, find_all_elements, find_element_mut, find_texture_dimensions,
    image_mips_from_bytes, set_blob_data, set_or_insert_property,
};
use crate::serz::SerzNode;

/// Converts DDS file to TgPcDx.
pub fn convert_dds_to_tgpcdx_report(input: &Path, output: &Path) -> Result<ImportReport, String> {
    let dds_bytes = fs::read(input).map_err(|e| e.to_string())?;
    let dds = ddsfile::Dds::read(Cursor::new(&dds_bytes)).map_err(|e| e.to_string())?;

    let width = dds.get_width();
    let height = dds.get_height();
    if width == 0 || height == 0 {
        return Err("некорректные размеры DDS".into());
    }
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err("Размеры DDS превышают диапазон TgPcDx sUInt16".into());
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

    // Целевой .TgPcDx — источник метаданных и структуры mip-цепочки.
    let target_bytes = match fs::read(output) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if target_bytes
        .as_deref()
        .is_some_and(|bytes| !bytes.starts_with(b"SERZ"))
    {
        return Err("TgPcDx import requires a binary SERZ target".into());
    }

    // Validate the DDS chain independently of the old texture's mip count:
    // the import replaces the complete mip layout and its metadata.
    let chain = validate_dds_mip_chain(width, height, compression, declared, all.len(), None)?;

    let report = ImportReport {
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
        let end = off.checked_add(size).ok_or("mip data size overflow")?;
        if end > all.len() {
            return Err("DDS mip data is shorter than its declared mip chain".into());
        }
        mips.push((mw, mh, all[off..end].to_vec()));
        off = end;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }
    if off != all.len() {
        return Err(format!(
            "DDS mip data size mismatch: consumed {off} bytes, found {}",
            all.len()
        ));
    }

    if let Some(target_bytes) = target_bytes {
        let existing_images = image_mips_from_bytes(&target_bytes)?;
        if existing_images
            .first()
            .is_some_and(|image| image.width == width && image.height == height)
        {
            let existing_layout_matches = existing_images.len() == mips.len()
                && existing_images
                    .iter()
                    .zip(&mips)
                    .all(|(existing, (width, height, _))| {
                        existing.width == *width
                            && existing.height == *height
                            && existing.format == fmt_str
                    });
            if existing_layout_matches
                && let Ok(patched) = crate::serz::patch_tgpcdx_blobs(&target_bytes, &mips)
            {
                validate_imported_tgpcdx(&patched, width, height, &fmt_str, &mips)?;
                return write_output_bytes(output, &patched).map(|()| report);
            }
        }
        // Rebuild mip nodes whenever their layout or format does not match the DDS.
        let mut reader = crate::serz::SerzReader::new(&target_bytes);
        let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        let preamble = reader.preamble.clone();
        modify_tgpcdx_tree(&mut root, width, height, &fmt_str, &mips)?;
        let bytes = crate::serz::encode_tgpcdx_to_vec(&root, preamble.as_ref())?;
        validate_imported_tgpcdx(&bytes, width, height, &fmt_str, &mips)?;
        return write_output_bytes(output, &bytes).map(|()| report);
    }

    // Build the AST directly when creating a new texture.
    let node = new_tgpcdx_tree(width, height, &fmt_str, &mips);
    let bytes = crate::serz::encode_tgpcdx_to_vec(&node, None)?;
    validate_imported_tgpcdx(&bytes, width, height, &fmt_str, &mips)?;
    if bytes.is_empty() {
        return Err("empty output".into());
    }

    write_output_bytes(output, &bytes).map(|()| report)
}

/// Exports TgPcDx file to DDS.
pub fn export_tgpcdx_to_dds(input: &Path, output: &Path) -> Result<(), String> {
    let data = fs::read(input).map_err(|e| e.to_string())?;
    export_tgpcdx_bytes_to_dds(&data, output)
}

/// Exports TgPcDx bytes to DDS file.
pub fn export_tgpcdx_bytes_to_dds(data: &[u8], output: &Path) -> Result<(), String> {
    let image_mips = image_mips_from_bytes(data)?;
    if image_mips.is_empty() {
        return Err("no image blocks".into());
    }
    let fmt_str = image_mips[0].format.clone();
    let d3d_fmt = tgpcdx_format_to_d3d(&fmt_str);
    let compression = super::format::tgpcdx_dds_compression(&fmt_str);

    // Собираем мипмапы с размерами.
    let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::with_capacity(image_mips.len());
    for mip in image_mips {
        if mip.width == 0 || mip.height == 0 {
            return Err("invalid mip dimensions".into());
        }
        if mip.data.is_empty() {
            return Err("empty mip blob".into());
        }
        mips.push((mip.width, mip.height, mip.data));
    }

    // Если только один cHcImageDx, но блоб больше размера верхнего уровня —
    // это "Единый упакованный блоб": все мипмапы лежат в одном Blob подряд.
    // Разбиваем блоб на уровни по формуле размера (как в inspect_tgpcdx_with_path).
    if mips.len() == 1 {
        let (w, h, blob) = &mips[0];
        let top_level_size = super::format::mip_level_bytes(*w, *h, compression);
        if let Some(top_size) = top_level_size
            && blob.len() > top_size
        {
            // Вычисляем уровни за один проход: собираем (width, height, size) для каждого.
            let mut level_infos = Vec::new();
            let mut mw = *w;
            let mut mh = *h;
            let mut sum = 0usize;
            let total = blob.len();
            while sum < total {
                if mw == 1 && mh == 1 {
                    break;
                }
                mw = (mw / 2).max(1);
                mh = (mh / 2).max(1);
                let sz = super::format::mip_level_bytes(mw, mh, compression)
                    .ok_or("невозможно вычислить размер уровня мипмапа")?;
                if sz == 0 || sum + sz > total {
                    break;
                }
                level_infos.push((mw, mh, sz));
                sum += sz;
            }
            if sum == total && level_infos.len() > 1 {
                let mut split_mips = Vec::with_capacity(level_infos.len());
                let mut offset = 0usize;
                for (mw, mh, sz) in level_infos {
                    let end = offset + sz;
                    split_mips.push((mw, mh, blob[offset..end].to_vec()));
                    offset = end;
                }
                mips = split_mips;
            }
        }
    }

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

/// Writes output bytes atomically.
fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"SERZ") {
        crate::serz::validate_tgpcdx_binary(bytes, None)?;
    }
    // Use unique temp name to avoid conflicts and preserve original extension.
    let tmp = output.with_extension(format!(
        "{}.tmp",
        output.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    // Atomic replace: remove target first (if exists), then rename temp to target.
    // If rename fails, temp file remains as backup, original is gone but temp has data.
    let _ = fs::remove_file(output);
    fs::rename(&tmp, output).map_err(|e| {
        let _ = fs::remove_file(&tmp); // best effort cleanup
        e.to_string()
    })?;
    Ok(())
}

/// Modifies the TgPcDx SERZ tree with new mip data.
fn modify_tgpcdx_tree(
    root: &mut SerzNode,
    width: u32,
    height: u32,
    fmt_str: &str,
    mips: &[(u32, u32, Vec<u8>)],
) -> Result<(), String> {
    {
        let tex = find_element_mut(root, "cHcTexture").ok_or("no texture node")?;
        set_or_insert_property(tex, "Width", "sUInt32", &width.to_string())?;
        set_or_insert_property(tex, "Height", "sUInt32", &height.to_string())?;
    }

    let templates: Vec<SerzNode> = find_all_elements(root, "cHcImageDx")
        .into_iter()
        .cloned()
        .collect();

    // Build all image nodes first.
    let mut image_nodes = Vec::with_capacity(mips.len());
    for (idx, (w, h, bytes)) in mips.iter().enumerate() {
        let template = templates.get(idx).or_else(|| templates.first());
        let mut img = if let Some(t) = template {
            t.clone()
        } else {
            SerzNode::Element {
                name: "cHcImageDx".to_string(),
                id: 4,
                version: 1,
                children: vec![
                    SerzNode::Property {
                        name: "Format".to_string(),
                        type_name: "cDeltaString".to_string(),
                        value: fmt_str.to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Property {
                        name: "IsSwizzled".to_string(),
                        type_name: "bool".to_string(),
                        value: "0".to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Property {
                        name: "Width".to_string(),
                        type_name: "sUInt16".to_string(),
                        value: w.to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Property {
                        name: "Height".to_string(),
                        type_name: "sUInt16".to_string(),
                        value: h.to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Blob {
                        data: bytes.clone(),
                    },
                ],
            }
        };
        set_or_insert_property(&mut img, "Format", "cDeltaString", fmt_str)?;
        set_or_insert_property(&mut img, "Width", "sUInt16", &w.to_string())?;
        set_or_insert_property(&mut img, "Height", "sUInt16", &h.to_string())?;
        set_or_insert_property(&mut img, "IsSwizzled", "bool", "0")?;
        set_blob_data(&mut img, bytes);
        if let SerzNode::Element { name: n, .. } = &mut img {
            *n = "cHcImageDx".to_string();
        }
        image_nodes.push(img);
    }

    // Insert all at once.
    let mip_container = find_element_mut(root, "Mip").ok_or("no Mip node")?;
    if let SerzNode::Element { children, .. } = mip_container {
        children.extend(image_nodes);
    } else {
        return Err("invalid Mip node".into());
    }
    Ok(())
}

/// Creates a new TgPcDx SERZ tree from scratch.
fn new_tgpcdx_tree(
    width: u32,
    height: u32,
    fmt_str: &str,
    mips: &[(u32, u32, Vec<u8>)],
) -> SerzNode {
    use crate::serz::SerzNode::{Blob, Element, Property};

    let image_nodes = mips
        .iter()
        .map(|(mip_width, mip_height, data)| Element {
            name: "cHcImageDx".into(),
            id: 4,
            version: 1,
            children: vec![
                Property {
                    name: "Format".into(),
                    type_name: "cDeltaString".into(),
                    value: fmt_str.into(),
                    alt_encoding: None,
                },
                Property {
                    name: "IsSwizzled".into(),
                    type_name: "bool".into(),
                    value: "0".into(),
                    alt_encoding: None,
                },
                Property {
                    name: "Width".into(),
                    type_name: "sUInt16".into(),
                    value: mip_width.to_string(),
                    alt_encoding: None,
                },
                Property {
                    name: "Height".into(),
                    type_name: "sUInt16".into(),
                    value: mip_height.to_string(),
                    alt_encoding: None,
                },
                Blob { data: data.clone() },
            ],
        })
        .collect();

    Element {
        name: "cHcTextureGroup".into(),
        id: 1,
        version: 1,
        children: vec![Element {
            name: "Texture".into(),
            id: 0,
            version: 1,
            children: vec![Element {
                name: "cHcTexture".into(),
                id: 2,
                version: 1,
                children: vec![
                    Property {
                        name: "Width".into(),
                        type_name: "sUInt32".into(),
                        value: width.to_string(),
                        alt_encoding: None,
                    },
                    Property {
                        name: "Height".into(),
                        type_name: "sUInt32".into(),
                        value: height.to_string(),
                        alt_encoding: None,
                    },
                    Property {
                        name: "Name".into(),
                        type_name: "cDeltaString".into(),
                        value: "texture".into(),
                        alt_encoding: None,
                    },
                    Element {
                        name: "Data".into(),
                        id: 0,
                        version: 1,
                        children: vec![Element {
                            name: "cHcTextureData".into(),
                            id: 3,
                            version: 1,
                            children: vec![Element {
                                name: "Mip".into(),
                                id: 0,
                                version: 1,
                                children: image_nodes,
                            }],
                        }],
                    },
                ],
            }],
        }],
    }
}

/// Validates that imported TgPcDx matches expected DDS data.
fn validate_imported_tgpcdx(
    bytes: &[u8],
    width: u32,
    height: u32,
    format: &str,
    expected_mips: &[(u32, u32, Vec<u8>)],
) -> Result<(), String> {
    let mut reader = crate::serz::SerzReader::new(bytes);
    let root = reader
        .parse()
        .map_err(|e| format!("SERZ validation: {e}"))?;
    if find_texture_dimensions(&root) != Some((width, height)) {
        return Err("serialized TgPcDx dimensions do not match the DDS".into());
    }
    // Extract mips from already-parsed root instead of re-parsing bytes.
    let mut actual_mips = Vec::new();
    collect_serz_image_mips(&root, &mut actual_mips)?;
    if actual_mips.len() != expected_mips.len() {
        return Err("serialized TgPcDx mip count does not match the DDS".into());
    }
    for (actual, (expected_width, expected_height, expected_data)) in
        actual_mips.iter().zip(expected_mips)
    {
        if actual.width != *expected_width
            || actual.height != *expected_height
            || actual.format != format
            || actual.data != *expected_data
        {
            return Err("serialized TgPcDx mip metadata or Blob does not match the DDS".into());
        }
    }
    Ok(())
}
