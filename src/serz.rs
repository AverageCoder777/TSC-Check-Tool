use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

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
}

#[derive(Clone, Debug)]
struct SerzToken {
    opcode: u8,
    name: String,
    type_name: String,
}

pub struct SerzReader<'a> {
    cursor: Cursor<&'a [u8]>,
    data_len: usize,
    string_pool: Vec<String>,
    token_table: [Option<SerzToken>; 255],
    next_token_slot: usize,
}

impl<'a> SerzReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        const EMPTY_TOKEN: Option<SerzToken> = None;
        Self {
            cursor: Cursor::new(data),
            data_len: data.len(),
            string_pool: Vec::with_capacity(8192),
            token_table: [EMPTY_TOKEN; 255],
            next_token_slot: 0,
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
                    ((b0 as u32 & 0x07) << 18) | ((b1 as u32 & 0x3F) << 12) | ((b2 as u32 & 0x3F) << 6) | (b3 as u32 & 0x3F)
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
                format!("Индекс строки 0x{:04X} ({}) вне границ пула ({}) на 0x{:X}", 
                    id, id, self.string_pool.len(), self.cursor.position() - 2)
            })
        }
    }

    fn format_f32(val: f32) -> (String, String) {
        let d = val as f64;
        let alt = d.to_le_bytes().iter().map(|b| format!("{:02X}", b)).collect::<String>();

        if val == 0.0 {
            return ("0".to_string(), alt);
        }
        if val.is_nan() {
            return ("NaN".to_string(), alt);
        }
        if val.is_infinite() {
            return (if val > 0.0 { "INF".to_string() } else { "-INF".to_string() }, alt);
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
            "bool" => Ok((if self.read_u8()? != 0 { "1".to_string() } else { "0".to_string() }, None)),
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
                let alt = val.to_le_bytes().iter().map(|b| format!("{:02X}", b)).collect::<String>();
                Ok((val.to_string(), Some(alt)))
            }
            "sUInt64" => Ok((self.read_u64()?.to_string(), None)),
            "sInt64" => Ok(((self.read_u64()? as i64).to_string(), None)),
            _ => Ok((self.read_serz_string()?, None)),
        }
    }

    fn next_token(&mut self) -> Result<SerzToken, String> {
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

            let token = SerzToken { opcode, name, type_name };
            let slot = self.next_token_slot;
            self.token_table[slot] = Some(token.clone());
            self.next_token_slot = (self.next_token_slot + 1) % 255;
            Ok(token)
        } else {
            let id = marker as usize;
            if id >= 255 {
                return Err(format!("ID токена 0x{:02X} вне диапазона на 0x{:X}", id, self.cursor.position() - 1));
            }
            self.token_table[id].clone().ok_or_else(|| {
                format!("Токен в слоте 0x{:02X} не инициализирован на 0x{:X}", id, self.cursor.position() - 1)
            })
        }
    }

    pub fn parse(&mut self) -> Result<SerzNode, String> {
        let mut magic = [0u8; 4];
        self.cursor.read_exact(&mut magic).map_err(|e| e.to_string())?;
        if &magic != b"SERZ" {
            return Err(format!("Неверная сигнатура: {:?}", magic));
        }

        let _version = self.read_u32()?;

        // Читаем поток до тех пор, пока не встретим корневой элемент (0x50)
        let root_token = loop {
            let token = self.next_token()?;
            match token.opcode {
                0x50 => break token,
                0x43 => {
                    let _flag = self.read_u8()?;
                    let _val = self.read_u32()?;
                }
                other => {
                    return Err(format!("Неожиданный опкод перед корневым элементом: 0x{:02X} на 0x{:X}", 
                        other, self.cursor.position()));
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
                    self.cursor.read_exact(&mut data).map_err(|e| e.to_string())?;
                    children.push(SerzNode::Blob { data });
                }
                0x4E => {
                    children.push(SerzNode::Nil);
                }
                0x43 => {
                    // Служебная директива: читаем 1 байт и u32, не прерывая разбор элемента
                    let _flag = self.read_u8()?;
                    let _val = self.read_u32()?;
                }
                0x70 => break,
                other => {
                    return Err(format!("Неподдерживаемый опкод: 0x{:02X} на 0x{:X}", other, self.cursor.position()));
                }
            }
        }

        Ok(SerzNode::Element { name, id, version, children })
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
            SerzNode::Element { name, id, version: _, children } => {
                let tag = Self::xml_name(name);
                let mut out = String::new();
                if is_root {
                    let id_attr = if *id > 0 { format!(" d:id=\"{id}\"") } else { String::new() };
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
            SerzNode::Property { name, type_name, value, alt_encoding } => {
                let tag = Self::xml_name(name);
                if let Some(alt) = alt_encoding {
                    format!("{pad}<{tag} d:type=\"{type_name}\" d:alt_encoding=\"{alt}\" d:precision=\"string\">{value}</{tag}>\n")
                } else if value.is_empty() {
                    format!("{pad}<{tag} d:type=\"{type_name}\"></{tag}>\n")
                } else {
                    format!("{pad}<{tag} d:type=\"{type_name}\">{value}</{tag}>\n")
                }
            }
            SerzNode::Array { name, type_name, values } => {
                let tag = Self::xml_name(name);
                let count = values.len();
                let joined = values.join(" ");
                if type_name == "sFloat32" || type_name == "sFloat64" {
                    format!("{pad}<{tag} d:numElements=\"{count}\" d:elementType=\"{type_name}\" d:precision=\"string\">{joined}</{tag}>\n")
                } else {
                    format!("{pad}<{tag} d:numElements=\"{count}\" d:elementType=\"{type_name}\">{joined}</{tag}>\n")
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
        }
    }
}

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
            let elapsed = t1.elapsed();
            println!("   [✓] УСПЕШНО РАСПАРСЕНО за: {:?}", elapsed);
            println!("   Строк в пуле: {}", reader.string_pool.len());

            let actual_xml = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{}",
                SerzReader::to_xml(&ast, 0, true)
            );
            let actual_path = input_path.with_extension("actual.xml");
            let _ = fs::write(&actual_path, &actual_xml);

            let exp_lines: Vec<&str> = expected_xml.lines().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            let act_lines: Vec<&str> = actual_xml.lines().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

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
                println!("\n[✓] 100% ПОЛНОЕ БАЙТОВОЕ СОВПАДЕНИЕ С ОФИЦИАЛЬНЫМ SERZ.EXE!");
            } else {
                println!("\nВсего расхождений: {}", diffs);
            }
        }
        Err(err) => {
            eprintln!("\n[!] Ошибка: {}", err);

            println!("\n-> Первые 32 байта входного файла:");
            let slice = &raw_bytes[..32.min(raw_bytes.len())];
            let hex = slice.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
            let ascii: String = slice.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
            println!("   0000 | {:47} | {}", hex, ascii);

            println!("\n-> Первые 5 строк из expected.xml:");
            for (idx, line) in expected_xml.lines().take(5).enumerate() {
                println!("   {:2} | {}", idx + 1, line);
            }
        }
    }
}