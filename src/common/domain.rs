use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::assets::{FileStamp, RouteAssetType};
use crate::common::archive::extract_archive_member;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Route {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) icon_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct IncompleteRouteFolder {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) icon_path: Option<PathBuf>,
    pub(crate) problems: Vec<String>,
    pub(crate) inventory: Vec<String>,
    pub(crate) scenario_clues: Vec<String>,
    pub(crate) file_count: usize,
    #[serde(default)]
    pub(crate) total_size_bytes: u64,
    pub(crate) suggested_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RouteScanResult {
    pub(crate) routes: Vec<Route>,
    pub(crate) incomplete_folders: Vec<IncompleteRouteFolder>,
}

#[derive(Debug)]
pub(crate) enum RouteScanItem {
    Route(Route),
    Incomplete(IncompleteRouteFolder),
}

#[derive(Debug)]
pub(crate) struct CheckResult {
    pub(crate) missing_assets: Vec<MissingAsset>,
    pub(crate) asset_inventory: Vec<ReferencedAsset>,
    pub(crate) umlaut_assets: Vec<UmlautAsset>,
    pub(crate) conversion_errors: Vec<String>,
    pub(crate) structure_issues: Vec<String>,
    pub(crate) route_properties_warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct MissingAsset {
    pub(crate) path: String,
    pub(crate) uses: Vec<AssetUse>,
    pub(crate) suggested_repair: Option<String>,
    pub(crate) is_corrupted_link: bool,
    pub(crate) asset_type: RouteAssetType,
}

#[derive(Clone, Debug)]
pub(crate) struct ReferencedAsset {
    pub(crate) path: String,
    pub(crate) uses: Vec<AssetUse>,
    pub(crate) missing: bool,
    pub(crate) path_search: String,
}

#[derive(Clone, Debug)]
pub(crate) struct UmlautAsset {
    pub(crate) path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AssetUse {
    pub(crate) source_file: RouteFile,
    pub(crate) tile: String,
    pub(crate) tile_kind: TileKind,
    pub(crate) kind: AssetKind,
    pub(crate) count: u32,
    pub(crate) tile_search: String,
    pub(crate) source_file_search: String,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum RouteFile {
    /// Файл лежит на диске.
    Path(PathBuf),
    /// Запись архива .ap.
    Archive {
        archive: PathBuf,
        name: String,
        stamp: Option<FileStamp>,
    },
}

impl RouteFile {
    pub(crate) fn route_name(&self, route_path: &Path) -> Option<String> {
        match self {
            RouteFile::Path(path) => {
                let relative = path.strip_prefix(route_path).ok()?;
                Some(relative.to_string_lossy().replace('\\', "/"))
            }
            RouteFile::Archive { name, .. } => {
                let clean = name.trim_start_matches(['/', '\\']);
                Some(clean.replace('\\', "/"))
            }
        }
    }

    pub(crate) fn file_name(&self) -> String {
        self.file_name_str().to_owned()
    }

    pub(crate) fn file_name_str(&self) -> &str {
        match self {
            RouteFile::Path(path) => path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
            RouteFile::Archive { name, .. } => Path::new(name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn install_target(&self, route_path: &Path) -> PathBuf {
        match self {
            RouteFile::Path(path) => path.clone(),
            RouteFile::Archive { name, .. } => {
                let clean = name.trim_start_matches(['/', '\\']);
                if clean.contains('\\') {
                    route_path.join(clean.replace('\\', "/"))
                } else {
                    route_path.join(clean)
                }
            }
        }
    }

    pub(crate) fn materialize(&self, temp_dir: &Path) -> Result<PathBuf, String> {
        let (archive, name) = match self {
            RouteFile::Path(path) => return Ok(path.clone()),
            RouteFile::Archive { archive, name, .. } => (archive, name),
        };

        // Защита от path traversal и сброса temp_dir до корня диска на Windows.
        let clean_name = name.trim_start_matches(['/', '\\']);
        let relative = Path::new(clean_name);
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }) {
            return Err(format!("Incorrect path inside archive: {name}"));
        }

        let target = temp_dir.join(relative);
        if target.is_file() {
            return Ok(target);
        }

        extract_archive_member(archive, name, &target)?;
        Ok(target)
    }

    pub(crate) fn extension(&self) -> Option<&str> {
        match self {
            RouteFile::Path(path) => path.extension()?.to_str(),
            RouteFile::Archive { name, .. } => Path::new(name).extension()?.to_str(),
        }
    }

    pub(crate) fn display(&self) -> String {
        match self {
            RouteFile::Path(path) => path.display().to_string(),
            RouteFile::Archive { archive, name, .. } => {
                format!("{}!{name}", archive.display())
            }
        }
    }

    pub(crate) fn stamp(&self) -> Option<FileStamp> {
        match self {
            RouteFile::Path(path) => FileStamp::of(path),
            RouteFile::Archive { stamp, .. } => *stamp,
        }
    }

    pub(crate) fn tile_kind(&self) -> TileKind {
        let path = self.raw_path_str();
        if ends_with_ignore_ascii_case(&path, "tracks.bin") {
            TileKind::TracksMaster
        } else if ends_with_ignore_ascii_case(&path, "lofts.bin") {
            TileKind::LoftsMaster
        } else if ends_with_ignore_ascii_case(&path, "roads.bin") {
            TileKind::RoadsMaster
        } else if ends_with_ignore_ascii_case(&path, "routeproperties.xml") {
            TileKind::RouteProps
        } else if contains_ignore_ascii_case(&path, "track tiles") {
            TileKind::Track
        } else if contains_ignore_ascii_case(&path, "road tiles") {
            TileKind::Road
        } else if contains_ignore_ascii_case(&path, "loft tiles") {
            TileKind::Loft
        } else {
            TileKind::Scenery
        }
    }

    pub fn is_protected_pathway(&self) -> bool {
        let path = self.raw_path_str();
        contains_path_ignore_ascii_case(&path, "/networks/tracks.bin")
            || contains_path_ignore_ascii_case(&path, "/networks/track tiles/")
    }

    fn raw_path_str(&self) -> Cow<'_, str> {
        match self {
            RouteFile::Path(path) => path.to_string_lossy(),
            RouteFile::Archive { name, .. } => Cow::Borrowed(name.as_str()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AssetKind {
    Loft,
    Scenery,
    Vehicle,
    Other,
}

impl AssetKind {
    pub(crate) fn is_vehicle(self) -> bool {
        self == AssetKind::Vehicle
    }

    pub(crate) fn to_code(self) -> u8 {
        match self {
            AssetKind::Loft => 0,
            AssetKind::Scenery => 1,
            AssetKind::Vehicle => 2,
            AssetKind::Other => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Self {
        match code {
            0 => AssetKind::Loft,
            1 => AssetKind::Scenery,
            2 => AssetKind::Vehicle,
            _ => AssetKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TileKind {
    Scenery,
    Loft,
    Road,
    Track,
    TracksMaster,
    LoftsMaster,
    RoadsMaster,
    RouteProps,
}

pub(crate) struct AssetReplacementReport {
    pub(crate) replaced_files: usize,
    pub(crate) replaced_instances: usize,
    pub(crate) backups: Vec<PathBuf>,
    pub(crate) replacement_asset: String,
    pub(crate) log_file: Option<PathBuf>,
    pub(crate) log_error: Option<String>,
}

pub(crate) struct UmlautRepairReport {
    pub(crate) new_asset_path: String,
}

#[derive(Serialize)]
pub(crate) struct AssetReplacementLogEntry {
    pub(crate) timestamp_unix_seconds: u64,
    pub(crate) operation: String,
    pub(crate) route_id: Option<String>,
    pub(crate) route_name: Option<String>,
    pub(crate) route_path: Option<PathBuf>,
    pub(crate) original_files: Vec<String>,
    pub(crate) missing_asset: Option<String>,
    pub(crate) replacement_asset: Option<String>,
    pub(crate) replaced_files: usize,
    pub(crate) replaced_instances: usize,
    pub(crate) uses: Vec<AssetReplacementLogUse>,
    pub(crate) backups: Vec<PathBuf>,
}

#[derive(Serialize)]
pub(crate) struct AssetReplacementLogUse {
    pub(crate) tile: String,
    pub(crate) source_file: String,
}

pub(crate) struct RouteScanOutcome {
    pub(crate) result: RouteScanResult,
    pub(crate) from_cache: bool,
    pub(crate) notice: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RouteScanCache {
    #[serde(default)]
    pub(crate) icon_scan_version: u8,
    pub(crate) game_path: String,
    pub(crate) folder_ids: Vec<String>,
    pub(crate) ignored_route_ids: Vec<String>,
    pub(crate) result: RouteScanResult,
}

fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    let h = haystack.as_bytes();
    let s = suffix.as_bytes();
    h.len()
        .checked_sub(s.len())
        .is_some_and(|start| h[start..].eq_ignore_ascii_case(s))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(n.len())
        .any(|window| window.eq_ignore_ascii_case(n))
}

fn contains_path_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    haystack.as_bytes().windows(n.len()).any(|window| {
        window.iter().zip(n).all(|(&hb, &nb)| {
            if nb == b'/' {
                hb == b'/' || hb == b'\\'
            } else {
                hb.eq_ignore_ascii_case(&nb)
            }
        })
    })
}
