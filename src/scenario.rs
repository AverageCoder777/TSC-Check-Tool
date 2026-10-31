//! Сценарии маршрута: папки в Scenarios и их содержимое.
//!
//! Метаданные сценария лежат в ScenarioProperties.xml, а составы техники — в
//! бинарном Scenario.bin, который приходится конвертировать через Serz.
//! Составы загружаются лениво: только при открытии окна конкретного сценария.

use std::fs;
use std::path::{Path, PathBuf};


/// Вид состава: игрок, ИИ или статический (loose).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum ConsistKind {
    #[default]
    Loose,
    Player,
    Ai,
}

/// Сценарий маршрута: папка и данные из её файлов.
#[derive(Clone, Default)]
pub(crate) struct RouteScenario {
    /// Папка сценария.
    pub(crate) path: PathBuf,
    /// Название сценария.
    pub(crate) name: String,
    /// Тип сценария: Standard, Career, Free Roam, Quick Drive.
    pub(crate) scenario_type: String,
    /// Метаданные уже прочитаны.
    pub(crate) metadata_loaded: bool,
    /// Дата сценария.
    pub(crate) date: String,
    /// Автор сценария.
    pub(crate) author: String,
    /// Сезон.
    pub(crate) season: String,
    /// Погода (блюпринт/имя).
    pub(crate) weather: String,
    /// Длительность в минутах.
    pub(crate) duration: String,
    /// Время начала суток.
    pub(crate) start_time: String,
    /// Состояние сценария.
    pub(crate) state: String,
    /// Состав игрока.
    pub(crate) player_consist: String,
    /// Подвижные составы сценария: в них перечислена техника.
    /// `None` пока не открыто окно сценария (загружается лениво).
    pub(crate) consists: Option<Vec<ScenarioConsist>>,
}

/// Подвижной состав сценария.
#[derive(Clone, Default)]
pub(crate) struct ScenarioConsist {
    pub(crate) name: String,
    pub(crate) vehicles: Vec<ScenarioVehicle>,
    pub(crate) is_player: bool,
    /// Порядковый номер cConsist в исходном XML (с учётом пустых составов),
    /// нужен для открытия нужного состава в LocoSwap.
    pub(crate) index: usize,
}

/// Находит позицию открывающего тега элемента (с учётом атрибутов).
fn find_tag(xml: &str, tag: &str) -> Option<usize> {
    let open = format!("<{}", tag);
    let mut pos = 0;
    while let Some(found) = xml[pos..].find(&open) {
        let abs = pos + found;
        let after = &xml[abs + open.len()..];
        if after.starts_with(' ') || after.starts_with('>') {
            return Some(abs);
        }
        pos = abs + open.len();
    }
    None
}

/// Название состава из cDriver/ServiceName/Localisation-cUserLocalisedString.
/// Берётся первый непустой элемент, не Key и не Other. Если состав без
/// водителя (Loose Consist) или имя не найдено — "Неназванный состав".
fn consist_name(consist: &str) -> String {
    let driver_start = match find_tag(consist, "Driver") {
        Some(pos) => pos,
        None => return "Неназванный состав".to_owned(),
    };
    let driver_rest = &consist[driver_start..];
    let driver_end = driver_rest.find("</Driver>").unwrap_or(driver_rest.len());
    let driver_block = &driver_rest[..driver_end];

    let cdriver_start = match find_tag(driver_block, "cDriver") {
        Some(pos) => pos,
        None => return "Неназванный состав".to_owned(),
    };
    let cdriver_rest = &driver_block[cdriver_start..];
    let cdriver_end = cdriver_rest
        .find("</cDriver>")
        .unwrap_or(cdriver_rest.len());
    let cdriver_block = &cdriver_rest[..cdriver_end];

    let sn_start = match find_tag(cdriver_block, "ServiceName") {
        Some(pos) => pos,
        None => return "Неназванный состав".to_owned(),
    };
    let sn_rest = &cdriver_block[sn_start..];
    let sn_end = sn_rest.find("</ServiceName>").unwrap_or(sn_rest.len());
    let sn_block = &sn_rest[..sn_end];

    let loc_start = match find_tag(sn_block, "Localisation-cUserLocalisedString") {
        Some(pos) => pos,
        None => return "Неназванный состав".to_owned(),
    };
    let loc_rest = &sn_block[loc_start..];
    let loc_end = loc_rest
        .find("</Localisation-cUserLocalisedString>")
        .unwrap_or(loc_rest.len());
    let loc_block = &loc_rest[..loc_end];

    let mut pos = 0;
    while let Some(tag_start) = loc_block[pos..].find('<') {
        let abs = pos + tag_start;
        let after = &loc_block[abs..];
        if after.starts_with("</") {
            if let Some(gt) = after.find('>') {
                pos = abs + gt + 1;
                continue;
            } else {
                break;
            }
        }
        let tag_end = match after.find('>') {
            Some(i) => i,
            None => break,
        };
        let tag_name = &after[1..tag_end];
        let tag_name = tag_name.split_whitespace().next().unwrap_or(tag_name);
        if tag_name == "Key" || tag_name == "Other" {
            let close_tag = format!("</{}>", tag_name);
            if let Some(ct) = loc_block[abs..].find(&close_tag) {
                pos = abs + ct + close_tag.len();
                continue;
            } else {
                break;
            }
        }
        let value_start = abs + tag_end + 1;
        let value_end = loc_block[value_start..]
            .find('<')
            .map(|i| value_start + i)
            .unwrap_or(loc_block.len());
        let value = crate::translit::decode_xml_text(&loc_block[value_start..value_end])
            .trim()
            .to_owned();
        if !value.is_empty() {
            return value;
        }
        pos = value_end;
    }

    "Неназванный состав".to_owned()
}

/// Определяет вид состава по содержимому cConsist.
fn consist_kind(consist: &str) -> ConsistKind {
    let Some(driver_start) = find_tag(consist, "Driver") else {
        return ConsistKind::Loose;
    };
    let driver_rest = &consist[driver_start..];
    let driver_end = driver_rest.find("</Driver>").unwrap_or(driver_rest.len());
    let driver_block = &driver_rest[..driver_end];

    let Some(cdriver_start) = find_tag(driver_block, "cDriver") else {
        return ConsistKind::Loose;
    };
    let cdriver_rest = &driver_block[cdriver_start..];
    let cdriver_end = cdriver_rest
        .find("</cDriver>")
        .unwrap_or(cdriver_rest.len());
    let cdriver_block = &cdriver_rest[..cdriver_end];

    if has_player_driver(cdriver_block) {
        ConsistKind::Player
    } else {
        ConsistKind::Ai
    }
}

/// Проверяет наличие флага PlayerDriver со значением 1/true.
fn has_player_driver(block: &str) -> bool {
    let Some(pos) = block.find("<PlayerDriver") else {
        return false;
    };
    let rest = &block[pos..];
    let end = rest.find("</PlayerDriver>").unwrap_or(rest.len());
    let tag_end = rest.find('>').unwrap_or(rest.len());
    let value = rest[tag_end + 1..end].trim();
    value == "1" || value.eq_ignore_ascii_case("true")
}

/// Единица подвижного состава: локомотив или вагон.
#[derive(Clone, Default)]
pub(crate) struct ScenarioVehicle {
    /// Разработчик дополнения.
    pub(crate) provider: String,
    /// Дополнение.
    pub(crate) product: String,
    /// Путь к файлу модели.
    pub(crate) blueprint: String,
    /// Отображаемое название модели.
    pub(crate) name: String,
    /// Номер единицы техники, если он указан в сценарии.
    pub(crate) number: Option<String>,
}

/// Читает сценарии маршрута: папки в Scenarios и их файлы.
pub(crate) fn read_route_scenarios(route_path: &Path, serz_path: &Path) -> Vec<RouteScenario> {
    use rayon::prelude::*;
    let scenarios_path = route_path.join("Scenarios");
    let Ok(entries) = fs::read_dir(&scenarios_path) else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    let mut scenarios: Vec<RouteScenario> = dirs
        .into_par_iter()
        .map(|path| {
            let id = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let mut scenario = RouteScenario {
                path: path.clone(),
                name: id.clone(),
                scenario_type: String::new(),
                metadata_loaded: false,
                ..RouteScenario::default()
            };
            let mut properties_had_type = false;
            if let Ok(xml) = fs::read_to_string(path.join("ScenarioProperties.xml")) {
                scenario.name = localised_text(&xml, "DisplayName");
                if scenario.name.is_empty() {
                    scenario.name = id;
                }
                scenario.author = element_text(&xml, "Author");
                scenario.season = match element_text(&xml, "Season").as_str() {
                    "SEASON_SPRING" => "Spring".to_owned(),
                    "SEASON_SUMMER" => "Summer".to_owned(),
                    "SEASON_AUTUMN" => "Autumn".to_owned(),
                    "SEASON_WINTER" => "Winter".to_owned(),
                    other => other.to_owned(),
                };
                scenario.weather = element_text(&xml, "WeatherBlueprint");
                if scenario.weather.is_empty() {
                    scenario.weather = element_text(&xml, "Weather");
                }
                if scenario.weather.is_empty()
                    && let Some(start) = xml.find("<WeatherBlueprint")
                {
                    let rest = &xml[start..];
                    let tag_end = rest.find('>').unwrap_or(rest.len());
                    let end = rest.find("</WeatherBlueprint>").unwrap_or(rest.len());
                    let block = &rest[tag_end + 1..end];
                    let bp = element_text(block, "BlueprintID");
                    scenario.weather = if bp.is_empty() {
                        block.trim().to_owned()
                    } else {
                        bp
                    };
                }
                scenario.duration = element_text(&xml, "DurationMins");
                scenario.start_time = scenario_start_time(&xml);
                scenario.state = element_text(&xml, "StateName");
                scenario.player_consist = element_text(&xml, "QuickDrivePlayerConsistName");
                scenario.date = scenario_date(&xml);
                scenario.scenario_type = scenario_type_from_xml(&xml);
                properties_had_type = has_scenario_type_marker(&xml);
                scenario.metadata_loaded = true;
            }
            if !properties_had_type && let Some(bin_xml) = read_scenario_file(&path, serz_path) {
                let from_bin = scenario_type_from_xml(&bin_xml);
                if has_scenario_type_marker(&bin_xml) {
                    scenario.scenario_type = from_bin;
                }
            }
            if scenario.scenario_type.is_empty() {
                scenario.scenario_type = "Standard".to_owned();
            }
            scenario
        })
        .collect();

    scenarios.sort_by(|left, right| left.name.cmp(&right.name));
    scenarios
}

/// Читает ScenarioProperties.xml только для одного сценария.
pub(crate) fn load_scenario_metadata(scenario: &mut RouteScenario) {
    if scenario.metadata_loaded {
        return;
    }
    if let Ok(xml) = fs::read_to_string(scenario.path.join("ScenarioProperties.xml")) {
        scenario.name = localised_text(&xml, "DisplayName");
        if scenario.name.is_empty() {
            scenario.name = scenario
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
        }
        scenario.author = element_text(&xml, "Author");
        scenario.season = match element_text(&xml, "Season").as_str() {
            "SEASON_SPRING" => "Spring".to_owned(),
            "SEASON_SUMMER" => "Summer".to_owned(),
            "SEASON_AUTUMN" => "Autumn".to_owned(),
            "SEASON_WINTER" => "Winter".to_owned(),
            other => other.to_owned(),
        };
        scenario.duration = element_text(&xml, "DurationMins");
        scenario.start_time = scenario_start_time(&xml);
        scenario.state = element_text(&xml, "StateName");
        scenario.player_consist = element_text(&xml, "QuickDrivePlayerConsistName");
        scenario.date = scenario_date(&xml);
        if scenario.scenario_type.is_empty() {
            scenario.scenario_type = scenario_type_from_xml(&xml);
        }
        scenario.weather = element_text(&xml, "WeatherBlueprint");
        if scenario.weather.is_empty() {
            scenario.weather = element_text(&xml, "Weather");
        }
        if scenario.weather.is_empty()
            && let Some(start) = xml.find("<WeatherBlueprint")
        {
            let rest = &xml[start..];
            let tag_end = rest.find('>').unwrap_or(rest.len());
            let end = rest.find("</WeatherBlueprint>").unwrap_or(rest.len());
            let block = &rest[tag_end + 1..end];
            let bp = element_text(block, "BlueprintID");
            scenario.weather = if bp.is_empty() {
                block.trim().to_owned()
            } else {
                bp
            };
        }
    } else if scenario.name.is_empty() {
        scenario.name = scenario
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
    }
    if scenario.scenario_type.is_empty() {
        scenario.scenario_type = "Standard".to_owned();
    }
    scenario.metadata_loaded = true;
}

/// Параллельно проверяет наличие блюпринтов техники в составах сценария.
///
/// Возвращает кортеж: (список (имя состава, все ли блюпринты найдены),
/// флаги отсутствия блюпринтов для каждой единицы техники). Выполняется вне
/// удержания глобального мьютекса приложения, чтобы не блокировать UI.
pub(crate) fn analyze_consists_parallel(
    consists: &[ScenarioConsist],
    rw_path: &str,
) -> (Vec<(String, bool)>, Vec<Vec<bool>>) {
    use rayon::prelude::*;
    consists
        .par_iter()
        .map(|c| {
            let mut all_ok = true;
            let mut flags = Vec::with_capacity(c.vehicles.len());
            for veh in &c.vehicles {
                let exists = crate::blueprint::vehicle_blueprint_exists(
                    std::path::Path::new(rw_path),
                    &veh.provider,
                    &veh.product,
                    &veh.blueprint,
                );
                flags.push(!exists);
                if !exists {
                    all_ok = false;
                }
            }
            ((c.name.clone(), all_ok), flags)
        })
        .unzip()
}

/// Загружает составы техники из Scenario.bin сценария (через Serz в XML).
pub(crate) fn load_consists(scenario_path: &Path, serz_path: &Path) -> Vec<ScenarioConsist> {
    let Some(xml) = read_scenario_file(scenario_path, serz_path) else {
        return Vec::new();
    };
    scenario_consists(&xml)
}

/// Читает файл сценария с составами: он может быть XML или бинарным.
pub(crate) fn read_scenario_file(scenario_path: &Path, serz_path: &Path) -> Option<String> {
    let binary = scenario_path.join("Scenario.bin");
    if binary.is_file()
        && let Ok(xml) = crate::assets::read_serz_xml(serz_path, &binary)
    {
        return Some(xml);
    }
    let text = scenario_path.join("Scenario.xml");
    if text.is_file() {
        return fs::read_to_string(text).ok();
    }
    None
}

/// Составы техники из файла сценария.
pub(crate) fn scenario_consists(xml: &str) -> Vec<ScenarioConsist> {
    let mut consists = Vec::new();
    let mut rest = xml;
    let mut consist_index = 0usize;
    while let Some(start) = rest.find("<cConsist") {
        rest = &rest[start..];
        let end = rest.find("</cConsist>").unwrap_or(rest.len());
        let consist = &rest[..end];
        let name = consist_name(consist);
        let vehicles = scenario_vehicles(consist);
        if !vehicles.is_empty() {
            let is_player = matches!(consist_kind(consist), ConsistKind::Player);
            consists.push(ScenarioConsist {
                name,
                vehicles,
                is_player,
                index: consist_index,
            });
        }
        consist_index += 1;
        rest = &rest[end..];
    }
    consists
}

/// Единицы техники в составе.
pub(crate) fn scenario_vehicles(consist: &str) -> Vec<ScenarioVehicle> {
    let mut vehicles = Vec::new();
    let mut rest = consist;
    while let Some(start) = rest.find("<cOwnedEntity") {
        rest = &rest[start..];
        let end = rest.find("</cOwnedEntity>").unwrap_or(rest.len());
        if let Some(vehicle) = scenario_vehicle(&rest[..end]) {
            vehicles.push(vehicle);
        }
        rest = &rest[end..];
    }
    vehicles
}

/// Данные об одной единице техники: файл модели и её название.
pub(crate) fn scenario_vehicle(entity: &str) -> Option<ScenarioVehicle> {
    let start = entity.find("<BlueprintID>")?;
    let end = entity[start..].find("<ReskinBlueprintID>")? + start;
    let block = absolute_blueprint_block(&entity[start..end])?;
    let provider = element_text(block, "Provider");
    let product = element_text(block, "Product");
    let blueprint = element_text(block, "BlueprintID");
    let name = element_text(entity, "Name");
    let number = ["Number", "UniqueNumber", "CarriageNumber"]
        .iter()
        .map(|tag| element_text(entity, tag))
        .find(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_owned());
    if blueprint.is_empty() && name.is_empty() {
        return None;
    }
    Some(ScenarioVehicle {
        provider,
        product,
        blueprint,
        name,
        number,
    })
}

/// Блок с абсолютной ссылкой на блюпринт.
pub(crate) fn absolute_blueprint_block(text: &str) -> Option<&str> {
    let start = text.find("<iBlueprintLibrary-cAbsoluteBlueprintID>")?;
    let end = text[start..].find("</iBlueprintLibrary-cAbsoluteBlueprintID>")? + start;
    Some(&text[start..end])
}
/// Текст элемента: содержимое между открывающим и закрывающим тегом.
pub(crate) fn element_text(xml: &str, element: &str) -> String {
    let open = format!("<{element}");
    let mut search_from = 0;
    while let Some(pos) = xml[search_from..].find(&open) {
        let abs = search_from + pos;
        let after = &xml[abs + open.len()..];
        if after.starts_with(' ') || after.starts_with('>') || after.starts_with('/') {
            let rest = &xml[abs..];
            let Some(close) = rest.find(&format!("</{element}>")) else {
                return String::new();
            };
            let value_start = rest.find('>').map(|index| index + 1).unwrap_or(0);
            let value = &rest[value_start..close];
            let text = match value.find('<') {
                Some(index) => &value[..index],
                None => value,
            };
            return crate::translit::decode_xml_text(text).trim().to_owned();
        }
        search_from = abs + open.len();
    }
    String::new()
}

/// Текст локализованного элемента: первый <English> внутри блока.
/// Предпочитает English, но при пустом значении берёт первый непустой
/// из остальных языков.
fn localised_text(xml: &str, block: &str) -> String {
    let open = format!("<{block}");
    let mut search_from = 0;
    while let Some(pos) = xml[search_from..].find(&open) {
        let abs = search_from + pos;
        let after = &xml[abs + open.len()..];
        if after.starts_with(' ') || after.starts_with('>') {
            let rest = &xml[abs..];
            let close_tag = format!("</{block}>");
            let end = rest.find(&close_tag).unwrap_or(rest.len());
            let block_xml = &rest[..end];
            // Сначала пробуем English.
            let english = element_text(block_xml, "English");
            if !english.is_empty() {
                return english;
            }
            // English пуст — берём первый непустой из остальных языков.
            for lang in [
                "German", "French", "Italian", "Spanish", "Dutch", "Polish", "Russian",
            ] {
                let val = element_text(block_xml, lang);
                if !val.is_empty() {
                    return val;
                }
            }
            return String::new();
        }
        search_from = abs + open.len();
    }
    String::new()
}

/// Время начала сценария: в файле оно в секундах от полуночи.
fn scenario_start_time(xml: &str) -> String {
    let seconds: i64 = element_text(xml, "StartTime").parse().unwrap_or_default();
    if seconds <= 0 {
        return String::new();
    }
    format!("{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60)
}

/// Преобразует блюпринт погоды в читаемое имя.
pub(crate) fn weather_display_name(
    rw_path: &Path,
    route_path: &Path,
    serz_path: &Path,
    blueprint: &str,
) -> String {
    let blueprint = blueprint.trim();
    if blueprint.is_empty() {
        return String::new();
    }
    let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
        rw_path, route_path, blueprint, "", "",
    );
    if let Some(path) = resolved {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let xml = if ext == "bin" {
            crate::assets::read_serz_xml(serz_path, &path).ok()
        } else {
            fs::read_to_string(&path).ok()
        };
        if let Some(xml) = xml {
            let mut name = element_text(&xml, "LocalisedDisplayName");
            if name.is_empty() {
                name = element_text(&xml, "Name");
            }
            // Если блюпринт обёрнут в cWeatherBlueprint, ищем внутри блока.
            if name.is_empty()
                && let Some(start) = xml.find("<cWeatherBlueprint")
            {
                let rest = &xml[start..];
                let tag_end = rest.find('>').unwrap_or(rest.len());
                let end = rest.find("</cWeatherBlueprint>").unwrap_or(rest.len());
                let block = &rest[tag_end + 1..end];
                name = element_text(block, "LocalisedDisplayName");
                if name.is_empty() {
                    name = element_text(block, "Name");
                }
                if name.is_empty() {
                    name = block.trim().to_owned();
                }
            }
            if !name.is_empty() {
                return name;
            }
        }
        if ext == "ap" {
            // Пытаемся прочитать архив как zip и найти xml с именем.
            // Минимально: возвращаем имя файла архива без расширения.
        }
    }
    let normalized = blueprint.replace('/', "\\");
    let path = Path::new(&normalized);
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| blueprint.to_owned())
}
fn scenario_date(xml: &str) -> String {
    let day: u32 = element_text(xml, "StartDD").parse().unwrap_or(0);
    let month: u32 = element_text(xml, "StartMM").parse().unwrap_or(0);
    let year: u32 = element_text(xml, "StartYYYY").parse().unwrap_or(0);
    if day == 0 || month == 0 || year == 0 {
        return String::new();
    }
    format!("{day:02}.{month:02}.{year}")
}
fn has_scenario_type_marker(xml: &str) -> bool {
    for tag in [
        "ScenarioClass",
        "ScenarioType",
        "ScenarioMode",
        "Type",
        "Class",
    ] {
        if !element_text(xml, tag).is_empty() {
            return true;
        }
    }
    if !element_text(xml, "QuickDrivePlayerConsistName").is_empty() {
        return true;
    }
    let lower = xml.to_lowercase().replace('_', " ");
    lower.contains("freeroam")
        || lower.contains("free roam")
        || lower.contains("quickdrive")
        || lower.contains("quick drive")
        || lower.contains("career")
        || lower.contains("<careerrules")
        || lower.contains("etemplate")
        || lower.contains("template scenario")
        || lower.contains("timetable")
        || lower.contains("etimetable")
}

/// Определяет тип сценария по XML свойств/сценария.
pub(crate) fn scenario_type_from_xml(xml: &str) -> String {
    // Явный тег ScenarioClass / аналоги.
    for tag in [
        "ScenarioClass",
        "ScenarioType",
        "ScenarioMode",
        "Type",
        "Class",
    ] {
        let value = element_text(xml, tag);
        if !value.is_empty() {
            let normalized = normalize_scenario_type(&value);
            if !normalized.is_empty() {
                return normalized;
            }
        }
    }

    // Быстрые выводы по структуре.
    if !element_text(xml, "QuickDrivePlayerConsistName").is_empty() {
        return "Quick Drive".to_owned();
    }
    let lower = xml.to_lowercase().replace('_', " ");
    if lower.contains("freeroam") || lower.contains("free roam") {
        return "Free Roam".to_owned();
    }
    if lower.contains("quickdrive")
        || lower.contains("quick drive")
        || lower.contains("etemplate")
        || lower.contains("template scenario")
    {
        return "Quick Drive".to_owned();
    }
    if lower.contains("timetable") || lower.contains("etimetable") {
        return "Timetable (Standard)".to_owned();
    }
    if lower.contains("<careerrules") {
        return "Career".to_owned();
    }
    if lower.contains("career") {
        return "Career".to_owned();
    }
    "Standard".to_owned()
}

fn normalize_scenario_type(value: &str) -> String {
    let v = value.trim().to_lowercase().replace(['_', '-'], " ");
    if v.contains("free") && v.contains("roam") {
        "Free Roam".to_owned()
    } else if v.contains("quick") && v.contains("drive") {
        "Quick Drive".to_owned()
    } else if v.contains("career") {
        "Career".to_owned()
    } else if v.contains("standard") || v.contains("normal") {
        "Standard".to_owned()
    } else if v.contains("timetable") {
        "Timetable (Standard)".to_owned()
    } else if v.contains("template") {
        "Quick Drive".to_owned()
    } else if v.is_empty() {
        String::new()
    } else {
        value.trim().to_owned()
    }
}

pub(crate) fn is_hidden_scenario_type(scenario_type: &str) -> bool {
    matches!(
        scenario_type.trim().to_lowercase().as_str(),
        "quick drive" | "free roam"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checks_real_route_x() {
        let rw_path = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks");
        let route = rw_path.join(r"Content\Routes\1f8f9c40-10bb-4310-ab5c-3a6a74893eb1");
        let serz = rw_path.join(r"serz.exe");
        if !route.is_dir() || !serz.is_file() {
            return;
        }
        let result = crate::assets::check_route(&route, &rw_path.join("Assets"), &serz, false, 1, |_| {});
        match result {
            Ok(r) => {
                println!(
                    "missing: {}, errors: {}, structure: {}",
                    r.missing_assets.len(),
                    r.conversion_errors.len(),
                    r.structure_issues.len()
                );
                if let Some(last) = r.conversion_errors.last() {
                    println!("последняя ошибка: {last}");
                }
            }
            Err(e) => panic!("{e}"),
        }
    }
}
