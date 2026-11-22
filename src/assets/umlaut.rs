use rayon::iter::*;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::assets::serz::read_serz_xml;
use crate::assets::{
    reference::extract_asset_references_with_locations,
    resolver::{resolve_asset_path, resolve_product_reference, unresolved_reference_error},
    scanner::create_temporary_work_directory,
    transaction::{
        FileTransaction, install_staged_file, next_backup_path, stage_renamed_file, stage_serz_xml,
    },
    xml_reference::*,
};
use crate::common::serz::{SerzNode, SerzReader, encode_to_vec};
use crate::common::{domain::*, translit::*};

/// Результат проверки цепочки ассета на сломанные символы.
pub(crate) struct UmlautChainEvidence {
    pub(crate) evidence: Vec<String>,
    pub(crate) unsupported: BTreeSet<char>,
}

/// Ставит подготовленный файл на место.
///
/// Файл из архива .ap не переносится: вместо этого рядом с архивом кладётся
/// изменённая копия, которую игра прочитает вместо записи архива.
pub struct StagedUmlautFile {
    pub source: RouteFile,
    pub target: PathBuf,
    pub staged: PathBuf,
    pub _temp: tempfile::TempDir,
}

fn is_serz_asset_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bin") || extension.eq_ignore_ascii_case("GeoPcDx")
    })
}

fn product_root_of(assets_root: &Path, asset_path: &Path) -> Result<PathBuf, String> {
    let relative = asset_path
        .strip_prefix(assets_root)
        .map_err(|_| format!("Ассет находится вне Assets: {}", asset_path.display()))?;
    let mut components = relative.components();
    let provider = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    let product = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?;
    Ok(assets_root.join(provider).join(product))
}

/// За один проход применяет точечные замены ссылок и транслитерирует остальной XML,
/// не затрагивая защищённые диапазоны (все смещения берутся из исходного `xml`).
fn apply_xml_edits_and_transliterate(
    xml: &str,
    edits: Vec<(Range<usize>, String)>,
    mut protected: Vec<Range<usize>>,
    fixes: &CharacterFixes,
) -> String {
    if edits.is_empty() {
        protected.sort_by_key(|range| range.start);
        return transliterate_xml_umlauts_outside(xml, &protected, fixes);
    }

    let mut spans: Vec<(Range<usize>, Option<String>)> =
        Vec::with_capacity(edits.len() + protected.len());
    spans.extend(edits.into_iter().map(|(range, value)| (range, Some(value))));
    spans.extend(protected.into_iter().map(|range| (range, None)));
    spans.sort_by_key(|(range, _)| range.start);

    let mut result = String::with_capacity(xml.len());
    let mut position = 0;
    for (range, replacement) in spans {
        let start = range.start.min(xml.len());
        let end = range.end.min(xml.len());
        if end <= position {
            continue;
        }
        if start > position {
            result.push_str(&transliterate_xml_umlauts(&xml[position..start], fixes));
        }
        match replacement {
            Some(value) => result.push_str(&value),
            None => result.push_str(&xml[start.max(position)..end]),
        }
        position = end;
    }
    if position < xml.len() {
        result.push_str(&transliterate_xml_umlauts(&xml[position..], fixes));
    }
    result
}

pub fn asset_chain_umlaut_evidence(
    assets_path: &Path,
    root_asset: &Path,
) -> Result<UmlautChainEvidence, String> {
    let assets_root = assets_path
        .canonicalize()
        .unwrap_or_else(|_| assets_path.to_path_buf());
    let canonical_root = match root_asset.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UmlautChainEvidence {
                evidence: Vec::new(),
                unsupported: BTreeSet::new(),
            });
        }
        Err(error) => {
            return Err(format!(
                "Не удалось проверить {}: {error}",
                root_asset.display()
            ));
        }
    };
    let product_root = product_root_of(&assets_root, &canonical_root)?;
    let scope_root = canonical_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| product_root.clone());
    let mut queue = vec![canonical_root];
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
        if !is_serz_asset_extension(&canonical) {
            continue;
        }
        let xml = read_serz_xml(&canonical).map_err(|e| e.to_string())?;
        if has_umlauts_in_xml(&xml) {
            evidence.push(format!("Внутри файла: {}", canonical.display()));
        }
        for (reference, _) in referenced_geometry_paths(&xml) {
            let reference_text = reference.to_text();
            if has_umlauts(&reference_text) {
                evidence.push(format!(
                    "Ссылка из {}: {reference_text}",
                    canonical.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            unsupported.extend(unsupported_characters(&reference_text));
            let Some((path, _)) =
                resolve_product_reference(&assets_root, &product_root, &canonical, &reference)
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
) -> (Option<UmlautAsset>, Option<String>) {
    let mut evidence = Vec::new();
    let mut unsupported = unsupported_characters(relative_asset);
    if has_umlauts(relative_asset) {
        evidence.push(format!("Умляут в пути ассета: {relative_asset}"));
    }
    let mut scan_error = None;
    let full_asset_path = match resolve_asset_path(assets_path, relative_asset) {
        Ok(path) => path,
        Err(error) => {
            evidence.push(error.clone());
            scan_error = Some(error);
            None
        }
    };
    if let Some(asset_path) = full_asset_path.as_deref() {
        let expected_name = Path::new(relative_asset).file_name();
        let actual_name = asset_path.file_name();
        if expected_name != actual_name {
            evidence.push(format!(
                "Файл на диске называется иначе: {}",
                asset_path.display()
            ));
        }
        if asset_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        {
            match asset_chain_umlaut_evidence(assets_path, asset_path) {
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
    }
    let asset = (full_asset_path.is_some() && (!evidence.is_empty() || !unsupported.is_empty()))
        .then(|| UmlautAsset {
            path: relative_asset.to_owned(),
        });
    (asset, scan_error)
}

pub(crate) fn scan_umlaut_assets(
    inventory: &[ReferencedAsset],
    assets_path: &Path,
    max_concurrent_serz: usize,
) -> Result<(Vec<UmlautAsset>, Vec<String>), String> {
    if inventory.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(max_concurrent_serz.max(1))
        .min(inventory.len());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
        .map_err(|error| format!("Не удалось создать пул проверки умляутов: {error}"))?;
    let scan: Vec<_> = pool.install(|| {
        inventory
            .par_iter()
            .map(|asset| inspect_umlaut_asset(&asset.path, assets_path))
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
    fixes: &CharacterFixes,
) -> Result<PathBuf, String> {
    let relative = source
        .strip_prefix(assets_path)
        .map_err(|_| format!("Ассет находится вне папки Assets: {}", source.display()))?;
    let mut components = relative.components();
    let provider = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?
        .as_os_str()
        .to_string_lossy();
    let product = components
        .next()
        .ok_or_else(|| "Некорректный путь ассета.".to_owned())?
        .as_os_str()
        .to_string_lossy();
    if has_umlauts(&provider) || has_umlauts(&product) {
        return Err(format!(
            "Умляут найден в имени Provider/Product для {}. Автоматическая транслитерация папок Provider/Product остановлена, чтобы не затронуть другие ассеты.",
            source.display()
        ));
    }
    let translated = transliterate_text(&relative.to_string_lossy(), fixes);
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
    for (index, component) in components.iter().enumerate().take(components.len() - 1) {
        current.push(component);
        let name = component.as_os_str().to_string_lossy();
        if index >= 2 && has_umlauts(&name) {
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
    let stem = Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or(Cow::Borrowed(file_name));

    let extra_ext = reference.extension.as_ref().map_or(0, |ext| ext.len() + 1);
    let mut text = String::with_capacity(
        folder.len() + reference.variant.len() + stem.len() + extra_ext,
    );
    text.push_str(folder);
    text.push_str(&reference.variant);
    text.push_str(&stem);
    if let Some(extension) = &reference.extension {
        text.push('.');
        text.push_str(extension);
    }
    Ok(text)
}

struct AssetRepairContext<'a> {
    assets_root: &'a Path,
    product_root: &'a Path,
    scope_root: &'a Path,
    fixes: &'a CharacterFixes,
}

fn process_serz_asset_queue(
    ctx: &AssetRepairContext<'_>,
    queue: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    staged_by_source: &mut HashMap<RouteFile, StagedUmlautFile>,
    renamed_tg_files: &mut HashMap<PathBuf, PathBuf>,
    scan_all_geometry: &mut bool,
) -> Result<(), String> {
    while let Some(source) = queue.pop() {
        let canonical = source.canonicalize().map_err(|error| {
            format!(
                "Не удалось открыть зависимость {}: {error}",
                source.display()
            )
        })?;
        if !canonical.starts_with(ctx.scope_root) || !visited.insert(canonical.clone()) {
            continue;
        }
        if !is_serz_asset_extension(&canonical) {
            continue;
        }
        let xml = read_serz_xml(&canonical).map_err(|e| e.to_string())?;
        let target_file = transliterated_asset_path(ctx.assets_root, &canonical, ctx.fixes)?;
        let mut edits = Vec::new();
        let mut protected_references: Vec<Range<usize>> = Vec::new();

        for (reference, range) in referenced_geometry_paths(&xml) {
            let reference_text = reference.to_text();
            let Some((dependency, base)) = resolve_product_reference(
                ctx.assets_root,
                ctx.product_root,
                &canonical,
                &reference,
            ) else {
                if has_umlauts(&reference_text) {
                    return Err(unresolved_reference_error(
                        &canonical,
                        &reference,
                        ctx.assets_root,
                        ctx.product_root,
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
            if !canonical_dependency.starts_with(ctx.product_root) {
                return Err(format!(
                    "Ссылка выходит за пределы продукта {}: {}",
                    ctx.product_root.display(),
                    dependency.display()
                ));
            }
            if !canonical_dependency.starts_with(ctx.scope_root) {
                protected_references.push(range);
                continue;
            }
            let translated_dependency =
                transliterated_asset_path(ctx.assets_root, &canonical_dependency, ctx.fixes)?;
            let dependency_extension = canonical_dependency
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if dependency_extension.eq_ignore_ascii_case("GeoPcDx") {
                queue.push(canonical_dependency.clone());
            } else if dependency_extension.eq_ignore_ascii_case("TgPcDx")
                && (has_umlauts(&reference_text)
                    || canonical_dependency
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(has_umlauts))
            {
                *scan_all_geometry = true;
                renamed_tg_files
                    .insert(canonical_dependency.clone(), translated_dependency.clone());
            }
            if translated_dependency != canonical_dependency {
                let replacement = translated_reference(
                    &translated_dependency,
                    ctx.assets_root,
                    ctx.product_root,
                    &target_file,
                    base,
                    &reference,
                )?;
                edits.push((range, escape_xml_text(&replacement)));
            }
        }

        let rewritten =
            apply_xml_edits_and_transliterate(&xml, edits, protected_references, ctx.fixes);
        if rewritten != xml || target_file != canonical {
            validate_xml_document(&rewritten).map_err(|error| {
                format!(
                    "Некорректный XML после обработки {}: {error}",
                    canonical.display()
                )
            })?;
            let staged = stage_serz_xml(&canonical, &target_file, &rewritten)?;
            staged_by_source.insert(RouteFile::Path(canonical), staged);
        }
    }
    Ok(())
}

fn stage_route_file_reference_update(
    source: &RouteFile,
    route_root: &Path,
    asset_relative_path: &str,
    new_provider: &str,
    new_product: &str,
    replacement_blueprint: &str,
) -> Result<StagedUmlautFile, String> {
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
    let source_parent = match source {
        RouteFile::Path(path) => path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| route_root.to_path_buf()),
        RouteFile::Archive { .. } => route_root.to_path_buf(),
    };
    let temp = create_temporary_work_directory(&source_parent)?;
    let file_path = source.materialize(temp.path())?;

    let mut xml = if is_binary {
        let bytes = fs::read(&file_path).map_err(|e| e.to_string())?;
        let mut reader = SerzReader::new(&bytes);
        let node = reader.parse().map_err(|e| e.to_string())?;
        SerzReader::to_xml(&node, 0, true)
    } else {
        fs::read_to_string(&file_path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", source.display()))?
    };

    let references = extract_asset_references_with_locations(&xml);
    let mut edits = Vec::new();
    for reference in references
        .into_iter()
        .filter(|reference| reference.path.eq_ignore_ascii_case(asset_relative_path))
    {
        edits.extend([
            (reference.provider_range, new_provider),
            (reference.product_range, new_product),
            (reference.blueprint_range, replacement_blueprint),
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
        xml.replace_range(range, &escape_xml_text(value));
    }
    validate_xml_document(&xml)
        .map_err(|error| format!("Некорректный XML маршрута {}: {error}", source.display()))?;

    let staged_path = if is_binary {
        let mut staged = temp.path().join("route-new");
        staged.set_extension(source.extension().unwrap_or("bin"));
        let (node, preamble) = SerzNode::from_xml_str(&xml).map_err(|e| e.to_string())?;
        let bytes = encode_to_vec(&node, preamble.as_ref()).map_err(|e| e.to_string())?;
        if bytes.len() < 1024 {
            return Err(format!(
                "Ошибка генерации: размер файла аномально мал ({} байт). Замена отменена.",
                bytes.len()
            ));
        }
        let mut validation_reader = SerzReader::new(&bytes);
        if let Err(error) = validation_reader.parse() {
            return Err(format!(
                "Ошибка сериализации: сгенерированный бинарник поврежден: {error}"
            ));
        }
        fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
        staged
    } else {
        let xml_path = temp.path().join("route.xml");
        fs::write(&xml_path, xml)
            .map_err(|error| format!("Не удалось записать подготовленный маршрут: {error}"))?;
        xml_path
    };

    Ok(StagedUmlautFile {
        source: source.clone(),
        target: source.install_target(route_root),
        staged: staged_path,
        _temp: temp,
    })
}

pub(crate) fn repair_umlaut_asset_chain(
    route_path: &Path,
    assets_path: &Path,
    asset_relative_path: &str,
    uses: &[AssetUse],
    fixes: &CharacterFixes,
) -> Result<UmlautRepairReport, String> {
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
        let found = asset_chain_umlaut_evidence(&assets_root, &canonical_asset)?;
        if found.evidence.is_empty() && found.unsupported.is_empty() {
            return Err("В файлах ассета не обнаружены поддерживаемые умляуты.".to_owned());
        }
    }

    let product_root = product_root_of(&assets_root, &canonical_asset)?;
    let renamed_folders = folders_with_umlauts(&assets_root, &canonical_asset)?;
    let mut scan_all_geometry = !renamed_folders.is_empty();
    let scope_root = renamed_folders
        .first()
        .cloned()
        .or_else(|| canonical_asset.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| product_root.clone());

    let mut queue = vec![canonical_asset.clone()];
    let mut visited = HashSet::new();
    let mut staged_by_source: HashMap<RouteFile, StagedUmlautFile> = HashMap::new();
    let mut renamed_tg_files: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut emptied_folders: Vec<PathBuf> = Vec::new();

    if let Some(root_umlaut_folder) = renamed_folders.first() {
        for entry in WalkDir::new(root_umlaut_folder).follow_links(false) {
            let entry = entry.map_err(|error| {
                format!(
                    "Ошибка обхода папки {}: {error}",
                    root_umlaut_folder.display()
                )
            })?;
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
            if is_serz_asset_extension(&canonical) {
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
            staged_by_source.insert(RouteFile::Path(canonical), staged);
        }
    }

    let repair_ctx = AssetRepairContext {
        assets_root: &assets_root,
        product_root: &product_root,
        scope_root: &scope_root,
        fixes,
    };

    process_serz_asset_queue(
        &repair_ctx,
        &mut queue,
        &mut visited,
        &mut staged_by_source,
        &mut renamed_tg_files,
        &mut scan_all_geometry,
    )?;

    if scan_all_geometry {
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
        process_serz_asset_queue(
            &repair_ctx,
            &mut queue,
            &mut visited,
            &mut staged_by_source,
            &mut renamed_tg_files,
            &mut scan_all_geometry,
        )?;
    }

    for (source, target) in &renamed_tg_files {
        let key = RouteFile::Path(source.clone());
        if staged_by_source.contains_key(&key) {
            continue;
        }
        let staged = stage_renamed_file(source, target)?;
        staged_by_source.insert(key, staged);
    }

    if new_relative_asset != asset_relative_path {
        let new_parts: Vec<_> = new_relative_asset.split('/').collect();
        let replacement_blueprint = Path::new(&new_parts[2..].join("\\"))
            .with_extension("xml")
            .to_string_lossy()
            .into_owned();
        let mut route_sources: HashSet<RouteFile> = HashSet::new();
        for usage in uses {
            let source = usage.source_file.clone();
            if let RouteFile::Path(path) = &source {
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
        for source in route_sources {
            let staged = stage_route_file_reference_update(
                &source,
                &route_root,
                asset_relative_path,
                new_parts[0],
                new_parts[1],
                &replacement_blueprint,
            )?;
            staged_by_source.insert(source, staged);
        }
    }

    if staged_by_source.is_empty() {
        return Err("Не найдено файлов для транслитерации.".to_owned());
    }
    let mut targets = HashSet::with_capacity(staged_by_source.len());
    for staged in staged_by_source.values() {
        if !targets.insert(staged.target.clone()) {
            return Err(format!(
                "Несколько файлов претендуют на путь {}.",
                staged.target.display()
            ));
        }
        let original_path = match &staged.source {
            RouteFile::Path(path) => Some(path.as_path()),
            RouteFile::Archive { .. } => None,
        };
        if staged.target.exists() && original_path != Some(staged.target.as_path()) {
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
        if let RouteFile::Path(original) = &staged.source {
            let backup = match next_backup_path(original) {
                Ok(path) => path,
                Err(error) => {
                    transaction.rollback();
                    return Err(error);
                }
            };
            if let Err(error) = fs::rename(original, &backup) {
                transaction.rollback();
                return Err(format!(
                    "Не удалось создать резервную копию {}: {error}.",
                    original.display()
                ));
            }
            transaction.record(original.clone(), backup);
        }
        if let Err(error) = install_staged_file(staged) {
            transaction.rollback();
            return Err(format!(
                "Не удалось установить {}: {error}.",
                staged.target.display()
            ));
        }
    }
    emptied_folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
    for folder in &emptied_folders {
        let _ = fs::remove_dir(folder);
    }
    transaction.commit();
    Ok(UmlautRepairReport {
        new_asset_path: new_relative_asset,
    })
}
