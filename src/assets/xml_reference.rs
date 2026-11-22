use std::collections::HashSet;
use std::ops::Range;
use std::sync::OnceLock;

use regex::Regex;

use crate::assets::reference::capture_value_range;
use crate::common::translit::decode_xml_text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GeometryReference {
    pub path: String,
    pub variant: String,
    pub extension: Option<String>,
}

impl GeometryReference {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let (folder, name) = match text.rfind(['\\', '/']) {
            Some(index) => (&text[..index], &text[index + 1..]),
            None => ("", text),
        };
        let (variant, name) = match name.find(']') {
            Some(index) if name.starts_with('[') && index > 1 => {
                let digits = &name[1..index];
                if digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    (&name[..=index], &name[index + 1..])
                } else {
                    ("", name)
                }
            }
            _ => ("", name),
        };
        let (name, extension) = match name.rsplit_once('.') {
            Some((stem, ext))
                if !stem.is_empty()
                    && (ext.eq_ignore_ascii_case("GeoPcDx")
                        || ext.eq_ignore_ascii_case("TgPcDx")) =>
            {
                (stem, Some(ext.to_owned()))
            }
            _ => (name, None),
        };
        Some(Self {
            path: if folder.is_empty() {
                name.to_owned()
            } else {
                format!("{folder}\\{name}")
            },
            variant: variant.to_owned(),
            extension,
        })
    }

    pub fn is_geometry_reference(&self) -> bool {
        self.extension.is_some() || !self.variant.is_empty()
    }

    pub fn disk_extensions(&self) -> Vec<String> {
        match &self.extension {
            Some(extension) => vec![extension.clone()],
            None => vec!["GeoPcDx".to_owned(), "TgPcDx".to_owned()],
        }
    }

    pub fn to_text(&self) -> String {
        let ext_len = self.extension.as_ref().map_or(0, |ext| ext.len() + 1);
        let mut text = String::with_capacity(self.path.len() + self.variant.len() + ext_len);
        let (folder, name) = match self.path.rfind(['\\', '/']) {
            Some(index) => (&self.path[..=index], &self.path[index + 1..]),
            None => ("", self.path.as_str()),
        };
        text.push_str(folder);
        text.push_str(&self.variant);
        text.push_str(name);
        if let Some(extension) = &self.extension {
            text.push('.');
            text.push_str(extension);
        }
        text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceBase {
    Assets,
    Product,
    Folder,
}

pub fn geometry_reference_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)>([^<>]*\.(?:GeoPcDx|TgPcDx))\s*</[^>]+>"#)
            .expect("Шаблон поиска ссылок на геометрию должен быть корректным")
    })
}

pub fn variant_reference_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)>([^<>]*\[[0-9]{2,3}\][^<>]*)\s*</[^>]+>"#)
            .expect("Шаблон поиска ссылок с вариантом должен быть корректным")
    })
}

pub fn referenced_geometry_paths(xml: &str) -> Vec<(GeometryReference, Range<usize>)> {
    let mut found: Vec<(GeometryReference, Range<usize>)> = Vec::new();
    let mut seen_ranges: HashSet<Range<usize>> = HashSet::new();

    for regex in [geometry_reference_regex(), variant_reference_regex()] {
        for captures in regex.captures_iter(xml) {
            let Some(matched) = captures.get(1) else {
                continue;
            };
            let text = decode_xml_text(matched.as_str().trim());
            let Some(reference) = GeometryReference::parse(&text) else {
                continue;
            };
            if !reference.is_geometry_reference() {
                continue;
            }
            let range = capture_value_range(&matched);
            if seen_ranges.insert(range.clone()) {
                found.push((reference, range));
            }
        }
    }

    found.sort_by_key(|(_, range)| range.start);
    found
}

pub fn escape_xml_text(text: &str) -> String {
    if !text.contains(['&', '<', '>']) {
        return text.to_owned();
    }
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn validate_xml_document(xml: &str) -> Result<(), String> {
    for event in xml::reader::EventReader::new(xml.as_bytes()) {
        event.map_err(|error| format!("Некорректный XML: {error}"))?;
    }
    Ok(())
}

pub fn first_blueprint_tag(xml: &str) -> Option<String> {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let re = REGEX
        .get_or_init(|| Regex::new(r"<(c[A-Za-z0-9]+Blueprint)").expect("regex must be valid"));
    re.captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}
