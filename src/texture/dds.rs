//! DDS import/export, mip chain validation, and SERZ tree modification.

use std::cmp::Reverse;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufWriter, Cursor, ErrorKind};
use std::path::{Path, PathBuf};

use super::format::{
    ImportReport, dds_compression_of, dds_format_to_tgpcdx, expected_mip_size, mip_level_bytes,
    tgpcdx_dds_compression, tgpcdx_format_to_d3d, validate_dds_mip_chain,
};
use super::serz_parse::{
    collect_serz_image_mips, find_all_elements, find_element_mut, find_texture_dimensions,
    image_mips_from_bytes, set_blob_data, set_or_insert_property,
};
use crate::common::serz::{
    SerzNode, SerzReader, encode_tgpcdx_to_vec, patch_tgpcdx_blobs, validate_tgpcdx_binary,
};

/// Converts DDS file to TgPcDx.
pub fn convert_dds_to_tgpcdx_report(input: &Path, output: &Path) -> Result<ImportReport, String> {
    let dds_bytes = fs::read(input).map_err(|e| e.to_string())?;
    let dds = ddsfile::Dds::read(Cursor::new(&dds_bytes)).map_err(|e| e.to_string())?;

    let width = dds.get_width();
    let height = dds.get_height();
    if width == 0 || height == 0 {
        return Err("некорректные размеры DDS".into());
    }
    if width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
        return Err("Размеры DDS превышают диапазон TgPcDx sUInt16".into());
    }
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
        Err(error) if error.kind() == ErrorKind::NotFound => None,
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
    let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::with_capacity(declared as usize);
    let mut off = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..declared {
        let pitch = format.get_pitch(mw).ok_or("pitch")?;
        let row_h = mh.div_ceil(format.get_pitch_height());
        let size = pitch.checked_mul(row_h).ok_or("mip data size overflow")? as usize;
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
        let existing_layout_matches = existing_images.len() == mips.len()
            && existing_images
                .iter()
                .zip(&mips)
                .all(|(existing, (w, h, _))| {
                    existing.width == *w && existing.height == *h && existing.format == fmt_str
                });
        if existing_layout_matches && let Ok(patched) = patch_tgpcdx_blobs(&target_bytes, &mips) {
            validate_imported_tgpcdx(&patched, width, height, &fmt_str, &mips)?;
            return write_output_bytes(output, &patched).map(|()| report);
        }

        // Rebuild mip nodes whenever their layout or format does not match the DDS.
        let mut reader = SerzReader::new(&target_bytes);
        let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        let preamble = reader.preamble.clone();
        modify_tgpcdx_tree(&mut root, width, height, &fmt_str, &mips)?;
        let bytes = encode_tgpcdx_to_vec(&root, preamble.as_ref())?;
        validate_imported_tgpcdx(&bytes, width, height, &fmt_str, &mips)?;
        return write_output_bytes(output, &bytes).map(|()| report);
    }

    // Build the AST directly when creating a new texture.
    let node = new_tgpcdx_tree(width, height, &fmt_str, &mips);
    let bytes = encode_tgpcdx_to_vec(&node, None)?;
    validate_imported_tgpcdx(&bytes, width, height, &fmt_str, &mips)?;
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
    let compression = tgpcdx_dds_compression(&fmt_str);

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

    // Сортируем от самого крупного уровня к самому мелкому (стандарт DirectX).
    mips.sort_by_key(|a| Reverse(u64::from(a.0) * u64::from(a.1)));

    let width = mips[0].0;
    let height = mips[0].1;

    // Если один cHcImageDx содержит единый упакованный блоб (все мипмапы подряд),
    // валидируем цепочку и копируем блоб в DDS без промежуточного разбиения.
    let mut mip_count = mips.len() as u32;
    let mut is_packed_blob = false;

    if mips.len() == 1 {
        let blob_len = mips[0].2.len();
        if let Some(top_size) = mip_level_bytes(width, height, compression)
            && blob_len > top_size
        {
            let mut levels = 1u32;
            let mut mw = width;
            let mut mh = height;
            let mut sum = top_size;
            while sum < blob_len && !(mw == 1 && mh == 1) {
                mw = (mw / 2).max(1);
                mh = (mh / 2).max(1);
                let sz = mip_level_bytes(mw, mh, compression)
                    .ok_or("невозможно вычислить размер уровня мипмапа")?;
                if sz == 0 || sz > blob_len - sum {
                    break;
                }
                sum += sz;
                levels += 1;
            }
            if sum == blob_len && levels > 1 {
                mip_count = levels;
                is_packed_blob = true;
            }
        }
    }

    if !is_packed_blob {
        for (w, h, bytes) in &mips {
            let expected = expected_mip_size(*w, *h, d3d_fmt);
            if expected > 0 && bytes.len() != expected {
                return Err(format!(
                    "mip {w}x{h} size mismatch: got {}, expected {expected}",
                    bytes.len()
                ));
            }
        }
    }

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
        let end = offset
            .checked_add(bytes.len())
            .ok_or("mip data size overflow")?;
        if end > dds.data.len() {
            return Err("mip data exceeds dds buffer".into());
        }
        dds.data[offset..end].copy_from_slice(bytes);
        offset = end;
    }

    let file = File::create(output).map_err(|e| e.to_string())?;
    let mut writer = BufWriter::new(file);
    dds.write(&mut writer).map_err(|e| e.to_string())?;
    Ok(())
}

/// Writes output bytes atomically without risking loss of the existing file.
fn write_output_bytes(output: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"SERZ") {
        validate_tgpcdx_binary(bytes, None)?;
    }

    let mut tmp_os = OsString::from(output.as_os_str());
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }

    if let Err(err) = fs::rename(&tmp, output) {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

/// Builds a fresh `cHcImageDx` SERZ node.
fn build_image_node(width: u32, height: u32, fmt_str: &str, data: &[u8]) -> SerzNode {
    SerzNode::Element {
        name: "cHcImageDx".into(),
        id: 4,
        version: 1,
        children: vec![
            SerzNode::Property {
                name: "Format".into(),
                type_name: "cDeltaString".into(),
                value: fmt_str.into(),
                alt_encoding: None,
            },
            SerzNode::Property {
                name: "IsSwizzled".into(),
                type_name: "bool".into(),
                value: "0".into(),
                alt_encoding: None,
            },
            SerzNode::Property {
                name: "Width".into(),
                type_name: "sUInt16".into(),
                value: width.to_string(),
                alt_encoding: None,
            },
            SerzNode::Property {
                name: "Height".into(),
                type_name: "sUInt16".into(),
                value: height.to_string(),
                alt_encoding: None,
            },
            SerzNode::Blob {
                data: data.to_vec(),
            },
        ],
    }
}

/// Clones a `cHcImageDx` template without copying its existing pixel `Blob`.
fn clone_image_template_without_blob(node: &SerzNode) -> SerzNode {
    match node {
        SerzNode::Element {
            name,
            id,
            version,
            children,
        } => SerzNode::Element {
            name: name.clone(),
            id: *id,
            version: *version,
            children: children
                .iter()
                .map(clone_image_template_without_blob)
                .collect(),
        },
        SerzNode::Blob { .. } => SerzNode::Blob { data: Vec::new() },
        other => other.clone(),
    }
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
        .map(clone_image_template_without_blob)
        .collect();

    let mut image_nodes = Vec::with_capacity(mips.len());
    for (idx, (w, h, bytes)) in mips.iter().enumerate() {
        let img = if let Some(template) = templates.get(idx).or_else(|| templates.first()) {
            let mut node = template.clone();
            set_or_insert_property(&mut node, "Format", "cDeltaString", fmt_str)?;
            set_or_insert_property(&mut node, "Width", "sUInt16", &w.to_string())?;
            set_or_insert_property(&mut node, "Height", "sUInt16", &h.to_string())?;
            set_or_insert_property(&mut node, "IsSwizzled", "bool", "0")?;
            set_blob_data(&mut node, bytes);
            node
        } else {
            build_image_node(*w, *h, fmt_str, bytes)
        };
        image_nodes.push(img);
    }

    let mip_container = find_element_mut(root, "Mip").ok_or("no Mip node")?;
    if let SerzNode::Element { children, .. } = mip_container {
        children.retain(
            |child| !matches!(child, SerzNode::Element { name, .. } if name == "cHcImageDx"),
        );
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
    let image_nodes = mips
        .iter()
        .map(|(w, h, data)| build_image_node(*w, *h, fmt_str, data))
        .collect();

    SerzNode::Element {
        name: "cHcTextureGroup".into(),
        id: 1,
        version: 1,
        children: vec![SerzNode::Element {
            name: "Texture".into(),
            id: 0,
            version: 1,
            children: vec![SerzNode::Element {
                name: "cHcTexture".into(),
                id: 2,
                version: 1,
                children: vec![
                    SerzNode::Property {
                        name: "Width".into(),
                        type_name: "sUInt32".into(),
                        value: width.to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Property {
                        name: "Height".into(),
                        type_name: "sUInt32".into(),
                        value: height.to_string(),
                        alt_encoding: None,
                    },
                    SerzNode::Property {
                        name: "Name".into(),
                        type_name: "cDeltaString".into(),
                        value: "texture".into(),
                        alt_encoding: None,
                    },
                    SerzNode::Element {
                        name: "Data".into(),
                        id: 0,
                        version: 1,
                        children: vec![SerzNode::Element {
                            name: "cHcTextureData".into(),
                            id: 3,
                            version: 1,
                            children: vec![SerzNode::Element {
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
    let mut reader = SerzReader::new(bytes);
    let root = reader
        .parse()
        .map_err(|e| format!("SERZ validation: {e}"))?;
    if find_texture_dimensions(&root) != Some((width, height)) {
        return Err("serialized TgPcDx dimensions do not match the DDS".into());
    }
    let mut actual_mips = Vec::with_capacity(expected_mips.len());
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
