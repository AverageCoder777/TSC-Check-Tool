//! Кэш разбора файлов маршрута.
//! Кэш лежит в папке самого маршрута и хранит результаты разбора тех файлов,
//! которые не менялись с прошлой проверки. Благодаря этому повторная проверка
//! маршрута разбирает только изменившиеся файлы, а остальные берёт из кэша.
//! В маршруте один и тот же ассет встречается в файле десятки раз, поэтому
//! хранится не каждая ссылка, а счётчик вхождений. Пути ассетов вынесены в
//! общую таблицу: на большом маршруте это уменьшает кэш в десятки раз.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Имя файла кэша в папке маршрута.
pub(crate) const ANALYSIS_CACHE_FILE_NAME: &str = ".ts-checker-analysis-cache.json";

/// Версия формата кэша.
/// Меняется при любом изменении формата: старый кэш тогда использовать нельзя.
const CACHE_VERSION: u32 = 4;

/// Признаки файла, по которым определяется его неизменность.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct FileStamp {
    pub(crate) modified: u64,
    pub(crate) size: u64,
    pub(crate) hash: u64,
}

impl FileStamp {
    pub(crate) fn new(modified: u64, size: u64, hash: u64) -> Self {
        Self { modified, size, hash }
    }

    pub(crate) fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        let size = metadata.len();

        let hash = compute_file_hash(path, size)?;
        Some(Self::new(modified, size, hash))
    }
}

/// Вычисляет хэш содержимого потоково блоками по 64 КБ без выделения гигабайтных буферов в куче.
fn compute_file_hash(path: &Path, size: u64) -> Option<u64> {
    use std::hash::{Hash, Hasher};

    let mut file = File::open(path).ok()?;
    let mut hasher = hash_map::DefaultHasher::new();

    // Длина массива хэшируется первой для идентичности со стандартным [u8]::hash
    size.hash(&mut hasher);

    const CHUNK_SIZE: usize = 64 * 1024;
    let mut buf = [0u8; CHUNK_SIZE];

    // Файлы маршрута (.bin / .xml) обычно меньше 16 МБ — хэшируем полностью
    const FULL_HASH_THRESHOLD: u64 = 16 * 1024 * 1024;

    if size <= FULL_HASH_THRESHOLD {
        loop {
            let n = file.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            hasher.write(&buf[..n]);
        }
    } else {
        // Для огромных .ap архивов (1-4 ГБ) чтение целиком заблокирует диск на минуты.
        // Вычисляем хэш по первому и последнему 1 МБ с учетом размера файла.
        const SAMPLE_SIZE: u64 = 1024 * 1024;

        let mut head_read = 0;
        while head_read < SAMPLE_SIZE {
            let to_read = (SAMPLE_SIZE - head_read).min(CHUNK_SIZE as u64) as usize;
            let n = file.read(&mut buf[..to_read]).ok()?;
            if n == 0 {
                break;
            }
            hasher.write(&buf[..n]);
            head_read += n as u64;
        }

        if size > SAMPLE_SIZE * 2 && file.seek(SeekFrom::End(-(SAMPLE_SIZE as i64))).is_ok() {
            let mut tail_read = 0;
            while tail_read < SAMPLE_SIZE {
                let to_read = (SAMPLE_SIZE - tail_read).min(CHUNK_SIZE as u64) as usize;
                let n = file.read(&mut buf[..to_read]).ok()?;
                if n == 0 {
                    break;
                }
                hasher.write(&buf[..n]);
                tail_read += n as u64;
            }
        }
    }

    Some(hasher.finish())
}

pub(crate) struct CachedFile {
    pub(crate) stamp: FileStamp,
    pub(crate) uses: Vec<(String, u32, crate::domain::AssetKind)>,
}

pub(crate) struct AnalysisCache {
    files: HashMap<String, CachedFile>,
}

impl AnalysisCache {
    pub(crate) fn empty() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    pub(crate) fn load(route_path: &Path) -> Self {
        let path = cache_path(route_path);
        let Ok(data) = fs::read(&path) else {
            return Self::empty();
        };
        let Ok(cache) = serde_json::from_slice::<StoredCache>(&data) else {
            return Self::empty();
        };
        if cache.version != CACHE_VERSION {
            return Self::empty();
        }
        let mut files = HashMap::with_capacity(cache.files.len());
        for (name, stored) in cache.files {
            let uses = stored
                .uses
                .into_iter()
                .filter_map(|[asset, count, kind]| {
                    let path = cache.assets.get(asset as usize)?.clone();
                    Some((path, count, crate::domain::AssetKind::from_code(kind as u8)))
                })
                .collect();
            files.insert(
                name,
                CachedFile {
                    stamp: FileStamp::new(stored.modified, stored.size, stored.hash),
                    uses,
                },
            );
        }
        Self { files }
    }

    pub(crate) fn get(&self, name: &str, stamp: FileStamp) -> Option<&CachedFile> {
        let cached = self.files.get(name)?;
        (cached.stamp == stamp).then_some(cached)
    }

    pub(crate) fn put(&mut self, name: String, file: CachedFile) {
        self.files.insert(name, file);
    }

    pub(crate) fn retain(&mut self, names: &HashMap<String, ()>) {
        self.files.retain(|name, _| names.contains_key(name));
    }

    pub(crate) fn save(&self, route_path: &Path) -> Result<(), String> {
        let path = cache_path(route_path);
        let mut assets: Vec<String> = Vec::new();
        let mut asset_indexes: HashMap<&str, u32> = HashMap::new();
        let mut files = HashMap::with_capacity(self.files.len());

        for (name, file) in &self.files {
            let mut uses = Vec::with_capacity(file.uses.len());
            for (asset, count, kind) in &file.uses {
                let index = match asset_indexes.get(asset.as_str()) {
                    Some(index) => *index,
                    None => {
                        let index = assets.len() as u32;
                        assets.push(asset.clone());
                        asset_indexes.insert(asset.as_str(), index);
                        index
                    }
                };
                uses.push([index, *count, kind.to_code().into()]);
            }
            files.insert(
                name.clone(),
                StoredFile {
                    modified: file.stamp.modified,
                    size: file.stamp.size,
                    hash: file.stamp.hash,
                    uses,
                },
            );
        }

        let stored = StoredCache {
            version: CACHE_VERSION,
            assets,
            files,
        };
        let data = serde_json::to_vec(&stored)
            .map_err(|error| format!("Не удалось подготовить кэш разбора: {error}"))?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, &data)
            .map_err(|error| format!("Не удалось записать кэш разбора: {error}"))?;

        if let Err(error) = fs::rename(&temporary, &path) {
            // На Windows rename завершается ошибкой, если целевой файл заблокирован антивирусом
            // или помечен как скрытый / только для чтения. Выполняем гарантированную перезапись.
            if fs::copy(&temporary, &path).is_ok() {
                let _ = fs::remove_file(&temporary);
            } else {
                let _ = fs::remove_file(&temporary);
                return Err(format!("Не удалось сохранить кэш разбора: {error}"));
            }
        }
        Ok(())
    }
}

pub(crate) fn cache_path(route_path: &Path) -> PathBuf {
    route_path.join(ANALYSIS_CACHE_FILE_NAME)
}

#[derive(Serialize, Deserialize)]
struct StoredCache {
    version: u32,
    assets: Vec<String>,
    files: HashMap<String, StoredFile>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    modified: u64,
    size: u64,
    hash: u64,
    uses: Vec<[u32; 3]>,
}

#[cfg(test)]
#[path = "tests/analysis_cache.rs"]
mod tests;
