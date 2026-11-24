//! Common SERZ/XML parsing helpers for texture handling.

use std::mem;

use crate::common::serz::{SerzNode, SerzReader};

/// Структура мипмапа с извлечёнными данными.
#[derive(Debug, Clone)]
pub struct ImageMip {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Извлекает мипмапы из байтов (SERZ или XML).
pub fn image_mips_from_bytes(data: &[u8]) -> Result<Vec<ImageMip>, String> {
    if data.starts_with(b"SERZ") {
        let mut reader = SerzReader::new(data);
        let mut root = reader.parse().map_err(|e| crate::gettext!("serz parse: {}",e))?;
        let mut mips = Vec::new();
        collect_serz_image_mips_owned(&mut root, &mut mips)?;
        return Ok(mips);
    }

    let xml = std::str::from_utf8(data).map_err(|e| crate::gettext!("invalid text: {}", e))?;
    all_image_blocks(xml)
        .into_iter()
        .map(|block| {
            let (width, height) = image_dimensions(block).ok_or(crate::gettext!("invalid mip dimensions"))?;
            let bytes = extract_blob_slice(block)
                .map(hex_decode)
                .unwrap_or_default();
            let declared = extract_blob_size(block) as usize;
            if declared > 0 && bytes.len() != declared {
                return Err(crate::gettext!("XML mip blob size does not match its declared size"));
            }
            Ok(ImageMip {
                format: extract_format(block),
                width,
                height,
                data: bytes,
            })
        })
        .collect()
}

/// Извлекает все блоки cHcImageDx из XML.
pub fn all_image_blocks(xml: &str) -> Vec<&str> {
    const START: &str = "<cHcImageDx";
    const END: &str = "</cHcImageDx>";

    let mut out = Vec::new();
    let mut search = xml;
    while let Some(start) = search.find(START) {
        let after_start = &search[start + START.len()..];
        if !after_start
            .as_bytes()
            .first()
            .is_some_and(|&b| b == b'>' || b.is_ascii_whitespace())
        {
            search = after_start;
            continue;
        }
        let Some(rel_end) = search[start..].find(END) else {
            break;
        };
        let end = start + rel_end + END.len();
        out.push(&search[start..end]);
        search = &search[end..];
    }
    out
}

/// Извлекает размеры изображения из XML-блока.
pub fn image_dimensions(block: &str) -> Option<(u32, u32)> {
    let width = extract_tag_slice(block, "Width", "sUInt16")
        .or_else(|| extract_tag_slice(block, "Width", "sUInt32"))?
        .parse()
        .ok()?;
    let height = extract_tag_slice(block, "Height", "sUInt16")
        .or_else(|| extract_tag_slice(block, "Height", "sUInt32"))?
        .parse()
        .ok()?;
    Some((width, height))
}

/// Извлекает тег из XML-блока.
pub fn extract_tag(block: &str, tag: &str, dtype: &str) -> Option<String> {
    extract_tag_slice(block, tag, dtype).map(str::to_owned)
}

fn extract_tag_slice<'a>(block: &'a str, tag: &str, dtype: &str) -> Option<&'a str> {
    let mut search = block;
    while let Some(lt) = search.find('<') {
        let rest = &search[lt + 1..];
        if let Some(after_tag) = rest.strip_prefix(tag)
            && let Some(after_attr) = after_tag.strip_prefix(" d:type=\"")
            && let Some(after_type) = after_attr.strip_prefix(dtype)
            && let Some(content) = after_type.strip_prefix("\">")
        {
            let end = content.find("</")?;
            return Some(content[..end].trim());
        }
        search = rest;
    }
    None
}

/// Извлекает формат из XML-блока.
pub fn extract_format(block: &str) -> String {
    if let Some(start) = block.find("<Format") {
        let rest = &block[start..];
        if let Some(gt) = rest.find('>') {
            let content = &rest[gt + 1..];
            if let Some(end) = content.find("</Format>") {
                let val = content[..end].trim();
                if !val.is_empty() {
                    return val.to_owned();
                }
            }
        }
    }
    // Fallback: scan block for known markers.
    if block.contains("INTERP_ALPHA") || block.contains("DXT5") || block.contains("BC3") {
        return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".to_owned();
    }
    if block.contains("EXPL_ALPHA") || block.contains("DXT3") || block.contains("BC2") {
        return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".to_owned();
    }
    if block.contains("DXT1") || block.contains("BC1") {
        return "HC_IMAGE_FORMAT_COMPRESSED".to_owned();
    }
    String::new()
}

fn extract_blob_slice(block: &str) -> Option<&str> {
    const MARKER: &str = "<d:blob d:size=\"";
    let start = block.find(MARKER)?;
    let rest = &block[start + MARKER.len()..];
    let gt = rest.find('>')?;
    let content = &rest[gt + 1..];
    let end = content.find("</d:blob>")?;
    Some(&content[..end])
}

/// Извлекает размер блоба из атрибута d:size.
pub fn extract_blob_size(block: &str) -> u64 {
    const MARKER: &str = "<d:blob d:size=\"";
    let Some(start) = block.find(MARKER) else {
        return 0;
    };
    let rest = &block[start + MARKER.len()..];
    rest.find('"')
        .and_then(|end| rest[..end].parse::<u64>().ok())
        .unwrap_or(0)
}

/// Считает количество cHcTexture в XML.
pub fn count_c_hc_textures(xml: &str) -> u32 {
    const TAG: &str = "<cHcTexture";
    let mut count = 0u32;
    let mut search = xml;
    while let Some(pos) = search.find(TAG) {
        let after = &search[pos + TAG.len()..];
        if after
            .as_bytes()
            .first()
            .is_some_and(|&b| b == b'>' || b.is_ascii_whitespace())
        {
            count += 1;
        }
        search = after;
    }
    count
}

/// Парсит булевый тег из XML-блока.
pub fn parse_bool_tag(block: &str, tag: &str) -> bool {
    for dtype in ["bool", "sUInt32", "sUInt8", "sInt32", "sUInt16"] {
        if let Some(v) = extract_tag_slice(block, tag, dtype) {
            if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes") {
                return true;
            }
            if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no") {
                return false;
            }
            if let Ok(n) = v.parse::<u32>() {
                return n != 0;
            }
        }
    }
    false
}

/// Извлекает unnamed u16 перед blob.
pub fn extract_unnamed_u16(block: &str) -> u16 {
    const TAG: &str = "<e d:type=\"sUInt16\">";
    let parse_after = |s: &str| {
        let end = s.find("</e>")?;
        s[..end].trim().parse::<u16>().ok()
    };

    if let Some(blob_pos) = block.find("<d:blob") {
        let pre = &block[..blob_pos];
        if let Some(start) = pre.rfind(TAG)
            && let Some(v) = parse_after(&pre[start + TAG.len()..])
        {
            return v;
        }
    }
    // Fallback: any <e d:type="sUInt16"> in block.
    if let Some(start) = block.find(TAG)
        && let Some(v) = parse_after(&block[start + TAG.len()..])
    {
        return v;
    }
    0
}

/// Декодирует hex-строку в байты без промежуточных строк.
pub fn hex_decode(s: &str) -> Vec<u8> {
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

/// Собирает мипмапы из SERZ дерева (заимствование).
pub fn collect_serz_image_mips(node: &SerzNode, mips: &mut Vec<ImageMip>) -> Result<(), String> {
    let SerzNode::Element { name, children, .. } = node else {
        return Ok(());
    };
    if name == "cHcImageDx" {
        let mut format = None;
        let mut width = None;
        let mut height = None;
        let mut blob = None;
        for child in children {
            match child {
                SerzNode::Property {
                    name,
                    type_name,
                    value,
                    ..
                } if name == "Format" && type_name == "cDeltaString" => {
                    format = Some(value.clone());
                }
                SerzNode::Property { name, value, .. } if name == "Width" => {
                    width = value.trim().parse::<u32>().ok();
                }
                SerzNode::Property { name, value, .. } if name == "Height" => {
                    height = value.trim().parse::<u32>().ok();
                }
                SerzNode::Blob { data } => blob = Some(data.clone()),
                _ => {}
            }
        }
        mips.push(ImageMip {
            format: format.ok_or("image has no Format property")?,
            width: width.ok_or("image has invalid Width property")?,
            height: height.ok_or("image has invalid Height property")?,
            data: blob.unwrap_or_default(),
        });
        return Ok(());
    }
    for child in children {
        collect_serz_image_mips(child, mips)?;
    }
    Ok(())
}

/// Собирает мипмапы из временного SERZ дерева с переносом Blob без копирования.
pub(super) fn collect_serz_image_mips_owned(
    node: &mut SerzNode,
    mips: &mut Vec<ImageMip>,
) -> Result<(), String> {
    let SerzNode::Element { name, children, .. } = node else {
        return Ok(());
    };
    if name == "cHcImageDx" {
        let mut format = None;
        let mut width = None;
        let mut height = None;
        let mut blob = None;
        for child in children.iter_mut() {
            match child {
                SerzNode::Property {
                    name,
                    type_name,
                    value,
                    ..
                } if name == "Format" && type_name == "cDeltaString" => {
                    format = Some(mem::take(value));
                }
                SerzNode::Property { name, value, .. } if name == "Width" => {
                    width = value.trim().parse::<u32>().ok();
                }
                SerzNode::Property { name, value, .. } if name == "Height" => {
                    height = value.trim().parse::<u32>().ok();
                }
                SerzNode::Blob { data } => blob = Some(mem::take(data)),
                _ => {}
            }
        }
        mips.push(ImageMip {
            format: format.ok_or("image has no Format property")?,
            width: width.ok_or("image has invalid Width property")?,
            height: height.ok_or("image has invalid Height property")?,
            data: blob.unwrap_or_default(),
        });
        return Ok(());
    }
    for child in children.iter_mut() {
        collect_serz_image_mips_owned(child, mips)?;
    }
    Ok(())
}

/// Находит элемент по имени в SERZ дереве (mut).
pub fn find_element_mut<'a>(node: &'a mut SerzNode, name: &str) -> Option<&'a mut SerzNode> {
    if let SerzNode::Element { name: n, .. } = &*node
        && n == name
    {
        return Some(node);
    }
    if let SerzNode::Element { children, .. } = node {
        for child in children.iter_mut() {
            if let Some(found) = find_element_mut(child, name) {
                return Some(found);
            }
        }
    }
    None
}

/// Находит все элементы по имени в SERZ дереве.
pub fn find_all_elements<'a>(node: &'a SerzNode, name: &str) -> Vec<&'a SerzNode> {
    fn walk<'a>(node: &'a SerzNode, name: &str, out: &mut Vec<&'a SerzNode>) {
        if let SerzNode::Element {
            name: n, children, ..
        } = node
        {
            if n == name {
                out.push(node);
            }
            for child in children {
                walk(child, name, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(node, name, &mut out);
    out
}

/// Устанавливает или вставляет property в SERZ элемент.
pub fn set_or_insert_property(
    elem: &mut SerzNode,
    name: &str,
    type_name: &str,
    value: &str,
) -> Result<(), String> {
    let SerzNode::Element { children, .. } = elem else {
        return Err(crate::gettext!("cannot set {} on a non-element node", name));
    };
    if let Some(SerzNode::Property { value: current, .. }) = children
        .iter_mut()
        .find(|child| matches!(child, SerzNode::Property { name: prop, .. } if prop == name))
    {
        current.clear();
        current.push_str(value);
    } else {
        children.push(SerzNode::Property {
            name: name.to_owned(),
            type_name: type_name.to_owned(),
            value: value.to_owned(),
            alt_encoding: None,
        });
    }
    Ok(())
}

/// Устанавливает blob данные в SERZ элемент.
pub fn set_blob_data(elem: &mut SerzNode, data: &[u8]) {
    let SerzNode::Element { children, .. } = elem else {
        return;
    };
    for child in children.iter_mut() {
        if let SerzNode::Blob { data: d } = child {
            d.clear();
            d.extend_from_slice(data);
            return;
        }
    }
    children.push(SerzNode::Blob {
        data: data.to_vec(),
    });
}

/// Находит размеры текстуры в SERZ дереве.
pub fn find_texture_dimensions(node: &SerzNode) -> Option<(u32, u32)> {
    let SerzNode::Element { name, children, .. } = node else {
        return None;
    };
    if name == "cHcTexture" {
        let mut width = None;
        let mut height = None;
        for child in children {
            if let SerzNode::Property {
                name: prop_name,
                value,
                ..
            } = child
            {
                match prop_name.as_str() {
                    "Width" => width = value.trim().parse().ok(),
                    "Height" => height = value.trim().parse().ok(),
                    _ => {}
                }
            }
        }
        if let (Some(w), Some(h)) = (width, height) {
            return Some((w, h));
        }
    }
    children.iter().find_map(find_texture_dimensions)
}
