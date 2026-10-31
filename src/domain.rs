use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Route {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) icon_path: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct IncompleteRouteFolder {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) icon_path: Option<PathBuf>,
    pub(crate) problems: Vec<String>,
    pub(crate) inventory: Vec<String>,
    pub(crate) scenario_clues: Vec<String>,
    pub(crate) file_count: usize,
    pub(crate) suggested_name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RouteScanResult {
    pub(crate) routes: Vec<Route>,
    pub(crate) incomplete_folders: Vec<IncompleteRouteFolder>,
}

pub(crate) enum RouteScanItem {
    Route(Route),
    Incomplete(IncompleteRouteFolder),
}

pub(crate) struct CheckResult {
    pub(crate) missing_assets: Vec<MissingAsset>,
    pub(crate) asset_inventory: Vec<ReferencedAsset>,
    pub(crate) umlaut_assets: Vec<UmlautAsset>,
    pub(crate) conversion_errors: Vec<String>,
    pub(crate) structure_issues: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct MissingAsset {
    pub(crate) path: String,
    pub(crate) uses: Vec<AssetUse>,
    pub(crate) suggested_repair: Option<String>,
    pub(crate) is_corrupted_link: bool,
    pub(crate) asset_type: crate::assets::RouteAssetType,
}

#[derive(Clone)]
pub(crate) struct ReferencedAsset {
    pub(crate) path: String,
    pub(crate) uses: Vec<AssetUse>,
    pub(crate) missing: bool,
}

#[derive(Clone)]
pub(crate) struct UmlautAsset {
    pub(crate) path: String,
}

#[derive(Clone)]
pub(crate) struct AssetUse {
    pub(crate) source_file: RouteFile,
    pub(crate) tile: String,
    pub(crate) tile_kind: TileKind,
    /// Что за объект: объект маршрута, файл состава или что-то другое.
    pub(crate) kind: AssetKind,
}

/// Файл маршрута: обычный файл на диске или запись архива .ap.
///
/// Игра читает архив .ap так, будто его нет: записи лежат рядом с файлами
/// маршрута. Поэтому архив читается прямо из zip, без распаковки целиком.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum RouteFile {
    /// Файл лежит на диске.
    Path(PathBuf),
    /// Запись архива .ap.
    Archive {
        archive: PathBuf,
        name: String,
        /// Отпечаток архива, вычисленный один раз при сборе файлов.
        stamp: Option<crate::analysis_cache::FileStamp>,
    },
}

thread_local! {
    static ZIP_ARCHIVE_CACHE: RefCell<HashMap<PathBuf, zip::ZipArchive<fs::File>>> =
        RefCell::new(HashMap::new());
}

impl RouteFile {
    /// Имя файла маршрута относительно его папки.
    pub(crate) fn route_name(&self, route_path: &Path) -> Option<String> {
        match self {
            RouteFile::Path(path) => {
                let relative = path.strip_prefix(route_path).ok()?;
                Some(relative.to_string_lossy().replace('\\', "/"))
            }
            RouteFile::Archive { name, .. } => Some(name.replace('\\', "/")),
        }
    }

    /// Имя файла без папок: по нему определяется тайл.
    pub(crate) fn file_name(&self) -> String {
        match self {
            RouteFile::Path(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            RouteFile::Archive { name, .. } => Path::new(name)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// Куда класть изменённую версию файла.
    ///
    /// Запись архива кладётся рядом с архивом: игра читает сначала архив, а
    /// потом файлы рядом с ним, поэтому изменённая версия перекроет оригинал, а
    /// сам архив останется нетронутым.
    pub(crate) fn install_target(&self, route_path: &Path) -> PathBuf {
        match self {
            RouteFile::Path(path) => path.clone(),
            RouteFile::Archive { name, .. } => route_path.join(name.replace('\\', "/")),
        }
    }

    /// Готовит файл на диске: запись архива распаковывается во временный файл.
    pub(crate) fn materialize(&self, temp_dir: &Path) -> Result<PathBuf, String> {
        let RouteFile::Archive { archive, name, .. } = self else {
            let RouteFile::Path(path) = self else {
                unreachable!()
            };
            return Ok(path.clone());
        };
        let relative = Path::new(name);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!("Недопустимый путь внутри архива: {name}"));
        }
        let target = temp_dir.join(relative);
        if target.is_file() {
            return Ok(target);
        }
        let data = ZIP_ARCHIVE_CACHE.with(|cache| -> Result<Vec<u8>, String> {
            let mut cache = cache.borrow_mut();
            if !cache.contains_key(archive) {
                let file = fs::File::open(archive).map_err(|error| {
                    format!("Не удалось открыть архив {}: {error}", archive.display())
                })?;
                let zip = zip::ZipArchive::new(file).map_err(|error| {
                    format!("Не удалось прочитать архив {}: {error}", archive.display())
                })?;
                cache.insert(archive.to_path_buf(), zip);
            }
            let zip = cache.get_mut(archive).unwrap();
            let mut member = zip
                .by_name(name)
                .map_err(|error| format!("Не удалось прочитать {name} из архива: {error}"))?;
            let member_size = member.size();
            let mut reader = BufReader::new(&mut member);
            let mut data = Vec::with_capacity(member_size as usize);
            reader
                .read_to_end(&mut data)
                .map_err(|error| format!("Не удалось распаковать {name}: {error}"))?;
            Ok(data)
        })?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("Не удалось создать папку {}: {error}", parent.display())
            })?;
        }
        let mut output =
            BufWriter::new(fs::File::create(&target).map_err(|error| {
                format!("Не удалось создать файл {}: {error}", target.display())
            })?);
        output
            .write_all(&data)
            .map_err(|error| format!("Не удалось распаковать {name}: {error}"))?;
        output
            .flush()
            .map_err(|error| format!("Не удалось записать {name}: {error}"))?;
        Ok(target)
    }

    /// Расширение файла.
    pub(crate) fn extension(&self) -> Option<String> {
        match self {
            RouteFile::Path(path) => path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.to_owned()),
            RouteFile::Archive { name, .. } => Path::new(name)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.to_owned()),
        }
    }

    /// Как файл показывать в интерфейсе.
    pub(crate) fn display(&self) -> String {
        match self {
            RouteFile::Path(path) => path.display().to_string(),
            RouteFile::Archive { archive, name, .. } => {
                format!("{}!{}", archive.display(), name)
            }
        }
    }

    /// Признаки файла для кэша разбора.
    pub(crate) fn stamp(&self) -> Option<crate::analysis_cache::FileStamp> {
        match self {
            RouteFile::Path(path) => crate::analysis_cache::FileStamp::of(path),
            RouteFile::Archive { stamp, .. } => *stamp,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AssetKind {
    Loft,
    Scenery,
    /// Файл состава: в XML он лежит внутри `iRailVehicleBlueprint`.
    Vehicle,
    Other,
}

impl AssetKind {
    /// Файл состава — это поезд, а не объект маршрута.
    pub(crate) fn is_vehicle(self) -> bool {
        self == AssetKind::Vehicle
    }

    /// Код типа для кэша разбора.
    pub(crate) fn to_code(self) -> u8 {
        match self {
            AssetKind::Loft => 0,
            AssetKind::Scenery => 1,
            AssetKind::Vehicle => 2,
            AssetKind::Other => 3,
        }
    }

    /// Восстанавливает тип по коду из кэша.
    pub(crate) fn from_code(code: u8) -> Self {
        match code {
            0 => AssetKind::Loft,
            1 => AssetKind::Scenery,
            2 => AssetKind::Vehicle,
            _ => AssetKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TileKind {
    Scenery,
    Loft,
    Road,
    Track,
    TracksMaster,
    LoftsMaster,
    RoadsMaster,
    RouteProps,
    Other,
}

impl RouteFile {
    pub(crate) fn tile_kind(&self) -> TileKind {
        let path = self.display().to_lowercase();
        if path.ends_with("tracks.bin") {
            TileKind::TracksMaster
        } else if path.ends_with("lofts.bin") {
            TileKind::LoftsMaster
        } else if path.ends_with("roads.bin") {
            TileKind::RoadsMaster
        } else if path.ends_with("routeproperties.xml") {
            TileKind::RouteProps
        } else if path.contains("track tiles")
            || path.contains("track tiles\\")
            || path.contains("track tiles/")
        {
            TileKind::Track
        } else if path.contains("road tiles") {
            TileKind::Road
        } else if path.contains("loft tiles") {
            TileKind::Loft
        } else if path.contains("scenery") {
            TileKind::Scenery
        } else {
            TileKind::Other
        }
    }
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
