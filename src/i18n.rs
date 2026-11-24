use gettext::Catalog;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Language {
    En = 0,
    Ru = 1,
    De = 2,
}

impl Language {
    pub fn from_code(code: &str) -> Self {
        let short = code.split(['-', '_', '.']).next().unwrap_or("en");
        match short.to_ascii_lowercase().as_str() {
            "ru" => Self::Ru,
            "de" => Self::De,
            _ => Self::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "", // пустая строка сбрасывает Slint на исходный язык
            Self::Ru => "ru",
            Self::De => "de",
        }
    }
}

static CURRENT_LANG: AtomicUsize = AtomicUsize::new(Language::En as usize);
static RU_CATALOG: OnceLock<Catalog> = OnceLock::new();
static DE_CATALOG: OnceLock<Catalog> = OnceLock::new();

fn catalog_for(lang: usize) -> Option<&'static Catalog> {
    match lang {
        1 => Some(RU_CATALOG.get_or_init(|| {
            let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/ru.mo"));
            Catalog::parse(&bytes[..]).expect("valid bundled ru.mo")
        })),
        2 => Some(DE_CATALOG.get_or_init(|| {
            let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/de.mo"));
            Catalog::parse(&bytes[..]).expect("valid bundled de.mo")
        })),
        _ => None,
    }
}

/// Переключает язык одновременно для Rust и Slint.
pub fn set_language(lang: Language) {
    CURRENT_LANG.store(lang as usize, Ordering::Relaxed);
    let _ = slint::select_bundled_translation(lang.code());
}

pub fn tr(msg: &str) -> String {
    match catalog_for(CURRENT_LANG.load(Ordering::Relaxed)) {
        Some(cat) => cat.gettext(msg).to_owned(),
        None => msg.to_owned(),
    }
}

pub fn ntr(singular: &str, plural: &str, n: u64) -> String {
    match catalog_for(CURRENT_LANG.load(Ordering::Relaxed)) {
        Some(cat) => cat.ngettext(singular, plural, n).to_owned(),
        None => {
            if n == 1 {
                singular.to_owned()
            } else {
                plural.to_owned()
            }
        }
    }
}

pub fn tr_fmt(msg: &str, args: &[&dyn Display]) -> String {
    format_runtime(&tr(msg), args)
}

pub fn ntr_fmt(singular: &str, plural: &str, n: u64, args: &[&dyn Display]) -> String {
    format_runtime(&ntr(singular, plural, n), args)
}

fn format_runtime(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut rest = template;
    let mut next_idx = 0usize;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];

        if let Some(after) = rest.strip_prefix('{') {
            out.push('{');
            rest = after;
            continue;
        }

        let Some(close) = rest.find('}') else {
            out.push('{');
            break;
        };

        let key = &rest[..close];
        rest = &rest[close + 1..];

        let idx = if key.is_empty() {
            let i = next_idx;
            next_idx += 1;
            Some(i)
        } else {
            key.parse::<usize>().ok()
        };

        if let Some(val) = idx.and_then(|i| args.get(i)) {
            use std::fmt::Write;
            let _ = write!(out, "{val}");
        } else {
            out.push('{');
            out.push_str(key);
            out.push('}');
        }
    }

    out.push_str(rest);
    out
}
