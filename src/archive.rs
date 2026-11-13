//! Unified archive (.ap / ZIP) indexing and streaming extraction.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use zip::ZipArchive;

/// Метаданные одного `.ap`-архива для дискового кэша индекса.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApArchiveMeta {
    path: String,
    size: u64,
    mtime: u64,
    files: Vec<String>,
}

/// Дисковый кэш индекса `.ap`-архивов.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ApIndexCacheFile {
    archives: Vec<ApArchiveMeta>,
}

#[derive(Default)]
pub struct ApArchiveCache {
    pub inner: Mutex<HashMap<PathBuf, HashSet<String>>>,
    /// Метаданные по каждому архиву (ключ — путь к .ap).
    archive_meta: Mutex<HashMap<PathBuf, ApArchiveMeta>>,
    /// Признак того, что кэш нужно сохранить на диск.
    dirty: Mutex<bool>,
    /// Флаг первичной загрузки дискового кэша.
    loaded: Mutex<bool>,
}

impl ApArchiveCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Загружает дисковый кэш индекса при первом обращении.
    fn ensure_loaded(&self) {
        let mut loaded = self.loaded.lock().unwrap();
        if *loaded {
            return;
        }
        *loaded = true;
        drop(loaded);

        let path = crate::settings::ap_index_cache_file();
        if let Ok(contents) = fs::read_to_string(&path)
            && let Ok(cache_file) = serde_json::from_str::<ApIndexCacheFile>(&contents)
        {
            let mut meta = self.archive_meta.lock().unwrap();
            for entry in cache_file.archives {
                let ap_path = PathBuf::from(&entry.path);
                meta.insert(ap_path, entry);
            }
        }
    }

    /// Сохраняет дисковый кэш атомарно (во временный файл + rename), если есть изменения.
    fn save_if_dirty(&self) {
        let is_dirty = {
            let mut d = self.dirty.lock().unwrap();
            if !*d {
                return;
            }
            *d = false;
            true
        };
        if !is_dirty {
            return;
        }

        let path = crate::settings::ap_index_cache_file();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let meta = self.archive_meta.lock().unwrap();
        let cache_file = ApIndexCacheFile {
            archives: meta.values().cloned().collect(),
        };
        drop(meta);

        let tmp_path = path.with_extension("json.tmp");
        if let Ok(data) = serde_json::to_string_pretty(&cache_file)
            && let Ok(file) = File::create(&tmp_path)
        {
            let mut file = BufWriter::new(file);
            let _ = write!(file, "{data}");
            let _ = file.flush();
            let _ = fs::rename(&tmp_path, &path);
        }
    }

    pub fn ensure_indexed(&self, product_dir: &Path) {
        self.ensure_loaded();
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
        let mut changed = false;
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

                // Проверяем метаданные архива: если размер и mtime не изменились — используем кэш.
                let metadata = match archive_path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let size = metadata.len();
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let use_cached = {
                    let meta_guard = self.archive_meta.lock().unwrap();
                    match meta_guard.get(&archive_path) {
                        Some(cached) => cached.size == size && cached.mtime == mtime,
                        None => false,
                    }
                };

                if use_cached {
                    let meta_guard = self.archive_meta.lock().unwrap();
                    if let Some(cached) = meta_guard.get(&archive_path) {
                        for f in &cached.files {
                            files.insert(f.clone());
                        }
                    }
                    continue;
                }

                // Индексируем архив заново.
                let Ok(file) = File::open(&archive_path) else {
                    continue;
                };
                let Ok(mut archive) = ZipArchive::new(file) else {
                    continue;
                };
                let mut archive_files = Vec::new();
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
                    archive_files.push(name.clone());
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

                // Обновляем метаданные архива.
                let mut meta_guard = self.archive_meta.lock().unwrap();
                meta_guard.insert(
                    archive_path.clone(),
                    ApArchiveMeta {
                        path: archive_path.to_string_lossy().into_owned(),
                        size,
                        mtime,
                        files: archive_files,
                    },
                );
                drop(meta_guard);
                changed = true;
            }
        }

        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(norm_dir, files);
        }
        if changed {
            *self.dirty.lock().unwrap() = true;
            self.save_if_dirty();
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
            || set
                .iter()
                .any(|v| v.eq_ignore_ascii_case(target.trim_start_matches('\\')))
            || set
                .iter()
                .any(|v| v.eq_ignore_ascii_case(target_stripped.trim_start_matches('\\')))
    }
}

pub fn extract_archive_member(
    archive_path: &Path,
    member: &str,
    dest: &Path,
) -> Result<(), String> {
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
    let idx = idx.ok_or_else(|| {
        format!(
            "Не удалось найти {member} в архиве {}",
            archive_path.display()
        )
    })?;

    let mut entry = zip
        .by_index(idx)
        .map_err(|e| format!("Не удалось прочитать {member} из архива: {e}"))?;
    let mut output = BufWriter::new(
        File::create(dest)
            .map_err(|e| format!("Не удалось создать файл {}: {e}", dest.display()))?,
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
