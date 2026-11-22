//! Binary SERZ serializer, TgPcDx validation, and in-place blob patching.

use std::collections::HashMap;
use std::io::Write;

use super::{SerzNode, SerzPreamble, SerzReader, SerzTokenKey};

const TGPCDX_SERZ_PREFIX: &[u8; 10] = b"SERZ\0\0\x01\0\xFF\x43";
const MAX_STRING_POOL_SIZE: usize = 0xFFFF;

pub struct SerzWriter<W: Write> {
    writer: W,
    string_map: HashMap<String, u16>,
    token_slots: [Option<SerzTokenKey>; 255],
    token_map: HashMap<SerzTokenKey, u8>,
    next_token_slot: usize,
    pub(super) force_colon_sep: bool,
}

pub fn encode_to_vec(root: &SerzNode, preamble: Option<&SerzPreamble>) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut writer = SerzWriter::new(&mut buffer);
    writer.write_node(root, true, preamble)?;
    Ok(buffer)
}

/// Encodes a texture SERZ stream with its required leading directive and
/// verifies that the serialized blob payloads survive a binary round trip.
pub fn encode_tgpcdx_to_vec(
    root: &SerzNode,
    preamble: Option<&SerzPreamble>,
) -> Result<Vec<u8>, String> {
    let default_preamble = SerzPreamble { flag: 0, val: 0 };
    let bytes = encode_to_vec(root, Some(preamble.unwrap_or(&default_preamble)))?;
    let decoded = validate_tgpcdx_binary(&bytes, None)?;
    let mut expected_blobs = Vec::new();
    let mut actual_blobs = Vec::new();
    collect_blob_data(root, &mut expected_blobs);
    collect_blob_data(&decoded, &mut actual_blobs);
    if expected_blobs != actual_blobs {
        return Err("SERZ blob payloads changed during serialization".into());
    }
    Ok(bytes)
}

pub fn validate_tgpcdx_binary(
    bytes: &[u8],
    expected_blob_lengths: Option<&[usize]>,
) -> Result<SerzNode, String> {
    if !bytes.starts_with(TGPCDX_SERZ_PREFIX) {
        return Err("invalid TgPcDx SERZ prefix".into());
    }
    let mut reader = SerzReader::new(bytes);
    let root = reader
        .parse()
        .map_err(|e| format!("SERZ validation: {e}"))?;
    if let Some(expected_lengths) = expected_blob_lengths {
        let mut actual_blobs = Vec::new();
        collect_blob_data(&root, &mut actual_blobs);
        let actual_lengths: Vec<usize> = actual_blobs.iter().map(|blob| blob.len()).collect();
        if actual_lengths != expected_lengths {
            return Err("SERZ blob lengths do not match the mip data".into());
        }
    }
    Ok(root)
}

fn collect_blob_data<'a>(node: &'a SerzNode, blobs: &mut Vec<&'a [u8]>) {
    match node {
        SerzNode::Element { children, .. } => {
            for child in children {
                collect_blob_data(child, blobs);
            }
        }
        SerzNode::Blob { data } => blobs.push(data),
        _ => {}
    }
}

impl<W: Write> SerzWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            string_map: HashMap::with_capacity(4096),
            token_slots: std::array::from_fn(|_| None),
            token_map: HashMap::with_capacity(256),
            next_token_slot: 0,
            force_colon_sep: false,
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
            let char_count = u32::try_from(s.chars().count())
                .map_err(|_| "SERZ string exceeds u32 length limit".to_string())?;
            self.writer
                .write_all(&char_count.to_le_bytes())
                .map_err(|e| e.to_string())?;
            self.writer
                .write_all(s.as_bytes())
                .map_err(|e| e.to_string())?;

            if self.string_map.len() < MAX_STRING_POOL_SIZE {
                let new_id = self.string_map.len() as u16;
                self.string_map.insert(s.to_string(), new_id);
            }
        }
        Ok(())
    }

    fn write_token(&mut self, opcode: u8, name: &str, type_name: &str) -> Result<(), String> {
        let canonical_name = if name == "e" {
            String::new()
        } else if name.contains('-')
            && (self.force_colon_sep
                || name.starts_with("iBrowseableBlueprint-")
                || name.starts_with("Localisation-")
                || name.starts_with("iBlueprintLibrary-")
                || name.starts_with("cSceneryRenderBlueprint-")
                || name.starts_with("cControl-"))
        {
            name.replace('-', "::")
        } else {
            name.to_string()
        };
        let key = SerzTokenKey {
            opcode,
            name: canonical_name.clone(),
            type_name: type_name.to_string(),
        };

        if let Some(&slot) = self.token_map.get(&key) {
            self.writer.write_all(&[slot]).map_err(|e| e.to_string())?;
        } else {
            self.writer
                .write_all(&[0xFF, opcode])
                .map_err(|e| e.to_string())?;

            if !matches!(opcode, 0x42 | 0x4E | 0x43) {
                self.write_string(&canonical_name)?;
            }
            if matches!(opcode, 0x56 | 0x41) {
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
        let bytes = s.as_bytes();
        if bytes.len() != 16 {
            return None;
        }
        let mut res = [0u8; 8];
        let (pairs, _) = bytes.as_chunks::<2>();
        for (dst, &[hi, lo]) in res.iter_mut().zip(pairs) {
            let hi = (hi as char).to_digit(16)?;
            let lo = (lo as char).to_digit(16)?;
            *dst = ((hi << 4) | lo) as u8;
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
                let b = u8::from(value == "1" || value == "true");
                self.writer.write_all(&[b]).map_err(|e| e.to_string())?;
            }
            "sUInt32" => {
                let v = value.parse::<u32>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt32" => {
                let v = value.parse::<i32>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt16" => {
                let v = value.parse::<u16>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt16" => {
                let v = value.parse::<i16>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt8" => {
                let v = value.parse::<u8>().map_err(|e| format!("{e}: {value}"))?;
                self.writer.write_all(&[v]).map_err(|e| e.to_string())?;
            }
            "sInt8" => {
                let v = value.parse::<i8>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sFloat32" => {
                if let Some(alt) = alt_encoding
                    && let Some(bytes) = Self::decode_hex16(alt)
                {
                    let f = f64::from_le_bytes(bytes) as f32;
                    self.writer
                        .write_all(&f.to_le_bytes())
                        .map_err(|e| e.to_string())?;
                    return Ok(());
                }
                let f = value.parse::<f32>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&f.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sFloat64" => {
                if let Some(alt) = alt_encoding
                    && let Some(bytes) = Self::decode_hex16(alt)
                {
                    self.writer.write_all(&bytes).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                let d = value.parse::<f64>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&d.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sUInt64" => {
                let v = value.parse::<u64>().map_err(|e| format!("{e}: {value}"))?;
                self.writer
                    .write_all(&v.to_le_bytes())
                    .map_err(|e| e.to_string())?;
            }
            "sInt64" => {
                let v = value.parse::<i64>().map_err(|e| format!("{e}: {value}"))?;
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
                let count = u8::try_from(values.len())
                    .map_err(|_| format!("SERZ array '{name}' exceeds 255 elements"))?;
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
                let len = u32::try_from(data.len())
                    .map_err(|_| "SERZ blob exceeds the u32 length limit".to_string())?;
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

/// Точечно заменяет бинарные блобы (опкод 0x42) в оригинальном SERZ-файле,
/// сохраняя заголовок, преамбулу, пул строк и токены байт-в-байт.
pub fn patch_tgpcdx_blobs(
    orig_bytes: &[u8],
    new_mips: &[(u32, u32, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    if !orig_bytes.starts_with(b"SERZ") {
        return Err("Файл не является бинарным SERZ".into());
    }

    let mut reader = SerzReader::new(orig_bytes);
    let mut blob_spans: Vec<(usize, usize)> = Vec::new();

    let mut magic = [0u8; 4];
    reader.read_exact_buf(&mut magic)?;
    let _ver = reader.read_u32()?;

    loop {
        let tok = reader.next_token()?;
        match tok.opcode {
            0x50 => break,
            0x43 => {
                let _flag = reader.read_u8()?;
                let _val = reader.read_u32()?;
            }
            other => return Err(format!("Неожиданный опкод перед корнем: 0x{other:02X}")),
        }
    }

    fn scan_element(
        reader: &mut SerzReader<'_>,
        spans: &mut Vec<(usize, usize)>,
    ) -> Result<(), String> {
        let _id = reader.read_u32()?;
        let _ver = reader.read_u32()?;
        while (reader.cursor.position() as usize) < reader.data_len {
            let tok = reader.next_token()?;
            match tok.opcode {
                0x50 => scan_element(reader, spans)?,
                0x56 => {
                    let _ = reader.read_property_value(&tok.type_name)?;
                }
                0x41 => {
                    let count = reader.read_u8()? as usize;
                    for _ in 0..count {
                        if tok.type_name == "sFloat32" {
                            let _ = reader.read_f32()?;
                        } else {
                            let _ = reader.read_property_value(&tok.type_name)?;
                        }
                    }
                }
                0x52 => {
                    let _ = reader.read_u32()?;
                }
                0x42 => {
                    let len_pos = reader.cursor.position() as usize;
                    let len = reader.read_u32()? as usize;
                    let data_end = len_pos
                        .checked_add(4)
                        .and_then(|v| v.checked_add(len))
                        .ok_or("Переполнение смещения блоба")?;
                    if data_end > reader.data_len {
                        return Err("Блоб выходит за пределы файла".into());
                    }
                    reader.cursor.set_position(data_end as u64);
                    spans.push((len_pos, data_end));
                }
                0x4E => {}
                0x43 => {
                    let _ = reader.read_u8()?;
                    let _ = reader.read_u32()?;
                }
                0x70 => break,
                other => return Err(format!("Неподдерживаемый опкод: 0x{other:02X}")),
            }
        }
        Ok(())
    }

    scan_element(&mut reader, &mut blob_spans)?;

    if blob_spans.len() != new_mips.len() {
        return Err(format!(
            "Количество мипмапов не совпадает: в оригинале {}, в DDS {}",
            blob_spans.len(),
            new_mips.len()
        ));
    }

    let mut out = Vec::with_capacity(orig_bytes.len());
    let mut prev = 0usize;
    for ((len_pos, data_end), (_, _, new_data)) in blob_spans.iter().zip(new_mips.iter()) {
        out.extend_from_slice(&orig_bytes[prev..*len_pos]);
        let new_len = u32::try_from(new_data.len())
            .map_err(|_| "SERZ blob exceeds the u32 length limit".to_string())?;
        out.extend_from_slice(&new_len.to_le_bytes());
        out.extend_from_slice(new_data);
        prev = *data_end;
    }
    out.extend_from_slice(&orig_bytes[prev..]);
    Ok(out)
}
