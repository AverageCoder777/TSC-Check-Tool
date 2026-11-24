use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::common::domain::AssetKind;

pub(crate) const ANALYSIS_CACHE_FILE_NAME: &str = ".ts-checker-analysis-cache.json";
const CACHE_VERSION: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct FileStamp {
    pub(crate) modified: u64,
    pub(crate) size: u64,
    pub(crate) hash: u64,
}

impl FileStamp {
    pub(crate) fn new(modified: u64, size: u64, hash: u64) -> Self {
        Self {
            modified,
            size,
            hash,
        }
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

struct Fnv1a64(u64);

impl Fnv1a64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

fn hash_up_to(file: &mut File, hasher: &mut Fnv1a64, buf: &mut [u8], limit: u64) -> Option<()> {
    let mut read_total = 0u64;
    while read_total < limit {
        let to_read = (limit - read_total).min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..to_read]).ok()?;
        if n == 0 {
            break;
        }
        hasher.write(&buf[..n]);
        read_total += n as u64;
    }
    Some(())
}

fn compute_file_hash(path: &Path, size: u64) -> Option<u64> {
    const CHUNK_SIZE: usize = 64 * 1024;
    const FULL_HASH_THRESHOLD: u64 = 16 * 1024 * 1024;
    const SAMPLE_SIZE: u64 = 1024 * 1024;

    let mut file = File::open(path).ok()?;
    let mut hasher = Fnv1a64::new();
    hasher.write(&size.to_le_bytes());

    let mut buf = [0u8; CHUNK_SIZE];
    if size <= FULL_HASH_THRESHOLD {
        hash_up_to(&mut file, &mut hasher, &mut buf, size)?;
    } else {
        hash_up_to(&mut file, &mut hasher, &mut buf, SAMPLE_SIZE)?;
        if size > SAMPLE_SIZE * 2 && file.seek(SeekFrom::End(-(SAMPLE_SIZE as i64))).is_ok() {
            hash_up_to(&mut file, &mut hasher, &mut buf, SAMPLE_SIZE)?;
        }
    }
    Some(hasher.finish())
}

pub(crate) struct CachedFile {
    pub(crate) stamp: FileStamp,
    pub(crate) uses: Vec<(String, u32, AssetKind)>,
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
                    Some((path, count, AssetKind::from_code(kind as u8)))
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
        let mut assets: Vec<&str> = Vec::new();
        let mut asset_indexes: HashMap<&str, u32> = HashMap::new();
        let mut files: HashMap<&str, StoredFile> = HashMap::with_capacity(self.files.len());

        for (name, file) in &self.files {
            let mut uses = Vec::with_capacity(file.uses.len());
            for (asset, count, kind) in &file.uses {
                let index = match asset_indexes.get(asset.as_str()) {
                    Some(&index) => index,
                    None => {
                        let index = assets.len() as u32;
                        assets.push(asset.as_str());
                        asset_indexes.insert(asset.as_str(), index);
                        index
                    }
                };
                uses.push([index, *count, u32::from(kind.to_code())]);
            }
            files.insert(
                name.as_str(),
                StoredFile {
                    modified: file.stamp.modified,
                    size: file.stamp.size,
                    hash: file.stamp.hash,
                    uses,
                },
            );
        }

        let stored = StoredCacheRef {
            version: CACHE_VERSION,
            assets,
            files,
        };

        let temporary = path.with_extension("tmp");
        {
            let file = File::create(&temporary)
                .map_err(|error| crate::gettext!("Failed to write analysis cache: {}", error))?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, &stored)
                .map_err(|error| crate::gettext!("Failed to prepare analysis cache: {}", error))?;
            writer
                .flush()
                .map_err(|error| crate::gettext!("Failed to write analysis cache: {}", error))?;
        }

        if let Err(error) = fs::rename(&temporary, &path) {
            if fs::copy(&temporary, &path).is_ok() {
                let _ = fs::remove_file(&temporary);
            } else {
                let _ = fs::remove_file(&temporary);
                return Err(crate::gettext!("Failed to save analysis cache: {}", error));
            }
        }
        Ok(())
    }
}

pub(crate) fn cache_path(route_path: &Path) -> PathBuf {
    route_path.join(ANALYSIS_CACHE_FILE_NAME)
}

#[derive(Deserialize)]
struct StoredCache {
    version: u32,
    assets: Vec<String>,
    files: HashMap<String, StoredFile>,
}

#[derive(Serialize)]
struct StoredCacheRef<'a> {
    version: u32,
    assets: Vec<&'a str>,
    files: HashMap<&'a str, StoredFile>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    modified: u64,
    size: u64,
    hash: u64,
    uses: Vec<[u32; 3]>,
}

#[cfg(test)]
#[path = "../tests/analysis_cache.rs"]
mod tests;
