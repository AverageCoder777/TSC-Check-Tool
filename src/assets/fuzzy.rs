use std::fs;
use std::path::{Path, PathBuf};

/// Буквы, которые выглядят одинаково, но различаются кодом.
///
/// В именах файлов игры встречается кириллическая «Е» вместо латинской: такой
/// файл нужно найти, но спутать его с другим нельзя.
pub(crate) const HOMOGLYPHS: [(char, char); 46] = [
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
pub(crate) fn is_homoglyph(left: char, right: char) -> bool {
    HOMOGLYPHS.iter().any(|(first, second)| {
        (*first == left && *second == right) || (*first == right && *second == left)
    })
}

/// Совпадают ли имена, если считать похожие буквы одинаковыми.
///
/// Отличаться могут только буквы-«двойники» и испорченные символы: любые другие
/// различия означают, что это другой файл.
pub(crate) fn names_match(expected: &str, candidate: &str) -> bool {
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

pub(crate) fn has_corrupted_or_nonascii(value: &str) -> bool {
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
pub(crate) const GLITCH_WILDCARD: char = '\u{E000}';

pub(crate) fn normalize_fuzzy_segment(value: &str) -> String {
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

pub(crate) fn fold_german_diphthongs(value: &str) -> String {
    value
        .replace("ae", "a")
        .replace("oe", "o")
        .replace("ue", "u")
}

pub(crate) fn glitch_match(query: &[char], candidate: &[char]) -> bool {
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

pub(crate) fn fuzzy_match_name(query: &str, candidate: &str) -> bool {
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

pub(crate) fn find_fuzzy_child(parent: &Path, query: &str, want_directory: bool) -> Option<PathBuf> {
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

pub(crate) fn fuzzy_resolve_disk_path(assets_path: &Path, relative_asset: &str) -> Option<(PathBuf, String)> {
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
