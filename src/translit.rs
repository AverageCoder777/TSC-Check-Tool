//! Перевод имён ассетов на английский.
//!
//! Немецкие умляуты переводятся по правилам немецкого языка, французские буквы —
//! по правилам французского. Символы без правила перевода (например, кириллица
//! в именах файлов) оставляются пользователю на откуп: он сам выбирает, чем их
//! заменить.

use std::collections::{BTreeMap, BTreeSet};

/// Расшифровка XML-сущностей: игра пишет в файлы и спецсимволы.
pub(crate) fn decode_xml_text(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(ampersand) = remaining.find('&') {
        decoded.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand + 1..];
        let Some(semicolon) = remaining.find(';') else {
            decoded.push('&');
            continue;
        };
        let entity = &remaining[..semicolon];
        let character = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            value if value.starts_with('#') => {
                value[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(character) = character {
            decoded.push(character);
        } else {
            decoded.push('&');
            decoded.push_str(entity);
            decoded.push(';');
        }
        remaining = &remaining[semicolon + 1..];
    }
    decoded.push_str(remaining);
    decoded.trim().to_owned()
}

/// Транслитерирует текст XML, не затрагивая указанные участки.
///
/// Участки нужны для ссылок на файлы за пределами папки ассета: их нельзя
/// переписывать, иначе ссылка перестанет указывать на существующий файл.
pub(crate) fn transliterate_xml_umlauts_outside(
    xml: &str,
    protected: &[std::ops::Range<usize>],
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
        if start < position {
            continue;
        }
        result.push_str(&transliterate_xml_umlauts(&xml[position..start], fixes));
        result.push_str(&xml[start..end]);
        position = end;
    }
    result.push_str(&transliterate_xml_umlauts(&xml[position..], fixes));
    result
}

/// Есть ли в XML символы, которые нужно перевести.
pub(crate) fn has_umlauts_in_xml(xml: &str) -> bool {
    has_umlauts(&decode_xml_text(xml))
}

pub(crate) fn transliterate_xml_umlauts(xml: &str, fixes: &CharacterFixes) -> String {
    let mut output = String::with_capacity(xml.len());
    let mut remaining = xml;
    while let Some(ampersand) = remaining.find('&') {
        output.push_str(&transliterate_text(&remaining[..ampersand], fixes));
        remaining = &remaining[ampersand..];
        let Some(semicolon) = remaining.find(';') else {
            output.push_str(&transliterate_text(remaining, fixes));
            return output;
        };
        let entity_end = semicolon + 1;
        let decoded = decode_xml_text(&remaining[..entity_end]);
        if decoded.chars().count() == 1 && has_umlauts(&decoded) {
            output.push_str(&transliterate_text(&decoded, fixes));
        } else {
            output.push_str(&remaining[..entity_end]);
        }
        remaining = &remaining[entity_end..];
    }
    output.push_str(&transliterate_text(remaining, fixes));
    output
}

/// Испорченные символы, которые встречаются в ссылках маршрута вместо умляута.
///
/// U+FFFD — стандартный символ замены, U+FFDF — такой же непечатаемый символ,
/// который раньше встречался в ссылках маршрутов.
pub(crate) const BROKEN_CHARACTERS: [char; 2] = ['\u{FFFD}', '\u{FFDF}'];

/// Настоящие умляуты, которыми может быть назван файл ассета.
pub(crate) const UMLAUT_CHARACTERS: [&str; 8] = ["ä", "Ä", "ö", "Ö", "ü", "Ü", "ß", "ẞ"];

/// Транслитерации умляутов, которыми могли оказаться переименованы файлы.
pub(crate) const TRANSLITERATED_CHARACTERS: [&str; 8] =
    ["ae", "Ae", "oe", "Oe", "ue", "Ue", "ss", "SS"];

/// Перевод букв с диакритикой на английский.
///
/// Немецкие умляуты переводятся по правилам немецкого языка (`ä` → `ae`),
/// французские — по правилам французского (`é` → `e`, `ç` → `c`).
const TRANSLITERATION_RULES: &[(char, &str)] = &[
    // Испорченные символы по умолчанию читаются как «ss»: раньше в ссылках
    // игры они означали именно «ß».
    ('\u{FFFD}', "ss"),
    ('\u{FFDF}', "ss"),
    ('ä', "ae"),
    ('Ä', "Ae"),
    ('ö', "oe"),
    ('Ö', "Oe"),
    ('ü', "ue"),
    ('Ü', "Ue"),
    ('ß', "ss"),
    ('ẞ', "SS"),
    ('à', "a"),
    ('À', "A"),
    ('á', "a"),
    ('Á', "A"),
    ('â', "a"),
    ('Â', "A"),
    ('ã', "a"),
    ('Ã', "A"),
    ('å', "a"),
    ('Å', "A"),
    ('æ', "ae"),
    ('Æ', "AE"),
    ('ç', "c"),
    ('Ç', "C"),
    ('è', "e"),
    ('È', "E"),
    ('é', "e"),
    ('É', "E"),
    ('ê', "e"),
    ('Ê', "E"),
    ('ë', "e"),
    ('Ë', "E"),
    ('ì', "i"),
    ('Ì', "I"),
    ('í', "i"),
    ('Í', "I"),
    ('î', "i"),
    ('Î', "I"),
    ('ï', "i"),
    ('Ï', "I"),
    ('ñ', "n"),
    ('Ñ', "N"),
    ('ò', "o"),
    ('Ò', "O"),
    ('ó', "o"),
    ('Ó', "O"),
    ('ô', "o"),
    ('Ô', "O"),
    ('õ', "o"),
    ('Õ', "O"),
    ('ø', "o"),
    ('Ø', "O"),
    ('œ', "oe"),
    ('Œ', "OE"),
    ('ù', "u"),
    ('Ù', "U"),
    ('ú', "u"),
    ('Ú', "U"),
    ('û', "u"),
    ('Û', "U"),
    ('ý', "y"),
    ('Ý', "Y"),
    ('ÿ', "y"),
    ('Ÿ', "Y"),
    ('š', "s"),
    ('Š', "S"),
    ('ž', "z"),
    ('Ž', "Z"),
    ('č', "c"),
    ('Č', "C"),
    ('ř', "r"),
    ('Ř', "R"),
    ('ł', "l"),
    ('Ł', "L"),
];

/// Замены символов, предложенные пользователем.
///
/// Для испорченных символов пользователь сам выбирает, чем их заменить:
/// обычно это английская «s» или «c», но назвать можно что угодно.
pub(crate) type CharacterFixes = BTreeMap<char, String>;

/// Есть ли правило перевода для символа.
fn has_transliteration_rule(character: char) -> bool {
    TRANSLITERATION_RULES
        .iter()
        .any(|(from, _)| *from == character)
}

/// Символы, которые игра не умеет загружать.
///
/// Кроме испорченных символов это кириллические и греческие буквы: игра
/// такими именами файлов не пользуется, а выглядят они как латинские.
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
    code > 0x7F && !has_transliteration_rule(character)
}

/// Все неподдерживаемые символы в строке.
pub(crate) fn unsupported_characters(value: &str) -> BTreeSet<char> {
    value
        .chars()
        .filter(|character| is_unsupported_character(*character))
        .collect()
}

/// Переводит буквы с диакритикой на английский и подставляет замены,
/// предложенные пользователем.
pub(crate) fn transliterate_text(value: &str, fixes: &CharacterFixes) -> String {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        if let Some(replacement) = fixes.get(&character) {
            result.push_str(replacement);
            continue;
        }
        match TRANSLITERATION_RULES
            .iter()
            .find(|(from, _)| *from == character)
        {
            Some((_, to)) => result.push_str(to),
            None => result.push(character),
        }
    }
    result
}

/// Сколько испорченных символов в ссылке ещё разбираются.
pub(crate) const MAX_BROKEN_CHARACTERS: usize = 3;

/// Сколько испорченных символов в ссылке ещё разбирается перебором вариантов.
///
/// Каждый символ умножает число вариантов на шестнадцать, поэтому больше двух
/// символов перебором не обрабатываются.
pub(crate) const MAX_REFERENCE_VARIANTS: usize = 2;

/// Имя элемента, без которого в файле нет ни одной ссылки на ассет.
pub(crate) const BLUEPRINT_ID_MARKER: &str = "BlueprintID";

/// Есть ли в строке символы, которые нужно перевести на английский.
///
/// Это немецкие умляуты, французские буквы и прочие буквы с диакритикой, а
/// также испорченные символы: игра такие имена не читает.
pub(crate) fn has_umlauts(value: &str) -> bool {
    value.chars().any(|character| {
        has_transliteration_rule(character) || BROKEN_CHARACTERS.contains(&character)
    })
}

/// Есть ли в строке испорченный символ.
pub(crate) fn has_broken_character(value: &str) -> bool {
    value
        .chars()
        .any(|character| BROKEN_CHARACTERS.contains(&character))
}

/// Заменяет испорченные символы в имени на переданную подстановку.
pub(crate) fn replace_broken_characters(name: &str, replacement: &str) -> String {
    BROKEN_CHARACTERS
        .iter()
        .fold(name.to_owned(), |value, character| {
            value.replace(*character, replacement)
        })
}

/// Варианты имени, где каждый испорченный символ заменён по отдельности.
///
/// Например, `Br\u{FFFD}cke Fu\u{FFFD}g\u{FFFD}nger` превращается в
/// `Brücke Fußgänger`, `Brücke Fussganger` и другие комбинации. Число вариантов
/// растёт как степень количества подстановок, поэтому ссылка с большим числом
/// испорченных символов не разбирается вовсе.
pub(crate) fn replace_broken_characters_combinations(
    name: &str,
    replacements: &[&str],
) -> Vec<String> {
    let mut variants = vec![String::new()];
    for character in name.chars() {
        let mut next = Vec::new();
        for variant in &variants {
            if BROKEN_CHARACTERS.contains(&character) {
                for replacement in replacements {
                    next.push(format!("{variant}{replacement}"));
                }
            } else {
                next.push(format!("{variant}{character}"));
            }
        }
        variants = next;
    }
    variants
}