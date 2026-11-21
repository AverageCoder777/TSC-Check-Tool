//! Binary SERZ reader and streaming XML serializer.

use std::fmt::Write as _;
use std::io::{Cursor, Read};

use super::{SerzNode, SerzPreamble, SerzTokenKey};

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

pub struct SerzReader<'a> {
    pub(super) cursor: Cursor<&'a [u8]>,
    pub(super) data_len: usize,
    pub(super) string_pool: Vec<String>,
    token_table: [Option<SerzTokenKey>; 255],
    next_token_slot: usize,
    pub preamble: Option<SerzPreamble>,
}

impl<'a> SerzReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(data),
            data_len: data.len(),
            string_pool: Vec::with_capacity(8192),
            token_table: std::array::from_fn(|_| None),
            next_token_slot: 0,
            preamble: None,
        }
    }

    pub(super) fn string_pool_len(&self) -> usize {
        self.string_pool.len()
    }

    pub(super) fn read_exact_buf(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let pos = self.cursor.position();
        self.cursor.read_exact(buf).map_err(|e| {
            format!(
                "Ошибка чтения на смещении 0x{pos:X} (байт {pos}): {e} (требовалось: {} байт, всего в файле: {})",
                buf.len(),
                self.data_len
            )
        })
    }

    pub(super) fn read_u8(&mut self) -> Result<u8, String> {
        let mut b = [0u8; 1];
        self.read_exact_buf(&mut b)?;
        Ok(b[0])
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        let mut b = [0u8; 2];
        self.read_exact_buf(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub(super) fn read_u32(&mut self) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.read_exact_buf(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.read_exact_buf(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub(super) fn read_f32(&mut self) -> Result<f32, String> {
        let mut b = [0u8; 4];
        self.read_exact_buf(&mut b)?;
        Ok(f32::from_le_bytes(b))
    }

    fn read_f64(&mut self) -> Result<f64, String> {
        let mut b = [0u8; 8];
        self.read_exact_buf(&mut b)?;
        Ok(f64::from_le_bytes(b))
    }

    fn read_serz_string(&mut self) -> Result<String, String> {
        let marker = self.read_u16()?;
        if marker == 0xFFFF {
            let char_count = self.read_u32()? as usize;
            let mut raw = Vec::with_capacity(char_count);

            for _ in 0..char_count {
                let b0 = self.read_u8()?;
                raw.push(b0);
                let extra = if b0 & 0x80 == 0 {
                    0
                } else if b0 & 0xE0 == 0xC0 {
                    1
                } else if b0 & 0xF0 == 0xE0 {
                    2
                } else if b0 & 0xF8 == 0xF0 {
                    3
                } else {
                    0
                };
                for _ in 0..extra {
                    raw.push(self.read_u8()?);
                }
            }

            let s = String::from_utf8(raw)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            self.string_pool.push(s.clone());
            Ok(s)
        } else {
            let id = marker as usize;
            self.string_pool.get(id).cloned().ok_or_else(|| {
                format!(
                    "Индекс строки 0x{id:04X} ({id}) вне границ пула ({}) на 0x{:X}",
                    self.string_pool.len(),
                    self.cursor.position() - 2
                )
            })
        }
    }

    fn encode_le_hex(bytes: &[u8; 8]) -> String {
        let mut out = String::with_capacity(16);
        for &b in bytes {
            out.push(HEX_UPPER[(b >> 4) as usize] as char);
            out.push(HEX_UPPER[(b & 0x0F) as usize] as char);
        }
        out
    }

    fn format_f32(val: f32) -> (String, String) {
        let alt = Self::encode_le_hex(&(val as f64).to_le_bytes());

        if val == 0.0 {
            return ("0".to_string(), alt);
        }
        if val.is_nan() {
            return ("NaN".to_string(), alt);
        }
        if val.is_infinite() {
            return (
                if val > 0.0 { "INF" } else { "-INF" }.to_string(),
                alt,
            );
        }

        let exp = val.abs().log10().floor() as i32;
        let text = if !(-4..6).contains(&exp) {
            let s = format!("{val:.5e}");
            if let Some((mantissa, exp_part)) = s.split_once('e') {
                let m_trimmed = mantissa.trim_end_matches('0').trim_end_matches('.');
                if let Ok(e_num) = exp_part.parse::<i32>() {
                    let sign = if e_num >= 0 { '+' } else { '-' };
                    format!("{m_trimmed}e{sign}{:03}", e_num.abs())
                } else {
                    s
                }
            } else {
                s
            }
        } else {
            let dec_places = (6 - (exp + 1)).max(0) as usize;
            let s = format!("{val:.dec_places$}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        };

        (text, alt)
    }

    pub(super) fn read_property_value(
        &mut self,
        type_name: &str,
    ) -> Result<(String, Option<String>), String> {
        match type_name {
            "cDeltaString" => Ok((self.read_serz_string()?, None)),
            "bool" => Ok((
                if self.read_u8()? != 0 { "1" } else { "0" }.to_string(),
                None,
            )),
            "sUInt32" => Ok((self.read_u32()?.to_string(), None)),
            "sInt32" => Ok(((self.read_u32()? as i32).to_string(), None)),
            "sUInt16" => Ok((self.read_u16()?.to_string(), None)),
            "sInt16" => Ok(((self.read_u16()? as i16).to_string(), None)),
            "sUInt8" => Ok((self.read_u8()?.to_string(), None)),
            "sInt8" => Ok(((self.read_u8()? as i8).to_string(), None)),
            "sFloat32" => {
                let (text, alt) = Self::format_f32(self.read_f32()?);
                Ok((text, Some(alt)))
            }
            "sFloat64" => {
                let val = self.read_f64()?;
                let alt = Self::encode_le_hex(&val.to_le_bytes());
                Ok((val.to_string(), Some(alt)))
            }
            "sUInt64" => Ok((self.read_u64()?.to_string(), None)),
            "sInt64" => Ok(((self.read_u64()? as i64).to_string(), None)),
            _ => Ok((self.read_serz_string()?, None)),
        }
    }

    pub(super) fn next_token(&mut self) -> Result<SerzTokenKey, String> {
        let marker = self.read_u8()?;
        if marker == 0xFF {
            let opcode = self.read_u8()?;
            let name = if !matches!(opcode, 0x42 | 0x4E | 0x43) {
                self.read_serz_string()?
            } else {
                String::new()
            };
            let type_name = if matches!(opcode, 0x56 | 0x41) {
                self.read_serz_string()?
            } else {
                String::new()
            };

            let token = SerzTokenKey {
                opcode,
                name,
                type_name,
            };
            let slot = self.next_token_slot;
            self.token_table[slot] = Some(token.clone());
            self.next_token_slot = (self.next_token_slot + 1) % 255;
            Ok(token)
        } else {
            let id = marker as usize;
            if id >= 255 {
                return Err(format!(
                    "ID токена 0x{id:02X} вне диапазона на 0x{:X}",
                    self.cursor.position() - 1
                ));
            }
            self.token_table[id].clone().ok_or_else(|| {
                format!(
                    "Токен в слоте 0x{id:02X} не инициализирован на 0x{:X}",
                    self.cursor.position() - 1
                )
            })
        }
    }

    pub fn parse(&mut self) -> Result<SerzNode, String> {
        let mut magic = [0u8; 4];
        self.read_exact_buf(&mut magic)?;
        if &magic != b"SERZ" {
            return Err(format!("Неверная сигнатура: {magic:?}"));
        }

        let _version = self.read_u32()?;

        let root_token = loop {
            let token = self.next_token()?;
            match token.opcode {
                0x50 => break token,
                0x43 => {
                    let flag = self.read_u8()?;
                    let val = self.read_u32()?;
                    self.preamble = Some(SerzPreamble { flag, val });
                }
                other => {
                    return Err(format!("Неожиданный опкод перед корнем: 0x{other:02X}"));
                }
            }
        };

        self.parse_element_body(root_token.name)
    }

    fn parse_element_body(&mut self, name: String) -> Result<SerzNode, String> {
        let id = self.read_u32()?;
        let version = self.read_u32()?;
        let mut children = Vec::new();

        while (self.cursor.position() as usize) < self.data_len {
            let token = self.next_token()?;
            match token.opcode {
                0x50 => children.push(self.parse_element_body(token.name)?),
                0x56 => {
                    let (value, alt_encoding) = self.read_property_value(&token.type_name)?;
                    children.push(SerzNode::Property {
                        name: token.name,
                        type_name: token.type_name,
                        value,
                        alt_encoding,
                    });
                }
                0x41 => {
                    let count = self.read_u8()? as usize;
                    let mut values = Vec::with_capacity(count);
                    for _ in 0..count {
                        if token.type_name == "sFloat32" {
                            values.push(format!("{:.7}", self.read_f32()?));
                        } else {
                            let (v, _) = self.read_property_value(&token.type_name)?;
                            values.push(v);
                        }
                    }
                    children.push(SerzNode::Array {
                        name: token.name,
                        type_name: token.type_name,
                        values,
                    });
                }
                0x52 => {
                    let target_id = self.read_u32()?;
                    children.push(SerzNode::Reference {
                        name: token.name,
                        target_id,
                    });
                }
                0x42 => {
                    let len = self.read_u32()? as usize;
                    let mut data = vec![0u8; len];
                    self.read_exact_buf(&mut data)?;
                    children.push(SerzNode::Blob { data });
                }
                0x4E => children.push(SerzNode::Nil),
                0x43 => {
                    let flag = self.read_u8()?;
                    let val = self.read_u32()?;
                    children.push(SerzNode::Directive { flag, val });
                }
                0x70 => break,
                other => {
                    return Err(format!(
                        "Неподдерживаемый опкод: 0x{other:02X} на 0x{:X}",
                        self.cursor.position()
                    ));
                }
            }
        }

        Ok(SerzNode::Element {
            name,
            id,
            version,
            children,
        })
    }

    pub fn to_xml(node: &SerzNode, indent: usize, is_root: bool) -> String {
        let mut out = String::with_capacity(4096);
        write_xml(node, indent, is_root, false, &mut out);
        out
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push('\t');
    }
}

fn push_xml_tag(out: &mut String, name: &str) {
    if name.is_empty() {
        out.push('e');
    } else if !name.contains("::") {
        out.push_str(name);
    } else {
        for (i, part) in name.split("::").enumerate() {
            if i > 0 {
                out.push('-');
            }
            out.push_str(part);
        }
    }
}

pub(super) fn write_xml(
    node: &SerzNode,
    indent: usize,
    is_root: bool,
    include_serz_ver: bool,
    out: &mut String,
) {
    match node {
        SerzNode::Element {
            name,
            id,
            version,
            children,
        } => {
            push_indent(out, indent);
            out.push('<');
            push_xml_tag(out, name);
            if is_root {
                out.push_str(" xmlns:d=\"http://www.kuju.com/TnT/2003/Delta\" d:version=\"1.0\"");
            }
            if *id > 0 {
                let _ = write!(out, " d:id=\"{id}\"");
            }
            if include_serz_ver && *version > 0 {
                let _ = write!(out, " d:serz_ver=\"{version}\"");
            }
            if !is_root && children.is_empty() {
                out.push_str("/>\n");
                return;
            }
            out.push_str(">\n");
            for child in children {
                write_xml(child, indent + 1, false, include_serz_ver, out);
            }
            push_indent(out, indent);
            out.push_str("</");
            push_xml_tag(out, name);
            out.push_str(">\n");
        }
        SerzNode::Property {
            name,
            type_name,
            value,
            alt_encoding,
        } => {
            push_indent(out, indent);
            out.push('<');
            push_xml_tag(out, name);
            let _ = write!(out, " d:type=\"{type_name}\"");
            if let Some(alt) = alt_encoding {
                let _ = write!(out, " d:alt_encoding=\"{alt}\" d:precision=\"string\"");
            }
            out.push('>');
            out.push_str(value);
            out.push_str("</");
            push_xml_tag(out, name);
            out.push_str(">\n");
        }
        SerzNode::Array {
            name,
            type_name,
            values,
        } => {
            push_indent(out, indent);
            out.push('<');
            push_xml_tag(out, name);
            let _ = write!(
                out,
                " d:numElements=\"{}\" d:elementType=\"{type_name}\"",
                values.len()
            );
            if type_name == "sFloat32" || type_name == "sFloat64" {
                out.push_str(" d:precision=\"string\"");
            }
            out.push('>');
            for (i, val) in values.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(val);
            }
            out.push_str("</");
            push_xml_tag(out, name);
            out.push_str(">\n");
        }
        SerzNode::Reference { name, target_id } => {
            push_indent(out, indent);
            out.push('<');
            push_xml_tag(out, name);
            let _ = write!(out, " d:type=\"ref\">{target_id}</");
            push_xml_tag(out, name);
            out.push_str(">\n");
        }
        SerzNode::Nil => {
            push_indent(out, indent);
            out.push_str("<d:nil/>\n");
        }
        SerzNode::Blob { data } => {
            push_indent(out, indent);
            let _ = write!(out, "<d:blob d:size=\"{}\">", data.len());
            out.reserve(data.len() * 2 + data.len() / 8 + 16);
            for (line_idx, chunk) in data.chunks(32).enumerate() {
                if line_idx > 0 {
                    out.push('\n');
                }
                for (group_idx, sub) in chunk.chunks(8).enumerate() {
                    if group_idx > 0 {
                        out.push(' ');
                    }
                    for &b in sub {
                        out.push(HEX_UPPER[(b >> 4) as usize] as char);
                        out.push(HEX_UPPER[(b & 0x0F) as usize] as char);
                    }
                }
            }
            out.push_str("</d:blob>\n");
        }
        SerzNode::Directive { .. } => {}
    }
}