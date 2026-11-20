//! Common SERZ/XML parsing helpers for texture handling.

use crate::serz::SerzNode;

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
        let mut reader = crate::serz::SerzReader::new(data);
        let root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;
        let mut mips = Vec::new();
        collect_serz_image_mips(&root, &mut mips)?;
        return Ok(mips);
    }

    let xml = String::from_utf8(data.to_vec()).map_err(|e| format!("invalid text: {e}"))?;
    all_image_blocks(&xml)
        .into_iter()
        .map(|block| {
            let (width, height) = image_dimensions(block).ok_or("invalid mip dimensions")?;
            let bytes = extract_blob(block)
                .map(|blob| hex_decode(&blob))
                .unwrap_or_default();
            if !bytes.is_empty() && bytes.len() != extract_blob_size(block) as usize {
                return Err("XML mip blob size does not match its declared size".into());
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

/// Извлекает размеры изображения из XML-блока.
pub fn image_dimensions(block: &str) -> Option<(u32, u32)> {
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

/// Извлекает тег из XML-блока.
pub fn extract_tag(block: &str, tag: &str, dtype: &str) -> Option<String> {
    let pattern = format!("<{} d:type=\"{}\">", tag, dtype);
    let start = block.find(&pattern)?;
    let rest = &block[start + pattern.len()..];
    let end = rest.find("</")?;
    Some(rest[..end].trim().to_string())
}

/// Извлекает формат из XML-блока.
pub fn extract_format(block: &str) -> String {
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

/// Извлекает hex-блоб из XML-блока.
pub fn extract_blob(block: &str) -> Option<String> {
    let marker = "<d:blob d:size=\"";
    let start = block.find(marker)?;
    let rest = &block[start..];
    let gt = rest.find('>')?;
    let content_start = start + gt + 1;
    let end_marker = "</d:blob>";
    let end = block[content_start..].find(end_marker)? + content_start;
    Some(block[content_start..end].to_string())
}

/// Извлекает размер blobа из атрибута d:size.
pub fn extract_blob_size(block: &str) -> u64 {
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

/// Считает количество cHcTexture в XML.
pub fn count_c_hc_textures(xml: &str) -> u32 {
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

/// Парсит булевый тег из XML-блока.
pub fn parse_bool_tag(block: &str, tag: &str) -> bool {
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

/// Извлекаетunnamed u16 перед blob.
pub fn extract_unnamed_u16(block: &str) -> u16 {
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

/// Собирает мипмапы из SERZ дерева.
pub fn collect_serz_image_mips(node: &SerzNode, mips: &mut Vec<ImageMip>) -> Result<(), String> {
    if let SerzNode::Element { name, children, .. } = node {
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
                        width = value.parse::<u32>().ok();
                    }
                    SerzNode::Property { name, value, .. } if name == "Height" => {
                        height = value.parse::<u32>().ok();
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
        }
        for child in children {
            collect_serz_image_mips(child, mips)?;
        }
    }
    Ok(())
}

/// Находит элемент по имени в SERZ дереве (mut).
pub fn find_element_mut<'a>(
    node: &'a mut SerzNode,
    name: &str,
) -> Option<&'a mut SerzNode> {
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
pub fn find_all_elements<'a>(
    node: &'a SerzNode,
    name: &str,
) -> Vec<&'a SerzNode> {
    let mut out = Vec::new();
    if let SerzNode::Element {
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

/// Устанавливает или вставляет property в SERZ элемент.
pub fn set_or_insert_property(
    elem: &mut SerzNode,
    name: &str,
    type_name: &str,
    value: &str,
) -> Result<(), String> {
    let SerzNode::Element { children, .. } = elem else {
        return Err(format!("cannot set {name} on a non-element node"));
    };
    if let Some(SerzNode::Property { value: current, .. }) = children.iter_mut().find(
        |child| matches!(child, SerzNode::Property { name: prop, .. } if prop == name),
    ) {
        *current = value.to_string();
    } else {
        children.push(SerzNode::Property {
            name: name.to_string(),
            type_name: type_name.to_string(),
            value: value.to_string(),
            alt_encoding: None,
        });
    }
    Ok(())
}

/// Устанавливает blob данные в SERZ элемент.
pub fn set_blob_data(elem: &mut SerzNode, data: &[u8]) {
    if let SerzNode::Element { children, .. } = elem {
        for child in children.iter_mut() {
            if let SerzNode::Blob { data: d } = child {
                *d = data.to_vec();
                return;
            }
        }
        children.push(SerzNode::Blob {
            data: data.to_vec(),
        });
    }
}

/// Находит размеры текстуры в SERZ дереве.
pub fn find_texture_dimensions(node: &SerzNode) -> Option<(u32, u32)> {
    if let SerzNode::Element { name, children, .. } = node {
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
                        "Width" => width = value.parse().ok(),
                        "Height" => height = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            return Some((width?, height?));
        }
        for child in children {
            if let Some(dimensions) = find_texture_dimensions(child) {
                return Some(dimensions);
            }
        }
    }
    None
}

/// Конвертирует SERZ байты в XML для метаданных (без blobов).
pub fn serz_bytes_to_metadata_xml(data: &[u8]) -> Result<String, String> {
    let mut reader = crate::serz::SerzReader::new(data);
    let mut root = reader.parse().map_err(|e| format!("serz parse: {e}"))?;

    fn clear_blobs(node: &mut SerzNode) {
        if let SerzNode::Element { children, .. } = node {
            for child in children {
                if let SerzNode::Blob { data } = child {
                    data.clear();
                } else {
                    clear_blobs(child);
                }
            }
        }
    }

    clear_blobs(&mut root);
    Ok(crate::serz::SerzReader::to_xml(&root, 0, true))
}
