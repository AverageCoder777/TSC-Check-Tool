use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use walkdir::WalkDir;

pub(crate) fn is_skipped_route_scan_directory(name: &str, ignore_scenarios: bool) -> bool {
    ignore_scenarios && name.eq_ignore_ascii_case("Scenarios")
}

pub fn is_allowed_route_directory(name: &str, ignore_scenarios: bool) -> bool {
    [
        "Scenery",
        "Scenarios",
        "RouteInfo",
        "RouteInformation",
        "Networks",
    ]
    .iter()
    .any(|allowed| name.eq_ignore_ascii_case(allowed))
        && !is_skipped_route_scan_directory(name, ignore_scenarios)
}

/// Префикс рабочих каталогов программы, которые создаются рядом с файлами маршрута
/// и ассетов. Такие каталоги не должны попадать в проверку маршрута, а после аварийного
/// завершения их легко узнать и удалить вручную.
pub(crate) const TEMP_WORK_DIRECTORY_PREFIX: &str = ".ts-checker-tmp-";

pub(crate) fn is_temporary_work_directory(name: &str) -> bool {
    name.starts_with(TEMP_WORK_DIRECTORY_PREFIX)
}

/// Создаёт рабочий каталог рядом с целевым файлом: переименование в пределах одного
/// тома обязательно для `install_converted_asset`, поэтому системный `%TEMP%` не подходит.
pub(crate) fn create_temporary_work_directory(parent: &Path) -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix(TEMP_WORK_DIRECTORY_PREFIX)
        .tempdir_in(parent)
        .map_err(|error| {
            format!(
                "Не удалось создать временные файлы рядом с {}: {error}",
                parent.display()
            )
        })
}

pub(crate) fn collect_route_files(
    route_path: &Path,
    ignore_scenarios: bool,
) -> Result<Vec<PathBuf>, String> {
    WalkDir::new(route_path)
        .into_iter()
        .filter_entry(|entry| {
            if is_temporary_work_directory(&entry.file_name().to_string_lossy()) {
                return false;
            }
            entry.depth() == 0
                || entry.depth() != 1
                || !entry.file_type().is_dir()
                || is_allowed_route_directory(
                    &entry.file_name().to_string_lossy(),
                    ignore_scenarios,
                )
        })
        .map(|entry| {
            entry.map_err(|error| format!("Ошибка при обходе {}: {error}", route_path.display()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|entries| {
            entries
                .into_iter()
                .filter(|entry| {
                    if !entry.file_type().is_file() {
                        return false;
                    }
                    let Some(relative) = entry.path().strip_prefix(route_path).ok() else {
                        return false;
                    };
                    let mut components = relative.components();
                    let Some(first) = components.next() else {
                        return false;
                    };
                    let in_route_root = components.next().is_none();
                    let in_allowed_directory =
                        first.as_os_str().to_str().is_some_and(|directory| {
                            is_allowed_route_directory(directory, ignore_scenarios)
                        });
                    let in_non_asset_directory =
                        first.as_os_str().to_str().is_some_and(|directory| {
                            matches!(
                                directory.to_ascii_lowercase().as_str(),
                                "terrain" | "mixmap" | "routeinfo" | "routeinformation"
                            )
                        });
                    (in_route_root || in_allowed_directory)
                        && !in_non_asset_directory
                        && entry.path().extension().is_some_and(|extension| {
                            extension.eq_ignore_ascii_case("bin")
                                || extension.eq_ignore_ascii_case("xml")
                        })
                })
                .map(|entry| entry.into_path())
                .collect()
        })
}

/// Читает записи архивов .ap маршрута.
///
/// Записи возвращаются как виртуальные файлы: архив не распаковывается целиком,
/// игра читает его так, будто его нет.
pub(crate) fn collect_archive_members(
    route_path: &Path,
) -> Result<Vec<crate::domain::RouteFile>, String> {
    let mut members = Vec::new();
    let entries = fs::read_dir(route_path)
        .map_err(|error| format!("Не удалось прочитать папку маршрута: {error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
        {
            continue;
        }
        let file = fs::File::open(&path)
            .map_err(|error| format!("Не удалось открыть архив {}: {error}", path.display()))?;
        let zip = zip::ZipArchive::new(file)
            .map_err(|error| format!("Не удалось прочитать архив {}: {error}", path.display()))?;
        let archive_stamp = crate::analysis_cache::FileStamp::of(&path);
        // Индекс имён записей архива строится один раз в памяти: поиск и
        // дедупликация имён идут через HashSet, а не через линейный обход zip.
        let mut seen_names: HashSet<String> = HashSet::new();
        for name_result in zip.file_names() {
            let Ok(name) = name_result else {
                continue;
            };
            let name = name.into_owned();
            let normalized_name = name.replace('\\', "/");
            if !seen_names.insert(normalized_name.to_lowercase()) {
                continue;
            }
            let relative = Path::new(&name);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                continue;
            }
            if !relative.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("bin") || extension.eq_ignore_ascii_case("xml")
            }) {
                continue;
            }
            members.push(crate::domain::RouteFile::Archive {
                archive: path.clone(),
                name: name.to_owned(),
                stamp: archive_stamp,
            });
        }
    }
    Ok(members)
}

pub(crate) fn unexpected_route_directories(route_path: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(route_path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", route_path.display()))?;
    let mut unexpected = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Ошибка чтения {}: {error}", route_path.display()))?;
        if entry
            .file_type()
            .map_err(|error| format!("Не удалось прочитать {}: {error}", entry.path().display()))?
            .is_dir()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let known = is_allowed_route_directory(&name, false)
                || name.eq_ignore_ascii_case("Terrain")
                || name.eq_ignore_ascii_case("MixMap");
            if !known {
                unexpected.push(name);
            }
        }
    }
    unexpected.sort_by_key(|name| name.to_lowercase());
    Ok(unexpected)
}

/// Находит рабочие каталоги программы, оставшиеся в маршруте после аварийного завершения.
pub(crate) fn leftover_temporary_work_directories(
    route_path: &Path,
) -> Result<Vec<String>, String> {
    let mut leftovers = Vec::new();
    for entry in WalkDir::new(route_path).min_depth(1).max_depth(3) {
        let entry = entry
            .map_err(|error| format!("Ошибка при обходе {}: {error}", route_path.display()))?;
        if entry.file_type().is_dir()
            && is_temporary_work_directory(&entry.file_name().to_string_lossy())
        {
            leftovers.push(entry.path().display().to_string());
        }
    }
    leftovers.sort();
    Ok(leftovers)
}

