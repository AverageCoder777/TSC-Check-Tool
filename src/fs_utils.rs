//! Shared filesystem/path helpers for RailWorks paths.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn clean_rw_path(input: &str) -> PathBuf {
    PathBuf::from(input.trim().trim_matches('"').trim_matches('\'').trim())
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

pub fn to_backslashes(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

pub fn path_eq_ignore_case(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

pub fn existing_directory_upwards(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.is_dir() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

pub fn sanitize_relative_member(member: &str) -> Option<PathBuf> {
    let clean = member.trim().trim_start_matches(|c| c == '/' || c == '\\');
    if clean.is_empty() {
        return None;
    }
    let path = PathBuf::from(clean.replace('/', "\\"));
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::Prefix(_)))
    {
        return None;
    }
    Some(path)
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
