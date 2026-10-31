//! Кэш разбора файлов маршрута.
//! Кэш лежит в папке самого маршрута и хранит результаты разбора тех файлов,
//! которые не менялись с прошлой проверки. Благодаря этому повторная проверка
//! маршрута разбирает только изменившиеся файлы, а остальные берёт из кэша.
//! В маршруте один и тот же ассет встречается в файле десятки раз, поэтому
//! хранится не каждая ссылка, а счётчик вхождений. Пути ассетов вынесены в
//! общую таблицу: на большом маршруте это уменьшает кэш в десятки раз.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Имя файла кэша в папке маршрута.
pub(crate) const ANALYSIS_CACHE_FILE_NAME: &str = ".ts-checker-analysis-cache.json";

/// Версия формата кэша.
/// Меняется при любом изменении формата: старый кэш тогда использовать нельзя.
const CACHE_VERSION: u32 = 4;

/// Признаки файла, по которым определяется его неизменность.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FileStamp {
    pub(crate) modified: u64,
    pub(crate) size: u64,
    pub(crate) hash: u64,
    pub(crate) content_hash: u64,
    pub(crate) crc: u64,
    pub(crate) content: u64,
    pub(crate) digest: u64,
    pub(crate) checksum: u64,
}

impl FileStamp {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        let size = metadata.len();
        let data = fs::read(path).ok()?;
        use std::hash::{Hash, Hasher};
        let mut hasher = hash_map::DefaultHasher::new();
        data.hash(&mut hasher);
        let hash = hasher.finish();
        Some(Self {
            modified,
            size,
            hash,
            content_hash: hash,
            crc: hash,
            content: hash,
            digest: hash,
            checksum: hash,
        })
    }
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
                    stamp: FileStamp {
                        modified: stored.modified,
                        size: stored.size,
                        hash: stored.hash,
                        content_hash: stored.content_hash,
                        crc: stored.crc,
                        content: stored.content,
                        digest: stored.digest,
                        checksum: stored.checksum,
                    },
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
        // Общая таблица путей: один путь хранится один раз на весь кэш.
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
                    content_hash: file.stamp.content_hash,
                    crc: file.stamp.crc,
                    content: file.stamp.content,
                    digest: file.stamp.digest,
                    checksum: file.stamp.checksum,
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
        fs::rename(&temporary, &path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("Не удалось сохранить кэш разбора: {error}")
        })
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
    content_hash: u64,
    crc: u64,
    content: u64,
    digest: u64,
    checksum: u64,
    uses: Vec<[u32; 3]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with_uses(stamp: FileStamp, uses: &[(&str, u32)]) -> CachedFile {
        CachedFile {
            stamp,
            uses: uses
                .iter()
                .map(|(path, count)| (path.to_string(), *count, crate::domain::AssetKind::Other))
                .collect(),
        }
    }

    #[test]
    fn saves_and_reuses_unchanged_files() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let file = route.join("Scenery").join("+000000+000000.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"payload").unwrap();
        let stamp = FileStamp::of(&file).unwrap();

        let mut cache = AnalysisCache::empty();
        cache.put(
            "Scenery/+000000+000000.bin".to_owned(),
            file_with_uses(stamp, &[("Provider/Product/Scenery/asset.bin", 3)]),
        );
        cache.save(route).expect("кэш должен сохраниться");

        let loaded = AnalysisCache::load(route);
        let cached = loaded
            .get("Scenery/+000000+000000.bin", stamp)
            .expect("файл не менялся — разбор должен взять из кэша");
        assert_eq!(cached.uses.len(), 1);
        assert_eq!(cached.uses[0].0, "Provider/Product/Scenery/asset.bin");
        assert_eq!(cached.uses[0].1, 3);
    }

    #[test]
    fn stores_repeated_assets_as_single_entry_with_count() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let stamp = FileStamp {
            modified: 5,
            size: 7,
            hash: 0,
            content_hash: 0,
            crc: 0,
            content: 0,
            digest: 0,
            checksum: 0,
        };
        let mut cache = AnalysisCache::empty();
        cache.put(
            "Scenery/+000000+000000.bin".to_owned(),
            file_with_uses(stamp, &[("Provider/Product/Scenery/asset.bin", 12)]),
        );
        cache.save(route).expect("кэш должен сохраниться");

        let data = fs::read(cache_path(route)).unwrap();
        let text = String::from_utf8_lossy(&data);
        assert_eq!(
            text.matches("Provider/Product/Scenery/asset.bin").count(),
            1,
            "путь должен храниться один раз"
        );
        let loaded = AnalysisCache::load(route);
        assert_eq!(
            loaded
                .get("Scenery/+000000+000000.bin", stamp)
                .unwrap()
                .uses[0]
                .1,
            12
        );
    }

    #[test]
    fn ignores_cache_of_changed_file() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let file = route.join("+000000+000000.bin");
        std::fs::write(&file, b"payload").unwrap();
        let stamp = FileStamp::of(&file).unwrap();

        let mut cache = AnalysisCache::empty();
        cache.put("+000000+000000.bin".to_owned(), file_with_uses(stamp, &[]));

        // Файл изменился: прежний разбор больше не подходит.
        let changed = FileStamp {
            modified: stamp.modified + 1,
            size: stamp.size,
            hash: stamp.hash,
            content_hash: stamp.content_hash,
            crc: stamp.crc,
            content: stamp.content,
            digest: stamp.digest,
            checksum: stamp.checksum,
        };
        assert!(cache.get("+000000+000000.bin", changed).is_none());
        assert!(cache.get("+000000+000000.bin", stamp).is_some());
    }

    #[test]
    fn ignores_damaged_cache() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        std::fs::write(cache_path(route), b"{ this is not json").unwrap();

        let cache = AnalysisCache::load(route);
        assert!(
            cache
                .get(
                    "+000000+000000.bin",
                    FileStamp {
                        modified: 0,
                        size: 0,
                        hash: 0,
                        content_hash: 0,
                        crc: 0,
                        content: 0,
                        digest: 0,
                        checksum: 0
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn ignores_cache_of_other_version() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        std::fs::write(
            cache_path(route),
            br#"{"version":1,"assets":[],"files":{}}"#,
        )
        .unwrap();

        let cache = AnalysisCache::load(route);
        assert!(
            cache
                .get(
                    "+000000+000000.bin",
                    FileStamp {
                        modified: 0,
                        size: 0,
                        hash: 0,
                        content_hash: 0,
                        crc: 0,
                        content: 0,
                        digest: 0,
                        checksum: 0
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn drops_entries_of_deleted_files() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let mut cache = AnalysisCache::empty();
        let stamp = FileStamp {
            modified: 1,
            size: 2,
            hash: 0,
            content_hash: 0,
            crc: 0,
            content: 0,
            digest: 0,
            checksum: 0,
        };
        cache.put("kept.bin".to_owned(), file_with_uses(stamp, &[]));
        cache.put("removed.bin".to_owned(), file_with_uses(stamp, &[]));

        let mut names = HashMap::new();
        names.insert("kept.bin".to_owned(), ());
        cache.retain(&names);
        cache.save(route).expect("кэш должен сохраниться");

        let loaded = AnalysisCache::load(route);
        assert!(loaded.get("kept.bin", stamp).is_some());
        assert!(loaded.get("removed.bin", stamp).is_none());
    }
}
