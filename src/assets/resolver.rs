use super::*;

pub fn resolve_asset_path(assets_path: &Path, relative_asset: &str) -> Result<Option<PathBuf>, String> {
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
        crate::translit::replace_broken_characters_combinations(
            relative_asset,
            &crate::translit::TRANSLITERATED_CHARACTERS,
        ),
        crate::translit::replace_broken_characters_combinations(
            relative_asset,
            &crate::translit::UMLAUT_CHARACTERS,
        ),
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
pub fn find_similar_asset_path(assets_path: &Path, relative_asset: &str) -> Option<PathBuf> {
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
pub fn resolve_product_reference(
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
            names.extend(crate::translit::replace_broken_characters_combinations(
                &base,
                &replacements,
            ));
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
pub fn find_entry_in_directory(
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
pub fn resolve_reference_path(base: &Path, relative: &str, extensions: &[String]) -> Option<PathBuf> {
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

