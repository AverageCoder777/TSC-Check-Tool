//! Persistent user settings, game path, and ignored-route list.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

fn default_language() -> String {
    "ru".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "default_language")]
    pub language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: "ru".to_owned(),
        }
    }
}

pub(crate) fn last_game_path_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("last_game_path.txt")
    })
}

pub(crate) fn route_thumbnail_setting_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("route_thumbnails_enabled.txt")
    })
}

pub(crate) fn serz_process_limit_setting_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("serz_process_limit.txt")
    })
}

pub(crate) fn locoswap_path_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("locoswap_path.txt")
    })
}

pub(crate) fn load_locoswap_path() -> String {
    locoswap_path_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
        .unwrap_or_default()
}

pub(crate) fn save_locoswap_path(path: &str) -> Result<(), String> {
    let path_file = locoswap_path_file()
        .ok_or_else(|| "Windows APPDATA не определена; путь к LocoSwap не сохранить.".to_owned())?;
    let parent = path_file
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам LocoSwap.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(&path_file, path).map_err(|error| {
        format!(
            "Не удалось сохранить путь к LocoSwap {}: {error}",
            path_file.display()
        )
    })
}

pub(crate) fn asset_replacement_log_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("replacement_log.jsonl")
    })
}

pub(crate) fn append_asset_replacement_log(
    path: &Path,
    entry: &mut crate::domain::AssetReplacementLogEntry,
) -> Result<(), String> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к журналу замен.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку журнала замен: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            format!(
                "Не удалось открыть журнал замен {}: {error}",
                path.display()
            )
        })?;
    entry.timestamp_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("Не удалось определить время записи журнала: {error}"))?
        .as_secs();
    let line = serde_json::to_vec(entry)
        .map_err(|error| format!("Не удалось подготовить запись журнала замен: {error}"))?;
    file.write_all(&line)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| {
            format!(
                "Не удалось записать журнал замен {}: {error}",
                path.display()
            )
        })
}

pub(crate) fn maximum_serz_processes() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

pub(crate) fn load_serz_process_limit() -> Result<usize, String> {
    let path = serz_process_limit_setting_file()
        .ok_or_else(|| "Windows APPDATA не определена; настройка Serz недоступна.".to_owned())?;
    read_serz_process_limit(&path)
}

pub(crate) fn read_serz_process_limit(path: &Path) -> Result<usize, String> {
    let value = match fs::read_to_string(path) {
        Ok(value) => value.trim().parse::<usize>().map_err(|_| {
            format!(
                "Некорректное значение ограничения Serz-процессов в {}.",
                path.display()
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => DEFAULT_CONCURRENT_SERZ,
        Err(error) => {
            return Err(format!(
                "Не удалось прочитать настройку Serz {}: {error}",
                path.display()
            ));
        }
    };
    if value == 0 {
        return Err(format!(
            "Количество параллельных Serz-процессов в {} должно быть не меньше 1.",
            path.display()
        ));
    }
    Ok(value.min(maximum_serz_processes()))
}

pub(crate) fn save_serz_process_limit(limit: usize) -> Result<(), String> {
    let path = serz_process_limit_setting_file()
        .ok_or_else(|| "Windows APPDATA не определена; настройку Serz не сохранили.".to_owned())?;
    write_serz_process_limit(&path, limit)
}

pub(crate) fn write_serz_process_limit(path: &Path, limit: usize) -> Result<(), String> {
    if limit == 0 || limit > maximum_serz_processes() {
        return Err(format!(
            "Количество параллельных Serz-процессов должно быть от 1 до {}.",
            maximum_serz_processes()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам Serz.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(path, limit.to_string()).map_err(|error| {
        format!(
            "Не удалось сохранить настройку Serz {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn route_scan_on_hdd_setting_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("route_scan_on_hdd.txt")
    })
}

pub(crate) fn load_route_scan_on_hdd() -> Result<bool, String> {
    let path = route_scan_on_hdd_setting_file()
        .ok_or_else(|| "Windows APPDATA не определена; настройка HDD недоступна.".to_owned())?;
    match fs::read_to_string(&path) {
        Ok(value) => match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!(
                "Некорректное значение настройки HDD в {}.",
                path.display()
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Не удалось прочитать настройку HDD {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn save_route_scan_on_hdd(enabled: bool) -> Result<(), String> {
    let path = route_scan_on_hdd_setting_file()
        .ok_or_else(|| "Windows APPDATA не определена; настройку HDD не сохранили.".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам HDD.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(&path, enabled.to_string()).map_err(|error| {
        format!(
            "Не удалось сохранить настройку HDD {}: {error}",
            path.display()
        )
    })
}

/// Количество потоков сканирования маршрутов.
/// На HDD — жёстко 4 (I/O-bound, больше потоков только гасят диск).
/// Иначе — лимит серз-процессов (CPU-bound).
pub(crate) fn route_scan_max_threads(serz_process_limit: usize, on_hdd: bool) -> usize {
    if on_hdd { 4 } else { serz_process_limit.max(1) }
}

pub(crate) fn load_route_thumbnail_setting() -> Result<bool, String> {
    let path = route_thumbnail_setting_file().ok_or_else(|| {
        "Windows APPDATA не определена; настройка миниатюр недоступна.".to_owned()
    })?;
    read_route_thumbnail_setting(&path)
}

pub(crate) fn read_route_thumbnail_setting(path: &Path) -> Result<bool, String> {
    match fs::read_to_string(path) {
        Ok(value) => match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!(
                "Некорректное значение настройки миниатюр в {}.",
                path.display()
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Не удалось прочитать настройку миниатюр {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn save_route_thumbnail_setting(enabled: bool) -> Result<(), String> {
    let path = route_thumbnail_setting_file().ok_or_else(|| {
        "Windows APPDATA не определена; настройку миниатюр не сохранили.".to_owned()
    })?;
    write_route_thumbnail_setting(&path, enabled)
}

pub(crate) fn write_route_thumbnail_setting(path: &Path, enabled: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам миниатюр.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(path, enabled.to_string()).map_err(|error| {
        format!(
            "Не удалось сохранить настройку миниатюр {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn ignored_route_ids_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("ignored_routes.txt")
    })
}

pub(crate) fn load_ignored_route_ids() -> Result<HashSet<String>, String> {
    let path = ignored_route_ids_file()
        .ok_or_else(|| "Windows APPDATA не определена; игнор-лист недоступен.".to_owned())?;
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(contents
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(error) => Err(format!(
            "Не удалось прочитать игнор-лист {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn load_last_game_path() -> String {
    last_game_path_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| r"C:\Program Files (x86)\Steam\steamapps\common\RailWorks".to_owned())
}

pub(crate) fn save_last_game_path(path: &str) -> Result<(), String> {
    let path_file = last_game_path_file()
        .ok_or_else(|| "Windows APPDATA не определена; путь к игре не сохранить.".to_owned())?;
    let parent = path_file
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам игры.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(&path_file, path).map_err(|error| {
        format!(
            "Не удалось сохранить путь к игре {}: {error}",
            path_file.display()
        )
    })
}

pub(crate) fn hide_quick_drive_free_roam_setting_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("hide_quick_drive_free_roam.txt")
    })
}

pub(crate) fn load_hide_quick_drive_free_roam() -> Result<bool, String> {
    let path = hide_quick_drive_free_roam_setting_file().ok_or_else(|| {
        "Windows APPDATA не определена; настройка сценариев недоступна.".to_owned()
    })?;
    match fs::read_to_string(&path) {
        Ok(value) => match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!(
                "Некорректное значение настройки сценариев в {}.",
                path.display()
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Не удалось прочитать настройку сценариев {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn save_hide_quick_drive_free_roam(enabled: bool) -> Result<(), String> {
    let path = hide_quick_drive_free_roam_setting_file().ok_or_else(|| {
        "Windows APPDATA не определена; настройку сценариев не сохранили.".to_owned()
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к настройкам сценариев.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    fs::write(&path, enabled.to_string()).map_err(|error| {
        format!(
            "Не удалось сохранить настройку сценариев {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn config_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("TS-Checker")
            .join("config.json")
    })
}

pub(crate) fn load_config() -> AppConfig {
    config_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str::<AppConfig>(&contents).ok())
        .unwrap_or_default()
}

pub(crate) fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_file()
        .ok_or_else(|| "Windows APPDATA не определена; конфигурацию не сохранить.".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "Некорректный путь к конфигурации.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
    let data = serde_json::to_string_pretty(config)
        .map_err(|error| format!("Не удалось подготовить конфигурацию: {error}"))?;
    fs::write(&path, data).map_err(|error| {
        format!(
            "Не удалось сохранить конфигурацию {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn load_language() -> String {
    load_config().language
}

pub(crate) fn save_language(lang: &str) -> Result<(), String> {
    let mut config = load_config();
    config.language = lang.to_owned();
    save_config(&config)
}

pub fn apply_saved_language(lang: &str) {
    if lang == "system" {
        return;
    }
    let _ = slint::select_bundled_translation(lang);
}

#[cfg(test)]
mod tests {
    use super::append_asset_replacement_log;
    use std::fs;

    #[test]
    fn appends_replacement_records_as_json_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs").join("replacement_log.jsonl");
        let mut entry = crate::domain::AssetReplacementLogEntry {
            timestamp_unix_seconds: 0,
            operation: "Замена отсутствующего ассета".to_owned(),
            route_id: Some("route-id".to_owned()),
            route_name: Some("crate::domain::Route".to_owned()),
            route_path: Some(temp.path().join("route")),
            original_files: vec![
                temp.path()
                    .join("route/Scenery/tile.bin")
                    .display()
                    .to_string(),
            ],
            missing_asset: Some("A/B/Scenery/old.bin".to_owned()),
            replacement_asset: Some("A/B/Scenery/new.bin".to_owned()),
            replaced_files: 1,
            replaced_instances: 2,
            uses: vec![crate::domain::AssetReplacementLogUse {
                tile: "тайл X=+000001 Z=-000002".to_owned(),
                source_file: temp
                    .path()
                    .join("route/Scenery/tile.bin")
                    .display()
                    .to_string(),
            }],
            backups: vec![temp.path().join("route/Scenery/tile.bin.bak1")],
        };

        append_asset_replacement_log(&path, &mut entry).unwrap();
        append_asset_replacement_log(&path, &mut entry).unwrap();

        let contents = fs::read_to_string(path).unwrap();
        let records: Vec<_> = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["missing_asset"], "A/B/Scenery/old.bin");
        assert_eq!(records[0]["replacement_asset"], "A/B/Scenery/new.bin");
        assert_eq!(records[0]["uses"][0]["tile"], "тайл X=+000001 Z=-000002");
        assert!(
            records[0]["backups"][0]
                .as_str()
                .unwrap()
                .ends_with(".bak1")
        );
        assert!(records[0]["timestamp_unix_seconds"].as_u64().unwrap() > 0);
    }
}
