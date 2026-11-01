//! Shared filesystem/path helpers for RailWorks paths.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn clean_rw_path(input: &str) -> PathBuf {
    let cleaned = input.trim().trim_matches('"').trim_matches('\'').trim();
    strip_unc_prefix(&PathBuf::from(cleaned))
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(std::path::Component::Normal(_)) = out.last() {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    strip_unc_prefix(&out.iter().collect::<PathBuf>())
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
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\[[0-9a-zA-Z_\-]+\]").unwrap());
    if let Some(idx) = raw.rfind(['\\', '/']) {
        let (head, tail) = raw.split_at(idx + 1);
        format!("{head}{}", re.replace(tail, ""))
    } else {
        re.replace(raw, "").to_string()
    }
}
