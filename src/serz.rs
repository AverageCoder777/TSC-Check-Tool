use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, PartialEq, Clone)]
pub struct SerzPreamble {
    pub flag: u8,
    pub val: u32,
}

#[derive(Debug, PartialEq, Clone)]
pub enum SerzNode {
    Element {
        name: String,
        id: u32,
        version: u32,
        children: Vec<SerzNode>,
    },
    Property {
        name: String,
        type_name: String,
        value: String,
        alt_encoding: Option<String>,
    },
    Array {
        name: String,
        type_name: String,
        values: Vec<String>,
    },
    Reference {
        name: String,
        target_id: u32,
    },
    Blob {
        data: Vec<u8>,
    },
    Nil,
    Directive {
        flag: u8,
        val: u32,
    },
}

impl SerzNode {
    pub fn to_serz_bytes(&self, preamble: Option<&SerzPreamble>) -> Result<Vec<u8>, String> {
        encode_to_vec(self, preamble)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SerzTokenKey {
    opcode: u8,
    name: String,
    type_name: String,
}

// ============================================================================
// SERZ READER
// ============================================================================

pub struct SerzReader<'a> {
    cursor: Cursor<&'a [u8]>,
    data_len: usize,
    string_pool: Vec<String>,
    token_table: [Option<SerzTokenKey>; 255],
    next_token_slot: usize,
    pub preamble: Option<SerzPreamble>,
}

impl<'a> SerzReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        const EMPTY_TOKEN: Option<SerzTokenKey> = None;
        Self {
            cursor: Cursor::new(data),
            data_len: data.len(),
            string_pool: Vec::with_capacity(8192),
            token_table: [EMPTY_TOKEN; 255],
            next_token_slot: 0,
            preamble: None,
        }
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let mut b = [0u8; 1];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(b[0])
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        let mut b = [0u8; 2];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(u16::from_le_bytes(b))
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(u64::from_le_bytes(b))
    }

    fn read_f32(&mut self) -> Result<f32, String> {
        let mut b = [0u8; 4];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(f32::from_le_bytes(b))
    }

    fn read_f64(&mut self) -> Result<f64, String> {
        let mut b = [0u8; 8];
        self.cursor.read_exact(&mut b).map_err(|e| e.to_string())?;
        Ok(f64::from_le_bytes(b))
    }

    fn read_serz_string(&mut self) -> Result<String, String> {
        let marker = self.read_u16()?;
        if marker == 0xFFFF {
            let char_count = self.read_u32()? as usize;
            let mut chars = Vec::with_capacity(char_count);

            for _ in 0..char_count {
                let b0 = self.read_u8()?;
                let code_point = if b0 & 0x80 == 0 {
                    b0 as u32
                } else if b0 & 0xE0 == 0xC0 {
                    let b1 = self.read_u8()?;
                    ((b0 as u32 & 0x1F) << 6) | (b1 as u32 & 0x3F)
                } else if b0 & 0xF0 == 0xE0 {
                    let b1 = self.read_u8()?;
                    let b2 = self.read_u8()?;
                    ((b0 as u32 & 0x0F) << 12) | ((b1 as u32 & 0x3F) << 6) | (b2 as u32 & 0x3F)
                } else if b0 & 0xF8 == 0xF0 {
                    let b1 = self.read_u8()?;
                    let b2 = self.read_u8()?;
                    let b3 = self.read_u8()?;
                    ((b0 as u32 & 0x07) << 18)
                        | ((b1 as u32 & 0x3F) << 12)
                        | ((b2 as u32 & 0x3F) << 6)
                        | (b3 as u32 & 0x3F)
                } else {
                    b0 as u32
                };

                chars.push(char::from_u32(code_point).unwrap_or('?'));
            }

            let s: String = chars.into_iter().collect();
            self.string_pool.push(s.clone());
            Ok(s)
        } else {
            let id = marker as usize;
            self.string_pool.get(id).cloned().ok_or_else(|| {
                format!(
                    "Индекс строки 0x{:04X} ({}) вне границ пула ({}) на 0x{:X}",
                    id,
                    id,
                    self.string_pool.len(),
                    self.cursor.position() - 2
                )
            })
        }
    }

    fn format_f32(val: f32) -> (String, String) {
        let d = val as f64;
        let alt = d
            .to_le_bytes()
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<String>();

        if val == 0.0 {
            return ("0".to_string(), alt);
        }
        if val.is_nan() {
            return ("NaN".to_string(), alt);
        }
        if val.is_infinite() {
            return (
                if val > 0.0 {
                    "INF".to_string()
                } else {
                    "-INF".to_string()
                },
                alt,
            );
        }

        let abs_val = val.abs();
        let exp = abs_val.log10().floor() as i32;

        let text = if exp < -4 || exp >= 6 {
            let s = format!("{:.5e}", val);
            if let Some((mantissa, exp_part)) = s.split_once('e') {
                let m_trimmed = mantissa.trim_end_matches('0').trim_end_matches('.');
                if let Ok(e_num) = exp_part.parse::<i32>() {
                    let sign = if e_num >= 0 { "+" } else { "-" };
                    format!("{}e{}{:03}", m_trimmed, sign, e_num.abs())
                } else {
                    s
                }
            } else {
                s
            }
        } else {
            let dec_places = (6 - (exp + 1)).max(0) as usize;
            let s = format!("{:.1$}", val, dec_places);
            let s = s.trim_end_matches('0');
            s.trim_end_matches('.').to_string()
        };

        (text, alt)
    }

    fn read_property_value(&mut self, type_name: &str) -> Result<(String, Option<String>), String> {
        match type_name {
            "cDeltaString" => Ok((self.read_serz_string()?, None)),
            "bool" => Ok((
                if self.read_u8()? != 0 {
                    "1".to_string()
                } else {
                    "0".to_string()
                },
                None,
            )),
            "sUInt32" => Ok((self.read_u32()?.to_string(), None)),
            "sInt32" => Ok(((self.read_u32()? as i32).to_string(), None)),
            "sUInt16" => Ok((self.read_u16()?.to_string(), None)),
            "sInt16" => Ok(((self.read_u16()? as i16).to_string(), None)),
            "sUInt8" => Ok((self.read_u8()?.to_string(), None)),
            "sInt8" => Ok(((self.read_u8()? as i8).to_string(), None)),
            "sFloat32" => {
                let val = self.read_f32()?;
                let (text, alt) = Self::format_f32(val);
                Ok((text, Some(alt)))
            }
            "sFloat64" => {
                let val = self.read_f64()?;
                let alt = val
                    .to_le_bytes()
                    .iter()
                    .map(|b| format!("{:02X}", b))
                    .collect::<String>();
                Ok((val.to_string(), Some(alt)))
            }
            "sUInt64" => Ok((self.read_u64()?.to_string(), None)),
            "sInt64" => Ok(((self.read_u64()? as i64).to_string(), None)),
            _ => Ok((self.read_serz_string()?, None)),
        }
    }

    fn next_token(&mut self) -> Result<SerzTokenKey, String> {
        let marker = self.read_u8()?;
        if marker == 0xFF {
            let opcode = self.read_u8()?;
            let mut name = String::new();
            let mut type_name = String::new();

            if opcode != 0x42 && opcode != 0x4E && opcode != 0x43 {
                name = self.read_serz_string()?;
            }

            if opcode == 0x56 || opcode == 0x41 {
                type_name = self.read_serz_string()?;
            }

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
                    "ID токена 0x{:02X} вне диапазона на 0x{:X}",
                    id,
                    self.cursor.position() - 1
                ));
            }
            self.token_table[id].clone().ok_or_else(|| {
                format!(
                    "Токен в слоте 0x{:02X} не инициализирован на 0x{:X}",
                    id,
                    self.cursor.position() - 1
                )
            })
        }
    }

    pub fn parse(&mut self) -> Result<SerzNode, String> {
        let mut magic = [0u8; 4];
        self.cursor
            .read_exact(&mut magic)
            .map_err(|e| e.to_string())?;
        if &magic != b"SERZ" {
            return Err(format!("Неверная сигнатура: {:?}", magic));
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
                    return Err(format!("Неожиданный опкод перед корнем: 0x{:02X}", other));
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
                0x50 => {
                    children.push(self.parse_element_body(token.name)?);
                }
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
                            let f = self.read_f32()?;
                            values.push(format!("{:.7}", f));
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
                    self.cursor
                        .read_exact(&mut data)
                        .map_err(|e| e.to_string())?;
                    children.push(SerzNode::Blob { data });
                }
                0x4E => {
                    children.push(SerzNode::Nil);
                }
                0x43 => {
                    let flag = self.read_u8()?;
                    let val = self.read_u32()?;
                    children.push(SerzNode::Directive { flag, val });
                }
                0x70 => break,
                other => {
                    return Err(format!(
                        "Неподдерживаемый опкод: 0x{:02X} на 0x{:X}",
                        other,
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

    fn xml_name(name: &str) -> String {
        if name.is_empty() {
            "e".to_string()
        } else {
            name.replace("::", "-")
        }
    }

    pub fn to_xml(node: &SerzNode, indent: usize, is_root: bool) -> String {
        let pad = "\t".repeat(indent);
        match node {
            SerzNode::Element {
                name,
                id,
                version: _,
                children,
            } => {
                let tag = Self::xml_name(name);
                let mut out = String::new();
                if is_root {
                    let id_attr = if *id > 0 {
                        format!(" d:id=\"{id}\"")
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "{pad}<{tag} xmlns:d=\"http://www.kuju.com/TnT/2003/Delta\" d:version=\"1.0\"{id_attr}>\n"
                    ));
                    for c in children {
                        out.push_str(&Self::to_xml(c, indent + 1, false));
                    }
                    out.push_str(&format!("{pad}</{tag}>\n"));
                } else if children.is_empty() {
                    if *id > 0 {
                        out.push_str(&format!("{pad}<{tag} d:id=\"{id}\"/>\n"));
                    } else {
                        out.push_str(&format!("{pad}<{tag}/>\n"));
                    }
                } else {
                    if *id > 0 {
                        out.push_str(&format!("{pad}<{tag} d:id=\"{id}\">\n"));
                    } else {
                        out.push_str(&format!("{pad}<{tag}>\n"));
                    }

                    for c in children {
                        out.push_str(&Self::to_xml(c, indent + 1, false));
                    }
                    out.push_str(&format!("{pad}</{tag}>\n"));
                }
                out
            }
            SerzNode::Property {
                name,
                type_name,
                value,
                alt_encoding,
            } => {
                let tag = Self::xml_name(name);
                if let Some(alt) = alt_encoding {
                    format!(
                        "{pad}<{tag} d:type=\"{type_name}\" d:alt_encoding=\"{alt}\" d:precision=\"string\">{value}</{tag}>\n"
                    )
                } else if value.is_empty() {
                    format!("{pad}<{tag} d:type=\"{type_name}\"></{tag}>\n")
                } else {
                    format!("{pad}<{tag} d:type=\"{type_name}\">{value}</{tag}>\n")
                }
            }
            SerzNode::Array {
                name,
                type_name,
                values,
            } => {
                let tag = Self::xml_name(name);
                let count = values.len();
                let joined = values.join(" ");
                if type_name == "sFloat32" || type_name == "sFloat64" {
                    format!(
                        "{pad}<{tag} d:numElements=\"{count}\" d:elementType=\"{type_name}\" d:precision=\"string\">{joined}</{tag}>\n"
                    )
                } else {
                    format!(
                        "{pad}<{tag} d:numElements=\"{count}\" d:elementType=\"{type_name}\">{joined}</{tag}>\n"
                    )
                }
            }
            SerzNode::Reference { name, target_id } => {
                let tag = Self::xml_name(name);
                format!("{pad}<{tag} d:type=\"ref\">{target_id}</{tag}>\n")
            }
            SerzNode::Nil => {
                format!("{pad}<d:nil/>\n")
            }
            SerzNode::Blob { data } => {
                let mut out = format!("{pad}<d:blob d:size=\"{}\">", data.len());
                let mut first = true;
                for chunk in data.chunks(32) {
                    if first {
                        first = false;
                    } else {
                        out.push('\n');
                    }
                    let mut sub_chunks = Vec::with_capacity(4);
                    for sub in chunk.chunks(8) {
                        let hex: String = sub.iter().map(|b| format!("{:02X}", b)).collect();
                        sub_chunks.push(hex);
                    }
                    out.push_str(&sub_chunks.join(" "));
                }
                out.push_str("</d:blob>\n");
                out
            }
            SerzNode::Directive { .. } => String::new(), // Директивы не выводятся в XML
        }
    }
}

// ============================================================================
// SERZ WRITER
// ============================================================================

pub struct SerzWriter<W: Write> {
    writer: W,
    string_map: HashMap<String, u16>,
    token_slots: [Option<SerzTokenKey>; 255],
    token_map: HashMap<SerzTokenKey, u8>,
    next_token_slot: usize,
}

pub fn encode_to_vec(root: &SerzNode, preamble: Option<&SerzPreamble>) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut writer = SerzWriter::new(&mut buffer);
    writer.write_node(root, true, preamble)?;
    Ok(buffer)
}

impl<W: Write> SerzWriter<W> {
    pub fn new(writer: W) -> Self {
        const EMPTY_TOKEN: Option<SerzTokenKey> = None;
        Self {
            writer,
            string_map: HashMap::with_capacity(4096),
            token_slots: [EMPTY_TOKEN; 255],
            token_map: HashMap::with_capacity(256),
            next_token_slot: 0,
        }
    }

    fn write_string(&mut self, s: &str) -> Result<(), String> {
        if let Some(&id) = self.string_map.get(s) {
            self.writer
                .write_all(&id.to_le_bytes())
                .map_err(|e| e.to_string())?;
        } else {
            self.writer
                .write_all(&0xFFFFu16.to_le_bytes())
                .map_err(|e| e.to_string())?;
            let char_count = s.chars().count() as u32;
            self.writer
                .write_all(&char_count.to_le_bytes())
                .map_err(|e| e.to_string())?;
            self.writer
                .write_all(s.as_bytes())
                .map_err(|e| e.to_string())?;

            let new_id = self.string_map.len() as u16;
            self.string_map.insert(s.to_string(), new_id);
        }
        Ok(())
    }

    fn write_token(&mut self, opcode: u8, name: &str, type_name: &str) -> Result<(), String> {
        let key = SerzTokenKey {
            opcode,
            name: name.to_string(),
            type_name: type_name.to_string(),
        };

        if let Some(&slot) = self.token_map.get(&key) {
            self.writer.write_all(&[slot]).map_err(|e| e.to_string())?;
        } else {
            self.writer
                .write_all(&[0xFF, opcode])
                .map_err(|e| e.to_string())?;

            if opcode != 0x42 && opcode != 0x4E && opcode != 0x43 {
                self.write_string(name)?;
            }
            if opcode == 0x56 || opcode == 0x41 {
                self.write_string(type_name)?;
            }

            let slot = self.next_token_slot as u8;
            if let Some(old_key) = &self.token_slots[self.next_token_slot] {
                self.token_map.remove(old_key);
            }
            self.token_slots[self.next_token_slot] = Some(key.clone());
            self.token_map.insert(key, slot);
            self.next_token_slot = (self.next_token_slot + 1) % 255;
        }
        Ok(())
    }

    fn decode_hex16(s: &str) -> Option<[u8; 8]> {
        if s.len() != 16 {
            return None;
        }
        let mut res = [0u8; 8];
        for i in 0..8 {
            let byte_str = &s[i * 2..i * 2 + 2];
            res[i] = u8::from_str_radix(byte_str, 16).ok()?;
        }
        Some(res)
    }

    fn write_property_value(
        &mut self,
        type_name: &str,
        value: &str,
        alt_encoding: Option<&str>,
    ) -> Result<(), String> {
        match type_name {
            "cDeltaString" => self.write_string(value)?,
            "bool" => {
                let b = if value == "1" || value == "true" {
                    1u8
                } else {
                    0u8
                };
                self.writer.write_all(&[b]).map_err(|e| e.to_string())?;
            }
            "sUInt32" => {
                let v = value
                    .parse::<u32>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt32" => {
                let v = value
                    .parse::<i32>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt16" => {
                let v = value
                    .parse::<u16>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt16" => {
                let v = value
                    .parse::<i16>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt8" => {
                let v = value
                    .parse::<u8>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer.write_all(&[v]).map_err(|e| e.to_string())?;
            }
            "sInt8" => {
                let v = value
                    .parse::<i8>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&[v as u8])
                    .map_err(|e| e.to_string())?;
            }
            "sFloat32" => {
                if let Some(alt) = alt_encoding {
                    if let Some(bytes) = Self::decode_hex16(alt) {
                        let d = f64::from_le_bytes(bytes);
                        let f = d as f32;
                        self.writer
                            .write_all(&f.to_le_bytes())
                            .map_err(|e| e.to_string())?;
                        return Ok(());
                    }
                }
                let f = value
                    .parse::<f32>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&f.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sFloat64" => {
                if let Some(alt) = alt_encoding {
                    if let Some(bytes) = Self::decode_hex16(alt) {
                        self.writer.write_all(&bytes).map_err(|e| e.to_string())?;
                        return Ok(());
                    }
                }
                let d = value
                    .parse::<f64>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&d.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt64" => {
                let v = value
                    .parse::<u64>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt64" => {
                let v = value
                    .parse::<i64>()
                    .map_err(|e| format!("{}: {}", e, value))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            _ => self.write_string(value)?,
        }
        Ok(())
    }

    pub fn write_node(
        &mut self,
        node: &SerzNode,
        is_root: bool,
        preamble: Option<&SerzPreamble>,
    ) -> Result<(), String> {
        if is_root {
            self.writer.write_all(b"SERZ").map_err(|e| e.to_string())?;
            self.writer
                .write_all(&0x00010000u32.to_le_bytes())
                .map_err(|e| e.to_string())?;

            if let Some(p) = preamble {
                self.write_token(0x43, "", "")?;
                self.writer
                    .write_all(&[p.flag])
                    .map_err(|e| e.to_string())?;
                self.writer
                    .write_all(&p.val.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
        }

        match node {
            SerzNode::Element {
                name,
                id,
                version,
                children,
            } => {
                self.write_token(0x50, name, "")?;
                self.writer
                    .write_all(&id.to_le_bytes())
                    .map_err(|e| e.to_string())?;
                self.writer
                    .write_all(&version.to_le_bytes())
                    .map_err(|e| e.to_string())?;

                for child in children {
                    self.write_node(child, false, None)?;
                }

                self.write_token(0x70, name, "")?;
            }
            SerzNode::Property {
                name,
                type_name,
                value,
                alt_encoding,
            } => {
                self.write_token(0x56, name, type_name)?;
                self.write_property_value(type_name, value, alt_encoding.as_deref())?;
            }
            SerzNode::Array {
                name,
                type_name,
                values,
            } => {
                self.write_token(0x41, name, type_name)?;
                let count = values.len() as u8;
                self.writer.write_all(&[count]).map_err(|e| e.to_string())?;
                for val in values {
                    self.write_property_value(type_name, val, None)?;
                }
            }
            SerzNode::Reference { name, target_id } => {
                self.write_token(0x52, name, "")?;
                self.writer
                    .write_all(&target_id.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            SerzNode::Blob { data } => {
                self.write_token(0x42, "", "")?;
                let len = data.len() as u32;
                self.writer
                    .write_all(&len.to_le_bytes())
                    .map_err(|e| e.to_string())?;
                self.writer.write_all(data).map_err(|e| e.to_string())?;
            }
            SerzNode::Nil => {
                self.write_token(0x4E, "", "")?;
            }
            SerzNode::Directive { flag, val } => {
                self.write_token(0x43, "", "")?;
                self.writer.write_all(&[*flag]).map_err(|e| e.to_string())?;
                self.writer
                    .write_all(&val.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(())
    }
}

// ============================================================================
// ТЕСТЫ И СРАВНЕНИЯ
// ============================================================================

pub fn run_test(input_file: &str, serz_exe: &str) {
    let input_path = PathBuf::from(input_file);
    let serz_exe_path = PathBuf::from(serz_exe);

    println!("========================================================");
    println!(" ТЕСТИРОВАНИЕ SERZ: {}", input_path.display());
    println!("========================================================");

    let temp_xml = input_path.with_extension("expected.xml");
    let t0 = Instant::now();
    let status = Command::new(&serz_exe_path)
        .arg(&input_path)
        .arg(format!("/xml:{}", temp_xml.display()))
        .status();

    if let Ok(s) = status {
        if s.success() {
            println!("-> serz.exe отработал за: {:?}", t0.elapsed());
        }
    }

    let expected_xml = fs::read_to_string(&temp_xml).unwrap_or_default();
    let raw_bytes = match fs::read(&input_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Ошибка чтения: {:?}", e);
            return;
        }
    };

    println!("-> Запуск Rust SerzReader...");
    let t1 = Instant::now();
    let mut reader = SerzReader::new(&raw_bytes);
    match reader.parse() {
        Ok(ast) => {
            let elapsed_read = t1.elapsed();
            println!("   [✓] ДЕКОДИРОВАНО за: {:?}", elapsed_read);
            println!("   Строк в пуле: {}", reader.string_pool.len());

            let actual_xml = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{}",
                SerzReader::to_xml(&ast, 0, true)
            );
            let actual_path = input_path.with_extension("actual.xml");
            let _ = fs::write(&actual_path, &actual_xml);

            let exp_lines: Vec<&str> = expected_xml
                .lines()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let act_lines: Vec<&str> = actual_xml
                .lines()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            println!("\n-> Сравнение строк XML:");
            println!("   Строк в expected.xml: {}", exp_lines.len());
            println!("   Строк в actual.xml:   {}", act_lines.len());

            let mut diffs = 0;
            for i in 0..exp_lines.len().min(act_lines.len()) {
                if exp_lines[i] != act_lines[i] {
                    diffs += 1;
                    if diffs <= 5 {
                        println!("\n[!] Расхождение на строке {}:", i + 1);
                        println!("  serz.exe: {}", exp_lines[i]);
                        println!("  Rust:     {}", act_lines[i]);
                    }
                }
            }

            if diffs == 0 && exp_lines.len() == act_lines.len() {
                println!("[✓] 100% ПОЛНОЕ СОВПАДЕНИЕ XML С ОФИЦИАЛЬНЫМ SERZ.EXE!");
            } else {
                println!("Всего расхождений: {}", diffs);
            }

            println!("\n-> Тестирование SerzWriter (обратная упаковка в бинарник)...");
            let t2 = Instant::now();
            match encode_to_vec(&ast, reader.preamble.as_ref()) {
                Ok(reencoded_bin) => {
                    let elapsed_write = t2.elapsed();
                    println!("   [✓] СЕРИАЛИЗОВАНО за: {:?}", elapsed_write);
                    println!("   Размер оригинала:    {} байт", raw_bytes.len());
                    println!("   Размер ре-экспорта:  {} байт", reencoded_bin.len());

                    let ext = input_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("bin");
                    let reencoded_ext = format!("reencoded.{}", ext);
                    let reencoded_path = input_path.with_extension(reencoded_ext);
                    
                    let _ = fs::write(&reencoded_path, &reencoded_bin);

                    if raw_bytes == reencoded_bin {
                        println!(
                            "\n[★] БИТ В БИТ! 100% ПОБАЙТОВЫЙ ПАРИТЕТ С ОРИГИНАЛЬНЫМ БИНАРНИКОМ!"
                        );
                    } else {
                        let mut re_reader = SerzReader::new(&reencoded_bin);
                        match re_reader.parse() {
                            Ok(re_ast) => {
                                if ast == re_ast {
                                    println!(
                                        "\n[✓] ROUND-TRIP УСПЕШЕН: AST полностью идентично исходному файлу!"
                                    );
                                } else {
                                    println!("\n[!] AST после повторного чтения отличается!");
                                }
                            }
                            Err(e) => {
                                eprintln!("\n[!] Ошибка чтения собственного ре-экспорта: {}", e)
                            }
                        }
                    }
                }
                Err(err) => eprintln!("\n[!] Ошибка сериализации: {}", err),
            }
        }
        Err(err) => eprintln!("\n[!] Ошибка чтения: {}", err),
    }
}
