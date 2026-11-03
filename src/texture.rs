#![allow(dead_code)]
//! TgPcDx texture decoding, DDS export/import.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn decode_tgpcdx_to_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let xml = if data.starts_with(b"SERZ") {
        serz_bytes_to_xml(data)?
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?
    };

    let image_block = first_image_block(&xml).ok_or("no image block")?;
    let format = extract_format(image_block);
    let width: u32 = extract_tag(image_block, "Width", "sUInt16")
        .or_else(|| extract_tag(image_block, "Width", "sUInt32"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let height: u32 = extract_tag(image_block, "Height", "sUInt16")
        .or_else(|| extract_tag(image_block, "Height", "sUInt32"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    let blob = extract_blob(image_block).ok_or("no blob")?;
    let bytes = hex_decode(&blob);

    let pixels = (width * height) as usize;
    let format_upper = format.to_uppercase();
    let is_bc3 = format_upper.contains("INTERP_ALPHA")
        || format_upper.contains("BC3")
        || format_upper.contains("DXT5");
    let is_bc1 = format_upper.contains("COMPRESSED") && !is_bc3;
    let is_uncompressed = format_upper.contains("RGBA")
        || format_upper.contains("BGRA")
        || format_upper.contains("ARGB")
        || format_upper.contains("UNCOMPRESSED");

    let mut rgba = Vec::with_capacity(pixels * 4);

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

    if rgba.len() < pixels * 4 {
        return Err("decoded data too small".into());
    }
    Ok((width, height, rgba))
}

pub fn rgba_to_slint_image(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        bytemuck::cast_slice(rgba),
        width,
        height,
    );
    slint::Image::from_rgba8(pixel_buffer)
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

    let first = blocks[0];
    let fmt_str = extract_format(first);
    let d3d_fmt = tgpcdx_format_to_d3d(&fmt_str);
    let width: u32 = extract_tag(first, "Width", "sUInt16")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let height: u32 = extract_tag(first, "Height", "sUInt16")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mip_count = blocks.len() as u32;

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
    for block in &blocks {
        let blob = extract_blob(block).ok_or("no blob")?;
        let bytes = hex_decode(&blob);
        let sz = bytes.len();
        if offset + sz <= dds.data.len() {
            dds.data[offset..offset + sz].copy_from_slice(&bytes);
        }
        offset += sz;
    }

    let mut file = fs::File::create(output).map_err(|e| e.to_string())?;
    dds.write(&mut file).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- import DDS -> TgPcDx ----------

pub fn convert_dds_to_tgpcdx(input: &Path, output: &Path) -> Result<(), String> {
    let file = fs::File::open(input).map_err(|e| e.to_string())?;
    let dds = ddsfile::Dds::read(file).map_err(|e| e.to_string())?;

    let width = dds.get_width();
    let height = dds.get_height();
    if !width.is_power_of_two() || !height.is_power_of_two() {
        return Err("dimensions must be powers of two".into());
    }
    let mip_count = dds.get_num_mipmap_levels();
    if mip_count < 1 {
        return Err("must contain mipmaps".into());
    }

    let fmt_str = dds_format_to_tgpcdx(&dds);
    let all = dds.get_data(0).map_err(|e| e.to_string())?;
    let format = dds.get_format().ok_or("unsupported format")?;
    let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut off = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..mip_count {
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

    // Быстрое построение XML без промежуточных коллекций строк
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

    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let xml_path = dir.path().join("texture.xml");
    fs::write(&xml_path, s).map_err(|e| e.to_string())?;

    let serz = find_serz()?;
    let status = Command::new(&serz)
        .arg(&xml_path)
        .status()
        .map_err(|e| e.to_string())?;
    let bin_path = xml_path.with_extension("bin");
    if !status.success() || !bin_path.is_file() {
        return Err("serz conversion failed".into());
    }
    let bytes = fs::read(&bin_path).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty output".into());
    }

    let tmp = output.with_extension("tmp");
    fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err("empty staged file".into());
    }
    let _ = fs::remove_file(output);
    fs::rename(&tmp, output).map_err(|e| e.to_string())?;
    Ok(())
}

fn find_serz() -> Result<PathBuf, String> {
    let rw = crate::fs_utils::clean_rw_path(&crate::settings::load_last_game_path());
    let preferred = crate::assets::preferred_serz_path(&rw);
    if preferred.is_file() {
        return Ok(preferred);
    }
    // fallback: try find_serz_path
    if let Some(path) = crate::assets::find_serz_path(&rw)
        && path.is_file()
    {
        return Ok(path);
    }
    let candidates = [
        "C:\\Program Files (x86)\\Steam\\steamapps\\common\\RailWorks\\serz.exe",
        "C:\\Program Files\\Steam\\steamapps\\common\\RailWorks\\serz.exe",
        "D:\\SteamLibrary\\steamapps\\common\\RailWorks\\serz.exe",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(out) = Command::new("where").arg("serz.exe").output()
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let p = PathBuf::from(line.trim());
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err("serz.exe not found".into())
}

fn serz_bytes_to_xml(data: &[u8]) -> Result<String, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let input = dir.path().join("texture.TgPcDx");
    let output = dir.path().join("texture.xml");
    fs::write(&input, data).map_err(|e| e.to_string())?;
    let serz = find_serz()?;
    let status = Command::new(&serz)
        .arg(&input)
        .arg(format!("/xml:{}", output.display()))
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !output.is_file() {
        return Err("serz conversion failed".into());
    }
    fs::read_to_string(&output).map_err(|e| e.to_string())
}

fn first_image_block(xml: &str) -> Option<&str> {
    let start = xml.find("<cHcImageDx")?;
    let end_marker = "</cHcImageDx>";
    let end = xml[start..].find(end_marker)? + start + end_marker.len();
    Some(&xml[start..end])
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
    if fmt.contains("INTERP_ALPHA") {
        ddsfile::D3DFormat::DXT5
    } else {
        ddsfile::D3DFormat::DXT1
    }
}

fn dds_format_to_tgpcdx(dds: &ddsfile::Dds) -> String {
    if let Some(ref fourcc) = dds.header.spf.fourcc {
        let s = format!("{:?}", fourcc);
        if s.contains("DXT5") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("DXT3") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
    }
    if let Some(d3d) = dds.get_d3d_format() {
        match d3d {
            ddsfile::D3DFormat::DXT5 => return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into(),
            ddsfile::D3DFormat::DXT3 => return "HC_IMAGE_FORMAT_COMPRESSED".into(),
            _ => {}
        }
    }
    if let Some(dxgi) = dds.get_dxgi_format() {
        let s = format!("{:?}", dxgi);
        if s.contains("BC3") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("BC2") | s.contains("BC1") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
    }
    "HC_IMAGE_FORMAT_COMPRESSED".into()
}
