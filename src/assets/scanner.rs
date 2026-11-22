use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

use crate::common::domain::RouteFile;

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

fn is_non_asset_directory(name: &str) -> bool {
    ["Terrain", "MixMap", "RouteInfo", "RouteInformation"]
        .iter()
        .any(|dir| name.eq_ignore_ascii_case(dir))
}

fn is_route_data_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bin") || extension.eq_ignore_ascii_case("xml")
    })
}

fn has_route_data_extension_str(name: &str) -> bool {
    let bytes = name.as_bytes();
    (bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".bin"))
        || (bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".xml"))
}

fn is_scannable_relative_path(relative: &Path, ignore_scenarios: bool) -> bool {
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    let in_route_root = components.next().is_none();
    let first_str = first.as_os_str().to_string_lossy();
    let in_allowed_directory = is_allowed_route_directory(&first_str, ignore_scenarios);
    let in_non_asset_directory = is_non_asset_directory(&first_str);

    (in_route_root || in_allowed_directory) && !in_non_asset_directory
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
    let mut files = Vec::new();
    let walker = WalkDir::new(route_path).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        if is_temporary_work_directory(&name) {
            return false;
        }
        entry.depth() != 1
            || !entry.file_type().is_dir()
            || (is_allowed_route_directory(&name, ignore_scenarios)
                && !is_non_asset_directory(&name))
    });

    for entry in walker {
        let entry = entry
            .map_err(|error| format!("Error during check {}: {error}", route_path.display()))?;
        if !entry.file_type().is_file() || !is_route_data_extension(entry.path()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(route_path) else {
            continue;
        };
        if is_scannable_relative_path(relative, ignore_scenarios) {
            files.push(entry.into_path());
        }
    }

    Ok(files)
}

/// Читает записи архивов .ap маршрута.
///
/// Записи возвращаются как виртуальные файлы: архив не распаковывается целиком,
/// игра читает его так, будто его нет.
pub(crate) fn collect_archive_members(
    route_path: &Path,
    ignore_scenarios: bool,
) -> Result<Vec<RouteFile>, String> {
    let mut members = Vec::new();
    let entries = fs::read_dir(route_path)
        .map_err(|error| format!("Failed to read route folder: {error}"))?;
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
            .map_err(|error| format!("Failed to open archive {}: {error}", path.display()))?;
        let zip = zip::ZipArchive::new(file)
            .map_err(|error| format!("Failed to read archive {}: {error}", path.display()))?;
        let archive_stamp = crate::assets::FileStamp::of(&path);
        // Индекс имён записей архива строится один раз в памяти: поиск и
        // дедупликация имён идут через HashSet, а не через линейный обход zip.
        let mut seen_names: HashSet<String> = HashSet::new();
        for name_result in zip.file_names() {
            let Ok(raw_name) = name_result else {
                continue;
            };
            let name_str: &str = raw_name.as_ref();
            if name_str.ends_with(['/', '\\']) || !has_route_data_extension_str(name_str) {
                continue;
            }

            let normalized_name = if name_str.contains('\\') {
                std::borrow::Cow::Owned(name_str.replace('\\', "/"))
            } else {
                std::borrow::Cow::Borrowed(name_str)
            };

            let relative = Path::new(normalized_name.as_ref());
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
            {
                continue;
            }
            if !is_scannable_relative_path(relative, ignore_scenarios) {
                continue;
            }
            if !seen_names.insert(normalized_name.to_lowercase()) {
                continue;
            }

            members.push(RouteFile::Archive {
                archive: path.clone(),
                name: raw_name.into_owned(),
                stamp: archive_stamp,
            });
        }
    }
    Ok(members)
}

pub(crate) fn unexpected_route_directories(route_path: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(route_path)
        .map_err(|error| format!("Failed to read {}: {error}", route_path.display()))?;
    let mut unexpected = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Read error {}: {error}", route_path.display()))?;
        if entry
            .file_type()
            .map_err(|error| format!("Failed to read {}: {error}", entry.path().display()))?
            .is_dir()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let known = is_temporary_work_directory(&name)
                || is_allowed_route_directory(&name, false)
                || is_non_asset_directory(&name);
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
            .map_err(|error| format!("Error during check {}: {error}", route_path.display()))?;
        if entry.file_type().is_dir()
            && is_temporary_work_directory(&entry.file_name().to_string_lossy())
        {
            leftovers.push(entry.path().display().to_string());
        }
    }
    leftovers.sort();
    Ok(leftovers)
}
