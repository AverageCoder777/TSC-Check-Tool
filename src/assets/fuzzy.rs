use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};

use crate::common::translit::BROKEN_CHARACTERS;

/// Являются ли две буквы похожими начертанием.
pub(crate) fn is_homoglyph(left: char, right: char) -> bool {
    let (cyrillic, ascii) = match (left.is_ascii(), right.is_ascii()) {
        (false, true) => (left, right),
        (true, false) => (right, left),
        _ => return false,
    };
    homoglyph_ascii(cyrillic) == Some(ascii)
}

/// Совпадают ли имена, если считать похожие буквы одинаковыми.
///
/// Отличаться могут только буквы-«двойники» и испорченные символы: любые другие
/// различия означают, что это другой файл.
pub(crate) fn names_match(expected: &str, candidate: &str) -> bool {
    let mut left_chars = expected.chars();
    let mut right_chars = candidate.chars();
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(left), Some(right)) => {
                let matched = left == right
                    || left.eq_ignore_ascii_case(&right)
                    || BROKEN_CHARACTERS.contains(&left)
                    || BROKEN_CHARACTERS.contains(&right)
                    || is_homoglyph(left, right);
                if !matched {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub(crate) fn has_corrupted_or_nonascii(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| !byte.is_ascii() || byte == b'?' || byte.is_ascii_control())
}

/// Ищет файл ассета по ссылке из маршрута.
///
/// Ссылка может содержать испорченные символы: сначала проверяются имена с
/// транслитерацией, затем — с настоящим умляутом. Так ассет, который на диске
/// называется с умляутом, не считается утерянным, а попадает в список на
/// переименование.
pub(crate) const GLITCH_WILDCARD: char = '\u{E000}';

pub(crate) fn normalize_fuzzy_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            let folded = match ch {
                'ä' | 'Ä' | 'á' | 'Á' | 'à' | 'À' | 'â' | 'Â' | 'ã' | 'Ã' | 'å' | 'Å' => 'a',
                'ö' | 'Ö' | 'ó' | 'Ó' | 'ò' | 'Ò' | 'ô' | 'Ô' | 'õ' | 'Õ' => 'o',
                'ü' | 'Ü' | 'ú' | 'Ú' | 'ù' | 'Ù' | 'û' | 'Û' => 'u',
                'é' | 'É' | 'è' | 'È' | 'ê' | 'Ê' | 'ë' | 'Ë' => 'e',
                'í' | 'Í' | 'ì' | 'Ì' | 'î' | 'Î' | 'ï' | 'Ï' => 'i',
                'ß' | 'ẞ' => 's',
                _ => ch.to_ascii_lowercase(),
            };
            out.push(folded);
        } else if matches!(ch, '¦' | '|' | '?') || BROKEN_CHARACTERS.contains(&ch) {
            out.push(GLITCH_WILDCARD);
        }
    }
    out
}

pub(crate) fn fold_german_diphthongs(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, 'a' | 'o' | 'u') && chars.peek() == Some(&'e') {
            chars.next();
        }
        out.push(ch);
    }
    out
}

pub(crate) fn glitch_match(query: &[char], candidate: &[char]) -> bool {
    let wildcards = query.iter().filter(|&&ch| ch == GLITCH_WILDCARD).count();
    let Some(extra_budget) = candidate.len().checked_sub(query.len()) else {
        return false;
    };
    if extra_budget > wildcards {
        return false;
    }
    glitch_match_inner(query, candidate, extra_budget, wildcards)
}

pub(crate) fn find_fuzzy_child(
    parent: &Path,
    query: &str,
    want_directory: bool,
) -> Option<PathBuf> {
    let prepared = PreparedFuzzyQuery::new(query)?;
    let entries = fs::read_dir(parent).ok()?;
    let mut fallback = None;

    for entry in entries.flatten() {
        if entry_is_dir(&entry) != want_directory {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !prepared.matches(&name) {
            continue;
        }
        if name.is_ascii() {
            return Some(entry.path());
        }
        if fallback.is_none() {
            fallback = Some(entry.path());
        }
    }
    fallback
}

pub(crate) fn fuzzy_resolve_disk_path(
    assets_path: &Path,
    relative_asset: &str,
) -> Option<(PathBuf, String)> {
    let mut parts = relative_asset
        .split(['/', '\\'])
        .filter(|part| !part.is_empty());
    let provider = parts.next()?;
    let product = parts.next()?;
    let relative_segments: Vec<&str> = parts.collect();
    if relative_segments.is_empty() {
        return None;
    }

    let (provider_dir, provider_name) = resolve_segment(assets_path, provider, true)?;
    let (mut current, product_name) = resolve_segment(&provider_dir, product, true)?;

    let last_index = relative_segments.len() - 1;
    let mut matched_segments = Vec::with_capacity(relative_segments.len());
    for (index, segment) in relative_segments.iter().enumerate() {
        let want_directory = index != last_index;
        let (matched, name) = resolve_segment(&current, segment, want_directory)?;
        matched_segments.push(name);
        current = matched;
    }

    let relative_repair = format!(
        "{provider_name}/{product_name}/{}",
        matched_segments.join("/")
    );
    Some((current, relative_repair))
}

struct PreparedFuzzyQuery<'a> {
    raw: &'a str,
    normalized: String,
    chars: Vec<char>,
    has_glitch: bool,
}

impl<'a> PreparedFuzzyQuery<'a> {
    fn new(query: &'a str) -> Option<Self> {
        let normalized = normalize_fuzzy_segment(query);
        if normalized.is_empty() {
            return None;
        }
        let has_glitch = normalized.contains(GLITCH_WILDCARD);
        let chars = if has_glitch {
            normalized.chars().collect()
        } else {
            Vec::new()
        };
        Some(Self {
            raw: query,
            normalized,
            chars,
            has_glitch,
        })
    }

    fn matches(&self, candidate: &str) -> bool {
        if !ascii_digits_equal(self.raw, candidate) {
            return false;
        }
        let right_raw = normalize_fuzzy_segment(candidate);
        if right_raw.is_empty() {
            return false;
        }
        if !self.has_glitch {
            // Clean path: exact normalized equality only.
            return self.normalized == right_raw;
        }

        // Glitch tolerance: allow 1-2 characters in the candidate at the glitch
        // position. Try both raw and diphthong-folded candidate forms.
        let right_raw_chars: Vec<char> = right_raw.chars().collect();
        if glitch_match(&self.chars, &right_raw_chars) {
            return true;
        }
        let right_folded = fold_german_diphthongs(&right_raw);
        if right_folded == right_raw {
            return false;
        }
        let right_folded_chars: Vec<char> = right_folded.chars().collect();
        glitch_match(&self.chars, &right_folded_chars)
    }
}

fn resolve_segment(parent: &Path, segment: &str, want_directory: bool) -> Option<(PathBuf, String)> {
    if !has_corrupted_or_nonascii(segment) {
        let direct = parent.join(segment);
        let matches_type = if want_directory {
            direct.is_dir()
        } else {
            direct.is_file()
        };
        if matches_type {
            return Some((direct, segment.to_owned()));
        }
    }
    let matched = find_fuzzy_child(parent, segment, want_directory)?;
    let name = matched.file_name()?.to_string_lossy().into_owned();
    Some((matched, name))
}

fn entry_is_dir(entry: &DirEntry) -> bool {
    entry.file_type().map_or_else(
        |_| entry.path().is_dir(),
        |file_type| {
            if file_type.is_symlink() {
                entry.path().is_dir()
            } else {
                file_type.is_dir()
            }
        },
    )
}

fn ascii_digits_equal(left: &str, right: &str) -> bool {
    left.bytes()
        .filter(u8::is_ascii_digit)
        .eq(right.bytes().filter(u8::is_ascii_digit))
}

fn glitch_match_inner(
    mut query: &[char],
    mut candidate: &[char],
    extra_budget: usize,
    wildcards: usize,
) -> bool {
    loop {
        if extra_budget > wildcards || candidate.len() < query.len() {
            return false;
        }
        let Some((&first, rest_query)) = query.split_first() else {
            return candidate.is_empty();
        };
        if first == GLITCH_WILDCARD {
            let next_wildcards = wildcards - 1;
            if extra_budget <= next_wildcards
                && glitch_match_inner(rest_query, &candidate[1..], extra_budget, next_wildcards)
            {
                return true;
            }
            return extra_budget > 0
                && candidate.len() >= 2
                && glitch_match_inner(
                    rest_query,
                    &candidate[2..],
                    extra_budget - 1,
                    next_wildcards,
                );
        }
        let Some((&cand_first, rest_cand)) = candidate.split_first() else {
            return false;
        };
        if first != cand_first {
            return false;
        }
        query = rest_query;
        candidate = rest_cand;
    }
}

fn homoglyph_ascii(character: char) -> Option<char> {
    let mapped = match character {
        'а' => 'a',
        'б' | 'в' | 'ъ' | 'ы' | 'ь' => 'b',
        'г' | 'я' => 'r',
        'д' => 'd',
        'е' | 'ё' | 'э' => 'e',
        'ж' | 'х' => 'x',
        'з' => '3',
        'и' | 'й' | 'ц' => 'u',
        'к' => 'k',
        'л' => 'l',
        'м' => 'm',
        'н' => 'h',
        'о' => 'o',
        'п' => 'n',
        'р' => 'p',
        'с' => 'c',
        'т' => 't',
        'у' => 'y',
        'ф' => 'f',
        'ч' => '4',
        'ш' | 'щ' => 'w',
        'ю' => 'i',
        'А' => 'A',
        'В' => 'B',
        'Е' => 'E',
        'К' => 'K',
        'М' => 'M',
        'Н' => 'H',
        'О' => 'O',
        'Р' => 'P',
        'С' => 'C',
        'Т' => 'T',
        'У' => 'Y',
        'Х' => 'X',
        _ => return None,
    };
    Some(mapped)
}
