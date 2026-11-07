use super::*;
use std::collections::HashSet;

pub fn is_safe_for_global_replacement(
    relative_path: &str,
    uses: &[crate::domain::AssetUse],
) -> bool {
    let item = RouteAssetItem {
        path: relative_path.to_owned(),
        uses: uses.to_vec(),
    };
    !is_protected_network_asset(&item)
}

/// Ключ ассета в списке: игра не различает регистр, поэтому пути, отличающиеся
/// только регистром, относятся к одному и тому же ассету.
pub fn asset_key(path: &str) -> String {
    path.to_lowercase()
}

/// Собирает список сценариев маршрута: имя папки и число файлов в ней.
///
pub(crate) fn replace_missing_asset(
    route_path: &Path,
    assets_path: &Path,
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
    if uses.is_empty() {
        return Err("Для отсутствующего ассета не найдены места использования.".to_owned());
    }

    let asset_type = classify_route_asset_type(old_asset, uses);
    let replacement_xml = read_serz_xml(&replacement_path)
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

    println!(
        "[Replacement] Замена ассета: {} -> {}",
        old_asset, new_asset
    );

    let old_parts: Vec<&str> = old_asset.split(['/', '\\']).collect();
    let old_provider = old_parts.first().copied().unwrap_or_default();
    let old_product = old_parts.get(1).copied().unwrap_or_default();
    let mut old_blueprint_path = PathBuf::new();
    for part in old_parts.iter().skip(2) {
        old_blueprint_path.push(part);
    }
    let old_blueprint = old_blueprint_path
        .with_extension("xml")
        .to_string_lossy()
        .replace('/', "\\");

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
            let bytes = std::fs::read(&file_path).map_err(|error| {
                format!(
                    "Не удалось прочитать {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
            let mut reader = crate::serz::SerzReader::new(&bytes);
            let mut root = reader.parse().map_err(|error| {
                format!(
                    "Не удалось прочитать {} перед заменой: {error}",
                    source_file.display()
                )
            })?;
            println!(
                "[Replacement Debug] Ищем: Provider='{}', Product='{}', Blueprint='{}'",
                old_provider, old_product, old_blueprint
            );
            println!(
                "[Replacement Debug] Меняем на: Provider='{}', Product='{}', Blueprint='{}'",
                replacement_provider, replacement_product, replacement_blueprint
            );
            let count = root.replace_blueprint_references(
                old_provider,
                old_product,
                &old_blueprint,
                &replacement_provider,
                &replacement_product,
                &replacement_blueprint,
            );
            if count == 0 {
                continue;
            }
            let bytes = crate::serz::encode_to_vec(&root, reader.preamble.as_ref()).map_err(|error| {
                format!(
                    "Не удалось подготовить изменённый BIN {}: {error}",
                    source_file.display()
                )
            })?;
            if bytes.len() < 1024 {
                return Err(format!(
                    "Ошибка генерации: размер файла аномально мал ({} байт). Замена отменена, оригинал не затронут.",
                    bytes.len()
                ));
            }
            let mut validation_reader = crate::serz::SerzReader::new(&bytes);
            if let Err(error) = validation_reader.parse() {
                return Err(format!(
                    "Ошибка сериализации: сгенерированный бинарник поврежден и не может быть прочитан обратно: {error}"
                ));
            }
            println!(
                "[Replacement] Сериализовано {} байт для {}",
                bytes.len(),
                source_file.display()
            );
            let converted_path = temp.path().join("replacement.bin");
            std::fs::write(&converted_path, &bytes).map_err(|error| {
                format!(
                    "Не удалось подготовить изменённый BIN {}: {error}",
                    source_file.display()
                )
            })?;
            replaced_instances += count;
            staged.push((source_file, temp, converted_path));
        } else {
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

            replaced_instances += instances;
            staged.push((source_file, temp, xml_path));
        }
    }

    if staged.is_empty() {
        return Err(format!(
            "В исходниках маршрута не найдено использований {}. Обновите проверку и повторите.",
            old_asset
        ));
    }

    let mut backups = Vec::new();
    let mut transaction = FileTransaction::new();
    let mut replaced_files = 0;
    for (source_file, _temp, converted_path) in staged {
        println!("[Replacement] Установка файла: {}", source_file.display());
        match install_route_file(route_path, &source_file, &converted_path) {
            Ok(backup) => {
                if let Some(backup_path) = &backup {
                    println!("[Replacement] Создан бэкап: {}", backup_path.display());
                    transaction.record(source_file.install_target(route_path), backup_path.clone());
                }
                replaced_files += 1;
                if let Some(backup) = backup {
                    backups.push(backup);
                }
            }
            Err(error) => {
                transaction.rollback();
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
    transaction.commit();
    Ok(crate::domain::AssetReplacementReport {
        replaced_files,
        replaced_instances,
        backups,
        replacement_asset: new_asset,
        log_file: None,
        log_error: None,
    })
}
