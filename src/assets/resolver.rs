use std::fs;
use std::path::{Path, PathBuf};

use crate::assets::{
    fuzzy::names_match,
    xml_reference::{GeometryReference, ReferenceBase},
};
use crate::common::translit::*;

pub fn resolve_asset_path(
    assets_path: &Path,
    relative_asset: &str,
) -> Result<Option<PathBuf>, String> {
    let exact = assets_path.join(relative_asset.replace('/', "\\"));
    if exact.is_file() {
        return Ok(Some(exact));
    }
    if !has_broken_character(relative_asset) {
        // Файла нет по точному имени: ищем ближайший по сходству. В игре
        // встречаются имена с кириллическими буквами вместо латинских.
        return Ok(find_similar_asset_path(assets_path, relative_asset));
    }
    let broken_count = relative_asset
        .chars()
        .filter(|character| BROKEN_CHARACTERS.contains(character))
        .count();
    if broken_count > MAX_BROKEN_CHARACTERS {
        return Ok(None);
    }

    let tiers = [
        replace_broken_characters_combinations(relative_asset, &TRANSLITERATED_CHARACTERS),
        replace_broken_characters_combinations(relative_asset, &UMLAUT_CHARACTERS),
    ];

    // Windows считает «ä» и «Ä» одним и тем же именем файла, поэтому одинаковые
    // файлы из разных вариантов отсекаются по каноническому пути.
    let mut candidates: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
    for tier in tiers {
        for variant in tier {
            let candidate = assets_path.join(variant.replace('/', "\\"));
            if !candidate.is_file() {
                continue;
            }
            let identity = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if !candidates
                .iter()
                .any(|(_, existing, _)| *existing == identity)
            {
                let is_ascii = variant.is_ascii();
                candidates.push((candidate, identity, is_ascii));
            }
        }
    }
    if let Some(position) = candidates.iter().position(|(_, _, is_ascii)| *is_ascii) {
        return Ok(Some(candidates.remove(position).0));
    }
    match candidates.len() {
        0 => Ok(find_similar_asset_path(assets_path, relative_asset)),
        1 => Ok(Some(candidates.remove(0).0)),
        _ => Err(format!(
            "Ссылка содержит испорченный символ и совпадает с несколькими файлами: {}",
            candidates
                .iter()
                .map(|(path, _, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Ищет файл, похожий на указанный путь.
///
/// Имена в игре иногда отличаются от ссылок одной буквой: кириллическая «Е»
/// вместо латинской или испорченный символ. Тогда ищем ближайшее по сходству.
pub fn find_similar_asset_path(assets_path: &Path, relative_asset: &str) -> Option<PathBuf> {
    let mut current = assets_path.to_path_buf();
    let components: Vec<&str> = relative_asset
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    for (index, component) in components.iter().enumerate() {
        if index + 1 == components.len() {
            // Сначала имя целиком, потом с расширениями: в ссылке расширение
            // может быть, а может и отсутствовать.
            return find_file_with_extensions(
                &current,
                component,
                &["bin", "GeoPcDx", "TgPcDx", "xml"],
                true,
            );
        }
        current = find_entry_in_directory(&current, component, true)?;
    }
    None
}

/// Описание ссылки, которая не нашлась на диске.
///
/// В сообщение попадает содержимое папки: без него непонятно, что именно не
/// совпало с именем файла.
pub fn unresolved_reference_error(
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
    let folders = [source.parent(), Some(product_root), Some(assets_root)];
    for folder in folders.into_iter().flatten() {
        if !folder.is_dir() {
            continue;
        }
        let names: Vec<String> = fs::read_dir(folder)
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
pub fn resolve_product_reference(
    assets_root: &Path,
    product_root: &Path,
    source_file: &Path,
    reference: &GeometryReference,
) -> Option<(PathBuf, ReferenceBase)> {
    let base = reference.path.replace('/', "\\");
    let mut names = vec![base.clone()];
    if has_broken_character(&base) {
        let broken_count = base
            .chars()
            .filter(|character| BROKEN_CHARACTERS.contains(character))
            .count();
        if broken_count <= MAX_REFERENCE_VARIANTS {
            // Испорченный символ в ссылке может означать и умляут, и уже
            // переписанное имя, причём в разных частях пути по-разному, поэтому
            // проверяются все сочетания.
            let mut replacements: Vec<&str> =
                Vec::with_capacity(TRANSLITERATED_CHARACTERS.len() + UMLAUT_CHARACTERS.len());
            replacements.extend_from_slice(&TRANSLITERATED_CHARACTERS);
            replacements.extend_from_slice(&UMLAUT_CHARACTERS);
            names.extend(replace_broken_characters_combinations(&base, &replacements));
        }
    }
    let extensions = reference.disk_extensions();
    let bases = [
        (assets_root, ReferenceBase::Assets),
        (product_root, ReferenceBase::Product),
        (source_file.parent()?, ReferenceBase::Folder),
    ];
    for &(directory, kind) in &bases {
        for name in &names {
            for extension in &extensions {
                let candidate = directory.join(format!("{name}.{extension}"));
                if candidate.is_file() {
                    return Some((candidate, kind));
                }
            }
        }
    }
    // Точного совпадения нет: ищем ближайшее по сходству. Имя папки может
    // отличаться регистром, кириллическими буквами или количеством букв.
    for &(directory, kind) in &bases {
        if let Some(path) = resolve_reference_path(directory, &base, &extensions) {
            return Some((path, kind));
        }
    }
    None
}

/// Ищет в папке файл или подпапку с подходящим именем.
///
/// Сначала проверяются точное имя и регистр, а при неудаче ищется ближайшее по
/// сходству: в игре встречаются имена с испорченными символами и с
/// кириллическими буквами вместо латинских.
pub fn find_entry_in_directory(
    directory: &Path,
    wanted: &str,
    want_directory: bool,
) -> Option<PathBuf> {
    let exact = directory.join(wanted);
    if let Ok(meta) = fs::metadata(&exact)
        && ((want_directory && meta.is_dir()) || (!want_directory && meta.is_file()))
    {
        return Some(exact);
    }
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        // Проверяем тип без дополнительных системных вызовов к диску
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir != want_directory {
            continue;
        }
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if name_str.eq_ignore_ascii_case(wanted) || names_match(wanted, &name_str) {
            return Some(entry.path());
        }
    }
    None
}

/// Разрешает путь ссылки, подбирая похожие имена файлов и папок.
pub fn resolve_reference_path(
    base: &Path,
    relative: &str,
    extensions: &[String],
) -> Option<PathBuf> {
    let mut current = base.to_path_buf();
    let components: Vec<&str> = relative
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    for (index, component) in components.iter().enumerate() {
        if index + 1 == components.len() {
            let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
            return find_file_with_extensions(&current, component, &ext_refs, false);
        }
        current = find_entry_in_directory(&current, component, true)?;
    }
    None
}

/// Ищет файл по базовому имени и набору расширений за один проход `read_dir`.
fn find_file_with_extensions(
    directory: &Path,
    stem: &str,
    extensions: &[&str],
    try_bare: bool,
) -> Option<PathBuf> {
    let mut wanted_names = Vec::with_capacity(extensions.len() + usize::from(try_bare));
    if try_bare {
        wanted_names.push(stem.to_owned());
    }
    for ext in extensions {
        wanted_names.push(format!("{stem}.{ext}"));
    }

    // Быстрый путь: точное совпадение на диске без чтения всей папки.
    for wanted in &wanted_names {
        let exact = directory.join(wanted);
        if exact.is_file() {
            return Some(exact);
        }
    }

    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if wanted_names
            .iter()
            .any(|wanted| name_str.eq_ignore_ascii_case(wanted) || names_match(wanted, &name_str))
        {
            return Some(entry.path());
        }
    }
    None
}
