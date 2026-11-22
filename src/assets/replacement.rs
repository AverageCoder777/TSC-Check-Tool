use super::*;
use crate::assets::reference::extract_asset_references_with_locations;
use crate::common::{
    domain::{AssetReplacementReport, AssetUse, RouteFile, TileKind},
    serz::{SerzNode, SerzPreamble, SerzReader, encode_to_vec},
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn is_safe_for_global_replacement(relative_path: &str, uses: &[AssetUse]) -> bool {
    let item = RouteAssetItem {
        path: relative_path.to_owned(),
        uses: uses.to_vec(),
    };
    !is_protected_network_asset(&item)
}

pub fn delete_scenery_asset(
    route_path: &Path,
    asset_path: &str,
    uses: &[AssetUse],
) -> Result<(usize, usize, Vec<String>), String> {
    if uses.is_empty() {
        return Err("Для объекта не найдены места использования.".to_owned());
    }
    for usage in uses {
        if usage.tile_kind != TileKind::Scenery {
            return Err(
                "Удаление путевых объектов и параметров маршрута доступно только в редакторе игры."
                    .to_owned(),
            );
        }
        if usage.source_file.is_protected_pathway() {
            return Err("Удаление этого объекта запрещено (путевая инфраструктура).".to_owned());
        }
    }

    delete_asset_from_tiles(
        route_path,
        asset_path,
        uses,
        "Scenery",
        |root, provider, product, blueprint| {
            root.remove_c_dynamic_entities(provider, product, blueprint)
                + root.remove_loft_ribbons(provider, product, blueprint)
        },
    )
}

/// Удаляет вхождения лофта/дороги из тайлов `Loft Tiles` / `Road Tiles`
/// (декоративные ленты без привязки к Tracks.bin). Путевые тайлы и
/// Tracks.bin остаются защищёнными: такой объект не попадёт в эту функцию.
pub fn delete_loft_asset(
    route_path: &Path,
    asset_path: &str,
    uses: &[AssetUse],
) -> Result<(usize, usize, Vec<String>), String> {
    if uses.is_empty() {
        return Err("Для объекта не найдены места использования.".to_owned());
    }
    for usage in uses {
        if usage.source_file.is_protected_pathway() {
            return Err("Удаление этого объекта запрещено (путевая инфраструктура).".to_owned());
        }
        if usage.tile_kind != TileKind::Loft && usage.tile_kind != TileKind::Road {
            return Err(format!(
                "Удаление поддерживается только для тайлов Loft/Road: {}",
                usage.source_file.display()
            ));
        }
    }

    delete_asset_from_tiles(
        route_path,
        asset_path,
        uses,
        "Loft/Road",
        |root, provider, product, blueprint| root.remove_loft_ribbons(provider, product, blueprint),
    )
}

fn delete_asset_from_tiles<F>(
    route_path: &Path,
    asset_path: &str,
    uses: &[AssetUse],
    tile_label: &str,
    mut remove_fn: F,
) -> Result<(usize, usize, Vec<String>), String>
where
    F: FnMut(&mut SerzNode, &str, &str, &str) -> usize,
{
    let (provider, product, blueprint) = asset_blueprint_ref(asset_path);
    let source_files = collect_and_validate_source_files(route_path, uses, "удаления")?;

    let mut staged = Vec::new();
    let mut removed_instances = 0usize;
    let mut untouched = Vec::new();

    for source_file in source_files {
        let is_binary = source_file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
        if !is_binary {
            return Err(format!(
                "Удаление поддерживается только для BIN-тайлов {tile_label}: {}",
                source_file.display()
            ));
        }

        let temp = create_temporary_work_directory(&source_work_parent(&source_file, route_path))?;
        let file_path = source_file.materialize(temp.path())?;
        let bytes = fs::read(&file_path).map_err(|error| {
            format!(
                "Не удалось прочитать {} перед удалением: {error}",
                source_file.display()
            )
        })?;
        let mut reader = SerzReader::new(&bytes);
        let mut root = reader.parse().map_err(|error| {
            format!(
                "Не удалось прочитать {} перед удалением: {error}",
                source_file.display()
            )
        })?;

        let count = remove_fn(&mut root, &provider, &product, &blueprint);
        if count == 0 {
            untouched.push(source_file.display().to_string());
            continue;
        }

        let (converted_path, _) = stage_modified_bin(
            &root,
            reader.preamble.as_ref(),
            &source_file,
            temp.path(),
            "deleted.bin",
            "Удаление отменено.",
        )?;
        removed_instances += count;
        staged.push((source_file, temp, converted_path));
    }

    if staged.is_empty() {
        return Err(format!(
            "В тайлах {tile_label} не найдено использований {asset_path}. Обновите проверку и повторите."
        ));
    }

    let mut transaction = FileTransaction::new();
    let mut replaced_files = 0usize;
    for (source_file, _temp, converted_path) in staged {
        match install_route_file(route_path, &source_file, &converted_path) {
            Ok(backup) => {
                if let Some(backup_path) = backup {
                    transaction.record(source_file.install_target(route_path), backup_path);
                }
                replaced_files += 1;
            }
            Err(error) => {
                transaction.rollback();
                return Err(format!(
                    "Удалено вхождений: {removed_instances}; успешно изменено файлов: {replaced_files}. Ошибка в {}: {error}.",
                    source_file.display()
                ));
            }
        }
    }
    transaction.commit();
    Ok((replaced_files, removed_instances, untouched))
}

fn collect_and_validate_source_files(
    route_path: &Path,
    uses: &[AssetUse],
    action_genitive: &str,
) -> Result<HashSet<RouteFile>, String> {
    let canonical_route = route_path
        .canonicalize()
        .map_err(|error| format!("Не удалось проверить папку маршрута: {error}"))?;
    let mut source_files = HashSet::with_capacity(uses.len());
    for usage in uses {
        // Файл из архива .ap нельзя править на месте: он лежит внутри архива.
        // Поэтому готовим временную копию, а результат кладём рядом с архивом.
        let source = usage.source_file.clone();
        if source.route_name(route_path).is_none() {
            return Err(format!(
                "Файл для {action_genitive} находится вне маршрута: {}",
                source.display()
            ));
        }
        if let RouteFile::Path(path) = &source {
            let canonical_source = path.canonicalize().map_err(|error| {
                format!(
                    "Не удалось проверить файл маршрута {}: {error}",
                    source.display()
                )
            })?;
            if !canonical_source.starts_with(&canonical_route) || !canonical_source.is_file() {
                return Err(format!(
                    "Файл для {action_genitive} находится вне маршрута или недоступен: {}",
                    source.display()
                ));
            }
        }
        source_files.insert(source);
    }
    Ok(source_files)
}

fn source_work_parent(source_file: &RouteFile, route_path: &Path) -> PathBuf {
    match source_file {
        RouteFile::Path(path) => path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| route_path.to_path_buf()),
        RouteFile::Archive { .. } => route_path.to_path_buf(),
    }
}

fn stage_modified_bin(
    root: &SerzNode,
    preamble: Option<&SerzPreamble>,
    source_file: &RouteFile,
    temp_dir: &Path,
    output_name: &str,
    abort_suffix: &str,
) -> Result<(PathBuf, usize), String> {
    let bytes = encode_to_vec(root, preamble).map_err(|error| {
        format!(
            "Не удалось подготовить изменённый BIN {}: {error}",
            source_file.display()
        )
    })?;
    if bytes.len() < 1024 {
        return Err(format!(
            "Ошибка генерации: размер файла аномально мал ({} байт). {abort_suffix}",
            bytes.len()
        ));
    }
    let mut validation_reader = SerzReader::new(&bytes);
    if let Err(error) = validation_reader.parse() {
        return Err(format!(
            "Ошибка сериализации: сгенерированный бинарник поврежден: {error}"
        ));
    }
    let converted_path = temp_dir.join(output_name);
    fs::write(&converted_path, &bytes).map_err(|error| {
        format!(
            "Не удалось подготовить изменённый BIN {}: {error}",
            source_file.display()
        )
    })?;
    Ok((converted_path, bytes.len()))
}

/// Разбирает путь ассета `Provider/Product/....xml` на provider, product и
/// нормализованный blueprint (с расширением .xml, обратными слешами).
fn asset_blueprint_ref(asset_path: &str) -> (String, String, String) {
    let mut parts = asset_path.split(['/', '\\']);
    let provider = parts.next().unwrap_or_default().to_string();
    let product = parts.next().unwrap_or_default().to_string();
    let mut blueprint_path = PathBuf::new();
    for part in parts {
        blueprint_path.push(part);
    }
    let blueprint = blueprint_path
        .with_extension("xml")
        .to_string_lossy()
        .replace('/', "\\");
    (provider, product, blueprint)
}

/// Ключ ассета в списке: игра не различает регистр, поэтому пути, отличающиеся
/// только регистром, относятся к одному и тому же ассету.
pub fn asset_key(path: &str) -> String {
    path.to_lowercase()
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(n.len())
        .any(|w| w.eq_ignore_ascii_case(n))
}

/// Заменяет все вхождения отсутствующего ассета в файлах маршрута на выбранный `.bin`.
pub(crate) fn replace_missing_asset(
    route_path: &Path,
    assets_path: &Path,
    old_asset: &str,
    replacement_path: &Path,
    uses: &[AssetUse],
) -> Result<AssetReplacementReport, String> {
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
    if uses.is_empty() {
        return Err("Для отсутствующего ассета не найдены места использования.".to_owned());
    }

    let asset_type = classify_route_asset_type(old_asset, uses);
    let replacement_xml = read_serz_xml(&replacement_path)
        .ok()
        .or_else(|| fs::read_to_string(&replacement_path).ok());
    let root_tag = replacement_xml
        .as_deref()
        .and_then(first_blueprint_tag);

    match asset_type {
        RouteAssetType::Scenery => {
            if let Some(tag) = &root_tag
                && ![
                    "cSceneryBlueprint",
                    "cAnimSceneryBlueprint",
                    "cTimeSceneryBlueprint",
                    "cSimpleWaterBlueprint",
                    "cPrecipitationBlueprint",
                ]
                .contains(&tag.as_str())
            {
                return Err(
                    "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned(),
                );
            }
        }
        RouteAssetType::Loft => {
            if let Some(tag) = &root_tag
                && ![
                    "cLoftBlueprint",
                    "cTrackSectionBlueprint",
                    "cRoadSectionBlueprint",
                ]
                .contains(&tag.as_str())
            {
                return Err(
                    "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned(),
                );
            }
        }
        RouteAssetType::Road => {
            if let Some(tag) = &root_tag
                && tag != "cRoadSectionBlueprint"
            {
                return Err(
                    "Тип выбранного ассета не совпадает с типом отсутствующего.".to_owned(),
                );
            }
        }
        RouteAssetType::TrackRules => {
            return Err("Замена путевых объектов в Tracks.bin заблокирована для предотвращения повреждения маршрута.".to_owned());
        }
        _ => {}
    }

    if contains_ignore_ascii_case(old_asset, "trackrules")
        || contains_ignore_ascii_case(old_asset, "junctions")
        || uses.iter().any(|usage| {
            let path = usage.source_file.display();
            contains_ignore_ascii_case(&path, "tracks.bin")
                || contains_ignore_ascii_case(&path, "/networks/")
                || contains_ignore_ascii_case(&path, "\\networks\\")
                || contains_ignore_ascii_case(&path, "trackrules")
                || contains_ignore_ascii_case(&path, "junctions")
        })
    {
        return Err("Замена путевых объектов в Tracks.bin заблокирована для предотвращения повреждения маршрута.".into());
    }

    let source_files = collect_and_validate_source_files(route_path, uses, "замены")?;

    println!("[Replacement] Замена ассета: {old_asset} -> {new_asset}");

    let (old_provider, old_product, old_blueprint) = asset_blueprint_ref(old_asset);

    let mut staged = Vec::new();
    let mut replaced_instances = 0;
    for source_file in source_files {
        println!("[Replacement] Обработка файла: {}", source_file.display());
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

        let temp = create_temporary_work_directory(&source_work_parent(&source_file, route_path))?;
        let file_path = source_file.materialize(temp.path())?;

        if is_binary {
            let bytes = fs::read(&file_path).map_err(|error| {
                format!(
                    "Не удалось прочитать {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
            let mut reader = SerzReader::new(&bytes);
            let mut root = reader.parse().map_err(|error| {
                format!(
                    "Не удалось прочитать {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
            println!(
                "[Replacement Debug] Ищем: Provider='{old_provider}', Product='{old_product}', Blueprint='{old_blueprint}'"
            );
            println!(
                "[Replacement Debug] Меняем на: Provider='{replacement_provider}', Product='{replacement_product}', Blueprint='{replacement_blueprint}'"
            );
            let count = root.replace_blueprint_references(
                &old_provider,
                &old_product,
                &old_blueprint,
                &replacement_provider,
                &replacement_product,
                &replacement_blueprint,
            );
            if count == 0 {
                continue;
            }
            let (converted_path, serialized_len) = stage_modified_bin(
                &root,
                reader.preamble.as_ref(),
                &source_file,
                temp.path(),
                "replacement.bin",
                "Замена отменена, оригинал не затронут.",
            )?;
            println!(
                "[Replacement] Сериализовано {serialized_len} байт для {}",
                source_file.display()
            );
            replaced_instances += count;
            staged.push((source_file, temp, converted_path));
        } else {
            let xml_path = temp.path().join("source.xml");
            fs::copy(&file_path, &xml_path).map_err(|error| {
                format!(
                    "Не удалось подготовить {} перед заменой: {error}",
                    source_file.display()
                )
            })?;

            let mut xml = fs::read_to_string(&xml_path).map_err(|error| {
                format!(
                    "Не удалось прочитать XML для {}: {error}",
                    source_file.display()
                )
            })?;
            let references = extract_asset_references_with_locations(&xml);
            let mut edits: Vec<_> = references
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

            replaced_instances += instances;
            staged.push((source_file, temp, xml_path));
        }
    }

    if staged.is_empty() {
        return Err(format!(
            "В исходниках маршрута не найдено использований {old_asset}. Обновите проверку и повторите."
        ));
    }

    let mut backups = Vec::new();
    let mut transaction = FileTransaction::new();
    let mut replaced_files = 0;
    for (source_file, _temp, converted_path) in staged {
        println!("[Replacement] Установка файла: {}", source_file.display());
        match install_route_file(route_path, &source_file, &converted_path) {
            Ok(backup) => {
                if let Some(backup_path) = backup {
                    println!("[Replacement] Создан бэкап: {}", backup_path.display());
                    transaction.record(source_file.install_target(route_path), backup_path.clone());
                    backups.push(backup_path);
                }
                replaced_files += 1;
            }
            Err(error) => {
                transaction.rollback();
                return Err(format!(
                    "Заменено размещений: {replaced_instances}; успешно изменено файлов: {replaced_files}. Ошибка в {}: {error}. Созданные резервные копии: {}",
                    source_file.display(),
                    backups
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    transaction.commit();
    Ok(AssetReplacementReport {
        replaced_files,
        replaced_instances,
        backups,
        replacement_asset: new_asset,
        log_file: None,
        log_error: None,
    })
}
