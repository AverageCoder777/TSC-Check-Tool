//! Unified archive (.ap / ZIP) indexing and streaming extraction.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::app::settings::ap_index_cache_file;
use crate::common::fs_utils::{normalize_path, strip_shader_prefix};

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

pub struct ApArchiveCache {
    pub inner: Mutex<HashMap<PathBuf, HashSet<String>>>,
    archive_meta: Mutex<HashMap<PathBuf, ApArchiveMeta>>,
    load_once: Once,
}

impl Default for ApArchiveCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ApArchiveCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            archive_meta: Mutex::new(HashMap::new()),
            load_once: Once::new(),
        }
    }

    /// Загружает дисковый кэш индекса при первом обращении (потокобезопасно).
    fn ensure_loaded(&self) {
        self.load_once.call_once(|| {
            let path = ap_index_cache_file();
            if let Ok(contents) = fs::read_to_string(&path)
                && let Ok(cache_file) = serde_json::from_str::<ApIndexCacheFile>(&contents)
                && let Ok(mut meta) = self.archive_meta.lock()
            {
                meta.reserve(cache_file.archives.len());
                for entry in cache_file.archives {
                    let ap_path = PathBuf::from(&entry.path);
                    meta.insert(ap_path, entry);
                }
            }
        });
    }

    /// Сохраняет дисковый кэш атомарно (во временный файл + rename).
    fn save_disk_cache(&self) {
        let path = ap_index_cache_file();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let cache_file = {
            let Ok(meta) = self.archive_meta.lock() else {
                return;
            };
            ApIndexCacheFile {
                archives: meta.values().cloned().collect(),
            }
        };

        let tmp_path = path.with_extension("json.tmp");
        let write_ok = File::create(&tmp_path).ok().is_some_and(|file| {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, &cache_file).is_ok() && writer.flush().is_ok()
        });

        // Файл уже закрыт здесь — на Windows rename безопасен.
        if write_ok {
            if fs::rename(&tmp_path, &path).is_err() {
                let _ = fs::remove_file(&tmp_path);
            }
        } else {
            let _ = fs::remove_file(&tmp_path);
        }
    }

    pub fn ensure_indexed(&self, product_dir: &Path) {
        self.ensure_loaded();
        let norm_dir = normalize_path(product_dir);
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
                if !archive_path.is_file()
                    || !archive_path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
                {
                    continue;
                }

                let Ok(metadata) = archive_path.metadata() else {
                    continue;
                };
                let size = metadata.len();
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());

                // Проверяем кэш за один захват мьютекса.
                let hit_cache = self.archive_meta.lock().ok().is_some_and(|meta_guard| {
                    if let Some(cached) = meta_guard.get(&archive_path)
                        && cached.size == size
                        && cached.mtime == mtime
                    {
                        for f in &cached.files {
                            insert_index_variants(&mut files, f);
                        }
                        true
                    } else {
                        false
                    }
                });
                if hit_cache {
                    continue;
                }

                // Индексируем архив заново.
                let Ok(file) = File::open(&archive_path) else {
                    continue;
                };
                let Ok(mut archive) = ZipArchive::new(file) else {
                    continue;
                };

                let mut archive_files = Vec::with_capacity(archive.len());
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
                    let name = normalize_member(raw_name.as_ref());
                    if name.is_empty() {
                        continue;
                    }
                    insert_index_variants(&mut files, &name);
                    archive_files.push(name);
                }

                if let Ok(mut meta_guard) = self.archive_meta.lock() {
                    meta_guard.insert(
                        archive_path.clone(),
                        ApArchiveMeta {
                            path: archive_path.to_string_lossy().into_owned(),
                            size,
                            mtime,
                            files: archive_files,
                        },
                    );
                }
                changed = true;
            }
        }

        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(norm_dir, files);
        }
        if changed {
            self.save_disk_cache();
        }
    }

    pub fn lookup(&self, product_dir: &Path, relative: &str) -> bool {
        self.ensure_indexed(product_dir);
        let Ok(guard) = self.inner.lock() else {
            return false;
        };
        let norm_dir = normalize_path(product_dir);
        let Some(set) = guard.get(&norm_dir) else {
            return false;
        };

        let target = normalize_member(relative);
        let target_trimmed = target.trim_start_matches('\\');
        if set.contains(&target) || set.contains(target_trimmed) {
            return true;
        }

        let target_stripped = normalize_member(&strip_shader_prefix(relative));
        if target_stripped != target {
            let stripped_trimmed = target_stripped.trim_start_matches('\\');
            return set.contains(&target_stripped) || set.contains(stripped_trimmed);
        }
        false
    }
}

/// Добавляет в индекс сам путь, вариант без shader-префикса и суффиксы без 1–2 верхних каталогов.
fn insert_index_variants(files: &mut HashSet<String>, name: &str) {
    insert_with_stripped(files, name);

    let mut trimmed = name.trim_matches('\\');
    for _ in 0..2 {
        let Some(sep) = trimmed.find('\\') else {
            break;
        };
        trimmed = trimmed[sep + 1..].trim_start_matches('\\');
        if trimmed.is_empty() {
            break;
        }
        insert_with_stripped(files, trimmed);
    }
}

fn insert_with_stripped(files: &mut HashSet<String>, value: &str) {
    if !files.contains(value) {
        files.insert(value.to_owned());
    }
    let stripped = strip_shader_prefix(value);
    if stripped != value && !files.contains(&stripped) {
        files.insert(stripped);
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
