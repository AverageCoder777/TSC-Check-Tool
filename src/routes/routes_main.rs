//! crate::domain::Route discovery, structure reports, names, icons, and route-scan caching.

use crate::app::{LogEntry, LogLevel};
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, mpsc};
use walkdir::WalkDir;
use zip::ZipArchive;

use crate::common::domain::{
    IncompleteRouteFolder, Route, RouteScanCache, RouteScanItem, RouteScanOutcome, RouteScanResult,
};
use crate::common::translit::decode_xml_text;

const ICON_SCAN_VERSION: u8 = 6;

// Safe from archive bomb or similar
const MAX_ARCHIVE_COMPRESSION_RATIO: u64 = 200;
const ARCHIVE_RATIO_CHECK_MIN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ICON_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
const INTERESTING_TAGS: [&str; 12] = [
    "ScenarioName",
    "DisplayName",
    "ScenarioDescription",
    "Description",
    "StartLocation",
    "StartLocationName",
    "StationName",
    "LocationName",
    "DestinationName",
    "MarkerName",
    "PlaceName",
    "StopName",
];
const REQUIRED_DIRECTORIES: [&str; 1] = ["Networks"];

static STRIP_TAGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("valid regex"));

static LOCALIZATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<Localisation-cUserLocalisedString\b[^>]*>(.*?)</Localisation-cUserLocalisedString\s*>",
    )
    .expect("valid regex")
});

static LANGUAGE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<([A-Za-z][A-Za-z0-9_-]*)\b[^>]*>([^<]*)</[A-Za-z][A-Za-z0-9_-]*\s*>")
        .expect("valid regex")
});

static ROUTE_NAME_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<RouteName\b[^>]*>(.*?)</RouteName>").expect("valid regex"));

static SCENARIO_TAG_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    INTERESTING_TAGS
        .iter()
        .map(|tag| {
            Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}\s*>")).expect("valid regex")
        })
        .collect()
});

fn is_ap_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
}

fn contains_ascii_ignore_case(haystack: &str, needle: &[u8]) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Возвращает true, если элемент архива выглядит как «бомба» и его не стоит распаковывать.
pub(crate) fn is_suspicious_archive_member(size: u64, compressed_size: u64) -> bool {
    if size > MAX_ARCHIVE_MEMBER_BYTES {
        return true;
    }
    if size <= ARCHIVE_RATIO_CHECK_MIN_BYTES || compressed_size == 0 {
        return false;
    }
    size > compressed_size.saturating_mul(MAX_ARCHIVE_COMPRESSION_RATIO)
}

pub(crate) fn route_scan_cache_file() -> PathBuf {
    crate::app::settings::route_cache_file()
}

pub(crate) fn read_route_folder_ids(routes_path: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(routes_path)
        .map_err(|error| format!("Failed to read {}: {error}", routes_path.display()))?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Error during reading routes list in {}: {error}",
                routes_path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("Failed to read {}: {error}", entry.path().display()))?;
        if metadata.file_type().is_dir() {
            ids.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    ids.sort();
    Ok(ids)
}

pub(crate) fn sorted_ids(ids: &HashSet<String>) -> Vec<String> {
    let mut ids: Vec<_> = ids.iter().cloned().collect();
    ids.sort();
    ids
}

fn clean_canonical_path(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Failed to check game folder {}: {error}", path.display()))?;
    let raw = canonical.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{rest}")))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        Ok(PathBuf::from(rest))
    } else {
        Ok(canonical)
    }
}

pub(crate) fn scan_routes_with_cache(
    rw_path: &Path,
    ignored_route_ids: &HashSet<String>,
    allow_cache: bool,
    max_threads: usize,
    log: Option<&RouteScanLog>,
) -> Result<RouteScanOutcome, String> {
    let canonical_game_path = clean_canonical_path(rw_path)?;
    let routes_path = canonical_game_path.join("Content").join("Routes");
    let folder_ids = read_route_folder_ids(&routes_path)?;
    let game_path = canonical_game_path.to_string_lossy().into_owned();
    let ignored_ids = sorted_ids(ignored_route_ids);
    let cache_file = route_scan_cache_file();
    let mut notice = None;

    if allow_cache {
        match fs::read(&cache_file) {
            Ok(contents) => match serde_json::from_slice::<RouteScanCache>(&contents) {
                Ok(cache)
                    if cache.game_path == game_path
                        && cache.icon_scan_version == ICON_SCAN_VERSION
                        && cache.folder_ids == folder_ids
                        && cache.ignored_route_ids == ignored_ids =>
                {
                    return Ok(RouteScanOutcome {
                        result: cache.result,
                        from_cache: true,
                        notice: None,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    notice = Some(crate::gettext!(
                        "Routes cache is broken and will be updated: {}.",
                        error
                    ));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                notice = Some(crate::gettext!(
                    "Failed to read cache {}: {}.",
                    cache_file.display(),
                    error
                ));
            }
        }
    }

    let route_folders: Vec<(String, PathBuf)> = folder_ids
        .iter()
        .map(|id| (id.clone(), routes_path.join(id)))
        .collect();
    let result = scan_route_folders(&route_folders, ignored_route_ids, max_threads, log)?;

    let cache = RouteScanCache {
        icon_scan_version: ICON_SCAN_VERSION,
        game_path,
        folder_ids,
        ignored_route_ids: ignored_ids,
        result: result.clone(),
    };
    let save_result = (|| {
        let parent = cache_file
            .parent()
            .ok_or_else(|| crate::gettext!("Incorrect path to routes cache: {}", cache_file.display()))?;
        fs::create_dir_all(parent).map_err(|error| {
            crate::gettext!(
                "Failed to create cache folder {}: {}",
                parent.display(),
                error
            )
        })?;
        let file = fs::File::create(&cache_file).map_err(|error| {
            crate::gettext!(
                "Failed to save routes cache {}: {}",
                cache_file.display(),
                error
            )
        })?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &cache)
            .map_err(|error| crate::gettext!("Failed to serialize routes cache: {}", error))?;
        writer.flush().map_err(|error| {
            crate::gettext!(
                "Failed to save routes cache {}: {}",
                cache_file.display(),
                error
            )
        })
    })();
    if let Err(error) = save_result {
        notice = Some(match notice {
            Some(previous) => format!("{previous} {error}"),
            None => error,
        });
    }

    Ok(RouteScanOutcome {
        result,
        from_cache: false,
        notice,
    })
}

#[cfg(test)]
pub(crate) fn scan_routes(rw_path: &Path) -> Result<RouteScanResult, String> {
    scan_routes_with_ignored(rw_path, &HashSet::new(), 4, None)
}

#[cfg(test)]
pub(crate) fn scan_routes_with_ignored(
    rw_path: &Path,
    ignored_route_ids: &HashSet<String>,
    max_threads: usize,
    log: Option<&RouteScanLog>,
) -> Result<RouteScanResult, String> {
    let routes_path = rw_path.join("Content").join("Routes");
    let folder_ids = read_route_folder_ids(&routes_path)?;
    let route_folders: Vec<(String, PathBuf)> = folder_ids
        .into_iter()
        .map(|id| {
            let path = routes_path.join(&id);
            (id, path)
        })
        .collect();
    scan_route_folders(&route_folders, ignored_route_ids, max_threads, log)
}

fn scan_route_folders(
    route_folders: &[(String, PathBuf)],
    ignored_route_ids: &HashSet<String>,
    max_threads: usize,
    log: Option<&RouteScanLog>,
) -> Result<RouteScanResult, String> {
    let thread_count = route_folders.len().clamp(1, max_threads.max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .map_err(|error| format!("Failed to start scan: {error}"))?;
    let scan_items = pool.install(|| {
        route_folders
            .par_iter()
            .map(|(id, path)| scan_route_folder(path, id, ignored_route_ids, log))
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut routes = Vec::new();
    let mut incomplete_folders = Vec::new();
    for item in scan_items {
        match item {
            RouteScanItem::Route(route) => routes.push(route),
            RouteScanItem::Incomplete(report) => incomplete_folders.push(report),
        }
    }

    routes.sort_by_cached_key(|route| (route.name.to_lowercase(), route.id.clone()));
    incomplete_folders.sort_by(|left, right| {
        left.file_count
            .cmp(&right.file_count)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(RouteScanResult {
        routes,
        incomplete_folders,
    })
}

pub(crate) fn scan_route_folder(
    path: &Path,
    id: &str,
    ignored_route_ids: &HashSet<String>,
    log: Option<&RouteScanLog>,
) -> Result<RouteScanItem, String> {
    if !ignored_route_ids.contains(id)
        && let Some(report) = inspect_route_folder(path, id, log)?
    {
        return Ok(RouteScanItem::Incomplete(report));
    }
    let name = read_route_name(path, id, log)?;
    Ok(RouteScanItem::Route(Route {
        id: id.to_owned(),
        name,
        path: path.to_path_buf(),
        icon_path: find_route_icon(path)?,
    }))
}

pub(crate) fn inspect_route_folder(
    path: &Path,
    id: &str,
    log: Option<&RouteScanLog>,
) -> Result<Option<IncompleteRouteFolder>, String> {
    let children: Vec<(PathBuf, String, bool)> = fs::read_dir(path)
        .map_err(|error| crate::gettext!("Failed to read {}: {}", path.display(), error))?
        .map(|entry| {
            let entry = entry
                .map_err(|error| crate::gettext!("Error reading content {}: {}", path.display(), error))?;
            let is_directory = entry
                .file_type()
                .map_err(|error| crate::gettext!("Failed to read {}: {}", entry.path().display(), error))?
                .is_dir();
            Ok((
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
                is_directory,
            ))
        })
        .collect::<Result<_, String>>()?;

    // Fast path: сначала проверяем диск — если всё есть, .ap-архивы можно не открывать.
    let mut has_route_properties = path.join("RouteProperties.xml").is_file();
    let mut has_required = [false; REQUIRED_DIRECTORIES.len()];
    for (required_index, required) in REQUIRED_DIRECTORIES.iter().enumerate() {
        let found_path = children
            .iter()
            .find_map(|(entry_path, name, is_directory)| {
                (name.eq_ignore_ascii_case(required) && *is_directory).then_some(entry_path)
            });
        if let Some(directory) = found_path {
            has_required[required_index] = directory_contains_files(directory)?;
        }
    }

    if !has_route_properties || has_required.iter().any(|&found| !found) {
        'archives: for (child_path, _, is_directory) in &children {
            if *is_directory || !is_ap_archive(child_path) {
                continue;
            }
            let archive_file = fs::File::open(child_path).map_err(|error| {
                crate::gettext!("Failed to open archive {}: {}", child_path.display(), error)
            })?;
            let archive = ZipArchive::new(archive_file).map_err(|error| {
                crate::gettext!("Failed to read archive {}: {}", child_path.display(), error)
            })?;
            for name_res in archive.file_names() {
                let Ok(raw_name) = name_res else {
                    continue;
                };
                if raw_name.ends_with(['/', '\\']) {
                    continue;
                }
                if !has_route_properties {
                    let base = raw_name.rsplit(['/', '\\']).next().unwrap_or_default();
                    if base.eq_ignore_ascii_case("RouteProperties.xml") {
                        has_route_properties = true;
                    }
                }
                for (required_index, required) in REQUIRED_DIRECTORIES.iter().enumerate() {
                    if !has_required[required_index]
                        && archive_member_is_under_directory(raw_name.as_ref(), required)
                    {
                        has_required[required_index] = true;
                    }
                }
                if has_route_properties && has_required.iter().all(|&found| found) {
                    break 'archives;
                }
            }
        }
    }

    let mut problems = Vec::new();
    if !has_route_properties {
        problems.push("RouteProperties.xml".to_owned());
    }
    for (required_index, required) in REQUIRED_DIRECTORIES.iter().enumerate() {
        if !has_required[required_index] {
            problems.push((*required).to_owned());
        }
    }

    if problems.is_empty() {
        return Ok(None);
    }

    // Slow path: сначала обходим диск, чтобы знать, какие ScenarioProperties.xml
    // перекрывают одноимённые файлы внутри .ap-архивов.
    let mut scenario_clues = ScenarioClueCollection::default();
    let mut disk_scenarios: HashSet<String> = HashSet::new();
    let mut inventory = Vec::new();
    let mut file_count = 0usize;
    let mut total_size_bytes = 0u64;

    for entry in WalkDir::new(path).follow_links(false) {
        let entry =
            entry.map_err(|error| crate::gettext!("Error during check {}: {}", path.display(), error))?;
        if entry.file_type().is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("ScenarioProperties.xml")
        {
            let bytes = fs::read(entry.path())
                .map_err(|error| crate::gettext!("Failed to read {}: {}", entry.path().display(), error))?;
            let contents = decode_xml_bytes(&bytes);
            let relative = entry
                .path()
                .strip_prefix(path)
                .unwrap_or(entry.path())
                .to_string_lossy();
            let normalized = relative.replace('\\', "/");
            disk_scenarios.insert(normalized.to_lowercase());
            scenario_clues.add(&contents, &relative);
        }

        if entry.path() == path {
            continue;
        }
        let relative = entry.path().strip_prefix(path).unwrap_or(entry.path());
        if entry.file_type().is_dir() {
            inventory.push(format!("[folder] {}", relative.display()));
        } else if entry.file_type().is_file() {
            file_count += 1;
            let size = entry
                .metadata()
                .map_err(|error| crate::gettext!("Failed to read {}: {}", entry.path().display(), error))?
                .len();
            total_size_bytes += size;
            inventory.push(format!("[file, {size} bytes] {}", relative.display()));
        } else if entry.file_type().is_symlink() {
            inventory.push(format!("[link, unchecked] {}", relative.display()));
        }
    }

    let mut archived_route_name = None;
    for (child_path, name, is_directory) in &children {
        if *is_directory || !is_ap_archive(child_path) {
            continue;
        }
        let archive_file = fs::File::open(child_path)
            .map_err(|error| crate::gettext!("Failed to open archive {}: {}", child_path.display(), error))?;
        let mut archive = ZipArchive::new(archive_file)
            .map_err(|error| crate::gettext!("Failed to read archive {}: {}", child_path.display(), error))?;

        // Читаем оглавление архива в памяти без seek, а через by_name открываем только нужные XML.
        let mut xml_to_read: Vec<(String, String, bool)> = Vec::new();
        for name_res in archive.file_names() {
            let Ok(raw_name) = name_res else {
                continue;
            };
            if raw_name.ends_with(['/', '\\']) {
                continue;
            }
            let member_name = raw_name.replace('\\', "/");
            file_count += 1;
            inventory.push(format!("[.ap: {name}] {member_name}"));

            let base_name = member_name.rsplit('/').next().unwrap_or_default();
            if base_name.eq_ignore_ascii_case("ScenarioProperties.xml") {
                if !disk_scenarios.contains(&member_name.to_lowercase()) {
                    xml_to_read.push((raw_name.into_owned(), member_name, true));
                }
            } else if archived_route_name.is_none()
                && (base_name.eq_ignore_ascii_case("RouteProperties.xml")
                    || (member_name.len() >= 4
                        && member_name[member_name.len() - 4..].eq_ignore_ascii_case(".xml")
                        && contains_ascii_ignore_case(&member_name, b"routeinformation")))
            {
                xml_to_read.push((raw_name.into_owned(), member_name, false));
            }
        }

        for (raw_name, member_name, is_scenario) in xml_to_read {
            if !is_scenario && archived_route_name.is_some() {
                continue;
            }
            let Ok(mut member) = archive.by_name(&raw_name) else {
                continue;
            };
            if !member.is_file()
                || is_suspicious_archive_member(member.size(), member.compressed_size())
            {
                continue;
            }
            let mut bytes = Vec::new();
            if is_scenario {
                member.read_to_end(&mut bytes).map_err(|error| {
                    format!(
                        "Failed to read {} from {}: {error}",
                        member_name,
                        child_path.display()
                    )
                })?;
                let contents = decode_xml_bytes(&bytes);
                scenario_clues.add(&contents, &format!("[.ap: {name} :: {member_name}]"));
            } else if member.read_to_end(&mut bytes).is_err() {
                log_route_warning(log, child_path, "error reading archive element");
            } else {
                let contents = decode_xml_bytes(&bytes);
                archived_route_name =
                    parse_route_name(&contents).or_else(|| parse_route_name_tag(&contents));
            }
        }
    }

    inventory.sort();
    let scenario_clues = scenario_clues.into_clues();

    let suggested_name = if problems.len() == 1 && problems[0] == "RouteProperties.xml" {
        archived_route_name.or_else(|| {
            children
                .iter()
                .filter(|(child_path, _, is_directory)| !is_directory && is_ap_archive(child_path))
                .filter_map(|(_, name, _)| {
                    Path::new(name)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
                .find(|name| {
                    !name.is_empty()
                        && ![
                            "route",
                            "routes",
                            "maincontent",
                            "routedata",
                            "content",
                            "assets",
                        ]
                        .iter()
                        .any(|generic| name.eq_ignore_ascii_case(generic))
                        && !is_uuid_like(name)
                })
        })
    } else {
        None
    };

    let icon_path = find_route_icon(path)?;
    Ok(Some(IncompleteRouteFolder {
        id: id.to_owned(),
        path: path.to_path_buf(),
        icon_path,
        problems,
        inventory,
        scenario_clues,
        file_count,
        total_size_bytes,
        suggested_name,
    }))
}

pub(crate) fn find_route_icon(route_path: &Path) -> Result<Option<PathBuf>, String> {
    let cache_directory = crate::app::settings::route_icons_dir();
    find_route_icon_with_cache(route_path, &cache_directory)
}

pub(crate) fn find_route_icon_with_cache(
    route_path: &Path,
    cache_directory: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut route_information = None;
    let mut ap_archives = Vec::new();
    for entry in fs::read_dir(route_path)
        .map_err(|error| crate::gettext!("Failed to read {}: {}", route_path.display(), error))?
    {
        let entry = entry
            .map_err(|error| crate::gettext!("Error reading content {}: {}", route_path.display(), error))?;
        let path = entry.path();
        if route_information.is_none()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("RouteInformation")
        {
            route_information = Some(path);
        } else if is_ap_archive(&path) {
            ap_archives.push(path);
        }
    }

    let mut disk_images = Vec::new();
    if let Some(route_information) = route_information.filter(|path| path.is_dir()) {
        for entry in fs::read_dir(&route_information).map_err(|error| {
            crate::gettext!(
                "Failed to read folder {}: {}",
                route_information.display(),
                error
            )
        })? {
            let entry = entry.map_err(|error| {
                crate::gettext!(
                    "Error reading content {}: {}",
                    route_information.display(),
                    error
                )
            })?;
            if !entry
                .file_type()
                .map_err(|error| crate::gettext!("Failed to read {}: {}", entry.path().display(), error))?
                .is_file()
            {
                continue;
            }
            let path = entry.path();
            if is_route_image(path.extension()) {
                let priority =
                    icon_priority(&path.file_name().unwrap_or_default().to_string_lossy());
                disk_images.push((priority, path));
            }
        }
    }
    disk_images.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let best_disk = disk_images.into_iter().next();

    // Если на диске уже лежит image.png (высший приоритет 0), архивы можно не открывать.
    if let Some(((0, _), path)) = &best_disk {
        return Ok(Some(path.clone()));
    }

    let mut archived_images = Vec::new();
    for archive_path in ap_archives {
        let archive_file = fs::File::open(&archive_path)
            .map_err(|error| crate::gettext!("Failed to open {}: {}", archive_path.display(), error))?;
        let archive = ZipArchive::new(archive_file)
            .map_err(|error| crate::gettext!("Failed to read {}: {}", archive_path.display(), error))?;
        for name_res in archive.file_names() {
            let Ok(raw_name) = name_res else {
                continue;
            };
            if raw_name.ends_with(['/', '\\']) || !is_route_information_image(raw_name.as_ref()) {
                continue;
            }
            let file_name = raw_name.rsplit(['/', '\\']).next().unwrap_or_default();
            archived_images.push((
                icon_priority(file_name),
                archive_path.clone(),
                raw_name.into_owned(),
            ));
        }
    }
    archived_images.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });

    match (best_disk, archived_images.into_iter().next()) {
        // При равном или лучшем приоритете побеждает файл на диске.
        (Some((disk_prio, disk_path)), Some((arch_prio, _, _))) if disk_prio <= arch_prio => {
            Ok(Some(disk_path))
        }
        (_, Some((_, archive_path, raw_member_name))) => {
            extract_archived_route_icon(&archive_path, &raw_member_name, cache_directory)
        }
        (Some((_, disk_path)), None) => Ok(Some(disk_path)),
        (None, None) => Ok(None),
    }
}

fn is_route_image(extension: Option<&std::ffi::OsStr>) -> bool {
    extension.is_some_and(|extension| {
        extension.eq_ignore_ascii_case("png")
            || extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
    })
}

fn is_route_information_image(member_name: &str) -> bool {
    let mut prev_is_route_info = false;
    for part in member_name
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
    {
        if prev_is_route_info && is_route_image(Path::new(part).extension()) {
            return true;
        }
        prev_is_route_info = part.eq_ignore_ascii_case("RouteInformation");
    }
    false
}

fn icon_priority(file_name: &str) -> (u8, String) {
    let name = file_name.to_lowercase();
    let priority = match name.as_str() {
        "image.png" => 0,
        "image.jpg" | "image.jpeg" => 1,
        _ if name.starts_with("image.") => 2,
        _ => 3,
    };
    (priority, name)
}

fn extract_archived_route_icon(
    archive_path: &Path,
    member_name: &str,
    cache_directory: &Path,
) -> Result<Option<PathBuf>, String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| crate::gettext!("Failed to open {}: {}", archive_path.display(), error))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| crate::gettext!("Failed to read {}: {}", archive_path.display(), error))?;
    let member = archive.by_name(member_name).map_err(|error| {
        crate::gettext!(
            "Failed to find {member_name} in {}: {}",
            archive_path.display(),
            error
        )
    })?;
    if member.size() > MAX_ICON_MEMBER_BYTES
        || is_suspicious_archive_member(member.size(), member.compressed_size())
    {
        return Ok(None);
    }

    fs::create_dir_all(cache_directory).map_err(|error| {
        crate::gettext!(
            "Failed to create icon cache {}: {}",
            cache_directory.display(),
            error
        )
    })?;
    let cache_key = stable_icon_cache_key(archive_path, member_name);
    let extension = Path::new(member_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("img");
    let output_path = cache_directory.join(format!("{cache_key}.{extension}"));
    let mut output = fs::File::create(&output_path).map_err(|error| {
        crate::gettext!(
            "Failed to create icon file {}: {}",
            output_path.display(),
            error
        )
    })?;
    io::copy(&mut member.take(MAX_ICON_MEMBER_BYTES), &mut output).map_err(|error| {
        crate::gettext!(
            "Failed to extract {member_name} from {}: {}",
            archive_path.display(),
            error
        )
    })?;
    Ok(Some(output_path))
}

fn stable_icon_cache_key(archive_path: &Path, member_name: &str) -> String {
    let input = format!("{}\0{member_name}", archive_path.to_string_lossy());
    let hash = input.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

pub(crate) fn is_uuid_like(value: &str) -> bool {
    let mut hex_count = 0usize;
    for ch in value.chars() {
        if ch == '-' {
            continue;
        }
        if !ch.is_ascii_hexdigit() {
            return false;
        }
        hex_count += 1;
        if hex_count > 32 {
            return false;
        }
    }
    hex_count == 32
}

pub(crate) fn extract_scenario_clues(xml: &str) -> Vec<String> {
    let mut clues = Vec::new();
    let mut seen = HashSet::new();
    for (index, tag) in INTERESTING_TAGS.iter().enumerate() {
        let element = &SCENARIO_TAG_RES[index];
        for capture in element.captures_iter(xml) {
            let Some(contents) = capture.get(1).map(|capture| capture.as_str()) else {
                continue;
            };
            let text = parse_route_name(contents).or_else(|| {
                let plain_text = STRIP_TAGS_RE.replace_all(contents, " ");
                let decoded = decode_xml_text(&plain_text);
                let normalized = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
                (!normalized.is_empty()).then_some(normalized)
            });
            if let Some(text) = text {
                if is_uuid_like(text.trim()) {
                    continue;
                }
                let clue = format!("{tag}: {text}");
                if seen.insert(clue.clone()) {
                    clues.push(clue);
                }
            }
        }
    }
    clues
}

#[derive(Default)]
pub(crate) struct ScenarioClueCollection {
    other_clues: HashSet<String>,
    start_locations: HashMap<String, (String, usize)>,
}

impl ScenarioClueCollection {
    pub(crate) fn add(&mut self, xml: &str, source: &str) {
        let mut starts_in_scenario = HashSet::new();
        for clue in extract_scenario_clues(xml) {
            let Some((tag, value)) = clue.split_once(": ") else {
                continue;
            };
            if tag == "StartLocation" || tag == "StartLocationName" {
                let key = value
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if key.is_empty() || !starts_in_scenario.insert(key.clone()) {
                    continue;
                }
                self.start_locations
                    .entry(key)
                    .and_modify(|(_, count)| *count += 1)
                    .or_insert_with(|| (value.to_owned(), 1));
            } else {
                self.other_clues.insert(format!("{source}: {clue}"));
            }
        }
    }

    pub(crate) fn into_clues(self) -> Vec<String> {
        let mut clues: Vec<String> = self.other_clues.into_iter().collect();
        for (value, count) in self.start_locations.into_values() {
            let suffix = if count > 1 {
                format!(" (mentioned in {count} scenarios)")
            } else {
                String::new()
            };
            clues.push(format!("StartLocation: {value}{suffix}"));
        }
        clues.sort();
        clues
    }
}

pub(crate) fn archive_member_is_under_directory(member: &str, directory: &str) -> bool {
    let mut parts = member.split(['/', '\\']).filter(|part| !part.is_empty());
    while let Some(component) = parts.next() {
        if component.eq_ignore_ascii_case(directory) {
            return parts.next().is_some();
        }
    }
    false
}

pub(crate) fn parse_route_name_tag(xml: &str) -> Option<String> {
    ROUTE_NAME_TAG_RE
        .captures(xml)
        .and_then(|capture| capture.get(1))
        .map(|value| decode_xml_text(value.as_str()))
        .filter(|name| !name.is_empty())
}

pub(crate) fn directory_contains_files(path: &Path) -> Result<bool, String> {
    for entry in WalkDir::new(path).follow_links(false).min_depth(1) {
        let entry =
            entry.map_err(|error| crate::gettext!("Error during check {}: {}", path.display(), error))?;
        if entry.file_type().is_file() {
            let file_path = entry.path();
            if is_ap_archive(file_path) {
                let file = fs::File::open(file_path)
                    .map_err(|error| crate::gettext!("Failed to open {}: {}", file_path.display(), error))?;
                let archive = ZipArchive::new(file).map_err(|error| {
                    crate::gettext!("Failed to read archive {}: {}", file_path.display(), error)
                })?;
                for name_res in archive.file_names() {
                    let name = name_res.map_err(|error| {
                        crate::gettext!("Error in archive {}: {}", file_path.display(), error)
                    })?;
                    if !name.ends_with(['/', '\\']) {
                        return Ok(true);
                    }
                }
            } else {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

type RouteScanLog = mpsc::Sender<LogEntry>;

/// Декодирует сырые байты XML-файла в `String`.
///
/// Поддерживает UTF-8 с BOM (`EF BB BF`), UTF-16 LE с BOM (`FF FE`),
/// UTF-16 BE с BOM (`FE FF`) и обычный UTF-8. Недопустимые последовательности
/// заменяются символом замены, поэтому декодирование не завершается ошибкой.
pub(crate) fn decode_xml_bytes(data: &[u8]) -> String {
    if data.len() >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
        return String::from_utf8_lossy(&data[3..]).into_owned();
    }
    if data.len() >= 2 {
        if data[0] == 0xFF && data[1] == 0xFE {
            let units: Vec<u16> = data[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            return String::from_utf16_lossy(&units);
        }
        if data[0] == 0xFE && data[1] == 0xFF {
            let units: Vec<u16> = data[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect();
            return String::from_utf16_lossy(&units);
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

/// Пишет предупреждение о нечитаемом или нераспознаваемом
/// RouteProperties.xml из .ap-архива в системный лог.
fn log_route_warning(log: Option<&RouteScanLog>, archive_path: &Path, reason: &str) {
    if let Some(tx) = log {
        let _ = tx.send(LogEntry {
            text: format!(
                "Failed to read RouteProperties.xml from {}: {reason}",
                archive_path.display()
            ),
            level: LogLevel::Warning,
        });
    }
}

pub(crate) fn read_route_name(
    route_path: &Path,
    fallback: &str,
    log: Option<&RouteScanLog>,
) -> Result<String, String> {
    let properties_path = route_path.join("RouteProperties.xml");
    let data = match fs::read(&properties_path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let entries = fs::read_dir(route_path)
                .map_err(|error| format!("Failed to read {}: {error}", route_path.display()))?;
            for entry in entries {
                let entry = entry
                    .map_err(|error| format!("Read error {}: {error}", route_path.display()))?;
                let archive_path = entry.path();
                if !is_ap_archive(&archive_path) {
                    continue;
                }
                let file = match fs::File::open(&archive_path) {
                    Ok(file) => file,
                    Err(error) => {
                        log_route_warning(
                            log,
                            &archive_path,
                            &crate::gettext!("Failed to open archive: {}", error),
                        );
                        continue;
                    }
                };
                let mut archive = match ZipArchive::new(file) {
                    Ok(archive) => archive,
                    Err(error) => {
                        log_route_warning(
                            log,
                            &archive_path,
                            &crate::gettext!("Failed to read archive: {}", error),
                        );
                        continue;
                    }
                };
                let candidates: Vec<String> = archive
                    .file_names()
                    .filter_map(Result::ok)
                    .filter(|raw_name| {
                        !raw_name.ends_with(['/', '\\'])
                            && raw_name
                                .rsplit(['/', '\\'])
                                .next()
                                .unwrap_or_default()
                                .eq_ignore_ascii_case("RouteProperties.xml")
                    })
                    .map(std::borrow::Cow::into_owned)
                    .collect();

                for raw_name in candidates {
                    let Ok(mut member) = archive.by_name(&raw_name) else {
                        continue;
                    };
                    if !member.is_file()
                        || is_suspicious_archive_member(member.size(), member.compressed_size())
                    {
                        continue;
                    }
                    let mut bytes = Vec::new();
                    if member.read_to_end(&mut bytes).is_err() {
                        log_route_warning(
                            log,
                            &archive_path,
                            &crate::gettext!("Error while reading archive element"),
                        );
                        continue;
                    }
                    let contents = decode_xml_bytes(&bytes);
                    if let Some(name) =
                        parse_route_name(&contents).or_else(|| parse_route_name_tag(&contents))
                    {
                        return Ok(name);
                    }
                    log_route_warning(
                        log,
                        &archive_path,
                        &crate::gettext!("Tag RouteName not found in RouteProperties.xml"),
                    );
                }
            }
            return Ok(crate::gettext!("Untitled — {}", fallback));
        }
        Err(error) => {
            return Err(crate::gettext!(
                "Failed to read {}: {}",
                properties_path.display(),
                error
            ));
        }
    };

    let text = decode_xml_bytes(&data);
    let Some(name) = parse_route_name(&text).or_else(|| parse_route_name_tag(&text)) else {
        return Ok(crate::gettext!("Untitled — {}", fallback));
    };
    Ok(name)
}

pub(crate) fn parse_route_name(xml: &str) -> Option<String> {
    let localized_contents = LOCALIZATION_RE
        .captures(xml)
        .and_then(|capture| capture.get(1))
        .map(|capture| capture.as_str())
        .unwrap_or(xml);
    let mut fallback = None;
    for capture in LANGUAGE_TAG_RE.captures_iter(localized_contents) {
        let Some(language) = capture.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(raw_value) = capture.get(2).map(|m| m.as_str()) else {
            continue;
        };
        let value = decode_xml_text(raw_value);
        if value.is_empty() {
            continue;
        }
        if language.eq_ignore_ascii_case("English") {
            return Some(value);
        }
        if fallback.is_none() {
            fallback = Some(value);
        }
    }
    fallback
}
