use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use walkdir::WalkDir;

/// Результат проверки цепочки ассета на сломанные символы.
pub(crate) struct UmlautChainEvidence {
    /// Обнаружения: что и где нашлось.
    pub(crate) evidence: Vec<String>,
    /// Символы, которые игра не умеет загружать.
    pub(crate) unsupported: BTreeSet<char>,
}

pub fn asset_chain_umlaut_evidence(
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
    let mut unsupported = BTreeSet::new();

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
        let xml = read_serz_xml(serz_path, &canonical).map_err(|e| e.to_string())?;
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

pub fn transliterated_asset_path(
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
pub fn folders_with_umlauts(assets_path: &Path, asset: &Path) -> Result<Vec<PathBuf>, String> {
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
pub fn translated_reference(
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
        let xml = read_serz_xml(serz_path, &canonical).map_err(|e| e.to_string())?;
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
        rewritten = crate::translit::transliterate_xml_umlauts_outside(
            &rewritten,
            &protected_references,
            fixes,
        );
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
            let xml = read_serz_xml(serz_path, &canonical).map_err(|e| e.to_string())?;
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
                } else if extension.eq_ignore_ascii_case("TgPcDx")
                    && crate::translit::has_umlauts(&reference_text)
                {
                    let target = transliterated_asset_path(&assets_root, &dependency, fixes)?;
                    renamed_tg_files.insert(dependency, target);
                }
            }
            let target = transliterated_asset_path(&assets_root, &canonical, fixes)?;
            let rewritten = crate::translit::transliterate_xml_umlauts_outside(
                &xml,
                &protected_references,
                fixes,
            );
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
            SerzTool::new(serz_path).bin_to_xml(&file_path, &xml_path).map_err(|e| e.to_string())?;
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
            SerzTool::new(serz_path).xml_to_bin(&xml_path, &staged).map_err(|e| e.to_string())?;
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
    let mut transaction = FileTransaction::new();
    for staged in &prepared {
        // Файл из архива .ap: оригинал остаётся в архиве, а изменённая копия
        // кладётся рядом и перекроет его, поэтому резервная копия не нужна.
        let original = match &staged.source {
            crate::domain::RouteFile::Path(path) => Some(path.clone()),
            crate::domain::RouteFile::Archive { .. } => None,
        };
        let _backup = match &original {
            Some(original) => {
                let backup = match next_backup_path(original) {
                    Ok(path) => path,
                    Err(error) => {
                        return Err(format!("{error}"));
                    }
                };
                if let Err(error) = fs::rename(original, &backup) {
                    return Err(format!(
                        "Не удалось создать резервную копию {}: {error}.",
                        original.display()
                    ));
                }
                transaction.record(original.clone(), backup.clone());
                Some(backup)
            }
            None => None,
        };
        if let Err(error) = install_staged_file(staged) {
            return Err(format!(
                "Не удалось установить {}: {error}.",
                staged.target.display()
            ));
        }
    }
    // Старые папки с умляутами остались пустыми: убираем их, чтобы в Assets
    // не осталось дублей с непонятным назначением.
    emptied_folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
    for folder in &emptied_folders {
        let _ = fs::remove_dir(folder);
    }
    transaction.commit();
    Ok(crate::domain::UmlautRepairReport {
        new_asset_path: new_relative_asset,
    })
}

/// Ставит подготовленный файл на место.
///
/// Файл из архива .ap не переносится: вместо этого рядом с архивом кладётся
/// изменённая копия, которую игра прочитает вместо записи архива.


pub struct StagedUmlautFile {
    /// Исходный файл: запись архива .ap или файл на диске.
    pub source: crate::domain::RouteFile,
    /// Куда класть результат.
    pub target: PathBuf,
    pub staged: PathBuf,
    pub _temp: tempfile::TempDir,
}

