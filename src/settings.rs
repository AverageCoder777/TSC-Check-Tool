//! Persistent user settings in a single JSON configuration file.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn default_language() -> String {
    "ru".to_owned()
}

fn default_true() -> bool {
    true
}

/// Единая конфигурация приложения, хранящаяся в `config.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub game_path: Option<PathBuf>,
    pub locoswap_path: Option<PathBuf>,
    pub serz_process_limit: usize,
    #[serde(default = "default_true")]
    pub route_thumbnails_enabled: bool,
    #[serde(default)]
    pub route_scan_on_hdd: bool,
    #[serde(default)]
    pub hide_quick_drive_free_roam: bool,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub ignore_incomplete_warning: bool,
}

impl AppConfig {
    /// Загружает конфигурацию. При отсутствии или порче файла — значения по умолчанию.
    pub fn load() -> Self {
        fs::read_to_string(config_file())
            .ok()
            .and_then(|contents| serde_json::from_str::<AppConfig>(&contents).ok())
            .unwrap_or_default()
    }

    /// Сохраняет конфигурацию в pretty JSON, создавая папку при необходимости.
    pub fn save(&self) -> Result<(), String> {
        let path = config_file();
        let parent = path
            .parent()
            .ok_or_else(|| "Некорректный путь к конфигурации.".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Не удалось создать папку настроек: {error}"))?;
        let data = serde_json::to_string_pretty(self)
            .map_err(|error| format!("Не удалось подготовить конфигурацию: {error}"))?;
        fs::write(&path, data).map_err(|error| {
            format!(
                "Не удалось сохранить конфигурацию {}: {error}",
                path.display()
            )
        })
    }
}

/// Папка данных приложения: `%APPDATA%\TSC Check Tool`,
/// либо `./TSC Check Tool`, если переменная APPDATA недоступна.
pub(crate) fn app_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(|app_data| PathBuf::from(app_data).join("TSC Check Tool"))
        .unwrap_or_else(|| PathBuf::from("TSC Check Tool"))
}

/// Путь к единому конфигурационному файлу.
pub(crate) fn config_file() -> PathBuf {
    app_data_dir().join("config.json")
}

/// Путь к кэшу сканирования маршрутов.
pub(crate) fn route_cache_file() -> PathBuf {
    app_data_dir().join("route_cache.json")
}

/// Папка с иконками маршрутов.
pub(crate) fn route_icons_dir() -> PathBuf {
    app_data_dir().join("route-icons")
}

pub(crate) fn load_config() -> AppConfig {
    AppConfig::load()
}

pub(crate) fn save_config(config: &AppConfig) -> Result<(), String> {
    config.save()
}

pub(crate) fn load_language() -> String {
    AppConfig::load().language
}

pub(crate) fn save_language(lang: &str) -> Result<(), String> {
    let mut config = AppConfig::load();
    config.language = lang.to_owned();
    config.save()
}

pub fn apply_saved_language(lang: &str) {
    if lang == "system" {
        return;
    }
    let _ = slint::select_bundled_translation(lang);
}

pub(crate) fn last_game_path_file() -> PathBuf {
    app_data_dir().join("last_game_path.txt")
}

pub(crate) fn load_last_game_path() -> String {
    fs::read_to_string(last_game_path_file())
        .ok()
        .map(|path| path.trim().trim_matches('"').trim_matches('\'').to_owned())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| r"C:\Program Files (x86)\Steam\steamapps\common\RailWorks".to_owned())
}

pub(crate) fn ignored_route_ids_file() -> PathBuf {
    app_data_dir().join("ignored_routes.txt")
}

pub(crate) fn load_ignored_route_ids() -> Result<HashSet<String>, String> {
    let path = ignored_route_ids_file();
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(contents
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(error) => Err(format!(
            "Не удалось прочитать игнор-лист {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn asset_replacement_log_file() -> PathBuf {
    app_data_dir().join("replacement_log.jsonl")
}

/// Путь к полному логу работы программы: `%APPDATA%\TSC Check Tool\app.log`.
pub(crate) fn log_file() -> PathBuf {
    app_data_dir().join("app.log")
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
    let mut line = serde_json::to_vec(entry)
        .map_err(|error| format!("Не удалось подготовить запись журнала замен: {error}"))?;
    line.push(b'\n');
    file.write_all(&line).map_err(|error| {
        format!(
            "Не удалось записать журнал замен {}: {error}",
            path.display()
        )
    })?;
    file.flush().map_err(|error| {
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

/// Количество потоков сканирования маршрутов.
/// На HDD — жёстко 4 (I/O-bound, больше потоков только гасят диск).
/// Иначе — лимит серз-процессов (CPU-bound).
pub(crate) fn route_scan_max_threads(serz_process_limit: usize, on_hdd: bool) -> usize {
    if on_hdd { 4 } else { serz_process_limit.max(1) }
}

/// Количество потоков сканирования маршрутов.
/// На HDD — жёстко 4 (I/O-bound, больше потоков только гасят диск).
/// Иначе — лимит серз-процессов (CPU-bound).
#[cfg(test)]
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Не удалось прочитать настройку миниатюр {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn read_serz_process_limit(path: &Path) -> Result<usize, String> {
    let value = match fs::read_to_string(path) {
        Ok(value) => value.trim().parse::<usize>().map_err(|_| {
            format!(
                "Некорректное значение ограничения Serz-процессов в {}.",
                path.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .max(1)
        }
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

#[cfg(test)]
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

#[cfg(test)]
#[path = "tests/settings.rs"]
mod tests;
