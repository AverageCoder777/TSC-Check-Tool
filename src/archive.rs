//! Unified archive (.ap / ZIP) indexing and streaming extraction.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zip::ZipArchive;

/// Auditor-compatible product-level archive index (flattened member set).
#[derive(Default)]
pub struct ApArchiveCache {
    pub inner: Mutex<HashMap<PathBuf, HashSet<String>>>,
}

impl ApArchiveCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ensure_indexed(&self, product_dir: &Path) {
        let norm_dir = crate::fs_utils::normalize_path(product_dir);
        {
            let Ok(guard) = self.inner.lock() else {
                return;
            };
            if guard.contains_key(&norm_dir) {
                return;
            }
        }

        let mut files = HashSet::new();
        if let Ok(entries) = fs::read_dir(product_dir) {
            for entry in entries.flatten() {
                let archive_path = entry.path();
                if !archive_path.is_file() {
                    continue;
                }
                if !archive_path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
                {
                    continue;
                }
                let Ok(file) = File::open(&archive_path) else {
                    continue;
                };
                let Ok(mut archive) = ZipArchive::new(file) else {
                    continue;
                };
                for i in 0..archive.len() {
                    let Ok(entry) = archive.by_index(i) else {
                        continue;
                    };
                    if entry.is_dir() {
                        continue;
                    }
                    let Ok(raw_name) = entry.name() else {
                        continue;
                    };
                    let name = raw_name.replace('/', "\\").trim().to_lowercase();
                    if name.is_empty() {
                        continue;
                    }
                    files.insert(name.clone());
                    files.insert(crate::fs_utils::strip_shader_prefix(&name));
                    let parts: Vec<&str> =
                        name.split('\\').filter(|part| !part.is_empty()).collect();
                    if parts.len() >= 3 {
                        let p2 = parts[2..].join("\\");
                        let p1 = parts[1..].join("\\");
                        files.insert(p2.clone());
                        files.insert(crate::fs_utils::strip_shader_prefix(&p2));
                        files.insert(p1.clone());
                        files.insert(crate::fs_utils::strip_shader_prefix(&p1));
                    } else if parts.len() == 2 {
                        let p1 = parts[1..].join("\\");
                        files.insert(p1.clone());
                        files.insert(crate::fs_utils::strip_shader_prefix(&p1));
                    }
                }
            }
        }

        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(norm_dir, files);
        }
    }

    pub fn lookup(&self, product_dir: &Path, relative: &str) -> bool {
        self.ensure_indexed(product_dir);
        let Ok(guard) = self.inner.lock() else {
            return false;
        };
        let norm_dir = crate::fs_utils::normalize_path(product_dir);
        let Some(set) = guard.get(&norm_dir) else {
            return false;
        };
        let target = normalize_member(relative);
        let target_stripped = normalize_member(&crate::fs_utils::strip_shader_prefix(relative));
        set.iter().any(|v| v.eq_ignore_ascii_case(&target))
            || set.iter().any(|v| v.eq_ignore_ascii_case(&target_stripped))
            || set.iter().any(|v| v.eq_ignore_ascii_case(target.trim_start_matches('\\')))
            || set
                .iter()
                .any(|v| v.eq_ignore_ascii_case(target_stripped.trim_start_matches('\\')))
    }
}

pub fn extract_archive_member(archive_path: &Path, member: &str, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Не удалось создать папку {}: {e}", parent.display()))?;
    }
    let file = File::open(archive_path)
        .map_err(|e| format!("Не удалось открыть архив {}: {e}", archive_path.display()))?;
    let mut zip = ZipArchive::new(file)
        .map_err(|e| format!("Не удалось прочитать архив {}: {e}", archive_path.display()))?;

    let target = normalize_member(member);
    let mut idx = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let Ok(raw_name) = entry.name() else {
            continue;
        };
        if normalize_member(raw_name.as_ref()) == target {
            idx = Some(i);
            break;
        }
    }
    let idx = idx.ok_or_else(|| format!("Не удалось найти {member} в архиве {}", archive_path.display()))?;

    let mut entry = zip
        .by_index(idx)
        .map_err(|e| format!("Не удалось прочитать {member} из архива: {e}"))?;
    let mut output = BufWriter::new(
        File::create(dest).map_err(|e| format!("Не удалось создать файл {}: {e}", dest.display()))?,
    );
    std::io::copy(&mut entry, &mut output)
        .map_err(|e| format!("Не удалось распаковать {member}: {e}"))?;
    output
        .flush()
        .map_err(|e| format!("Не удалось записать {}: {e}", dest.display()))?;
    Ok(())
}

pub fn normalize_member(value: &str) -> String {
    value.replace('/', "\\").trim().to_ascii_lowercase()
}
