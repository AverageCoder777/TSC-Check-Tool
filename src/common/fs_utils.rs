//! Shared filesystem/path helpers for RailWorks paths.

use std::path::{Component, Path, PathBuf};

pub fn clean_rw_path(input: &str) -> PathBuf {
    let cleaned = input.trim().trim_matches('"').trim_matches('\'').trim();
    strip_unc_prefix(Path::new(cleaned))
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let base = match text.strip_prefix(r"\\?\") {
        Some(rest) => Path::new(rest),
        None => path,
    };

    let mut out = Vec::new();
    for comp in base.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = out.last() {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    out.iter().collect()
}

pub fn strip_unc_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

pub fn strip_shader_prefix(raw: &str) -> String {
    if let Some(idx) = raw.rfind(['\\', '/']) {
        let (head, tail) = raw.split_at(idx + 1);
        let stripped = strip_leading_shader_tag(tail);
        if stripped.len() == tail.len() {
            raw.to_owned()
        } else {
            format!("{head}{stripped}")
        }
    } else {
        strip_leading_shader_tag(raw).to_owned()
    }
}

pub fn to_assets_relative_display(path: &Path) -> String {
    let s = path.to_string_lossy();
    let bytes = s.as_bytes();

    let asset_pos = if bytes.len() >= 7
        && bytes[..6].eq_ignore_ascii_case(b"assets")
        && is_path_sep(bytes[6])
    {
        Some(0usize)
    } else {
        bytes
            .windows(8)
            .position(|w| {
                is_path_sep(w[0]) && w[1..7].eq_ignore_ascii_case(b"assets") && is_path_sep(w[7])
            })
            .map(|i| i + 1)
    };

    match asset_pos {
        Some(p) => {
            let tail = &s[p..];
            if tail.contains('/') {
                tail.replace('/', "\\")
            } else {
                tail.to_owned()
            }
        }
        None => s.into_owned(),
    }
}

fn is_path_sep(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

fn strip_leading_shader_tag(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'[') {
        return s;
    }
    let mut i = 1;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
    {
        i += 1;
    }
    if i > 1 && bytes.get(i) == Some(&b']') {
        &s[i + 1..]
    } else {
        s
    }
}
