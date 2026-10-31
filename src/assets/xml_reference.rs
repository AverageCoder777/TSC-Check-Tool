use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use walkdir::WalkDir;

/// Ссылка на объект ассета внутри XML RailWorks.
///
/// В игре такая ссылка выглядит как `Provider\Product\папка\[00]имя`: без
/// расширения и с суффиксом варианта. Иногда расширение в ссылке всё же есть,
/// поэтому разбираются оба вида.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GeometryReference {
    /// Путь без суффикса варианта и без расширения.
    pub path: String,
    /// Суффикс варианта, например `[00]`.
    pub variant: String,
    /// Расширение из ссылки, если оно там было.
    pub extension: Option<String>,
}

impl GeometryReference {
    /// Разбирает текст ссылки из XML.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        // Ссылка состоит из папки, суффикса варианта, имени файла и иногда
        // расширения: `Provider\Product\папка\[00]имя.GeoPcDx`.
        let (folder, name) = match text.rfind(['\\', '/']) {
            Some(index) => (&text[..index], &text[index + 1..]),
            None => ("", text),
        };
        // Суффикс варианта стоит в начале имени файла: [00], [01] и так далее.
        let (variant, name) = match name.find(']') {
            Some(index) if name.starts_with('[') && index > 1 => {
                let digits = &name[1..index];
                if !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_digit()) {
                    (&name[..=index], &name[index + 1..])
                } else {
                    ("", name)
                }
            }
            _ => ("", name),
        };
        // Расширение тоже может быть указано явно.
        let (name, extension) = match Path::new(name).extension() {
            Some(extension) => {
                let extension = extension.to_string_lossy().into_owned();
                if extension.eq_ignore_ascii_case("GeoPcDx")
                    || extension.eq_ignore_ascii_case("TgPcDx")
                {
                    (&name[..name.len() - extension.len() - 1], Some(extension))
                } else {
                    (name, None)
                }
            }
            None => (name, None),
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

    /// Указывает ли ссылка на объект ассета.
    pub fn is_geometry_reference(&self) -> bool {
        self.extension.is_some() || !self.variant.is_empty()
    }

    /// Расширение, с которым файл лежит на диске.
    ///
    /// Если расширения в ссылке не было, файл может оказаться и геометрией, и
    /// текстурой: проверяются оба варианта.
    pub fn disk_extensions(&self) -> Vec<String> {
        match &self.extension {
            Some(extension) => vec![extension.clone()],
            None => vec!["GeoPcDx".to_owned(), "TgPcDx".to_owned()],
        }
    }

    /// Текст ссылки в исходном виде.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
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

/// Относительно чего задан путь в ссылке на объект.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceBase {
    /// Относительно папки Assets.
    Assets,
    /// Относительно папки продукта.
    Product,
    /// Относительно папки файла, в котором встретилась ссылка.
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

/// Находит в XML ссылки на объекты ассетов.
///
/// Учитываются оба вида ссылок: с расширением (`.GeoPcDx`) и без него, но с
/// суффиксом варианта (`[00]`) — именно так их пишет игра.
pub fn referenced_geometry_paths(xml: &str) -> Vec<(GeometryReference, std::ops::Range<usize>)> {
    let mut found: Vec<(GeometryReference, std::ops::Range<usize>)> = Vec::new();
    for regex in [geometry_reference_regex(), variant_reference_regex()] {
        for captures in regex.captures_iter(xml) {
            let Some(matched) = captures.get(1) else {
                continue;
            };
            let text = crate::translit::decode_xml_text(matched.as_str().trim());
            let Some(reference) = GeometryReference::parse(&text) else {
                continue;
            };
            if !reference.is_geometry_reference() {
                continue;
            }
            let range = crate::reference::capture_value_range(&matched);
            if found
                .iter()
                .any(|(_, existing_range)| existing_range == &range)
            {
                continue;
            }
            found.push((reference, range));
        }
    }
    found.sort_by_key(|(_, range)| range.start);
    found
}

pub fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

