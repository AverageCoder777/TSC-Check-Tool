//! XML <-> SERZ AST conversion and Tracks.bin metadata-preserving round-trip.

use std::fmt::Write as _;

use xml::attribute::OwnedAttribute;
use xml::reader::{EventReader, XmlEvent};

use super::reader::write_xml;
use super::{SerzNode, SerzPreamble, SerzReader, SerzWriter};

impl SerzNode {
    pub fn from_xml_str(xml: &str) -> Result<(Self, Option<SerzPreamble>), String> {
        let root = parse_xml_root(xml, false)?;
        Ok((root, None))
    }
}

/// Экспорт Tracks.bin в XML с сохранением метаданных (преамбулы, разделителей и d:serz_ver).
pub fn tracks_bin_to_xml(raw_bytes: &[u8]) -> Result<String, String> {
    let mut reader = SerzReader::new(raw_bytes);
    let ast = reader.parse()?;

    let sep_str = if reader.string_pool.iter().any(|s| s.contains("::")) {
        "colon"
    } else {
        "dash"
    };

    let mut out = String::with_capacity(raw_bytes.len().saturating_mul(2).max(4096));
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- serz-meta: preamble=");
    match &reader.preamble {
        Some(p) => {
            let _ = write!(out, "{:02X}{:08X}", p.flag, p.val);
        }
        None => out.push_str("none"),
    }
    let _ = writeln!(out, "; sep={sep_str} -->");
    write_xml(&ast, 0, true, true, &mut out);
    Ok(out)
}

/// Обратная компиляция XML от Tracks.bin в бинарный формат.
pub fn compile_tracks_xml_to_bin(xml: &str) -> Result<Vec<u8>, String> {
    let mut preamble: Option<SerzPreamble> = None;
    let mut use_colon_sep = true;

    if let Some(meta_start) = xml.find("serz-meta:")
        && let Some(meta_end) = xml[meta_start..].find("-->")
    {
        let meta = &xml[meta_start + "serz-meta:".len()..meta_start + meta_end];
        for part in meta.split(';') {
            let part = part.trim();
            if let Some(p_hex) = part.strip_prefix("preamble=")
                && p_hex != "none"
                && p_hex.len() == 10
                && let (Ok(flag), Ok(val)) = (
                    u8::from_str_radix(&p_hex[0..2], 16),
                    u32::from_str_radix(&p_hex[2..10], 16),
                )
            {
                preamble = Some(SerzPreamble { flag, val });
            } else if let Some(sep) = part.strip_prefix("sep=") {
                use_colon_sep = sep.trim() == "colon";
            }
        }
    }

    let root = parse_xml_root(xml, true)?;
    let mut buffer = Vec::with_capacity(512 * 1024);
    let mut writer = SerzWriter::new(&mut buffer);
    writer.force_colon_sep = use_colon_sep;
    writer.write_node(&root, true, preamble.as_ref())?;
    Ok(buffer)
}

fn parse_xml_root(xml: &str, tracks_mode: bool) -> Result<SerzNode, String> {
    let parser = EventReader::new(xml.as_bytes());
    let mut iter = parser.into_iter();
    loop {
        match iter.next() {
            None => return Err("no root element".to_string()),
            Some(Err(e)) => return Err(e.to_string()),
            Some(Ok(XmlEvent::StartElement {
                name, attributes, ..
            })) => {
                return parse_element(&mut iter, name.local_name, attributes, tracks_mode);
            }
            Some(Ok(_)) => continue,
        }
    }
}

fn parse_next<I>(iter: &mut I, tracks_mode: bool) -> Result<Option<SerzNode>, String>
where
    I: Iterator<Item = Result<XmlEvent, xml::reader::Error>>,
{
    loop {
        match iter.next() {
            None => return Ok(None),
            Some(Err(e)) => return Err(e.to_string()),
            Some(Ok(XmlEvent::StartElement {
                name, attributes, ..
            })) => {
                return parse_element(iter, name.local_name, attributes, tracks_mode).map(Some);
            }
            Some(Ok(XmlEvent::EndElement { .. })) => return Ok(None),
            Some(Ok(_)) => continue,
        }
    }
}

fn read_element_text<I>(iter: &mut I) -> Result<String, String>
where
    I: Iterator<Item = Result<XmlEvent, xml::reader::Error>>,
{
    let mut text = String::new();
    for event in iter.by_ref() {
        match event.map_err(|e| e.to_string())? {
            XmlEvent::Characters(s) | XmlEvent::Whitespace(s) => text.push_str(&s),
            XmlEvent::EndElement { .. } => break,
            _ => {}
        }
    }
    Ok(text)
}

fn decode_hex_lenient(s: &str) -> Vec<u8> {
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

fn decode_hex_strict(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut hi: Option<u8> = None;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let digit = ch
            .to_digit(16)
            .ok_or_else(|| "SERZ blob contains invalid hexadecimal data".to_string())?
            as u8;
        if let Some(h) = hi.take() {
            out.push((h << 4) | digit);
        } else {
            hi = Some(digit);
        }
    }
    if hi.is_some() {
        return Err("SERZ blob contains an incomplete hexadecimal byte".into());
    }
    Ok(out)
}

fn parse_element<I>(
    iter: &mut I,
    raw_name: String,
    attrs: Vec<OwnedAttribute>,
    tracks_mode: bool,
) -> Result<SerzNode, String>
where
    I: Iterator<Item = Result<XmlEvent, xml::reader::Error>>,
{
    let name = if tracks_mode && raw_name == "e" {
        String::new()
    } else {
        raw_name
    };

    let mut id = 0u32;
    let mut version = if tracks_mode { 0u32 } else { 1u32 };
    let mut type_name: Option<String> = None;
    let mut alt_encoding: Option<String> = None;
    let mut num_elements: Option<usize> = None;
    let mut element_type: Option<String> = None;
    let mut blob_size: Option<usize> = None;

    for attr in attrs {
        match attr.name.local_name.as_str() {
            "id" => id = attr.value.parse().unwrap_or(0),
            "serz_ver" if tracks_mode => version = attr.value.parse().unwrap_or(0),
            "type" => type_name = Some(attr.value),
            "alt_encoding" => alt_encoding = Some(attr.value),
            "numElements" => num_elements = attr.value.parse().ok(),
            "elementType" => element_type = Some(attr.value),
            "size" if tracks_mode => {
                blob_size = Some(
                    attr.value
                        .parse()
                        .map_err(|_| format!("Invalid SERZ blob size: {}", attr.value))?,
                );
            }
            _ => {}
        }
    }

    if name == "blob" {
        let text = read_element_text(iter)?;
        let data = if tracks_mode {
            let decoded = decode_hex_strict(&text)?;
            if let Some(size) = blob_size
                && decoded.len() != size
            {
                return Err(format!(
                    "SERZ blob size is {size}, but XML contains {} bytes",
                    decoded.len()
                ));
            }
            decoded
        } else {
            decode_hex_lenient(&text)
        };
        return Ok(SerzNode::Blob { data });
    }

    if name == "nil" {
        for event in iter.by_ref() {
            if let XmlEvent::EndElement { .. } = event.map_err(|e| e.to_string())? {
                break;
            }
        }
        return Ok(SerzNode::Nil);
    }

    if num_elements.is_some() {
        let text = read_element_text(iter)?;
        return Ok(SerzNode::Array {
            name,
            type_name: element_type.unwrap_or_default(),
            values: text.split_whitespace().map(str::to_string).collect(),
        });
    }

    if let Some(t) = type_name {
        let text = read_element_text(iter)?;
        if t == "ref" {
            return Ok(SerzNode::Reference {
                name,
                target_id: text.trim().parse().unwrap_or(0),
            });
        }
        return Ok(SerzNode::Property {
            name,
            type_name: t,
            value: text.trim().to_string(),
            alt_encoding,
        });
    }

    let mut children = Vec::new();
    while let Some(child) = parse_next(iter, tracks_mode)? {
        children.push(child);
    }
    Ok(SerzNode::Element {
        name,
        id,
        version,
        children,
    })
}