//! Serz conversion and asset-reference checks.

use super::*;
use std::collections::{HashMap, HashSet};
use walkdir::WalkDir;

pub fn is_safe_for_global_replacement(relative_path: &str, uses: &[crate::domain::AssetUse]) -> bool {
    let item = RouteAssetItem {
        path: relative_path.to_owned(),
        uses: uses.to_vec(),
    };
    !is_protected_network_asset(&item)
}

#[derive(Clone)]
pub struct RouteAssetItem {
    pub path: String,
    pub uses: Vec<crate::domain::AssetUse>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RouteAssetType {
    Scenery,
    Loft,
    Road,
    Track,
    TrackRules,
    Weather,
    Audio,
    Other,
}

impl RouteAssetType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scenery => "Сценери",
            Self::Loft => "Лофт",
            Self::Road => "Дорога",
            Self::Track => "Пути / Сигналы",
            Self::TrackRules => "Правило пути",
            Self::Weather => "Погода",
            Self::Audio => "Звук",
            Self::Other => "Прочее",
        }
    }
}

pub fn classify_route_asset_type(asset: &str, uses: &[crate::domain::AssetUse]) -> RouteAssetType {
    let mut track = false;
    let mut trackrules = false;
    let mut road = false;
    let mut loft = false;
    let mut weather = false;
    let mut audio = false;
    let mut scenery = false;

    for usage in uses {
        let source = usage.source_file.display().to_lowercase();
        if source.contains("trackrules") {
            trackrules = true;
        } else if source.contains("track tiles") || source.contains("tracks.bin") {
            track = true;
        } else if source.contains("road tiles") || source.contains("roads.bin") {
            road = true;
        } else if source.contains("loft tiles") || source.contains("lofts.bin") {
            loft = true;
        } else if source.contains("routeproperties.xml") {
            let asset_lower = asset.to_lowercase();
            if asset_lower.contains("sky")
                || asset_lower.contains("weather")
                || source.contains("weather")
            {
                weather = true;
            } else {
                scenery = true;
            }
        } else if source.contains("scenery") {
            scenery = true;
        }
        if source.contains("sounds") || source.contains("audio") {
            audio = true;
        }
    }

    let asset_lower = asset.to_lowercase();
    if asset_lower.ends_with(".wav")
        || asset_lower.ends_with(".ogg")
        || asset_lower.ends_with(".mp3")
    {
        audio = true;
    }

    if trackrules || asset_lower.contains("trackrules") {
        RouteAssetType::TrackRules
    } else if track {
        RouteAssetType::Track
    } else if road {
        RouteAssetType::Road
    } else if loft {
        RouteAssetType::Loft
    } else if weather {
        RouteAssetType::Weather
    } else if audio {
        RouteAssetType::Audio
    } else if scenery || asset_lower.contains("scenery") {
        RouteAssetType::Scenery
    } else if asset_lower.contains("track") {
        RouteAssetType::Track
    } else if asset_lower.contains("road") {
        RouteAssetType::Road
    } else if asset_lower.contains("loft") {
        RouteAssetType::Loft
    } else {
        RouteAssetType::Other
    }
}
pub fn is_protected_network_asset(asset: &RouteAssetItem) -> bool {
    if asset.uses.iter().any(|usage| {
        let path = usage.source_file.display().to_lowercase();
        path.contains("tracks.bin")
            || path.contains("/networks/")
            || path.contains("\\networks\\")
            || path.contains("trackrules")
            || path.contains("junctions")
    }) {
        return true;
    }

    let path_lower = asset.path.to_lowercase();
    if path_lower.contains("trackrules") || path_lower.contains("junctions") {
        return true;
    }

    if path_lower.contains("signal")
        && asset.uses.iter().any(|usage| {
            let path = usage.source_file.display().to_lowercase();
            path.contains("tracks.bin")
                || path.contains("/networks/")
                || path.contains("\\networks\\")
        })
    {
        return true;
    }

    false
}

pub(crate) fn is_skipped_route_scan_directory(name: &str, ignore_scenarios: bool) -> bool {
    ignore_scenarios && name.eq_ignore_ascii_case("Scenarios")
}

fn is_allowed_route_directory(name: &str, ignore_scenarios: bool) -> bool {
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
pub(crate) fn collect_archive_members(route_path: &Path) -> Result<Vec<crate::domain::RouteFile>, String> {
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

pub(crate) fn preferred_serz_path(rw_path: &Path) -> PathBuf {
    let serz64 = rw_path.join("serz64.exe");
    if serz64.is_file() {
        serz64
    } else {
        rw_path.join("serz.exe")
    }
}

pub(crate) fn fallback_serz_path(serz_path: &Path) -> Option<PathBuf> {
    if !serz_path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("serz64.exe"))
    {
        return None;
    }
    let fallback = serz_path.with_file_name("serz.exe");
    fallback.is_file().then_some(fallback)
}

pub(crate) fn find_serz_path(target: &Path) -> Option<PathBuf> {
    for ancestor in target.ancestors() {
        if ancestor
            .file_name()
            .map(|n| n.eq_ignore_ascii_case("Assets"))
            .unwrap_or(false)
            && let Some(root) = ancestor.parent()
        {
            for name in ["serz64.exe", "serz.exe"] {
                let p = root.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    for mut dir in [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|x| x.to_path_buf())),
    ]
    .into_iter()
    .flatten()
    {
        loop {
            for name in ["serz64.exe", "serz.exe"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
            if !dir.pop() {
                break;
            }
        }
    }
    None
}

struct FileAnalysis {
    /// Разбор взят из кэша: файл не менялся с прошлой проверки.
    from_cache: bool,
    file: crate::domain::RouteFile,
    /// Сколько раз каждый ассет встречается в файле.
    uses: Vec<(String, u32, crate::domain::AssetKind)>,
    error: Option<String>,
}

/// Буквы, которые выглядят одинаково, но различаются кодом.
///
/// В именах файлов игры встречается кириллическая «Е» вместо латинской: такой
/// файл нужно найти, но спутать его с другим нельзя.
const HOMOGLYPHS: [(char, char); 46] = [
    ('а', 'a'),
    ('б', 'b'),
    ('в', 'b'),
    ('г', 'r'),
    ('д', 'd'),
    ('е', 'e'),
    ('ё', 'e'),
    ('ж', 'x'),
    ('з', '3'),
    ('и', 'u'),
    ('й', 'u'),
    ('к', 'k'),
    ('л', 'l'),
    ('м', 'm'),
    ('н', 'h'),
    ('о', 'o'),
    ('п', 'n'),
    ('р', 'p'),
    ('с', 'c'),
    ('т', 't'),
    ('у', 'y'),
    ('ф', 'f'),
    ('х', 'x'),
    ('ц', 'u'),
    ('ч', '4'),
    ('ш', 'w'),
    ('щ', 'w'),
    ('ъ', 'b'),
    ('ы', 'b'),
    ('ь', 'b'),
    ('э', 'e'),
    ('ю', 'i'),
    ('я', 'r'),
    ('с', 'c'),
    ('А', 'A'),
    ('В', 'B'),
    ('Е', 'E'),
    ('К', 'K'),
    ('М', 'M'),
    ('Н', 'H'),
    ('О', 'O'),
    ('Р', 'P'),
    ('С', 'C'),
    ('Т', 'T'),
    ('У', 'Y'),
    ('Х', 'X'),
];

/// Являются ли две буквы похожими начертанием.
fn is_homoglyph(left: char, right: char) -> bool {
    HOMOGLYPHS.iter().any(|(first, second)| {
        (*first == left && *second == right) || (*first == right && *second == left)
    })
}

/// Совпадают ли имена, если считать похожие буквы одинаковыми.
///
/// Отличаться могут только буквы-«двойники» и испорченные символы: любые другие
/// различия означают, что это другой файл.
fn names_match(expected: &str, candidate: &str) -> bool {
    let expected: Vec<char> = expected.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    expected.len() == candidate.len()
        && expected.iter().zip(candidate.iter()).all(|(left, right)| {
            left == right
                || left.eq_ignore_ascii_case(right)
                || crate::translit::BROKEN_CHARACTERS.contains(left)
                || crate::translit::BROKEN_CHARACTERS.contains(right)
                || is_homoglyph(*left, *right)
        })
}

fn has_corrupted_or_nonascii(value: &str) -> bool {
    !value.is_ascii()
        || crate::translit::has_broken_character(value)
        || value.chars().any(|character| {
            matches!(character, '\u{FFFD}' | '?' | '¦' | '¢') || character.is_control()
        })
}

/// Ищет файл ассета по ссылке из маршрута.
///
/// Ссылка может содержать испорченные символы: сначала проверяются имена с
/// транслитерацией, затем — с настоящим умляутом. Так ассет, который на диске
/// называется с умляутом, не считается утерянным, а попадает в список на
/// переименование.
const GLITCH_WILDCARD: char = '\u{E000}';

fn normalize_fuzzy_segment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            let folded = match ch {
                'ä' | 'Ä' | 'á' | 'à' | 'â' | 'ã' | 'å' => 'a',
                'ö' | 'Ö' | 'ó' | 'ò' | 'ô' | 'õ' => 'o',
                'ü' | 'Ü' | 'ú' | 'ù' | 'û' => 'u',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'í' | 'ì' | 'î' | 'ï' => 'i',
                'ß' => 's',
                _ => ch.to_ascii_lowercase(),
            };
            out.push(folded);
        } else if matches!(ch, '¦' | '|' | '\u{FFFD}' | '?') {
            out.push(GLITCH_WILDCARD);
        }
    }
    out
}

fn fold_german_diphthongs(value: &str) -> String {
    value
        .replace("ae", "a")
        .replace("oe", "o")
        .replace("ue", "u")
}

fn glitch_match(query: &[char], candidate: &[char]) -> bool {
    if query.is_empty() {
        return candidate.is_empty();
    }
    if query[0] == GLITCH_WILDCARD {
        for extra in 1..=2 {
            if candidate.len() >= extra && glitch_match(&query[1..], &candidate[extra..]) {
                return true;
            }
        }
        false
    } else {
        !candidate.is_empty()
            && query[0] == candidate[0]
            && glitch_match(&query[1..], &candidate[1..])
    }
}

fn fuzzy_match_name(query: &str, candidate: &str) -> bool {
    let query_digits: String = query
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    let candidate_digits: String = candidate
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    if query_digits != candidate_digits {
        return false;
    }

    let left = normalize_fuzzy_segment(query);
    let right_raw = normalize_fuzzy_segment(candidate);
    if left.is_empty() || right_raw.is_empty() {
        return false;
    }

    let has_glitch = left.contains(GLITCH_WILDCARD);
    if !has_glitch {
        // Clean path: exact normalized equality only.
        return left == right_raw;
    }

    // Glitch tolerance: allow 1-2 characters in the candidate at the glitch
    // position. Try both raw and diphthong-folded candidate forms.
    let right_folded = fold_german_diphthongs(&right_raw);
    let left_chars: Vec<char> = left.chars().collect();
    let right_raw_chars: Vec<char> = right_raw.chars().collect();
    let right_folded_chars: Vec<char> = right_folded.chars().collect();
    glitch_match(&left_chars, &right_raw_chars) || glitch_match(&left_chars, &right_folded_chars)
}

fn find_fuzzy_child(parent: &Path, query: &str, want_directory: bool) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() != want_directory {
                continue;
            }
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if fuzzy_match_name(query, &name) {
                candidates.push(path);
            }
        }
    }
    candidates
        .iter()
        .find(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().is_ascii())
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn fuzzy_resolve_disk_path(assets_path: &Path, relative_asset: &str) -> Option<(PathBuf, String)> {
    let normalized = relative_asset.replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let mut current = assets_path.join(parts[0]).join(parts[1]);
    if !current.is_dir() {
        current = find_fuzzy_child(assets_path, parts[0], true)?.join(parts[1]);
        if !current.is_dir() {
            return None;
        }
    }
    let relative_segments = &parts[2..];
    let last_index = relative_segments.len().saturating_sub(1);
    let mut matched_segments = Vec::new();
    for (index, segment) in relative_segments.iter().enumerate() {
        if index == last_index {
            let matched = find_fuzzy_child(&current, segment, false)?;
            let name = matched
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())?;
            matched_segments.push(name);
            let disk_path = matched;
            let relative_repair =
                format!("{}/{}/{}", parts[0], parts[1], matched_segments.join("/"));
            return Some((disk_path, relative_repair));
        }
        let matched = find_fuzzy_child(&current, segment, true)?;
        let name = matched
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())?;
        matched_segments.push(name);
        current = matched;
    }
    None
}

fn resolve_asset_path(assets_path: &Path, relative_asset: &str) -> Result<Option<PathBuf>, String> {
    let exact = assets_path.join(relative_asset.replace('/', "\\"));
    if exact.is_file() {
        return Ok(Some(exact));
    }
    if !crate::translit::has_broken_character(relative_asset) {
        // Файла нет по точному имени: ищем ближайший по сходству. В игре
        // встречаются имена с кириллическими буквами вместо латинских.
        return Ok(find_similar_asset_path(assets_path, relative_asset));
    }
    let broken_count = relative_asset
        .chars()
        .filter(|character| crate::translit::BROKEN_CHARACTERS.contains(character))
        .count();
    if broken_count > crate::translit::MAX_BROKEN_CHARACTERS {
        return Ok(None);
    }

    let tiers = [
        crate::translit::replace_broken_characters_combinations(relative_asset, &crate::translit::TRANSLITERATED_CHARACTERS),
        crate::translit::replace_broken_characters_combinations(relative_asset, &crate::translit::UMLAUT_CHARACTERS),
    ];

    // Windows считает «ä» и «Ä» одним и тем же именем файла, поэтому одинаковые
    // файлы из разных вариантов отсекаются по каноническому пути.
    let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
    for tier in tiers {
        for variant in tier {
            let candidate = assets_path.join(variant.replace('/', "\\"));
            if !candidate.is_file() {
                continue;
            }
            let identity = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if !candidates.iter().any(|(_, existing)| *existing == identity) {
                candidates.push((candidate, identity));
            }
        }
    }
    if let Some(position) = candidates
        .iter()
        .position(|(path, _)| path.to_string_lossy().is_ascii())
    {
        return Ok(Some(candidates.remove(position).0));
    }
    match candidates.len() {
        0 => Ok(find_similar_asset_path(assets_path, relative_asset)),
        1 => Ok(Some(candidates.remove(0).0)),
        _ => Err(format!(
            "Ссылка содержит испорченный символ и совпадает с несколькими файлами: {}",
            candidates
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Ищет файл, похожий на указанный путь.
///
/// Имена в игре иногда отличаются от ссылок одной буквой: кириллическая «Е»
/// вместо латинской или испорченный символ. Тогда ищем ближайшее по сходству.
fn find_similar_asset_path(assets_path: &Path, relative_asset: &str) -> Option<PathBuf> {
    let relative = relative_asset.replace('/', "\\");
    let mut current = assets_path.to_path_buf();
    let components: Vec<&str> = relative
        .split('\\')
        .filter(|part| !part.is_empty())
        .collect();
    for (index, component) in components.iter().enumerate() {
        if index + 1 == components.len() {
            // Сначала имя целиком, потом с расширениями: в ссылке расширение
            // может быть, а может и отсутствовать.
            if let Some(path) = find_entry_in_directory(&current, component, false) {
                return Some(path);
            }
            for extension in ["bin", "GeoPcDx", "TgPcDx", "xml"] {
                let wanted = format!("{component}.{extension}");
                if let Some(path) = find_entry_in_directory(&current, &wanted, false) {
                    return Some(path);
                }
            }
            return None;
        }
        current = find_entry_in_directory(&current, component, true)?;
    }
    None
}

pub(crate) fn read_serz_xml(serz_path: &Path, asset_path: &Path) -> Result<String, String> {
    let temp = tempdir().map_err(|error| format!("Не удалось создать временную папку: {error}"))?;
    let xml_path = temp.path().join("asset.xml");
    let conversion = convert_bin_to_xml(serz_path, asset_path, &xml_path).or_else(|error| {
        let Some(fallback) = fallback_serz_path(serz_path) else {
            return Err(error);
        };
        convert_bin_to_xml(&fallback, asset_path, &xml_path).map_err(|fallback_error| {
            format!(
                "{error}; повторная попытка через {} не удалась: {fallback_error}",
                fallback.display()
            )
        })
    });
    conversion?;
    let bytes = fs::read(&xml_path)
        .map_err(|error| format!("Не удалось прочитать XML {}: {error}", xml_path.display()))?;
    Ok(decode_xml_bytes(&bytes))
}

/// Ссылка на объект ассета внутри XML RailWorks.
///
/// В игре такая ссылка выглядит как `Provider\Product\папка\[00]имя`: без
/// расширения и с суффиксом варианта. Иногда расширение в ссылке всё же есть,
/// поэтому разбираются оба вида.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GeometryReference {
    /// Путь без суффикса варианта и без расширения.
    path: String,
    /// Суффикс варианта, например `[00]`.
    variant: String,
    /// Расширение из ссылки, если оно там было.
    extension: Option<String>,
}

impl GeometryReference {
    /// Разбирает текст ссылки из XML.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        // Ссылка состоит из папки, суффикса варианта, имени файла и иногда
        // расширения: `Provider\Product\папка\[00]имя.GeoPcDx`.
        let (folder, name) = match text.rfind(['\\', '/']) {
            Some(index) => (&text[..index], &text[index + 1..]),
            None => ("", text),
        };
        // Суффикс варианта стоит в начале имени файла: [00], [01] и так далее.
        let (variant, name) = match name.find(']') {
            Some(index) if name.starts_with('[') && index > 1 => {
                let digits = &name[1..index];
                if !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_digit()) {
                    (&name[..=index], &name[index + 1..])
                } else {
                    ("", name)
                }
            }
            _ => ("", name),
        };
        // Расширение тоже может быть указано явно.
        let (name, extension) = match Path::new(name).extension() {
            Some(extension) => {
                let extension = extension.to_string_lossy().into_owned();
                if extension.eq_ignore_ascii_case("GeoPcDx")
                    || extension.eq_ignore_ascii_case("TgPcDx")
                {
                    (&name[..name.len() - extension.len() - 1], Some(extension))
                } else {
                    (name, None)
                }
            }
            None => (name, None),
        };
        Some(Self {
            path: if folder.is_empty() {
                name.to_owned()
            } else {
                format!("{folder}\\{name}")
            },
            variant: variant.to_owned(),
            extension,
        })
    }

    /// Указывает ли ссылка на объект ассета.
    fn is_geometry_reference(&self) -> bool {
        self.extension.is_some() || !self.variant.is_empty()
    }

    /// Расширение, с которым файл лежит на диске.
    ///
    /// Если расширения в ссылке не было, файл может оказаться и геометрией, и
    /// текстурой: проверяются оба варианта.
    fn disk_extensions(&self) -> Vec<String> {
        match &self.extension {
            Some(extension) => vec![extension.clone()],
            None => vec!["GeoPcDx".to_owned(), "TgPcDx".to_owned()],
        }
    }

    /// Текст ссылки в исходном виде.
    fn to_text(&self) -> String {
        let mut text = String::new();
        let (folder, name) = match self.path.rfind(['\\', '/']) {
            Some(index) => (&self.path[..=index], &self.path[index + 1..]),
            None => ("", self.path.as_str()),
        };
        text.push_str(folder);
        text.push_str(&self.variant);
        text.push_str(name);
        if let Some(extension) = &self.extension {
            text.push('.');
            text.push_str(extension);
        }
        text
    }
}

/// Относительно чего задан путь в ссылке на объект.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceBase {
    /// Относительно папки Assets.
    Assets,
    /// Относительно папки продукта.
    Product,
    /// Относительно папки файла, в котором встретилась ссылка.
    Folder,
}

fn geometry_reference_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)>([^<>]*\.(?:GeoPcDx|TgPcDx))\s*</[^>]+>"#)
            .expect("Шаблон поиска ссылок на геометрию должен быть корректным")
    })
}

fn variant_reference_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)>([^<>]*\[[0-9]{2,3}\][^<>]*)\s*</[^>]+>"#)
            .expect("Шаблон поиска ссылок с вариантом должен быть корректным")
    })
}

/// Находит в XML ссылки на объекты ассетов.
///
/// Учитываются оба вида ссылок: с расширением (`.GeoPcDx`) и без него, но с
/// суффиксом варианта (`[00]`) — именно так их пишет игра.
fn referenced_geometry_paths(xml: &str) -> Vec<(GeometryReference, std::ops::Range<usize>)> {
    let mut found: Vec<(GeometryReference, std::ops::Range<usize>)> = Vec::new();
    for regex in [geometry_reference_regex(), variant_reference_regex()] {
        for captures in regex.captures_iter(xml) {
            let Some(matched) = captures.get(1) else {
                continue;
            };
            let text = crate::translit::decode_xml_text(matched.as_str().trim());
            let Some(reference) = GeometryReference::parse(&text) else {
                continue;
            };
            if !reference.is_geometry_reference() {
                continue;
            }
            let range = crate::reference::capture_value_range(&matched);
            if found
                .iter()
                .any(|(_, existing_range)| existing_range == &range)
            {
                continue;
            }
            found.push((reference, range));
        }
    }
    found.sort_by_key(|(_, range)| range.start);
    found
}

/// Описание ссылки, которая не нашлась на диске.
///
/// В сообщение попадает содержимое папки: без него непонятно, что именно не
/// совпало с именем файла.
fn unresolved_reference_error(
    source: &Path,
    reference: &GeometryReference,
    assets_root: &Path,
    product_root: &Path,
) -> String {
    let mut message = format!(
        "Ссылка с умляутом не разрешается в файл: {} → {}",
        source.display(),
        reference.to_text()
    );
    let mut folders = vec![
        source.parent().map(Path::to_path_buf),
        Some(product_root.to_path_buf()),
        Some(assets_root.to_path_buf()),
    ];
    folders.retain(|folder| folder.is_some());
    for folder in folders.into_iter().flatten() {
        if !folder.is_dir() {
            continue;
        }
        let names: Vec<String> = fs::read_dir(&folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .take(40)
            .collect();
        if names.is_empty() {
            continue;
        }
        message.push_str(&format!(
            "\nВ папке {}: {}",
            folder.display(),
            names.join(", ")
        ));
        break;
    }
    message
}

/// Ссылка на объект, путь которого указан относительно папки продукта.
///
/// Символы в ссылке могут быть испорчены, поэтому перебираются варианты имени с
/// настоящими умляутами.
fn resolve_product_reference(
    assets_root: &Path,
    product_root: &Path,
    source_file: &Path,
    reference: &GeometryReference,
) -> Option<(PathBuf, ReferenceBase)> {
    let base = reference.path.replace('/', "\\");
    let mut names = vec![base.clone()];
    if crate::translit::has_broken_character(&base) {
        let broken_count = base
            .chars()
            .filter(|character| crate::translit::BROKEN_CHARACTERS.contains(character))
            .count();
        if broken_count <= crate::translit::MAX_REFERENCE_VARIANTS {
            // Испорченный символ в ссылке может означать и умляут, и уже
            // переписанное имя, причём в разных частях пути по-разному, поэтому
            // проверяются все сочетания.
            let mut replacements: Vec<&str> = crate::translit::TRANSLITERATED_CHARACTERS.to_vec();
            replacements.extend_from_slice(&crate::translit::UMLAUT_CHARACTERS);
            names.extend(crate::translit::replace_broken_characters_combinations(&base, &replacements));
        }
    }
    let extensions = reference.disk_extensions();
    let bases = [
        (assets_root.to_path_buf(), ReferenceBase::Assets),
        (product_root.to_path_buf(), ReferenceBase::Product),
        (source_file.parent()?.to_path_buf(), ReferenceBase::Folder),
    ];
    for (directory, kind) in &bases {
        for name in &names {
            for extension in &extensions {
                let candidate = directory.join(format!("{name}.{extension}"));
                if candidate.is_file() {
                    return Some((candidate, *kind));
                }
            }
        }
    }
    // Точного совпадения нет: ищем ближайшее по сходству. Имя папки может
    // отличаться регистром, кириллическими буквами или количеством букв.
    for (directory, kind) in &bases {
        if let Some(path) = resolve_reference_path(directory, &base, &extensions) {
            return Some((path, *kind));
        }
    }
    None
}

/// Ищет в папке файл или подпапку с подходящим именем.
///
/// Сначала проверяются точное имя и регистр, а при неудаче ищется ближайшее по
/// сходству: в игре встречаются имена с испорченными символами и с
/// кириллическими буквами вместо латинских.
fn find_entry_in_directory(
    directory: &Path,
    wanted: &str,
    want_directory: bool,
) -> Option<PathBuf> {
    let exact = directory.join(wanted);
    if exact.is_dir() == want_directory && (exact.is_file() || exact.is_dir()) {
        return Some(exact);
    }
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() != want_directory {
            continue;
        }
        let Some(file_name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if file_name.eq_ignore_ascii_case(wanted) {
            return Some(path);
        }
        // Похожие буквы («Е» кириллическая вместо латинской) считаем совпадением,
        // а вот любые другие различия — нет: это другой файл.
        if names_match(wanted, &file_name) {
            return Some(path);
        }
    }
    None
}

/// Разрешает путь ссылки, подбирая похожие имена файлов и папок.
fn resolve_reference_path(base: &Path, relative: &str, extensions: &[String]) -> Option<PathBuf> {
    let mut current = base.to_path_buf();
    let components: Vec<_> = relative
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    for (index, component) in components.iter().enumerate() {
        if index + 1 == components.len() {
            for extension in extensions {
                let wanted = format!("{component}.{extension}");
                if let Some(path) = find_entry_in_directory(&current, &wanted, false) {
                    return Some(path);
                }
            }
            return None;
        }
        current = find_entry_in_directory(&current, component, true)?;
    }
    None
}

/// Результат проверки цепочки ассета на сломанные символы.
pub(crate) struct UmlautChainEvidence {
    /// Обнаружения: что и где нашлось.
    pub(crate) evidence: Vec<String>,
    /// Символы, которые игра не умеет загружать.
    pub(crate) unsupported: std::collections::BTreeSet<char>,
}

fn asset_chain_umlaut_evidence(
    assets_path: &Path,
    serz_path: &Path,
    root_asset: &Path,
) -> Result<UmlautChainEvidence, String> {
    let relative = root_asset
        .strip_prefix(assets_path)
        .map_err(|_| format!("Ассет находится вне Assets: {}", root_asset.display()))?;
    let mut relative_components = relative.components();
    let provider = relative_components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    let product = relative_components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    let product_root = assets_path.join(provider).join(product);
    // Работаем только с файлами рядом с ассетом: соседние папки продукта
    // относятся к другим объектам, и трогать их не нужно.
    let scope_root = root_asset
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| product_root.clone());
    let mut queue = vec![root_asset.to_path_buf()];
    let mut visited = HashSet::new();
    let mut evidence = Vec::new();
    let mut unsupported = std::collections::BTreeSet::new();

    while let Some(asset_path) = queue.pop() {
        let canonical = match asset_path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "Не удалось проверить {}: {error}",
                    asset_path.display()
                ));
            }
        };
        if !canonical.starts_with(&scope_root) || !visited.insert(canonical.clone()) {
            continue;
        }
        let extension = canonical
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !extension.eq_ignore_ascii_case("bin") && !extension.eq_ignore_ascii_case("GeoPcDx") {
            continue;
        }
        let xml = read_serz_xml(serz_path, &canonical)?;
        if crate::translit::has_umlauts_in_xml(&xml) {
            evidence.push(format!("Внутри файла: {}", canonical.display()));
        }
        for (reference, _) in referenced_geometry_paths(&xml) {
            let reference_text = reference.to_text();
            if crate::translit::has_umlauts(&reference_text) {
                evidence.push(format!(
                    "Ссылка из {}: {reference_text}",
                    canonical.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            // Символы, которые игра не грузит, пользователь заменяет сам.
            unsupported.extend(crate::translit::unsupported_characters(&reference_text));
            let Some((path, _)) =
                resolve_product_reference(assets_path, &product_root, &canonical, &reference)
            else {
                continue;
            };
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("GeoPcDx"))
            {
                queue.push(path);
            }
        }
    }

    evidence.sort();
    evidence.dedup();
    Ok(UmlautChainEvidence {
        evidence,
        unsupported,
    })
}

pub(crate) fn inspect_umlaut_asset(
    relative_asset: &str,
    assets_path: &Path,
    serz_path: &Path,
) -> (Option<crate::domain::UmlautAsset>, Option<String>) {
    let mut evidence = Vec::new();
    let mut unsupported = crate::translit::unsupported_characters(relative_asset);
    let path_has_umlauts = crate::translit::has_umlauts(relative_asset);
    if path_has_umlauts {
        evidence.push(format!("Умляут в пути ассета: {relative_asset}"));
    }
    let mut scan_error = None;
    let full_asset_path = match resolve_asset_path(assets_path, relative_asset) {
        Ok(Some(path)) => Some(path),
        Ok(None) => None,
        Err(error) => {
            evidence.push(error.clone());
            scan_error = Some(error);
            None
        }
    };
    if let Some(asset_path) = full_asset_path.as_ref() {
        // Файл может лежать по верному пути, но называться иначе, чем в ссылке:
        // тогда его нужно переименовать, а не искать замену.
        let expected_name = Path::new(relative_asset).file_name();
        let actual_name = asset_path.file_name();
        if expected_name != actual_name {
            evidence.push(format!(
                "Файл на диске называется иначе: {}",
                asset_path.display()
            ));
        }
    }
    if let Some(asset_path) = full_asset_path.clone().filter(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
    }) {
        match asset_chain_umlaut_evidence(assets_path, serz_path, &asset_path) {
            Ok(found) => {
                evidence.extend(found.evidence);
                unsupported.extend(found.unsupported);
            }
            Err(error) => {
                evidence.push(format!(
                    "Цепочка зависимостей не проверена полностью: {error}"
                ));
                scan_error = Some(error);
            }
        }
    }
    // Ассета, которого нет на диске, чинить нечем: для него есть замена.
    let asset = (full_asset_path.is_some() && (!evidence.is_empty() || !unsupported.is_empty()))
        .then(|| crate::domain::UmlautAsset {
            path: relative_asset.to_owned(),
        });
    (asset, scan_error)
}

pub(crate) fn scan_umlaut_assets(
    inventory: &[crate::domain::ReferencedAsset],
    assets_path: &Path,
    serz_path: &Path,
    max_concurrent_serz: usize,
) -> Result<(Vec<crate::domain::UmlautAsset>, Vec<String>), String> {
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(max_concurrent_serz.max(1))
        .min(inventory.len().max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
        .map_err(|error| format!("Не удалось создать пул проверки умляутов: {error}"))?;
    let scan: Vec<_> = pool.install(|| {
        inventory
            .par_iter()
            .map(|asset| inspect_umlaut_asset(&asset.path, assets_path, serz_path))
            .collect()
    });
    let mut umlaut_assets = Vec::new();
    let mut errors = Vec::new();
    for (asset, error) in scan {
        if let Some(asset) = asset {
            umlaut_assets.push(asset);
        }
        if let Some(error) = error {
            errors.push(error);
        }
    }
    umlaut_assets.sort_by(|left, right| left.path.cmp(&right.path));
    errors.sort();
    Ok((umlaut_assets, errors))
}

pub(crate) fn decode_xml_bytes(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\u{FFFD}')
    {
        return text.to_owned();
    }
    let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
    decoded.into_owned()
}

fn analyze_route_file(
    route_file: &crate::domain::RouteFile,
    index: usize,
    temp_dir: &Path,
    serz_path: &Path,
) -> FileAnalysis {
    // Запись архива читается через временный файл: конвертеру Serz нужен
    // настоящий файл на диске.
    let file_path = match route_file.materialize(temp_dir) {
        Ok(path) => path,
        Err(error) => {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: Some(error),
            };
        }
    };
    let is_binary = file_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
    let xml_data = if is_binary {
        if fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            < 256
        {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: None,
            };
        }
        // Serz хранит имена элементов обычными строками, поэтому файл без
        // `BlueprintID` заведомо не содержит ссылок на ассеты. Такие файлы не
        // конвертируются: на больших маршрутах это основная часть работы.
        if !may_contain_asset_reference(&file_path) {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: None,
            };
        }
        let xml_path = temp_dir.join(format!("{index:08}.xml"));
        let conversion_result = convert_bin_to_xml(serz_path, &file_path, &xml_path).or_else(
            |primary_error| {
                let Some(fallback) = fallback_serz_path(serz_path) else {
                    return Err(primary_error);
                };
                convert_bin_to_xml(&fallback, &file_path, &xml_path).map_err(|fallback_error| {
                    format!(
                        "{primary_error}; повторная попытка через {} тоже не удалась: {fallback_error}",
                        fallback.display()
                    )
                })
            },
        );
        if let Err(error) = conversion_result {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: Some(format!("{}: {error}", file_path.display())),
            };
        }
        match fs::read(xml_path) {
            Ok(data) => data,
            Err(error) => {
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    uses: Vec::new(),
                    error: Some(format!(
                        "{}: не удалось прочитать результат Serz: {error}",
                        file_path.display()
                    )),
                };
            }
        }
    } else {
        match fs::read(&file_path) {
            Ok(data) => data,
            Err(error) => {
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    uses: Vec::new(),
                    error: Some(format!(
                        "{}: не удалось прочитать XML: {error}",
                        file_path.display()
                    )),
                };
            }
        }
    };

    let xml_text = decode_xml_bytes(&xml_data);

    let references = crate::reference::extract_asset_references_with_locations(&xml_text);

    FileAnalysis {
        from_cache: false,
        file: route_file.clone(),
        uses: count_asset_uses(references),
        error: None,
    }
}

/// Считает, сколько раз каждый ассет встречается в файле.
///
/// Порядок первого появления сохраняется: по нему собирается список
/// использований в инвентаре. Вместе с ассетом запоминается, что он из себя
/// представляет: объект маршрута или файл состава.
fn count_asset_uses(references: Vec<crate::reference::AssetReference>) -> Vec<(String, u32, crate::domain::AssetKind)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut kinds: HashMap<String, crate::domain::AssetKind> = HashMap::new();
    for reference in references {
        let counter = counts.entry(reference.path.clone()).or_insert_with(|| {
            order.push(reference.path.clone());
            kinds.insert(reference.path.clone(), reference.kind);
            0
        });
        *counter += 1;
    }
    order
        .into_iter()
        .map(|path| {
            let count = counts.get(&path).copied().unwrap_or_default();
            let kind = kinds.get(&path).copied().unwrap_or(crate::domain::AssetKind::Other);
            (path, count, kind)
        })
        .collect()
}

/// Может ли файл содержать ссылку на ассет.
///
/// Serz хранит имена элементов обычными строками, поэтому файл без подстроки
/// `BlueprintID` не содержит ни одной ссылки на ассет: его конвертация в XML
/// не нужна. При неудачном чтении файл считается подозрительным и проверяется
/// обычным порядком.
fn may_contain_asset_reference(file_path: &Path) -> bool {
    let Ok(data) = fs::read(file_path) else {
        return true;
    };
    data.windows(crate::translit::BLUEPRINT_ID_MARKER.len())
        .any(|window| window == crate::translit::BLUEPRINT_ID_MARKER.as_bytes())
}

/// Ставит изменённый файл маршрута на место.
///
/// Для файла из архива .ap копия кладётся рядом с архивом: игра читает сначала
/// архив, а потом файлы рядом с ним, поэтому изменённая версия перекроет
/// оригинальную, а сам архив останется нетронутым.
fn install_route_file(
    route_path: &Path,
    source_file: &crate::domain::RouteFile,
    converted_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let target = source_file.install_target(route_path);
    // У записи из архива оригинала на диске нет: резервная копия не нужна, а
    // изменённая версия просто кладётся рядом с архивом.
    let original = match source_file {
        crate::domain::RouteFile::Path(path) => Some(path.clone()),
        crate::domain::RouteFile::Archive { .. } => None,
    };
    if let Some(original) = original
        && target == original
    {
        return install_converted_asset(&original, converted_path).map(Some);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Не удалось создать папку {}: {error}", parent.display()))?;
    }
    fs::copy(converted_path, &target)
        .map_err(|error| format!("Не удалось записать {}: {error}", target.display()))?;
    Ok(None)
}

pub(crate) fn check_route<F>(
    route_path: &Path,
    assets_path: &Path,
    serz_path: &Path,
    ignore_scenarios: bool,
    max_concurrent_serz: usize,
    report_progress: F,
) -> Result<crate::domain::CheckResult, String>
where
    F: Fn(&str) + Sync,
{
    if !serz_path.is_file() {
        return Err(format!(
            "Не найден serz.exe: {}. Выберите корневую папку RailWorks.",
            serz_path.display()
        ));
    }

    report_progress("Сканирование файлов маршрута...");
    let collected_route_files = collect_route_files(route_path, ignore_scenarios)?;

    let mut structure_issues = Vec::new();
    if !route_path.join("RouteProperties.xml").is_file() {
        structure_issues.push("не найден RouteProperties.xml".to_owned());
    }
    if !assets_path.is_dir() {
        structure_issues.push("не найдена папка Assets".to_owned());
    }
    if collected_route_files.is_empty() {
        structure_issues.push("не найдено файлов .bin или .xml для проверки".to_owned());
    }
    let unexpected_directories = unexpected_route_directories(route_path)?;
    if !unexpected_directories.is_empty() {
        structure_issues.push(format!(
            "неизвестные папки маршрута не проверялись: {}",
            unexpected_directories.join(", ")
        ));
    }
    let leftover_work_directories = leftover_temporary_work_directories(route_path)?;
    if !leftover_work_directories.is_empty() {
        structure_issues.push(format!(
            "в маршруте остались рабочие каталоги программы (можно удалить): {}",
            leftover_work_directories.join(", ")
        ));
    }

    // Основные файлы маршрута могут быть упакованы в .ap...
    let mut route_files: Vec<crate::domain::RouteFile> = collected_route_files
        .into_iter()
        .map(crate::domain::RouteFile::Path)
        .collect();
    let outside: Vec<String> = route_files
        .iter()
        .filter_map(|file| file.route_name(route_path))
        .collect();
    for member in collect_archive_members(route_path)? {
        let name = member.route_name(route_path).unwrap_or_default();
        if outside
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            // Такой файл лежит рядом с архивом и перекрывает запись архива.
            continue;
        }
        route_files.push(member);
    }

    // Тяжёлый глобальный файл сети путей проверяем отдельно и в самом конце:
    // его конвертация занимает минуты и не должна задерживать остальные файлы
    // и отчёты о ходе проверки.
    let is_tracks_bin = |route_file: &crate::domain::RouteFile| {
        route_file
            .route_name(route_path)
            .map(|name| name.eq_ignore_ascii_case("Networks/Tracks.bin"))
            .unwrap_or(false)
    };
    route_files.sort_by_key(|a| is_tracks_bin(a));
    let normal_files: Vec<crate::domain::RouteFile> = route_files
        .iter()
        .filter(|file| !is_tracks_bin(file))
        .cloned()
        .collect();
    let heavy_files: Vec<crate::domain::RouteFile> = route_files
        .iter()
        .filter(|file| is_tracks_bin(file))
        .cloned()
        .collect();
    // LPT: тяжёлые файлы (Tracks.bin) ставим первыми, чтобы пул не простаивал.
    let mut all_files = heavy_files;
    all_files.extend(normal_files.iter().cloned());

    let temp_dir = tempdir()
        .map_err(|error| format!("Не удалось создать временную папку для Serz: {error}"))?;

    // Архивы маршрута (.ap) не распаковываются целиком: записи materialize()
    // читаются из zip по требованию только для анализируемых .bin/.xml файлов.

    let normal_binary_count = all_files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        })
        .count();
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(max_concurrent_serz.max(1))
        .min(normal_binary_count.max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
        .map_err(|error| format!("Не удалось создать пул проверки файлов: {error}"))?;

    let mut analyses = Vec::with_capacity(route_files.len());
    // Кэш разбора лежит в папке маршрута: неизменившиеся файлы берутся из
    // него, а заново разбираются только те, что изменились или появились.
    let mut cache = crate::analysis_cache::AnalysisCache::load(route_path);
    for (chunk_index, chunk) in all_files.chunks(32).enumerate() {
        let start_index = chunk_index * 32;
        report_progress(&format!(
            "Анализ файлов: {} из {}; параллельных Serz-процессов: {}...",
            start_index + 1,
            all_files.len(),
            worker_count.min(normal_binary_count.max(1))
        ));
        let chunk_analyses: Vec<FileAnalysis> = pool.install(|| {
            chunk
                .par_iter()
                .enumerate()
                .map(|(offset, route_file)| {
                    let name = route_file.route_name(route_path);
                    let stamp = route_file.stamp();
                    if let (Some(name), Some(stamp)) = (&name, stamp)
                        && let Some(cached) = cache.get(name, stamp)
                    {
                        return FileAnalysis {
                            from_cache: true,
                            file: route_file.clone(),
                            uses: cached.uses.clone(),
                            error: None,
                        };
                    }
                    analyze_route_file(route_file, start_index + offset, temp_dir.path(), serz_path)
                })
                .collect()
        });
        analyses.extend(chunk_analyses);
    }
    let mut asset_uses: HashMap<String, Vec<crate::domain::AssetUse>> = HashMap::new();
    // Написание пути, как оно встретилось в маршруте: ключ в asset_uses приведён
    // к нижнему регистру, потому что игра регистр не различает.
    let mut asset_names: HashMap<String, String> = HashMap::new();
    let mut conversion_errors = Vec::new();
    for analysis in analyses {
        if !analysis.from_cache {
            // В кэш попадают только заново разобранные файлы: для остальных
            // прежняя запись и так актуальна.
            if let Some(name) = analysis.file.route_name(route_path)
                && let Some(stamp) = analysis.file.stamp()
            {
                cache.put(
                    name,
                    crate::analysis_cache::CachedFile {
                        stamp,
                        uses: analysis.uses.clone(),
                    },
                );
            }
        }
        let tile = crate::reference::route_tile_label(route_path, &analysis.file);
        for (asset, count, kind) in analysis.uses {
            // Игра читает пути без учёта регистра, поэтому ссылки, отличающиеся
            // регистром, относятся к одному ассету.
            let key = asset_key(&asset);
            asset_names
                .entry(key.clone())
                .or_insert_with(|| asset.clone());
            let uses = asset_uses.entry(key).or_default();
            for _ in 0..count {
                uses.push(crate::domain::AssetUse {
                    source_file: analysis.file.clone(),
                    tile: tile.clone(),
                    tile_kind: analysis.file.tile_kind(),
                    kind,
                });
            }
        }
        if let Some(error) = analysis.error {
            conversion_errors.push(format!(
                "{} — {error}",
                analysis
                    .file
                    .route_name(route_path)
                    .unwrap_or_else(|| analysis.file.display())
            ));
        }
    }
    conversion_errors.sort();

    // Удаляем из кэша файлы, которых в маршруте больше нет, и сохраняем кэш.
    let current_files: HashMap<String, ()> = route_files
        .iter()
        .filter_map(|path| path.route_name(route_path))
        .map(|name| (name, ()))
        .collect();
    cache.retain(&current_files);
    if let Err(error) = cache.save(route_path) {
        structure_issues.push(format!("Кэш разбора не сохранён: {error}"));
    }

    report_progress(&format!(
        "Уникальных ссылок: {}. Проверка файлов и архивов .ap...",
        asset_uses.len(),
    ));
    let mut ap_cache: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    // Уникальные папки продуктов, для которых нужно проверить .ap-архивы.
    // Читаем архивы параллельно: открытие большого .ap занимает секунды, и
    // последовательное чтение нескольких архивов задерживает всю проверку.
    let product_dirs: HashSet<PathBuf> = asset_uses
        .keys()
        .filter_map(|relative_asset| {
            let parts: Vec<&str> = relative_asset.split('/').collect();
            if parts.len() < 3 {
                return None;
            }
            // Если ассет уже есть на диске по точному пути, архив для него
            // проверять не нужно.
            let exact = assets_path.join(relative_asset.replace('/', "\\"));
            if exact.is_file() {
                return None;
            }
            Some(assets_path.join(parts[0]).join(parts[1]))
        })
        .collect();

    let archive_results: Result<Vec<(PathBuf, HashSet<String>, Vec<String>)>, String> = pool
        .install(|| {
            product_dirs
                .par_iter()
                .map(|product_dir| {
                    let mut files = HashSet::new();
                    let mut errors = Vec::new();
                    match fs::read_dir(product_dir) {
                        Ok(entries) => {
                            for entry in entries {
                                let archive_path = match entry {
                                    Ok(entry) => entry.path(),
                                    Err(error) => {
                                        errors.push(format!(
                                            "Ошибка при чтении {}: {error}",
                                            product_dir.display()
                                        ));
                                        continue;
                                    }
                                };
                                if !archive_path
                                    .extension()
                                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
                                {
                                    continue;
                                }
                                let archive_file = match fs::File::open(&archive_path) {
                                    Ok(file) => file,
                                    Err(error) => {
                                        errors.push(format!(
                                            "Не удалось открыть {}: {error}",
                                            archive_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                let archive = match ZipArchive::new(archive_file) {
                                    Ok(archive) => archive,
                                    Err(error) => {
                                        errors.push(format!(
                                            "Не удалось прочитать архив ассетов {}: {error}",
                                            archive_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                for name in archive.file_names().flatten() {
                                    files.insert(name.replace('/', "\\").trim().to_lowercase());
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!(
                                "Не удалось прочитать {}: {error}",
                                product_dir.display()
                            ));
                        }
                    }
                    Ok((product_dir.clone(), files, errors))
                })
                .collect()
        });
    let archive_results = archive_results?;
    for (product_dir, files, errors) in archive_results {
        structure_issues.extend(errors);
        ap_cache.insert(product_dir, files);
    }

    report_progress("Проверка имён и строк ассетов на умляуты...");
    let inventory_seed: Vec<_> = asset_uses
        .iter()
        .map(|(path, uses)| crate::domain::ReferencedAsset {
            path: asset_names
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.clone()),
            uses: uses.clone(),
            missing: false,
        })
        .collect();
    let (umlaut_assets, umlaut_errors) =
        scan_umlaut_assets(&inventory_seed, assets_path, serz_path, max_concurrent_serz)?;
    conversion_errors.extend(umlaut_errors);
    conversion_errors.sort();

    // Проверки подключённых в RouteProperties.xml продуктов здесь нет и не
    // должно быть: новый провайдер с продуктом добавляют как раз для замены
    // ассетов — в кастомном бинарнике ссылки ведут на геометрию из другой
    // папки, и её продукт подключают к маршруту, даже если прямых ссылок на
    // него в файлах маршрута нет.

    let mut asset_inventory = Vec::with_capacity(asset_uses.len());
    let mut missing_assets = Vec::new();
    for (relative_asset, uses) in asset_uses {
        // Файлы составов (поездов) при проверке маршрута не учитываются: игра
        // ссылается на них отдельно от объектов маршрута, а проверять их нужно
        // вместе со сценариями.
        if uses.iter().any(|usage| usage.kind.is_vehicle()) {
            continue;
        }
        // Показываем путь так, как он записан в маршруте, но ищем файл без
        // учёта регистра: игра регистр не различает.
        let display_path = asset_names
            .get(&relative_asset)
            .cloned()
            .unwrap_or_else(|| relative_asset.clone());
        let full_asset_path = match resolve_asset_path(assets_path, &relative_asset) {
            Ok(path) => path,
            Err(error) => {
                structure_issues.push(format!(
                    "Не удалось однозначно сопоставить ассет {display_path}: {error}"
                ));
                None
            }
        };
        let parts: Vec<&str> = relative_asset.split('/').collect();
        let in_archive = if parts.len() >= 3 {
            let product_dir = assets_path.join(parts[0]).join(parts[1]);
            let inner_path = parts[2..].join("\\").to_lowercase();
            let product_path = parts[1..].join("\\").to_lowercase();
            let full_path = relative_asset.replace('/', "\\").to_lowercase();
            let mut variants = vec![inner_path.clone(), product_path.clone(), full_path.clone()];
            for base in [inner_path, product_path, full_path] {
                variants.push(crate::translit::replace_broken_characters(&base, "ss"));
                variants.push(crate::translit::replace_broken_characters(&base, "ß"));
            }
            // Texture slot prefixes: ensure a trailing extension for image refs.
            let mut texture_variants = Vec::new();
            for variant in &variants {
                if !variant.ends_with(".tgpcdx")
                    && !variant.ends_with(".geopcdx")
                    && !variant.ends_with(".bin")
                {
                    texture_variants.push(format!("{}.tgpcdx", variant));
                }
            }
            variants.extend(texture_variants);
            ap_cache.get(&product_dir).is_some_and(|files| {
                variants
                    .iter()
                    .any(|variant| files.contains(&variant.trim().to_lowercase()))
            })
        } else {
            false
        };
        let is_in_archive = in_archive;
        let (resolved_disk_path, suggested_repair, is_corrupted) =
            if let Some(exact_path) = full_asset_path {
                (Some(exact_path), None, false)
            } else if is_in_archive {
                // Exists inside .ap archive - perfectly valid!
                (None, None, false)
            } else if let Some((fuzzy_disk_path, relative_repair)) =
                fuzzy_resolve_disk_path(assets_path, &relative_asset)
            {
                // Fuzzy candidates are only auto-repairable corrupted/encoding variants.
                // Clean ASCII names that merely look similar are genuinely missing.
                let query_corrupted = has_corrupted_or_nonascii(&relative_asset);
                let umlaut_folded_equivalent = !relative_asset.is_ascii()
                    && normalize_fuzzy_segment(&relative_asset)
                        == normalize_fuzzy_segment(&relative_repair);
                if query_corrupted || umlaut_folded_equivalent {
                    (Some(fuzzy_disk_path), Some(relative_repair), true)
                } else {
                    (None, None, false)
                }
            } else {
                // Genuinely missing
                (None, None, false)
            };

        let is_missing = !is_in_archive && resolved_disk_path.is_none() && !is_corrupted;

        if is_missing || is_corrupted {
            missing_assets.push(crate::domain::MissingAsset {
                path: display_path.clone(),
                uses: uses.clone(),
                suggested_repair,
                is_corrupted_link: is_corrupted,
                asset_type: classify_route_asset_type(&display_path, &uses),
            });
        }

        asset_inventory.push(crate::domain::ReferencedAsset {
            path: display_path,
            uses,
            missing: is_missing,
        });
    }
    missing_assets.sort_by(|left, right| left.path.cmp(&right.path));
    asset_inventory.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(crate::domain::CheckResult {
        missing_assets,
        asset_inventory,
        umlaut_assets,
        conversion_errors,
        structure_issues,
    })
}

/// Ключ ассета в списке: игра не различает регистр, поэтому пути, отличающиеся
/// только регистром, относятся к одному и тому же ассету.
fn asset_key(path: &str) -> String {
    path.to_lowercase()
}

/// Собирает список сценариев маршрута: имя папки и число файлов в ней.
///
pub(crate) fn replace_missing_asset(
    route_path: &Path,
    assets_path: &Path,
    serz_path: &Path,
    old_asset: &str,
    replacement_path: &Path,
    uses: &[crate::domain::AssetUse],
) -> Result<crate::domain::AssetReplacementReport, String> {
    let replacement_path = replacement_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить новый ассет: {error}"))?;
    let assets_root = assets_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить папку Assets: {error}"))?;
    let replacement_relative = replacement_path
        .strip_prefix(&assets_root)
        .map_err(|_| "Новый объект должен находиться внутри папки Assets.".to_owned())?;
    if !replacement_path.is_file()
        || !replacement_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
    {
        return Err("Для замены выберите существующий файл BIN ассета.".to_owned());
    }

    let replacement_parts: Vec<String> = replacement_relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if replacement_parts.len() < 3
        || replacement_parts
            .iter()
            .any(|component| component == "." || component == "..")
    {
        return Err("Путь нового ассета должен иметь вид Provider/Product/....bin.".to_owned());
    }
    let replacement_provider = replacement_parts[0].clone();
    let replacement_product = replacement_parts[1].clone();
    let mut replacement_blueprint = PathBuf::new();
    for component in &replacement_parts[2..] {
        replacement_blueprint.push(component);
    }
    let replacement_blueprint = replacement_blueprint
        .with_extension("xml")
        .to_string_lossy()
        .replace('/', "\\");
    let new_asset = replacement_relative.to_string_lossy().replace('\\', "/");
    if !serz_path.is_file() {
        return Err(format!("Не найден Serz: {}", serz_path.display()));
    }
    if uses.is_empty() {
        return Err("Для отсутствующего ассета не найдены места использования.".to_owned());
    }

    let asset_type = classify_route_asset_type(old_asset, uses);
    let replacement_xml = read_serz_xml(serz_path, &replacement_path)
        .ok()
        .or_else(|| std::fs::read_to_string(&replacement_path).ok());
    let root_tag = replacement_xml
        .as_ref()
        .and_then(|xml| first_blueprint_tag(xml));
    let extension_ok = replacement_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bin") || ext.eq_ignore_ascii_case("xml"));
    match asset_type {
        RouteAssetType::Scenery => {
            if let Some(tag) = &root_tag {
                if ![
                    "cSceneryBlueprint",
                    "cAnimSceneryBlueprint",
                    "cTimeSceneryBlueprint",
                    "cSimpleWaterBlueprint",
                    "cPrecipitationBlueprint",
                ]
                .contains(&tag.as_str())
                {
                    return Err(
                        "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned()
                    );
                }
            } else if !extension_ok {
                return Err("Для scenery-объекта выберите замену из папки Scenery.".to_owned());
            }
        }
        RouteAssetType::Loft => {
            if let Some(tag) = &root_tag {
                if ![
                    "cLoftBlueprint",
                    "cTrackSectionBlueprint",
                    "cRoadSectionBlueprint",
                ]
                .contains(&tag.as_str())
                {
                    return Err(
                        "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned()
                    );
                }
            } else if !extension_ok {
                return Err("Для loft-объекта выберите замену из папки Networks.".to_owned());
            }
        }
        RouteAssetType::Road => {
            if let Some(tag) = &root_tag {
                if tag != "cRoadSectionBlueprint" {
                    return Err(
                        "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned()
                    );
                }
            } else if !extension_ok {
                return Err("Для road-объекта выберите замену из папки Networks.".to_owned());
            }
        }
        RouteAssetType::TrackRules => {
            return Err("Замена путевых объектов в Tracks.bin заблокирована для предотвращения повреждения маршрута.".to_owned());
        }
        _ => {
            if root_tag.is_none() && !extension_ok {
                return Err("Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned());
            }
        }
    }

    if old_asset.to_lowercase().contains("trackrules")
        || old_asset.to_lowercase().contains("junctions")
        || uses.iter().any(|usage| {
            let path = usage.source_file.display().to_lowercase();
            path.contains("tracks.bin")
                || path.contains("/networks/")
                || path.contains("\\networks\\")
                || path.contains("trackrules")
                || path.contains("junctions")
        })
    {
        return Err("Замена путевых объектов в Tracks.bin заблокирована для предотвращения повреждения маршрута.".into());
    }

    let canonical_route = route_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить папку маршрута: {error}"))?;
    let mut source_files = HashSet::new();
    for usage in uses {
        // Файл из архива .ap нельзя править на месте: он лежит внутри архива.
        // Поэтому готовим временную копию, а результат кладём рядом с архивом.
        let source = usage.source_file.clone();
        if source.route_name(route_path).is_none() {
            return Err(format!(
                "Файл для замены находится вне маршрута: {}",
                source.display()
            ));
        }
        if let crate::domain::RouteFile::Path(path) = &source {
            let canonical_source = path.canonicalize().map_err(|error| {
                format!(
                    "Не удалось проверить файл маршрута {}: {error}",
                    source.display()
                )
            })?;
            if !canonical_source.starts_with(&canonical_route) || !canonical_source.is_file() {
                return Err(format!(
                    "Файл для замены находится вне маршрута или недоступен: {}",
                    source.display()
                ));
            }
        }
        source_files.insert(source);
    }

    let mut staged = Vec::new();
    let mut replaced_instances = 0;
    for source_file in source_files {
        let is_binary = source_file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
        if !is_binary
            && !source_file
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
        {
            return Err(format!(
                "Неизвестный формат файла маршрута: {}",
                source_file.display()
            ));
        }
        // Запись из архива лежит только внутри архива: папки с таким именем на
        // диске нет, поэтому временные файлы кладём в папку маршрута.
        let source_parent = match &source_file {
            crate::domain::RouteFile::Path(path) => path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| route_path.to_path_buf()),
            crate::domain::RouteFile::Archive { .. } => route_path.to_path_buf(),
        };
        let temp = create_temporary_work_directory(&source_parent)?;
        let file_path = source_file.materialize(temp.path())?;
        let xml_path = temp.path().join("source.xml");
        if is_binary {
            let conversion =
                convert_bin_to_xml(serz_path, &file_path, &xml_path).or_else(|error| {
                    let Some(fallback) = fallback_serz_path(serz_path) else {
                        return Err(error);
                    };
                    convert_bin_to_xml(&fallback, &file_path, &xml_path).map_err(
                        |fallback_error| {
                            format!(
                                "{error}; повторная попытка через Serz тоже не удалась: {fallback_error}"
                            )
                        },
                    )
                });
            conversion.map_err(|error| {
                format!(
                    "Не удалось прочитать {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
        } else {
            fs::copy(&file_path, &xml_path).map_err(|error| {
                format!(
                    "Не удалось подготовить {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
        }

        let mut xml = fs::read_to_string(&xml_path).map_err(|error| {
            format!(
                "Не удалось прочитать XML для {}: {error}",
                source_file.display()
            )
        })?;
        let references = crate::reference::extract_asset_references_with_locations(&xml);
        let edits: Vec<_> = references
            .into_iter()
            .filter(|reference| reference.path.eq_ignore_ascii_case(old_asset))
            .flat_map(|reference| {
                [
                    (
                        reference.provider_range,
                        escape_xml_text(&replacement_provider),
                    ),
                    (
                        reference.product_range,
                        escape_xml_text(&replacement_product),
                    ),
                    (
                        reference.blueprint_range,
                        escape_xml_text(&replacement_blueprint),
                    ),
                ]
            })
            .collect();
        let instances = edits.len() / 3;
        if instances == 0 {
            continue;
        }
        let mut edits = edits;
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, value) in edits {
            xml.replace_range(range, &value);
        }
        validate_xml_document(&xml).map_err(|error| {
            format!(
                "Замена в {} создала некорректный XML: {error}",
                source_file.display()
            )
        })?;
        fs::write(&xml_path, xml).map_err(|error| {
            format!(
                "Не удалось записать временный XML для {}: {error}",
                source_file.display()
            )
        })?;

        let converted_path = if is_binary {
            let converted_path = temp.path().join("replacement.bin");
            let conversion = convert_xml_to_bin(serz_path, &xml_path, &converted_path).or_else(
                |error| {
                    let Some(fallback) = fallback_serz_path(serz_path) else {
                        return Err(error);
                    };
                    convert_xml_to_bin(&fallback, &xml_path, &converted_path).map_err(
                        |fallback_error| {
                            format!(
                                "{error}; повторная попытка через Serz тоже не удалась: {fallback_error}"
                            )
                        },
                    )
                },
            );
            conversion.map_err(|error| {
                format!(
                    "Не удалось подготовить изменённый BIN {}: {error}",
                    source_file.display()
                )
            })?;
            converted_path
        } else {
            xml_path
        };
        replaced_instances += instances;
        staged.push((source_file, temp, converted_path));
    }

    if staged.is_empty() {
        return Err(format!(
            "В исходниках маршрута не найдено использований {}. Обновите проверку и повторите.",
            old_asset
        ));
    }

    let mut backups = Vec::new();
    let mut replaced_files = 0;
    for (source_file, _temp, converted_path) in staged {
        match install_route_file(route_path, &source_file, &converted_path) {
            Ok(backup) => {
                replaced_files += 1;
                if let Some(backup) = backup {
                    backups.push(backup);
                }
            }
            Err(error) => {
                return Err(format!(
                    "Заменено размещений: {replaced_instances}; успешно изменено файлов: {replaced_files}. Ошибка в {}: {error}. Созданные резервные копии: {}",
                    source_file.display(),
                    backups
                        .iter()
                        .map(|path: &PathBuf| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    Ok(crate::domain::AssetReplacementReport {
        replaced_files,
        replaced_instances,
        backups,
        replacement_asset: new_asset,
        log_file: None,
        log_error: None,
    })
}

struct StagedUmlautFile {
    /// Исходный файл: запись архива .ap или файл на диске.
    source: crate::domain::RouteFile,
    /// Куда класть результат.
    target: PathBuf,
    staged: PathBuf,
    _temp: tempfile::TempDir,
}

fn transliterated_asset_path(
    assets_path: &Path,
    source: &Path,
    fixes: &crate::translit::CharacterFixes,
) -> Result<PathBuf, String> {
    let relative = source
        .strip_prefix(assets_path)
        .map_err(|_| format!("Ассет находится вне папки Assets: {}", source.display()))?;
    let mut components = relative.components();
    let provider = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    let product = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    if crate::translit::has_umlauts(&provider) || crate::translit::has_umlauts(&product) {
        return Err(format!(
            "Умляут найден в имени Provider/Product для {}. Автоматическая транслитерация папок Provider/Product остановлена, чтобы не затронуть другие ассеты.",
            source.display()
        ));
    }
    // Вложенные папки переименовываются вместе с файлами: все ссылки на них
    // обновляются каскадом.
    let translated = crate::translit::transliterate_text(&relative.to_string_lossy(), fixes);
    Ok(assets_path.join(translated))
}

/// Папки внутри продукта, в именах которых есть умляуты.
///
/// Такие папки приходится переименовывать целиком: иначе файлы внутри них
/// останутся со старыми именами, а ссылки на них — со старыми путями.
fn folders_with_umlauts(assets_path: &Path, asset: &Path) -> Result<Vec<PathBuf>, String> {
    let relative = asset
        .strip_prefix(assets_path)
        .map_err(|_| format!("Ассет находится вне папки Assets: {}", asset.display()))?;
    let components: Vec<_> = relative.components().collect();
    if components.len() < 3 {
        return Ok(Vec::new());
    }
    let mut current = assets_path.to_path_buf();
    let mut folders = Vec::new();
    // Последний компонент — имя файла, его пропускаем.
    for (index, component) in components.iter().enumerate().take(components.len() - 1) {
        current.push(component);
        // Первые два компонента — Provider и Product, их не переименовываем.
        let name = component.as_os_str().to_string_lossy();
        if index >= 2 && crate::translit::has_umlauts(&name) {
            folders.push(current.clone());
        }
    }
    Ok(folders)
}

/// Собирает новый текст ссылки на объект.
///
/// Ссылка сохраняет вид, в котором она была записана: суффикс варианта стоит
/// перед именем файла, расширение остаётся, если оно было.
fn translated_reference(
    target_file: &Path,
    assets_root: &Path,
    product_root: &Path,
    target_source: &Path,
    base: ReferenceBase,
    reference: &GeometryReference,
) -> Result<String, String> {
    let base_path = match base {
        ReferenceBase::Assets => assets_root,
        ReferenceBase::Product => product_root,
        ReferenceBase::Folder => target_source.parent().ok_or_else(|| {
            format!(
                "Некорректный путь к исходному ассету: {}",
                target_source.display()
            )
        })?,
    };
    let relative = target_file.strip_prefix(base_path).map_err(|_| {
        format!(
            "Путь {} вышел за пределы {}",
            target_file.display(),
            base_path.display()
        )
    })?;
    let relative = relative.to_string_lossy().replace('/', "\\");
    let (folder, file_name) = match relative.rfind('\\') {
        Some(index) => (&relative[..=index], &relative[index + 1..]),
        None => ("", relative.as_str()),
    };
    // Если в ссылке расширения не было, то и в новой его быть не должно: игра
    // подставляет его сама.
    let name = match &reference.extension {
        Some(_) => file_name.to_owned(),
        None => match Path::new(file_name).file_stem() {
            Some(stem) => stem.to_string_lossy().into_owned(),
            None => file_name.to_owned(),
        },
    };
    let mut text = String::from(folder);
    text.push_str(&reference.variant);
    text.push_str(&name);
    if let Some(extension) = &reference.extension {
        text.push('.');
        text.push_str(extension);
    }
    Ok(text)
}

fn stage_serz_xml(
    source: &Path,
    target: &Path,
    xml: &str,
    serz_path: &Path,
) -> Result<StagedUmlautFile, String> {
    let parent = source
        .parent()
        .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?;
    let temp = create_temporary_work_directory(parent)?;
    let xml_path = temp.path().join("transliterated.xml");
    fs::write(&xml_path, xml)
        .map_err(|error| format!("Не удалось записать временный XML: {error}"))?;
    // Serz собирает только .bin: файл геометрии приходится собирать тем же
    // форматом, а нужное расширение получит при переносе на место.
    let staged = temp.path().join("transliterated.bin");
    let conversion = convert_xml_to_bin(serz_path, &xml_path, &staged).or_else(|error| {
        let Some(fallback) = fallback_serz_path(serz_path) else {
            return Err(error);
        };
        convert_xml_to_bin(&fallback, &xml_path, &staged).map_err(|fallback_error| {
            format!(
                "{error}; повторная попытка через {} не удалась: {fallback_error}",
                fallback.display()
            )
        })
    });
    conversion.map_err(|error| {
        format!(
            "Не удалось подготовить изменённый файл {}: {error}",
            source.display()
        )
    })?;
    Ok(StagedUmlautFile {
        source: crate::domain::RouteFile::Path(source.to_path_buf()),
        target: target.to_path_buf(),
        staged,
        _temp: temp,
    })
}

fn stage_renamed_file(source: &Path, target: &Path) -> Result<StagedUmlautFile, String> {
    let parent = source
        .parent()
        .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?;
    let temp = create_temporary_work_directory(parent)?;
    let staged = temp.path().join(
        source
            .file_name()
            .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?,
    );
    fs::copy(source, &staged).map_err(|error| {
        format!(
            "Не удалось подготовить переименование {}: {error}",
            source.display()
        )
    })?;
    Ok(StagedUmlautFile {
        source: crate::domain::RouteFile::Path(source.to_path_buf()),
        target: target.to_path_buf(),
        staged,
        _temp: temp,
    })
}

pub(crate) fn repair_umlaut_asset_chain(
    route_path: &Path,
    assets_path: &Path,
    serz_path: &Path,
    asset_relative_path: &str,
    uses: &[crate::domain::AssetUse],
    fixes: &crate::translit::CharacterFixes,
) -> Result<crate::domain::UmlautRepairReport, String> {
    if !serz_path.is_file() {
        return Err(format!("Не найден Serz: {}", serz_path.display()));
    }
    let assets_root = assets_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить папку Assets: {error}"))?;
    let route_root = route_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить папку маршрута: {error}"))?;
    let source_asset = resolve_asset_path(&assets_root, asset_relative_path)?.ok_or_else(|| {
        format!(
            "Не удалось найти BIN для ссылки {}. Проверены исходный путь и варианты с ß/ss.",
            asset_relative_path
        )
    })?;
    let canonical_asset = source_asset.canonicalize().map_err(|error| {
        format!(
            "Не удалось открыть ассет {}: {error}",
            source_asset.display()
        )
    })?;
    if !canonical_asset.starts_with(&assets_root) || !canonical_asset.is_file() {
        return Err(format!(
            "Ассет находится вне Assets или недоступен: {}",
            source_asset.display()
        ));
    }
    let new_asset_path = transliterated_asset_path(&assets_root, &canonical_asset, fixes)?;
    let new_relative_asset = new_asset_path
        .strip_prefix(&assets_root)
        .map_err(|_| "Новый путь ассета выходит за пределы Assets.".to_owned())?
        .to_string_lossy()
        .replace('\\', "/");
    if new_relative_asset.split('/').count() < 3 {
        return Err("Путь ассета должен иметь структуру Provider/Product/файл.".to_owned());
    }
    if new_relative_asset == asset_relative_path {
        let found = asset_chain_umlaut_evidence(&assets_root, serz_path, &canonical_asset)?;
        if found.evidence.is_empty() && found.unsupported.is_empty() {
            return Err("В файлах ассета не обнаружены поддерживаемые умляуты.".to_owned());
        }
    }

    let relative = canonical_asset
        .strip_prefix(&assets_root)
        .map_err(|_| "Ассет находится вне папки Assets.".to_owned())?;
    let mut components = relative.components();
    let provider = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    let product = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    let product_root = assets_root.join(provider).join(product);
    // Работаем только с файлами рядом с ассетом: соседние папки продукта
    // относятся к другим объектам, их переименование сломало бы их ссылки.
    let scope_root = canonical_asset
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| product_root.clone());
    let mut queue = vec![canonical_asset.clone()];
    let mut visited = HashSet::new();
    let mut staged_by_source: HashMap<crate::domain::RouteFile, StagedUmlautFile> = HashMap::new();
    let mut renamed_tg_files: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut scan_all_geometry = false;

    // Папки с умляутами переименовываются целиком: все файлы внутри них
    // переезжают вместе с папкой, даже если их содержимое не меняется.
    let renamed_folders = folders_with_umlauts(&assets_root, &canonical_asset)?;
    if !renamed_folders.is_empty() {
        // Ссылки на переехавшие файлы могут быть в соседних файлах папки
        // ассета, поэтому приходится просмотреть её целиком.
        scan_all_geometry = true;
    }

    // Файлы из переименованных папок переезжают вместе с папками: их имена и
    // содержимое могут быть в порядке, но путь изменился.
    let mut emptied_folders: Vec<PathBuf> = Vec::new();
    for folder in &renamed_folders {
        for entry in WalkDir::new(folder).follow_links(false) {
            let entry = entry
                .map_err(|error| format!("Ошибка обхода папки {}: {error}", folder.display()))?;
            if entry.file_type().is_dir() {
                emptied_folders.push(entry.into_path());
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let source = entry.into_path();
            let canonical = source.canonicalize().map_err(|error| {
                format!("Не удалось открыть файл {}: {error}", source.display())
            })?;
            let extension = canonical
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if extension.eq_ignore_ascii_case("bin") || extension.eq_ignore_ascii_case("GeoPcDx") {
                // Такие файлы могут ссылаться на другие объекты: их ссылки
                // тоже нужно обновить, поэтому они идут в цепочку.
                queue.push(canonical);
                continue;
            }
            let target = transliterated_asset_path(&assets_root, &canonical, fixes)?;
            if target == canonical {
                continue;
            }
            let staged = stage_renamed_file(&canonical, &target)?;
            staged_by_source.insert(crate::domain::RouteFile::Path(canonical), staged);
        }
    }
    while let Some(source) = queue.pop() {
        let canonical = source.canonicalize().map_err(|error| {
            format!(
                "Не удалось открыть зависимость {}: {error}",
                source.display()
            )
        })?;
        if !canonical.starts_with(&scope_root) || !visited.insert(canonical.clone()) {
            continue;
        }
        let extension = canonical
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !extension.eq_ignore_ascii_case("bin") && !extension.eq_ignore_ascii_case("GeoPcDx") {
            continue;
        }
        let xml = read_serz_xml(serz_path, &canonical)?;
        let target_file = transliterated_asset_path(&assets_root, &canonical, fixes)?;
        let mut edits = Vec::new();
        let mut protected_references: Vec<std::ops::Range<usize>> = Vec::new();
        for (reference, range) in referenced_geometry_paths(&xml) {
            let reference_text = reference.to_text();
            let Some((dependency, base)) =
                resolve_product_reference(&assets_root, &product_root, &canonical, &reference)
            else {
                if crate::translit::has_umlauts(&reference_text) {
                    return Err(unresolved_reference_error(
                        &canonical,
                        &reference,
                        &assets_root,
                        &product_root,
                    ));
                }
                continue;
            };
            let canonical_dependency = dependency.canonicalize().map_err(|error| {
                format!(
                    "Не удалось проверить зависимость {}: {error}",
                    dependency.display()
                )
            })?;
            if !canonical_dependency.starts_with(&product_root) {
                return Err(format!(
                    "Ссылка выходит за пределы продукта {}: {}",
                    product_root.display(),
                    dependency.display()
                ));
            }
            if !canonical_dependency.starts_with(&scope_root) {
                // Файл лежит за пределами папки ассета: его не трогаем, и ссылку
                // на него тоже оставляем как есть.
                protected_references.push(range);
                continue;
            }
            let translated_dependency =
                transliterated_asset_path(&assets_root, &canonical_dependency, fixes)?;
            let dependency_extension = canonical_dependency
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if dependency_extension.eq_ignore_ascii_case("GeoPcDx") {
                queue.push(canonical_dependency.clone());
            } else if dependency_extension.eq_ignore_ascii_case("TgPcDx")
                && (crate::translit::has_umlauts(&reference_text)
                    || canonical_dependency
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(crate::translit::has_umlauts))
            {
                // Текстура — это готовый файл: её достаточно переименовать,
                // внутри умляутов у неё не бывает.
                scan_all_geometry = true;
                renamed_tg_files
                    .insert(canonical_dependency.clone(), translated_dependency.clone());
            }
            if translated_dependency != canonical_dependency {
                let replacement = translated_reference(
                    &translated_dependency,
                    &assets_root,
                    &product_root,
                    &target_file,
                    base,
                    &reference,
                )?;
                edits.push((range, escape_xml_text(&replacement)));
            }
        }
        let mut rewritten = xml.clone();
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, value) in edits {
            rewritten.replace_range(range, &value);
        }
        rewritten = crate::translit::transliterate_xml_umlauts_outside(&rewritten, &protected_references, fixes);
        if rewritten != xml || target_file != canonical {
            validate_xml_document(&rewritten).map_err(|error| {
                format!(
                    "Некорректный XML после обработки {}: {error}",
                    canonical.display()
                )
            })?;
            let staged = stage_serz_xml(&canonical, &target_file, &rewritten, serz_path)?;
            staged_by_source.insert(crate::domain::RouteFile::Path(canonical), staged);
        }
    }

    // Файлы из переименованных папок переезжают вместе с папками: их имена и
    // содержимое могут быть в порядке, но путь изменился.
    if scan_all_geometry {
        // Обход ограничен папкой самого ассета: соседние папки продукта не
        // относятся к нему, и трогать их не нужно.
        for entry in WalkDir::new(&scope_root).follow_links(false) {
            let entry = entry.map_err(|error| {
                format!("Ошибка обхода папки {}: {error}", scope_root.display())
            })?;
            if entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("GeoPcDx"))
            {
                queue.push(entry.into_path());
            }
        }

        while let Some(source) = queue.pop() {
            let canonical = source.canonicalize().map_err(|error| {
                format!("Не удалось открыть геометрию {}: {error}", source.display())
            })?;
            if !canonical.starts_with(&scope_root) || !visited.insert(canonical.clone()) {
                continue;
            }
            let xml = read_serz_xml(serz_path, &canonical)?;
            let mut protected_references: Vec<std::ops::Range<usize>> = Vec::new();
            for (reference, range) in referenced_geometry_paths(&xml) {
                let reference_text = reference.to_text();
                let Some((dependency, _)) =
                    resolve_product_reference(&assets_root, &product_root, &canonical, &reference)
                else {
                    if crate::translit::has_umlauts(&reference_text) {
                        return Err(unresolved_reference_error(
                            &canonical,
                            &reference,
                            &assets_root,
                            &product_root,
                        ));
                    }
                    continue;
                };
                let dependency = dependency.canonicalize().map_err(|error| {
                    format!(
                        "Не удалось проверить зависимость {}: {error}",
                        dependency.display()
                    )
                })?;
                if !dependency.starts_with(&product_root) {
                    return Err(format!(
                        "Ссылка выходит за пределы продукта {}: {}",
                        product_root.display(),
                        dependency.display()
                    ));
                }
                if !dependency.starts_with(&scope_root) {
                    // Файл лежит за пределами папки ассета: его не трогаем, и
                    // ссылку на него тоже оставляем как есть.
                    protected_references.push(range);
                    continue;
                }
                let extension = dependency
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                if extension.eq_ignore_ascii_case("GeoPcDx") {
                    queue.push(dependency.clone());
                } else if extension.eq_ignore_ascii_case("TgPcDx") && crate::translit::has_umlauts(&reference_text) {
                    let target = transliterated_asset_path(&assets_root, &dependency, fixes)?;
                    renamed_tg_files.insert(dependency, target);
                }
            }
            let target = transliterated_asset_path(&assets_root, &canonical, fixes)?;
            let rewritten = crate::translit::transliterate_xml_umlauts_outside(&xml, &protected_references, fixes);
            if rewritten != xml || target != canonical {
                validate_xml_document(&rewritten).map_err(|error| {
                    format!(
                        "Некорректный XML после обработки {}: {error}",
                        canonical.display()
                    )
                })?;
                let staged = stage_serz_xml(&canonical, &target, &rewritten, serz_path)?;
                staged_by_source.insert(crate::domain::RouteFile::Path(canonical), staged);
            }
        }
    }

    let mut renamed_staged: Vec<(crate::domain::RouteFile, StagedUmlautFile)> = Vec::new();
    for (source, target) in &renamed_tg_files {
        let key = crate::domain::RouteFile::Path(source.clone());
        if staged_by_source.contains_key(&key) {
            continue;
        }
        let staged = stage_renamed_file(source, target)?;
        renamed_staged.push((key, staged));
    }
    staged_by_source.extend(renamed_staged);

    let new_parts: Vec<_> = new_relative_asset.split('/').collect();
    let replacement_blueprint = Path::new(&new_parts[2..].join("\\"))
        .with_extension("xml")
        .to_string_lossy()
        .into_owned();
    let mut route_sources: HashSet<crate::domain::RouteFile> = HashSet::new();
    if new_relative_asset != asset_relative_path {
        for usage in uses {
            let source = usage.source_file.clone();
            if let crate::domain::RouteFile::Path(path) = &source {
                let canonical = path.canonicalize().map_err(|error| {
                    format!(
                        "Не удалось открыть файл маршрута {}: {error}",
                        usage.source_file.display()
                    )
                })?;
                if !canonical.starts_with(&route_root) {
                    return Err(format!(
                        "Файл использования находится вне маршрута: {}",
                        source.display()
                    ));
                }
            }
            route_sources.insert(source);
        }
    }
    for source in route_sources {
        let is_binary = source
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
        if !is_binary
            && !source
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
        {
            return Err(format!(
                "Неподдерживаемый файл маршрута: {}",
                source.display()
            ));
        }
        // Запись из архива лежит только внутри архива: папки с таким именем на
        // диске нет, поэтому временные файлы кладём в папку маршрута.
        let source_parent = match &source {
            crate::domain::RouteFile::Path(path) => path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| route_root.clone()),
            crate::domain::RouteFile::Archive { .. } => route_root.clone(),
        };
        let temp = create_temporary_work_directory(&source_parent)?;
        let file_path = source.materialize(temp.path())?;
        let xml_path = temp.path().join("route.xml");
        if is_binary {
            let conversion =
                convert_bin_to_xml(serz_path, &file_path, &xml_path).or_else(|error| {
                    let Some(fallback) = fallback_serz_path(serz_path) else {
                        return Err(error);
                    };
                    convert_bin_to_xml(&fallback, &file_path, &xml_path).map_err(|fallback_error| {
                        format!(
                            "{error}; повторная попытка через {} не удалась: {fallback_error}",
                            fallback.display()
                        )
                    })
                });
            conversion?;
        } else {
            fs::copy(&file_path, &xml_path)
                .map_err(|error| format!("Не удалось подготовить {}: {error}", source.display()))?;
        }
        let mut xml = fs::read_to_string(&xml_path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", xml_path.display()))?;
        let references = crate::reference::extract_asset_references_with_locations(&xml);
        let mut edits = Vec::new();
        for reference in references
            .into_iter()
            .filter(|reference| reference.path.eq_ignore_ascii_case(asset_relative_path))
        {
            edits.extend([
                (reference.provider_range, new_parts[0].to_owned()),
                (reference.product_range, new_parts[1].to_owned()),
                (reference.blueprint_range, replacement_blueprint.clone()),
            ]);
        }
        if edits.is_empty() {
            return Err(format!(
                "Ссылка {} не найдена в файле маршрута {}.",
                asset_relative_path,
                source.display()
            ));
        }
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, value) in edits {
            xml.replace_range(range, &escape_xml_text(&value));
        }
        validate_xml_document(&xml)
            .map_err(|error| format!("Некорректный XML маршрута {}: {error}", source.display()))?;
        fs::write(&xml_path, xml)
            .map_err(|error| format!("Не удалось записать подготовленный маршрут: {error}"))?;
        let staged_path = if is_binary {
            let mut staged = temp.path().join("route-new");
            staged.set_extension(source.extension().unwrap());
            let conversion = convert_xml_to_bin(serz_path, &xml_path, &staged).or_else(|error| {
                let Some(fallback) = fallback_serz_path(serz_path) else {
                    return Err(error);
                };
                convert_xml_to_bin(&fallback, &xml_path, &staged).map_err(|fallback_error| {
                    format!(
                        "{error}; повторная попытка через {} не удалась: {fallback_error}",
                        fallback.display()
                    )
                })
            });
            conversion?;
            staged
        } else {
            xml_path
        };
        staged_by_source.insert(
            source.clone(),
            StagedUmlautFile {
                source: source.clone(),
                // Файл из архива .ap кладём рядом с архивом: игра читает
                // сначала архив, потом файлы рядом с ним.
                target: source.install_target(&route_root),
                staged: staged_path,
                _temp: temp,
            },
        );
    }

    if staged_by_source.is_empty() {
        return Err("Не найдено файлов для транслитерации.".to_owned());
    }
    let mut targets = HashSet::new();
    for staged in staged_by_source.values() {
        if !targets.insert(staged.target.clone()) {
            return Err(format!(
                "Несколько файлов претендуют на путь {}.",
                staged.target.display()
            ));
        }
        let original_path = match &staged.source {
            crate::domain::RouteFile::Path(path) => Some(path.clone()),
            crate::domain::RouteFile::Archive { .. } => None,
        };
        if staged.target.exists() && original_path.as_deref() != Some(staged.target.as_path()) {
            return Err(format!(
                "После транслитерации возникнет конфликт имён: {} уже существует.",
                staged.target.display()
            ));
        }
    }

    let mut prepared: Vec<_> = staged_by_source.into_values().collect();
    prepared.sort_by(|left, right| left.source.cmp(&right.source));
    for staged in &prepared {
        if let Some(parent) = staged.target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("Не удалось создать папку {}: {error}", parent.display())
            })?;
        }
    }
    let mut committed: Vec<(PathBuf, PathBuf, Option<PathBuf>)> = Vec::new();
    for staged in &prepared {
        // Файл из архива .ap: оригинал остаётся в архиве, а изменённая копия
        // кладётся рядом и перекроет его, поэтому резервная копия не нужна.
        let original = match &staged.source {
            crate::domain::RouteFile::Path(path) => Some(path.clone()),
            crate::domain::RouteFile::Archive { .. } => None,
        };
        let backup = match &original {
            Some(original) => {
                let backup = match next_backup_path(original) {
                    Ok(path) => path,
                    Err(error) => {
                        let rollback_errors = rollback_umlaut_changes(&committed);
                        let rollback_detail = if rollback_errors.is_empty() {
                            String::new()
                        } else {
                            format!(" Ошибки отката: {}", rollback_errors.join("; "))
                        };
                        return Err(format!("{error}.{rollback_detail}"));
                    }
                };
                if let Err(error) = fs::rename(original, &backup) {
                    let rollback_errors = rollback_umlaut_changes(&committed);
                    let rollback_detail = if rollback_errors.is_empty() {
                        String::new()
                    } else {
                        format!(" Ошибки отката: {}", rollback_errors.join("; "))
                    };
                    return Err(format!(
                        "Не удалось создать резервную копию {}: {error}.{rollback_detail}",
                        original.display()
                    ));
                }
                Some(backup)
            }
            None => None,
        };
        if let Err(error) = install_staged_file(staged) {
            let restore = match (&backup, &original) {
                (Some(backup), Some(original)) => fs::rename(backup, original),
                _ => Ok(()),
            };
            let rollback_errors = rollback_umlaut_changes(&committed);
            let rollback_detail = if rollback_errors.is_empty() {
                String::new()
            } else {
                format!(
                    " Дополнительные ошибки отката: {}",
                    rollback_errors.join("; ")
                )
            };
            return Err(match restore {
                Ok(()) => format!(
                    "Не удалось установить {}: {error}. Файл восстановлен.{rollback_detail}",
                    staged.target.display()
                ),
                Err(restore_error) => format!(
                    "Не удалось установить {}: {error}; восстановление тоже не удалось: {restore_error}.{rollback_detail}",
                    staged.target.display()
                ),
            });
        }
        committed.push((
            original.unwrap_or_else(|| staged.target.clone()),
            staged.target.clone(),
            backup,
        ));
    }
    // Старые папки с умляутами остались пустыми: убираем их, чтобы в Assets
    // не осталось дублей с непонятным назначением.
    emptied_folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
    for folder in &emptied_folders {
        let _ = fs::remove_dir(folder);
    }
    Ok(crate::domain::UmlautRepairReport {
        new_asset_path: new_relative_asset,
    })
}

/// Ставит подготовленный файл на место.
///
/// Файл из архива .ap не переносится: вместо этого рядом с архивом кладётся
/// изменённая копия, которую игра прочитает вместо записи архива.
fn install_staged_file(staged: &StagedUmlautFile) -> Result<(), String> {
    match &staged.source {
        crate::domain::RouteFile::Path(_) => fs::rename(&staged.staged, &staged.target)
            .map_err(|error| format!("Не удалось установить {}: {error}", staged.target.display())),
        crate::domain::RouteFile::Archive { .. } => {
            if let Some(parent) = staged.target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("Не удалось создать папку {}: {error}", parent.display())
                })?;
            }
            fs::copy(&staged.staged, &staged.target)
                .map(|_| ())
                .map_err(|error| {
                    format!("Не удалось записать {}: {error}", staged.target.display())
                })
        }
    }
}

fn rollback_umlaut_changes(committed: &[(PathBuf, PathBuf, Option<PathBuf>)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (source, target, backup) in committed.iter().rev() {
        if target.exists()
            && let Err(error) = fs::remove_file(target)
        {
            errors.push(format!("не удалось удалить {}: {error}", target.display()));
            continue;
        }
        let Some(backup) = backup else {
            continue;
        };
        if let Err(error) = fs::rename(backup, source) {
            errors.push(format!(
                "не удалось восстановить {} из {}: {error}",
                source.display(),
                backup.display()
            ));
        }
    }
    errors
}

fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Запускает serz.exe с таймаутом, чтобы зависший дочерний процесс не
/// блокировал пул потоков навсегда.
fn serz_output(serz_path: &Path, args: &[String]) -> Result<std::process::Output, String> {
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let parent = serz_path
        .parent()
        .ok_or_else(|| format!("Некорректный путь к Serz: {}", serz_path.display()))?;
    let mut cmd = Command::new(serz_path);
    cmd.current_dir(parent)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .map_err(|error| format!("не удалось запустить serz.exe: {error}"))?;
    let child = Arc::new(Mutex::new(Some(child)));
    let waiter_child = child.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut guard = waiter_child.lock().unwrap();
        if let Some(child) = guard.take() {
            let _ = tx.send(child.wait_with_output());
        }
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(format!("не удалось дождаться serz.exe: {error}")),
        Err(_) => {
            if let Some(mut child) = child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err("serz.exe завис и был остановлен по таймауту".to_owned())
        }
    }
}

pub(crate) fn convert_bin_to_xml(
    serz_path: &Path,
    bin_path: &Path,
    xml_path: &Path,
) -> Result<(), String> {
    if xml_path.exists() {
        fs::remove_file(xml_path).map_err(|error| {
            format!(
                "не удалось удалить предыдущий временный XML {}: {error}",
                xml_path.display()
            )
        })?;
    }
    let mut args = vec![legacy_windows_path(bin_path).display().to_string()];
    args.push(format!("/xml:{}", legacy_windows_path(xml_path).display()));
    let output = serz_output(serz_path, &args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_serz_process(output.status.success(), &stdout, &stderr)
        .map_err(|error| format!("{error} (код выхода {})", output.status))?;
    let metadata = fs::metadata(xml_path).map_err(|error| {
        format!(
            "Serz завершился успешно, но выходной XML {} недоступен: {error}",
            xml_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "Serz завершился успешно, но не создал непустой XML-файл: {}",
            xml_path.display()
        ));
    }
    if !has_complete_xml_tail(xml_path)? {
        return Err(format!(
            "Serz создал незавершённый XML для {}. Файл использует формат, который не поддерживает конвертация в XML.",
            bin_path.display()
        ));
    }
    Ok(())
}

/// Проверяет, что XML-файл закрыт корневым элементом: Serz возвращает обрывок разметки
/// для некоторых устаревших форматов файлов маршрута.
fn has_complete_xml_tail(xml_path: &Path) -> Result<bool, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(xml_path).map_err(|error| {
        format!(
            "не удалось прочитать {} после конвертации: {error}",
            xml_path.display()
        )
    })?;
    let length = file.seek(SeekFrom::End(0)).map_err(|error| {
        format!(
            "не удалось определить размер {}: {error}",
            xml_path.display()
        )
    })?;
    let read_size = length.min(64);
    file.seek(SeekFrom::End(-(read_size as i64)))
        .map_err(|error| format!("не удалось прочитать {}: {error}", xml_path.display()))?;
    let mut tail = vec![0u8; read_size as usize];
    file.read_exact(&mut tail)
        .map_err(|error| format!("не удалось прочитать {}: {error}", xml_path.display()))?;
    Ok(tail
        .iter()
        .rev()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'>'))
}

pub(crate) fn convert_xml_to_bin(
    serz_path: &Path,
    xml_path: &Path,
    target_asset_path: &Path,
) -> Result<(), String> {
    let staging_output = xml_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("staging_output.bin");
    let _ = fs::remove_file(&staging_output);

    let mut args = vec![legacy_windows_path(xml_path).display().to_string()];
    args.push(format!(
        "/bin:{}",
        legacy_windows_path(&staging_output).display()
    ));
    println!(
        "[Serz Exec] Serz: {:?}, XML: {:?}, Staging BIN: {:?}, Target: {:?}",
        serz_path, xml_path, staging_output, target_asset_path
    );
    let output = serz_output(serz_path, &args)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("[Serz Output] stdout: {}, stderr: {}", stdout, stderr);
    validate_serz_process(output.status.success(), &stdout, &stderr).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!("{error} (код выхода {})", output.status)
    })?;

    let metadata = fs::metadata(&staging_output).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!(
            "Serz завершился успешно, но выходной бинарный файл {} недоступен: {error}",
            staging_output.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        let _ = fs::remove_file(&staging_output);
        return Err(format!(
            "Serz не сгенерировал данные или вернул пустой файл: {}",
            staging_output.display()
        ));
    }

    fs::copy(&staging_output, target_asset_path).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!(
            "Не удалось скопировать скомпилированный BIN в {}: {error}",
            target_asset_path.display()
        )
    })?;
    let _ = fs::remove_file(&staging_output);
    Ok(())
}

pub(crate) fn validate_serz_process(
    status_success: bool,
    stdout: &str,
    stderr: &str,
) -> Result<(), String> {
    if !status_success {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_owned()
        } else if stdout.trim().is_empty() {
            "Serz завершился с ошибкой без диагностического сообщения.".to_owned()
        } else {
            stdout.trim().to_owned()
        };
        return Err(detail);
    }
    if !stderr.trim().is_empty() {
        return Err(format!("Serz сообщил об ошибке: {}", stderr.trim()));
    }
    Ok(())
}

pub(crate) fn legacy_windows_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
        const UNC_PREFIX: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];

        let path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if let Some(path_wide) = path_wide.strip_prefix(&VERBATIM_PREFIX) {
            if let Some(unc_path) = path_wide.strip_prefix(&UNC_PREFIX) {
                let mut legacy_unc_path = vec![b'\\' as u16, b'\\' as u16];
                legacy_unc_path.extend_from_slice(unc_path);
                return PathBuf::from(std::ffi::OsString::from_wide(&legacy_unc_path));
            }
            return PathBuf::from(std::ffi::OsString::from_wide(path_wide));
        }
    }
    path.to_path_buf()
}

pub(crate) fn validate_xml_document(xml: &str) -> Result<(), String> {
    for event in xml::reader::EventReader::new(xml.as_bytes()) {
        event.map_err(|error| format!("Некорректный XML: {error}"))?;
    }
    Ok(())
}

pub(crate) fn install_converted_asset(
    source_path: &Path,
    converted_path: &Path,
) -> Result<PathBuf, String> {
    let backup_path = next_backup_path(source_path)?;
    fs::rename(source_path, &backup_path).map_err(|error| {
        format!(
            "Не удалось сохранить резервную копию {}: {error}",
            backup_path.display()
        )
    })?;
    if let Err(error) = fs::rename(converted_path, source_path) {
        return match fs::rename(&backup_path, source_path) {
            Ok(()) => Err(format!(
                "Не удалось записать новый файл: {error}. Исходник восстановлен."
            )),
            Err(restore_error) => Err(format!(
                "Не удалось записать новый файл: {error}. Не удалось восстановить исходник из {}: {restore_error}",
                backup_path.display()
            )),
        };
    }
    Ok(backup_path)
}

pub(crate) fn next_backup_path(source_path: &Path) -> Result<PathBuf, String> {
    let file_name = source_path
        .file_name()
        .ok_or_else(|| format!("Некорректный путь к файлу: {}", source_path.display()))?;
    for index in 1_u64.. {
        let mut backup_name = file_name.to_os_string();
        backup_name.push(format!(".bak{index}"));
        let backup_path = source_path.with_file_name(backup_name);
        if !backup_path.exists() {
            return Ok(backup_path);
        }
    }
    Err(format!(
        "Не удалось подобрать имя резервной копии для {}",
        source_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        TEMP_WORK_DIRECTORY_PREFIX,
        collect_route_files, leftover_temporary_work_directories, referenced_geometry_paths,
    };
    use std::path::Path;
    use std::path::PathBuf;

    /// Кэш разбора: повторная проверка берёт неизменившиеся файлы из кэша.
    #[test]
    fn rechecks_route_using_analysis_cache() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("route");
        let scenery = route.join("Scenery");
        std::fs::create_dir_all(&scenery).unwrap();
        let first = scenery.join("+000000+000000.xml");
        let second = scenery.join("+000000+000001.xml");
        std::fs::write(
            &first,
            "<Provider>First</Provider><Product>ProductA</Product><BlueprintID>Scenery\\one.xml</BlueprintID>\
             <Provider>First</Provider><Product>ProductA</Product><BlueprintID>Scenery\\one.xml</BlueprintID>",
        )
        .unwrap();
        std::fs::write(
            &second,
            "<Provider>Second</Provider><Product>ProductB</Product><BlueprintID>Scenery\\two.xml</BlueprintID>",
        )
        .unwrap();
        // Serz для XML-файлов не нужен, но путь должен существовать.
        let serz = temp.path().join("serz.exe");
        std::fs::write(&serz, b"stub").unwrap();
        let assets = temp.path().join("Assets");
        std::fs::create_dir_all(&assets).unwrap();

        let check = super::check_route(&route, &assets, &serz, true, 2, |_| {}).unwrap();
        assert_eq!(check.asset_inventory.len(), 2);
        assert!(crate::analysis_cache::cache_path(&route).is_file());
        // Счётчик использований считает каждое вхождение, а не только файлы.
        let first_asset = check
            .asset_inventory
            .iter()
            .find(|asset| asset.path == "First/ProductA/Scenery/one.bin")
            .expect("ассет должен попасть в инвентарь");
        assert_eq!(first_asset.uses.len(), 2);

        // Файлы не менялись — всё берётся из кэша.
        let check = super::check_route(&route, &assets, &serz, true, 2, |_| {}).unwrap();
        assert_eq!(check.asset_inventory.len(), 2);
        let first_asset = check
            .asset_inventory
            .iter()
            .find(|asset| asset.path == "First/ProductA/Scenery/one.bin")
            .expect("ассет должен попасть в инвентарь");
        assert_eq!(first_asset.uses.len(), 2);

        // Один файл изменился: пересобирается только он.
        std::fs::write(
            &second,
            "<Provider>Second</Provider><Product>ProductB</Product><BlueprintID>Scenery\\two-new.xml</BlueprintID>",
        )
        .unwrap();
        let check = super::check_route(&route, &assets, &serz, true, 2, |_| {}).unwrap();
        assert_eq!(check.asset_inventory.len(), 2);
        let paths: Vec<&str> = check
            .asset_inventory
            .iter()
            .map(|asset| asset.path.as_str())
            .collect();
        assert!(
            paths.contains(&"Second/ProductB/Scenery/two-new.bin"),
            "{paths:?}"
        );
    }

    /// Архивы .ap распаковываются: без них список ассетов остаётся пустым.
    #[test]
    fn extracts_route_archives() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let archive_path = route.join("MainContent.ap");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "Networks/Loft Tiles/+000000+000000.bin",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"data").unwrap();
            zip.finish().unwrap();
        }

        let members = super::collect_archive_members(route).unwrap();
        assert_eq!(members.len(), 1);
        assert!(
            members[0].file_name().ends_with("+000000+000000.bin"),
            "запись архива должна сохранять имя файла"
        );
    }

    /// Файл рядом с архивом перекрывает одноимённый файл из архива.
    #[test]
    fn files_outside_archive_override_archive_members() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        {
            let file = std::fs::File::create(route.join("MainContent.ap")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "TemplateRoutes/route.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"<root/>").unwrap();
            zip.finish().unwrap();
        }
        let members = super::collect_archive_members(route).unwrap();
        let inside = members[0].route_name(route).unwrap();
        assert_eq!(inside, "TemplateRoutes/route.xml");

        // Тот же файл рядом с архивом: игра читает его вместо файла из архива.
        let outside = route.join("TemplateRoutes/route.xml");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"<root/>").unwrap();
        let name = crate::domain::RouteFile::Path(outside).route_name(route).unwrap();
        assert!(name.eq_ignore_ascii_case(&inside));
    }

    /// Изменённый файл из архива кладётся рядом с архивом: так он перекроет
    /// оригинал, а сам архив останется нетронутым.
    #[test]
    fn changed_archive_file_is_installed_next_to_the_archive() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let inside = crate::domain::RouteFile::Archive {
            archive: route.join("MainContent.ap"),
            name: "TemplateRoutes/route.xml".to_owned(),
            stamp: None,
        };
        let target = inside.install_target(route);
        assert_eq!(target, route.join("TemplateRoutes/route.xml"));
    }

    /// Ссылки на составы лежат внутри iRailVehicleBlueprint: такие ассеты —
    /// файлы поездов, а не объекты маршрута.
    #[test]
    fn detects_vehicle_references_by_their_context() {
        let xml = r#"
            <root>
                <SceneryRenderBlueprint>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">Prov</Provider>
                            <Product d:type="cDeltaString">Prod</Product>
                        </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Scenery/House.bin</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </SceneryRenderBlueprint>
                <iRailVehicleBlueprint>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">Loco</Provider>
                            <Product d:type="cDeltaString">Wagons</Product>
                        </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Trains/Loco.bin</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </iRailVehicleBlueprint>
            </root>
        "#;
        let references = crate::reference::extract_asset_references_with_locations(xml);
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].kind, crate::domain::AssetKind::Scenery);
        assert_eq!(references[1].kind, crate::domain::AssetKind::Vehicle);
    }

    /// Замена ассета в маршруте, упакованном в .ap: файл из архива читается
    /// прямо из zip, а изменённая копия кладётся рядом с архивом.
    #[test]
    fn replaces_asset_in_route_packed_into_archive() {
        use std::io::Write;

        let serz = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks\serz.exe");
        if !serz.is_file() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let archive = route.join("MainContent.ap");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "Scenery/Grass.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(
                br#"<root xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices"><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type="cDeltaString">Prov</Provider><Product d:type="cDeltaString">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type="cDeltaString">Scenery/Old.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let assets = temp.path().join("Assets");
        std::fs::create_dir_all(assets.join("Prov2/Prod2/Scenery")).unwrap();
        std::fs::write(assets.join("Prov2/Prod2/Scenery/New.bin"), b"new").unwrap();

        let uses = [crate::domain::AssetUse {
            source_file: crate::domain::RouteFile::Archive {
                archive: archive.clone(),
                name: "Scenery/Grass.xml".to_owned(),
                stamp: None,
            },
            tile: "тайл X=+000000 Z=+000000".to_owned(),
            tile_kind: crate::domain::RouteFile::Archive {
                archive: archive.clone(),
                name: "Scenery/Grass.xml".to_owned(),
                stamp: None,
            }
            .tile_kind(),
            kind: crate::domain::AssetKind::Scenery,
        }];

        let xml = std::fs::read_to_string(&archive).ok();
        let _ = xml;
        let extracted = super::extract_asset_references_with_locations(
            "<root><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type=\"cDeltaString\">Prov</Provider><Product d:type=\"cDeltaString\">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type=\"cDeltaString\">Scenery/Old.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>",
        );
        eprintln!(
            "DBG references: {:?}",
            extracted
                .iter()
                .map(|reference| reference.path.clone())
                .collect::<Vec<_>>()
        );

        let report = super::replace_missing_asset(
            route,
            &assets,
            &serz,
            "Prov/Prod/Scenery/Old.bin",
            &assets.join("Prov2/Prod2/Scenery/New.bin"),
            &uses,
        )
        .expect("замена ассета в архиве должна проходить");

        assert_eq!(report.replaced_files, 1);
        // Изменённый файл лежит рядом с архивом: игра прочитает его вместо
        // записи внутри архива.
        let installed = route.join("Scenery/Grass.xml");
        assert!(
            installed.is_file(),
            "изменённый файл должен лежать рядом с архивом"
        );
        let contents = std::fs::read_to_string(&installed).unwrap();
        assert!(
            contents.contains(r"Scenery\New.xml"),
            "ссылка в файле должна вести на новый ассет: {contents}"
        );
    }

    /// Пути, отличающиеся только регистром, — это один и тот же ассет: игра
    /// регистр не различает.
    #[test]
    fn assets_with_different_case_are_the_same_asset() {
        assert_eq!(
            super::asset_key("RSSLO_Routes/Wien/Scenery/Grass/Grass_Mix_06.bin"),
            super::asset_key("RSSLO_Routes/wien/scenery/grass/grass_mix_06.bin")
        );
    }

    /// Ссылки, отличающиеся регистром, попадают в один ассет: игра читает
    /// пути без учёта регистра.
    #[test]
    fn merges_assets_referenced_with_different_case() {
        let serz = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks\serz.exe");
        if !serz.is_file() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        std::fs::create_dir_all(route.join("Scenery")).unwrap();
        for (name, folder) in [("a.xml", "Scenery"), ("b.xml", "scenery")] {
            let xml = format!(
                "<root xmlns:d=\"http://schemas.microsoft.com/ado/2007/08/dataservices\"><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type=\"cDeltaString\">Prov</Provider><Product d:type=\"cDeltaString\">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type=\"cDeltaString\">{folder}/Grass.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>"
            );
            std::fs::write(route.join(name), xml).unwrap();
        }
        let assets = temp.path().join("Assets");
        std::fs::create_dir_all(&assets).unwrap();

        let result = super::check_route(route, &assets, &serz, true, 1, |_| {})
            .expect("проверка маршрута должна пройти");

        let scenery: Vec<_> = result
            .asset_inventory
            .iter()
            .filter(|asset| asset.path.to_lowercase().contains("grass.bin"))
            .collect();
        assert_eq!(
            scenery.len(),
            1,
            "ссылки с разным регистром должны попасть в один ассет"
        );
        assert_eq!(
            scenery[0].uses.len(),
            2,
            "оба использования должны попасть в ассет"
        );
    }

    #[test]
    fn excludes_leftover_work_directories_from_route_files() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path().join("route");
        std::fs::create_dir_all(route.join("Scenery")).unwrap();
        std::fs::write(route.join("Scenery").join("tile.bin"), b"data").unwrap();
        let leftover = route
            .join("Scenery")
            .join(format!("{TEMP_WORK_DIRECTORY_PREFIX}abcd1234"));
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("source.xml"), b"<route />").unwrap();
        std::fs::write(leftover.join("replacement.bin"), b"data").unwrap();

        let files = collect_route_files(&route, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("Scenery\\tile.bin") || files[0].ends_with("Scenery/tile.bin"));
        assert_eq!(
            leftover_temporary_work_directories(&route).unwrap(),
            [leftover.display().to_string()]
        );
    }

    #[test]
    fn extracts_locations_for_multiple_asset_references() {
        let xml = r#"
            <Provider>First</Provider><Product>ProductA</Product>
            <BlueprintID>Scenery\first.xml</BlueprintID>
            <Provider>Second</Provider><Product>ProductB</Product>
            <BlueprintID>Networks\second.xml</BlueprintID>
        "#;
        let references = crate::reference::extract_asset_references_with_locations(xml);

        assert_eq!(references.len(), 2);
        assert_eq!(references[0].path, "First/ProductA/Scenery/first.bin");
        assert_eq!(references[1].path, "Second/ProductB/Networks/second.bin");
        assert_eq!(&xml[references[0].provider_range.clone()], "First");
        assert_eq!(&xml[references[1].product_range.clone()], "ProductB");
    }

    #[test]
    fn extracts_references_from_wrapped_absolute_blueprint_blocks() {
        let xml = r#"
            <BlueprintID>
                <iBlueprintLibrary-cAbsoluteBlueprintID>
                    <BlueprintSetID>
                        <iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">JustTrains</Provider>
                            <Product d:type="cDeltaString">CommonLibrary</Product>
                        </iBlueprintLibrary-cBlueprintSetID>
                    </BlueprintSetID>
                    <BlueprintID d:type="cDeltaString">Scenery\Clutter\jt_clutter_pipe_low.xml</BlueprintID>
                </iBlueprintLibrary-cAbsoluteBlueprintID>
            </BlueprintID>
        "#;
        let references = crate::reference::extract_asset_references_with_locations(xml);

        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].path,
            "JustTrains/CommonLibrary/Scenery/Clutter/jt_clutter_pipe_low.bin"
        );
        assert_eq!(&xml[references[0].provider_range.clone()], "JustTrains");
        assert_eq!(&xml[references[0].product_range.clone()], "CommonLibrary");
        assert_eq!(
            &xml[references[0].blueprint_range.clone()],
            r"Scenery\Clutter\jt_clutter_pipe_low.xml"
        );
    }

    #[test]
    fn empty_reskin_block_does_not_hide_the_next_object_reference() {
        let xml = r#"
            <cDynamicEntity>
                <BlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString">First</Provider>
                                <Product d:type="cDeltaString">ProductA</Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Scenery\first.xml</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </BlueprintID>
                <ReskinBlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString"></Provider>
                                <Product d:type="cDeltaString"></Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString"></BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </ReskinBlueprintID>
            </cDynamicEntity>
            <cDynamicEntity>
                <BlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString">Second</Provider>
                                <Product d:type="cDeltaString">ProductB</Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Networks\second.xml</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </BlueprintID>
                <ReskinBlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString"></Provider>
                                <Product d:type="cDeltaString"></Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString"></BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </ReskinBlueprintID>
            </cDynamicEntity>
        "#;
        let references = crate::reference::extract_asset_references_with_locations(xml);

        assert_eq!(references.len(), 2);
        assert_eq!(references[0].path, "First/ProductA/Scenery/first.bin");
        assert_eq!(references[1].path, "Second/ProductB/Networks/second.bin");
    }

    #[test]
    fn detects_asset_paths_encoded_as_numeric_xml_entities() {
        let xml = r#"
            <Provider>RWAustria_3</Provider>
            <Product>Marchegger_Ostbahn</Product>
            <BlueprintID>Marchegg\Bruecke Fu&#223;ganger 5.xml</BlueprintID>
        "#;
        let references = crate::reference::extract_asset_references_with_locations(xml);

        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].path,
            "RWAustria_3/Marchegger_Ostbahn/Marchegg/Bruecke Fußganger 5.bin"
        );
        assert!(crate::translit::has_umlauts(&references[0].path));
    }

    /// Файл без `BlueprintID` не конвертируется: ссылок в нём быть не может.
    #[test]
    fn skips_bin_files_without_blueprint_marker() {
        let temp = tempfile::tempdir().unwrap();
        let without_marker = temp.path().join("plain.bin");
        std::fs::write(&without_marker, b"SERZ\x00\x00\x01\x00some payload").unwrap();
        let with_marker = temp.path().join("with.bin");
        std::fs::write(&with_marker, b"SERZ\x00\x00\x01\x00BlueprintID payload").unwrap();

        assert!(!super::may_contain_asset_reference(&without_marker));
        assert!(super::may_contain_asset_reference(&with_marker));
    }

    #[test]
    fn extracts_tile_coordinates_from_route_file_names() {
        let route = Path::new(r"C:\Routes\RouteA");
        let source = route.join("Scenery").join("+000012-000034.bin");
        assert_eq!(
            crate::reference::route_tile_label(route, &crate::domain::RouteFile::Path(source.clone())),
            "тайл X=+000012 Z=-000034"
        );
    }

    #[test]
    fn detects_geometry_and_texture_references_in_asset_xml() {
        let xml = r#"
            <GeometryID d:type="cDeltaString">RailVehicles\Brücke.GeoPcDx</GeometryID>
            <Texture d:type="cDeltaString">Textures\Bäume.TgPcDx</Texture>
        "#;
        let references = referenced_geometry_paths(xml);

        assert_eq!(references.len(), 2);
        assert_eq!(references[0].0.to_text(), r"RailVehicles\Brücke.GeoPcDx");
        assert_eq!(references[1].0.to_text(), r"Textures\Bäume.TgPcDx");
    }
}

fn first_blueprint_tag(xml: &str) -> Option<String> {
    let regex = Regex::new(r"<(c[A-Za-z0-9]+Blueprint)").ok()?;
    regex
        .captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}
