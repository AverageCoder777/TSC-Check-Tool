//! crate::domain::Route discovery, structure reports, names, icons, and route-scan caching.

use super::*;
use std::collections::{HashMap, HashSet};
use walkdir::WalkDir;

/// Реальные файлы в .ap сжимаются в десятки раз: в проверенных архивах максимум
/// около 54x на текстурах и 6x на XML. «Бомба» даёт коэффициент в тысячи раз,
/// поэтому порог в 200 оставляет большой запас для настоящих файлов.
const MAX_ARCHIVE_COMPRESSION_RATIO: u64 = 200;
/// Файлы меньше этого размера разжимать безопасно независимо от коэффициента.
const ARCHIVE_RATIO_CHECK_MIN_BYTES: u64 = 8 * 1024 * 1024;
/// Предел для элемента архива, который программа распаковывает в память.
const MAX_ARCHIVE_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
/// Иконка маршрута — это картинка, больше 64 МБ она быть не может.
const MAX_ICON_MEMBER_BYTES: u64 = 64 * 1024 * 1024;

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

pub(crate) fn route_scan_cache_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("route_cache.json")
    })
}

pub(crate) fn read_route_folder_ids(routes_path: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(routes_path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", routes_path.display()))?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Ошибка при чтении списка маршрутов в {}: {error}",
                routes_path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("Не удалось прочитать {}: {error}", entry.path().display()))?;
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

pub(crate) fn scan_routes_with_cache(
    rw_path: &Path,
    ignored_route_ids: &HashSet<String>,
    allow_cache: bool,
    max_threads: usize,
) -> Result<crate::domain::RouteScanOutcome, String> {
    let canonical_game_path = rw_path.canonicalize().map_err(|error| {
        format!(
            "Не удалось проверить папку игры {}: {error}",
            rw_path.display()
        )
    })?;
    let routes_path = canonical_game_path.join("Content").join("Routes");
    let folder_ids = read_route_folder_ids(&routes_path)?;
    let game_path = canonical_game_path.to_string_lossy().into_owned();
    let ignored_ids = sorted_ids(ignored_route_ids);
    let cache_file = route_scan_cache_file();
    let mut notice = None;

    if allow_cache {
        if let Some(cache_path) = &cache_file {
            match fs::read(cache_path) {
                Ok(contents) => match serde_json::from_slice::<crate::domain::RouteScanCache>(&contents) {
                    Ok(cache)
                        if cache.game_path == game_path
                            && cache.icon_scan_version == 4
                            && cache.folder_ids == folder_ids
                            && cache.ignored_route_ids == ignored_ids =>
                    {
                        return Ok(crate::domain::RouteScanOutcome {
                            result: cache.result,
                            from_cache: true,
                            notice: None,
                        });
                    }
                    Ok(_) => {}
                    Err(error) => {
                        notice = Some(format!(
                            "Кэш маршрутов повреждён и будет обновлён: {error}."
                        ));
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    notice = Some(format!(
                        "Не удалось прочитать кэш {}: {error}.",
                        cache_path.display()
                    ));
                }
            }
        } else {
            notice = Some("APPDATA недоступна; кэш маршрутов не используется.".to_owned());
        }
    }

    let result = scan_routes_with_ignored(&canonical_game_path, ignored_route_ids, max_threads)?;
    if let Some(cache_path) = cache_file {
        let cache = crate::domain::RouteScanCache {
            icon_scan_version: 4,
            game_path,
            folder_ids,
            ignored_route_ids: ignored_ids,
            result: result.clone(),
        };
        let save_result = (|| {
            let parent = cache_path.parent().ok_or_else(|| {
                format!(
                    "Некорректный путь к кэшу маршрутов: {}",
                    cache_path.display()
                )
            })?;
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "Не удалось создать папку кэша {}: {error}",
                    parent.display()
                )
            })?;
            let contents = serde_json::to_vec(&cache)
                .map_err(|error| format!("Не удалось сериализовать кэш маршрутов: {error}"))?;
            fs::write(&cache_path, contents).map_err(|error| {
                format!(
                    "Не удалось сохранить кэш маршрутов {}: {error}",
                    cache_path.display()
                )
            })
        })();
        if let Err(error) = save_result {
            notice = Some(match notice {
                Some(previous) => format!("{previous} {error}"),
                None => error,
            });
        }
    }

    Ok(crate::domain::RouteScanOutcome {
        result,
        from_cache: false,
        notice,
    })
}

#[cfg(test)]
pub(crate) fn scan_routes(rw_path: &Path) -> Result<crate::domain::RouteScanResult, String> {
    scan_routes_with_ignored(rw_path, &HashSet::new(), 4)
}

pub(crate) fn scan_routes_with_ignored(
    rw_path: &Path,
    ignored_route_ids: &HashSet<String>,
    max_threads: usize,
) -> Result<crate::domain::RouteScanResult, String> {
    let routes_path = rw_path.join("Content").join("Routes");
    let entries = fs::read_dir(&routes_path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", routes_path.display()))?;
    let mut route_folders = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Ошибка при чтении списка маршрутов в {}: {error}",
                routes_path.display()
            )
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
        if metadata.file_type().is_dir() {
            route_folders.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }

    let thread_count = route_folders.len().clamp(1, max_threads);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .map_err(|error| format!("Не удалось запустить параллельное сканирование: {error}"))?;
    let scan_items = pool.install(|| {
        route_folders
            .par_iter()
            .map(|(id, path)| scan_route_folder(path, id, ignored_route_ids))
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut routes = Vec::new();
    let mut incomplete_folders = Vec::new();
    for item in scan_items {
        match item {
            crate::domain::RouteScanItem::Route(route) => routes.push(route),
            crate::domain::RouteScanItem::Incomplete(report) => incomplete_folders.push(report),
        }
    }

    routes.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    incomplete_folders.sort_by(|left, right| {
        left.file_count
            .cmp(&right.file_count)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(crate::domain::RouteScanResult {
        routes,
        incomplete_folders,
    })
}

pub(crate) fn scan_route_folder(
    path: &Path,
    id: &str,
    ignored_route_ids: &HashSet<String>,
) -> Result<crate::domain::RouteScanItem, String> {
    if ignored_route_ids.contains(id) {
        let name = read_route_name(path, id)?;
        return Ok(crate::domain::RouteScanItem::Route(crate::domain::Route {
            id: id.to_owned(),
            name,
            path: path.to_path_buf(),
            icon_path: find_route_icon(path)?,
        }));
    }
    if let Some(report) = inspect_route_folder(path, id)? {
        Ok(crate::domain::RouteScanItem::Incomplete(report))
    } else {
        let name = read_route_name(path, id)?;
        Ok(crate::domain::RouteScanItem::Route(crate::domain::Route {
            id: id.to_owned(),
            name,
            path: path.to_path_buf(),
            icon_path: find_route_icon(path)?,
        }))
    }
}

pub(crate) fn inspect_route_folder(
    path: &Path,
    id: &str,
) -> Result<Option<crate::domain::IncompleteRouteFolder>, String> {
    const REQUIRED_DIRECTORIES: [&str; 4] = ["Networks", "RouteInformation", "Scenery", "Terrain"];
    let children: Vec<(PathBuf, String, bool)> = fs::read_dir(path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?
        .map(|entry| {
            let entry = entry.map_err(|error| {
                format!("Ошибка чтения содержимого {}: {error}", path.display())
            })?;
            let is_directory = entry
                .file_type()
                .map_err(|error| {
                    format!("Не удалось прочитать {}: {error}", entry.path().display())
                })?
                .is_dir();
            Ok((
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
                is_directory,
            ))
        })
        .collect::<Result<_, String>>()?;

    let mut archive_members = Vec::new();
    let mut archived_route_name = None;
    let mut scenario_clues = ScenarioClueCollection::default();
    for (child_path, name, is_directory) in &children {
        if *is_directory
            || !Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
        {
            continue;
        }
        let archive_file = fs::File::open(child_path).map_err(|error| {
            format!("Не удалось открыть архив {}: {error}", child_path.display())
        })?;
        let mut archive = ZipArchive::new(archive_file).map_err(|error| {
            format!(
                "Не удалось прочитать архив {}: {error}",
                child_path.display()
            )
        })?;
        for index in 0..archive.len() {
            let mut member = archive
                .by_index(index)
                .map_err(|error| format!("Ошибка в архиве {}: {error}", child_path.display()))?;
            let member_name = match member.name() {
                Ok(name) => name.replace('\\', "/"),
                Err(_) => continue,
            };
            if member.is_file()
                && member_name
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("ScenarioProperties.xml"))
                && !is_suspicious_archive_member(member.size(), member.compressed_size())
            {
                let mut contents = String::new();
                member.read_to_string(&mut contents).map_err(|error| {
                    format!(
                        "Не удалось прочитать {} из {}: {error}",
                        member_name,
                        child_path.display()
                    )
                })?;
                scenario_clues.add(&contents, &format!("[.ap: {name} :: {member_name}]"));
            }
            if member.is_file()
                && (member_name
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("RouteProperties.xml"))
                    || (member_name.to_lowercase().contains("routeinformation")
                        && member_name.to_lowercase().ends_with(".xml")))
                && !is_suspicious_archive_member(member.size(), member.compressed_size())
            {
                let mut contents = String::new();
                if member.read_to_string(&mut contents).is_ok() {
                    archived_route_name = archived_route_name
                        .or_else(|| parse_route_name(&contents))
                        .or_else(|| parse_route_name_tag(&contents));
                }
            }
            archive_members.push((name.clone(), member_name, member.is_file()));
        }
    }

    let route_properties_exists = path.join("RouteProperties.xml").is_file()
        || archive_members.iter().any(|(_, entry, is_file)| {
            *is_file
                && entry
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("RouteProperties.xml"))
        });
    let mut problems = Vec::new();
    if !route_properties_exists {
        problems.push("нет RouteProperties.xml".to_owned());
    }

    for required in REQUIRED_DIRECTORIES {
        let found_path = children
            .iter()
            .find_map(|(entry_path, name, is_directory)| {
                (name.eq_ignore_ascii_case(required) && *is_directory).then_some(entry_path)
            });

        let has_disk_files = match found_path {
            Some(directory) => directory_contains_files(directory)?,
            None => false,
        };
        let has_archived_files = archive_members.iter().any(|(_, member, is_file)| {
            *is_file && archive_member_is_under_directory(member, required)
        });
        if !has_disk_files && !has_archived_files {
            problems.push(if found_path.is_some() {
                format!("папка {required} пуста (файлы не найдены на диске или в .ap)")
            } else {
                format!("нет папки {required} с файлами на диске или в .ap")
            });
        }
    }

    if problems.is_empty() {
        return Ok(None);
    }

    for entry in WalkDir::new(path).follow_links(false) {
        let entry =
            entry.map_err(|error| format!("Ошибка при обходе {}: {error}", path.display()))?;
        if !entry.file_type().is_file()
            || !entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("ScenarioProperties.xml")
        {
            continue;
        }
        let contents = fs::read_to_string(entry.path())
            .map_err(|error| format!("Не удалось прочитать {}: {error}", entry.path().display()))?;
        let relative = entry
            .path()
            .strip_prefix(path)
            .unwrap_or(entry.path())
            .display();
        scenario_clues.add(&contents, &relative.to_string());
    }
    let scenario_clues = scenario_clues.into_clues();

    let mut inventory = Vec::new();
    let mut file_count = 0;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry =
            entry.map_err(|error| format!("Ошибка при обходе {}: {error}", path.display()))?;
        if entry.path() == path {
            continue;
        }
        let relative = entry.path().strip_prefix(path).unwrap_or(entry.path());
        if entry.file_type().is_dir() {
            inventory.push(format!("[папка] {}", relative.display()));
        } else if entry.file_type().is_file() {
            file_count += 1;
            let size = entry
                .metadata()
                .map_err(|error| {
                    format!("Не удалось прочитать {}: {error}", entry.path().display())
                })?
                .len();
            inventory.push(format!("[файл, {size} байт] {}", relative.display()));
        } else if entry.file_type().is_symlink() {
            inventory.push(format!("[ссылка, не обходилась] {}", relative.display()));
        }
    }
    for (archive, member, is_file) in &archive_members {
        if *is_file {
            file_count += 1;
            inventory.push(format!("[.ap: {archive}] {member}"));
        }
    }
    inventory.sort();
    let suggested_name = if problems.len() == 2
        && problems
            .iter()
            .any(|problem| problem == "нет RouteProperties.xml")
        && problems
            .iter()
            .any(|problem| problem.contains("RouteInformation"))
    {
        archived_route_name.or_else(|| {
            children
                .iter()
                .filter(|(_, _, is_directory)| !is_directory)
                .filter_map(|(_, name, _)| {
                    let archive_path = Path::new(name);
                    if !archive_path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
                    {
                        return None;
                    }
                    archive_path
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
    Ok(Some(crate::domain::IncompleteRouteFolder {
        id: id.to_owned(),
        path: path.to_path_buf(),
        icon_path,
        problems,
        inventory,
        scenario_clues,
        file_count,
        suggested_name,
    }))
}

pub(crate) fn find_route_icon(route_path: &Path) -> Result<Option<PathBuf>, String> {
    let cache_directory = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("TS-Checker")
        .join("route-icons");
    find_route_icon_with_cache(route_path, &cache_directory)
}

pub(crate) fn find_route_icon_with_cache(
    route_path: &Path,
    cache_directory: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut route_information = None;
    for entry in fs::read_dir(route_path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", route_path.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "Ошибка чтения содержимого {}: {error}",
                route_path.display()
            )
        })?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("RouteInformation")
        {
            route_information = Some(entry.path());
            break;
        }
    }
    let mut images = Vec::new();
    if let Some(route_information) = route_information.filter(|path| path.is_dir()) {
        for entry in fs::read_dir(&route_information).map_err(|error| {
            format!(
                "Не удалось прочитать папку {}: {error}",
                route_information.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!(
                    "Ошибка чтения содержимого {}: {error}",
                    route_information.display()
                )
            })?;
            if !entry
                .file_type()
                .map_err(|error| {
                    format!("Не удалось прочитать {}: {error}", entry.path().display())
                })?
                .is_file()
            {
                continue;
            }
            let path = entry.path();
            if is_route_image(path.extension()) {
                images.push(path);
            }
        }
    }
    images
        .sort_by_key(|path| icon_priority(&path.file_name().unwrap_or_default().to_string_lossy()));
    if let Some(image) = images.into_iter().next() {
        return Ok(Some(image));
    }

    let mut archived_images = Vec::new();
    for entry in fs::read_dir(route_path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", route_path.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "Ошибка чтения содержимого {}: {error}",
                route_path.display()
            )
        })?;
        if !entry
            .path()
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
        {
            continue;
        }

        let archive_file = fs::File::open(entry.path())
            .map_err(|error| format!("Не удалось открыть {}: {error}", entry.path().display()))?;
        let mut archive = ZipArchive::new(archive_file)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", entry.path().display()))?;
        for index in 0..archive.len() {
            let member = archive
                .by_index(index)
                .map_err(|error| format!("Ошибка в архиве {}: {error}", entry.path().display()))?;
            let member_name = match member.name() {
                Ok(name) => name.replace('\\', "/"),
                Err(_) => continue,
            };
            if !member.is_file() || !is_route_information_image(&member_name) {
                continue;
            }
            let file_name = member_name.rsplit('/').next().unwrap_or_default();
            archived_images.push((icon_priority(file_name), entry.path(), member_name));
        }
    }
    archived_images.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1).then_with(|| left.2.cmp(&right.2)))
    });
    let Some((_, archive_path, member_name)) = archived_images.into_iter().next() else {
        return Ok(None);
    };
    extract_archived_route_icon(&archive_path, &member_name, cache_directory)
}

fn is_route_image(extension: Option<&std::ffi::OsStr>) -> bool {
    extension.is_some_and(|extension| {
        extension.eq_ignore_ascii_case("png")
            || extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
    })
}

fn is_route_information_image(member_name: &str) -> bool {
    let components: Vec<_> = member_name
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    components.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("RouteInformation")
            && is_route_image(Path::new(pair[1]).extension())
    })
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
        .map_err(|error| format!("Не удалось открыть {}: {error}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", archive_path.display()))?;
    let member = archive.by_name(member_name).map_err(|error| {
        format!(
            "Не удалось найти {member_name} в {}: {error}",
            archive_path.display()
        )
    })?;
    if member.size() > MAX_ICON_MEMBER_BYTES
        || is_suspicious_archive_member(member.size(), member.compressed_size())
    {
        return Ok(None);
    }

    fs::create_dir_all(cache_directory).map_err(|error| {
        format!(
            "Не удалось создать кэш миниатюр {}: {error}",
            cache_directory.display()
        )
    })?;
    let cache_key = stable_icon_cache_key(archive_path, member_name);
    let extension = Path::new(member_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("img");
    let output_path = cache_directory.join(format!("{cache_key}.{extension}"));
    let mut output = fs::File::create(&output_path).map_err(|error| {
        format!(
            "Не удалось создать файл миниатюры {}: {error}",
            output_path.display()
        )
    })?;
    io::copy(&mut member.take(MAX_ICON_MEMBER_BYTES), &mut output).map_err(|error| {
        format!(
            "Не удалось извлечь {member_name} из {}: {error}",
            archive_path.display()
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
    let compact: String = value
        .chars()
        .filter(|character| *character != '-')
        .collect();
    compact.len() == 32
        && compact
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

pub(crate) fn extract_scenario_clues(xml: &str) -> Vec<String> {
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
    let Ok(strip_tags) = Regex::new(r"(?is)<[^>]+>") else {
        return Vec::new();
    };
    let mut clues = Vec::new();
    for tag in INTERESTING_TAGS {
        let Ok(element) = Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}\s*>")) else {
            continue;
        };
        for capture in element.captures_iter(xml) {
            let Some(contents) = capture.get(1).map(|capture| capture.as_str()) else {
                continue;
            };
            let text = parse_route_name(contents).or_else(|| {
                let plain_text = strip_tags.replace_all(contents, " ");
                let decoded = crate::translit::decode_xml_text(&plain_text);
                let normalized = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
                (!normalized.is_empty()).then_some(normalized)
            });
            if let Some(text) = text {
                if is_uuid_like(text.trim()) {
                    continue;
                }
                let clue = format!("{tag}: {text}");
                if !clues.contains(&clue) {
                    clues.push(clue);
                }
            }
        }
    }
    clues
}

#[derive(Default)]
pub(crate) struct ScenarioClueCollection {
    other_clues: Vec<String>,
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
                let sourced_clue = format!("{source}: {clue}");
                if !self.other_clues.contains(&sourced_clue) {
                    self.other_clues.push(sourced_clue);
                }
            }
        }
    }

    pub(crate) fn into_clues(mut self) -> Vec<String> {
        for (value, count) in self.start_locations.into_values() {
            let suffix = if count > 1 {
                format!(" (упоминается в {count} сценариях)")
            } else {
                String::new()
            };
            self.other_clues
                .push(format!("StartLocation: {value}{suffix}"));
        }
        self.other_clues.sort();
        self.other_clues
    }
}

pub(crate) fn archive_member_is_under_directory(member: &str, directory: &str) -> bool {
    let components: Vec<&str> = member.split('/').filter(|part| !part.is_empty()).collect();
    components.iter().enumerate().any(|(index, component)| {
        component.eq_ignore_ascii_case(directory) && index + 1 < components.len()
    })
}

pub(crate) fn parse_route_name_tag(xml: &str) -> Option<String> {
    let route_name = Regex::new(r"(?is)<RouteName\b[^>]*>(.*?)</RouteName>").ok()?;
    route_name
        .captures(xml)
        .and_then(|capture| capture.get(1))
        .map(|value| crate::translit::decode_xml_text(value.as_str()))
        .filter(|name| !name.is_empty())
}

pub(crate) fn directory_contains_files(path: &Path) -> Result<bool, String> {
    for entry in WalkDir::new(path).follow_links(false).min_depth(1) {
        let entry =
            entry.map_err(|error| format!("Ошибка при обходе {}: {error}", path.display()))?;
        if entry.file_type().is_file() {
            let file_path = entry.path();
            if file_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
            {
                let file = fs::File::open(file_path).map_err(|error| {
                    format!("Не удалось открыть {}: {error}", file_path.display())
                })?;
                let mut archive = ZipArchive::new(file).map_err(|error| {
                    format!(
                        "Не удалось прочитать архив {}: {error}",
                        file_path.display()
                    )
                })?;
                for index in 0..archive.len() {
                    if archive
                        .by_index(index)
                        .map_err(|error| {
                            format!("Ошибка в архиве {}: {error}", file_path.display())
                        })?
                        .is_file()
                    {
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

pub(crate) fn read_route_name(route_path: &Path, fallback: &str) -> Result<String, String> {
    let properties_path = route_path.join("RouteProperties.xml");
    let data = match fs::read(&properties_path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let entries = fs::read_dir(route_path).map_err(|error| {
                format!("Не удалось прочитать {}: {error}", route_path.display())
            })?;
            for entry in entries {
                let entry = entry
                    .map_err(|error| format!("Ошибка чтения {}: {error}", route_path.display()))?;
                if !entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
                {
                    continue;
                }
                let file = fs::File::open(entry.path()).map_err(|error| {
                    format!("Не удалось открыть {}: {error}", entry.path().display())
                })?;
                let mut archive = ZipArchive::new(file).map_err(|error| {
                    format!("Не удалось прочитать {}: {error}", entry.path().display())
                })?;
                for index in 0..archive.len() {
                    let mut member = archive.by_index(index).map_err(|error| {
                        format!("Ошибка в архиве {}: {error}", entry.path().display())
                    })?;
                    let member_name = match member.name() {
                        Ok(name) => name.to_string(),
                        Err(_) => continue,
                    };
                    if !member.is_file()
                        || !member_name
                            .rsplit(['/', '\\'])
                            .next()
                            .is_some_and(|name| name.eq_ignore_ascii_case("RouteProperties.xml"))
                        || is_suspicious_archive_member(member.size(), member.compressed_size())
                    {
                        continue;
                    }
                    let mut contents = String::new();
                    member.read_to_string(&mut contents).map_err(|error| {
                        format!("Не удалось прочитать RouteProperties.xml из .ap: {error}")
                    })?;
                    if let Some(name) =
                        parse_route_name(&contents).or_else(|| parse_route_name_tag(&contents))
                    {
                        return Ok(name);
                    }
                }
            }
            return Ok(format!("Без названия — {fallback}"));
        }
        Err(error) => {
            return Err(format!(
                "Не удалось прочитать {}: {error}",
                properties_path.display()
            ));
        }
    };

    let text = String::from_utf8_lossy(&data);
    let Some(name) = parse_route_name(&text) else {
        return Ok(format!("Без названия — {fallback}"));
    };
    Ok(name)
}

pub(crate) fn parse_route_name(xml: &str) -> Option<String> {
    let localization = Regex::new(
        r"(?is)<Localisation-cUserLocalisedString\b[^>]*>(.*?)</Localisation-cUserLocalisedString\s*>",
    )
    .ok()?;
    let localized_contents = localization
        .captures(xml)
        .and_then(|capture| capture.get(1))
        .map(|capture| capture.as_str())
        .unwrap_or(xml);
    let languages =
        Regex::new(r"(?is)<([A-Za-z][A-Za-z0-9_-]*)\b[^>]*>([^<]*)</[A-Za-z][A-Za-z0-9_-]*\s*>")
            .ok()?;
    let values: Vec<_> = languages
        .captures_iter(localized_contents)
        .filter_map(|capture| {
            let language = capture.get(1)?.as_str();
            let value = crate::translit::decode_xml_text(capture.get(2)?.as_str());
            (!value.is_empty()).then_some((language.eq_ignore_ascii_case("English"), value))
        })
        .collect();
    values
        .iter()
        .find(|(is_english, _)| *is_english)
        .or_else(|| values.first())
        .map(|(_, name)| name.clone())
}
