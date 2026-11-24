use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

pub(crate) const BROKEN_CHARACTERS: [char; 2] = ['\u{FFFD}', '\u{FFDF}'];
pub(crate) const UMLAUT_CHARACTERS: [&str; 8] = ["ä", "Ä", "ö", "Ö", "ü", "Ü", "ß", "ẞ"];
pub(crate) const TRANSLITERATED_CHARACTERS: [&str; 8] =
    ["ae", "Ae", "oe", "Oe", "ue", "Ue", "ss", "SS"];
pub(crate) const MAX_BROKEN_CHARACTERS: usize = 3;
pub(crate) const MAX_REFERENCE_VARIANTS: usize = 2;
pub(crate) const BLUEPRINT_ID_MARKER: &str = "BlueprintID";

pub(crate) type CharacterFixes = BTreeMap<char, String>;

pub(crate) fn decode_xml_text(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.contains('&') {
        return trimmed.to_owned();
    }
    let mut decoded = String::with_capacity(trimmed.len());
    let mut remaining = trimmed;
    while let Some(ampersand) = remaining.find('&') {
        decoded.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand + 1..];
        let Some(semicolon) = remaining.find(';') else {
            decoded.push('&');
            continue;
        };
        let entity = &remaining[..semicolon];
        if let Some(character) = decode_xml_entity(entity) {
            decoded.push(character);
        } else {
            decoded.push('&');
            decoded.push_str(entity);
            decoded.push(';');
        }
        remaining = &remaining[semicolon + 1..];
    }
    decoded.push_str(remaining);
    decoded
}

pub(crate) fn transliterate_xml_umlauts_outside(
    xml: &str,
    protected: &[Range<usize>],
    fixes: &CharacterFixes,
) -> String {
    if protected.is_empty() {
        return transliterate_xml_umlauts(xml, fixes);
    }
    let mut result = String::with_capacity(xml.len());
    let mut position = 0;
    for range in protected {
        let start = range.start.min(xml.len());
        let end = range.end.min(xml.len());
        if end <= position {
            continue;
        }
        if start > position {
            transliterate_xml_umlauts_into(&xml[position..start], fixes, &mut result);
        }
        let protected_start = start.max(position);
        result.push_str(&xml[protected_start..end]);
        position = end;
    }
    transliterate_xml_umlauts_into(&xml[position..], fixes, &mut result);
    result
}

pub(crate) fn has_umlauts_in_xml(xml: &str) -> bool {
    if has_umlauts(xml) {
        return true;
    }
    let mut remaining = xml;
    while let Some(ampersand) = remaining.find('&') {
        remaining = &remaining[ampersand + 1..];
        let Some(semicolon) = remaining.find(';') else {
            break;
        };
        let entity = &remaining[..semicolon];
        if let Some(ch) = decode_numeric_xml_entity(entity) {
            if transliteration_rule(ch).is_some() {
                return true;
            }
            remaining = &remaining[semicolon + 1..];
        }
    }
    false
}

pub(crate) fn transliterate_xml_umlauts(xml: &str, fixes: &CharacterFixes) -> String {
    let mut output = String::with_capacity(xml.len());
    transliterate_xml_umlauts_into(xml, fixes, &mut output);
    output
}

pub(crate) fn unsupported_characters(value: &str) -> BTreeSet<char> {
    value
        .chars()
        .filter(|character| is_unsupported_character(*character))
        .collect()
}

pub(crate) fn transliterate_text(value: &str, fixes: &CharacterFixes) -> String {
    let mut result = String::with_capacity(value.len());
    transliterate_text_into(value, fixes, &mut result);
    result
}

pub(crate) fn has_umlauts(value: &str) -> bool {
    value
        .chars()
        .any(|character| transliteration_rule(character).is_some())
}

pub(crate) fn has_broken_character(value: &str) -> bool {
    value
        .chars()
        .any(|character| BROKEN_CHARACTERS.contains(&character))
}

pub(crate) fn replace_broken_characters(name: &str, replacement: &str) -> String {
    if !has_broken_character(name) {
        return name.to_owned();
    }
    let mut result = String::with_capacity(name.len());
    for character in name.chars() {
        if BROKEN_CHARACTERS.contains(&character) {
            result.push_str(replacement);
        } else {
            result.push(character);
        }
    }
    result
}

pub(crate) fn replace_broken_characters_combinations(
    name: &str,
    replacements: &[&str],
) -> Vec<String> {
    let mut variants = vec![String::with_capacity(name.len())];
    for character in name.chars() {
        if BROKEN_CHARACTERS.contains(&character) {
            let mut next = Vec::with_capacity(variants.len() * replacements.len());
            for variant in &variants {
                for replacement in replacements {
                    let mut expanded = String::with_capacity(variant.len() + replacement.len());
                    expanded.push_str(variant);
                    expanded.push_str(replacement);
                    next.push(expanded);
                }
            }
            variants = next;
        } else {
            for variant in &mut variants {
                variant.push(character);
            }
        }
    }
    variants
}

fn decode_xml_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => decode_numeric_xml_entity(entity),
    }
}

fn decode_numeric_xml_entity(entity: &str) -> Option<char> {
    let rest = entity.strip_prefix('#')?;
    let code = if let Some(hex) = rest.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        rest.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

fn transliterate_xml_umlauts_into(xml: &str, fixes: &CharacterFixes, output: &mut String) {
    let mut remaining = xml;
    while let Some(ampersand) = remaining.find('&') {
        transliterate_text_into(&remaining[..ampersand], fixes, output);
        let after_amp = &remaining[ampersand + 1..];
        let Some(semicolon) = after_amp.find(';') else {
            output.push('&');
            transliterate_text_into(after_amp, fixes, output);
            return;
        };
        let entity = &after_amp[..semicolon];
        if let Some(ch) = decode_numeric_xml_entity(entity)
            && transliteration_rule(ch).is_some()
        {
            push_transliterated_char(ch, fixes, output);
            remaining = &after_amp[semicolon + 1..];
        } else {
            output.push('&');
            remaining = after_amp;
        }
    }
    transliterate_text_into(remaining, fixes, output);
}

fn transliterate_text_into(value: &str, fixes: &CharacterFixes, output: &mut String) {
    if fixes.is_empty() && value.is_ascii() {
        output.push_str(value);
        return;
    }
    for character in value.chars() {
        push_transliterated_char(character, fixes, output);
    }
}

fn push_transliterated_char(character: char, fixes: &CharacterFixes, output: &mut String) {
    if !fixes.is_empty()
        && let Some(replacement) = fixes.get(&character)
    {
        output.push_str(replacement);
        return;
    }
    match transliteration_rule(character) {
        Some(to) => output.push_str(to),
        None => output.push(character),
    }
}

fn transliteration_rule(character: char) -> Option<&'static str> {
    if character.is_ascii() {
        return None;
    }
    let mapped = match character {
        '\u{FFFD}' | '\u{FFDF}' | 'ß' => "ss",
        'ẞ' => "SS",
        'ä' | 'æ' => "ae",
        'Ä' => "Ae",
        'Æ' => "AE",
        'ö' | 'œ' => "oe",
        'Ö' => "Oe",
        'Œ' => "OE",
        'ü' => "ue",
        'Ü' => "Ue",
        'à' | 'á' | 'â' | 'ã' | 'å' => "a",
        'À' | 'Á' | 'Â' | 'Ã' | 'Å' => "A",
        'ç' | 'č' => "c",
        'Ç' | 'Č' => "C",
        'è' | 'é' | 'ê' | 'ë' => "e",
        'È' | 'É' | 'Ê' | 'Ë' => "E",
        'ì' | 'í' | 'î' | 'ï' => "i",
        'Ì' | 'Í' | 'Î' | 'Ï' => "I",
        'ñ' => "n",
        'Ñ' => "N",
        'ò' | 'ó' | 'ô' | 'õ' | 'ø' => "o",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ø' => "O",
        'ù' | 'ú' | 'û' => "u",
        'Ù' | 'Ú' | 'Û' => "U",
        'ý' | 'ÿ' => "y",
        'Ý' | 'Ÿ' => "Y",
        'š' => "s",
        'Š' => "S",
        'ž' => "z",
        'Ž' => "Z",
        'ř' => "r",
        'Ř' => "R",
        'ł' => "l",
        'Ł' => "L",
        _ => return None,
    };
    Some(mapped)
}

fn is_unsupported_character(character: char) -> bool {
    if BROKEN_CHARACTERS.contains(&character) {
        return true;
    }
    let code = character as u32;
    if code < 0x20 || code == 0x7F {
        // Управляющие символы в именах файлов недопустимы.
        return true;
    }
    if (0x0400..=0x04FF).contains(&code) || (0x0370..=0x03FF).contains(&code) {
        // Кириллица и грека выглядят как латиница, но игра их не грузит.
        return true;
    }
    code > 0x7F && transliteration_rule(character).is_none()
}
