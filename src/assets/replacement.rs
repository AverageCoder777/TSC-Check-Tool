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
        return Err(crate::gettext!("No places of use were found for the object."));
    }
    for usage in uses {
        if usage.tile_kind != TileKind::Scenery {
            return Err(crate::gettext!(
                "Deleting track infrastructure and route parameters is only available in the game editor."
            ));
        }
        if usage.source_file.is_protected_pathway() {
            return Err(crate::gettext!(
                "You can't change the track infrastructure parts through the program!"
            ));
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

pub fn delete_loft_asset(
    route_path: &Path,
    asset_path: &str,
    uses: &[AssetUse],
) -> Result<(usize, usize, Vec<String>), String> {
    if uses.is_empty() {
        return Err(crate::gettext!(
            "No places of use were found for the object."
        ));
    }
    for usage in uses {
        if usage.source_file.is_protected_pathway() {
            return Err(crate::gettext!(
                "You can't change the track infrastructure parts through the program!"
            ));
        }
        if usage.tile_kind != TileKind::Loft && usage.tile_kind != TileKind::Road {
            return Err(crate::gettext!(
                "Deletion working only for Loft/Road tiles: {}",
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
    let source_files = collect_and_validate_source_files(route_path, uses, &crate::gettext!("deletions"))?;

    let mut staged = Vec::new();
    let mut removed_instances = 0usize;
    let mut untouched = Vec::new();

    for source_file in source_files {
        let is_binary = source_file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
        if !is_binary {
            return Err(crate::gettext!(
                "Deletion working only for BIN-tiles {}: {}",
                tile_label,
                source_file.display()
            ));
        }

        let temp = create_temporary_work_directory(&source_work_parent(&source_file, route_path))?;
        let file_path = source_file.materialize(temp.path())?;
        let bytes = fs::read(&file_path).map_err(|error| {
            crate::gettext!(
                "Failed to read {} before deletion: {}",
                source_file.display(),
                error
            )
        })?;
        let mut reader = SerzReader::new(&bytes);
        let mut root = reader.parse().map_err(|error| {
            crate::gettext!(
                "Failed to read {} before deletion: {}",
                source_file.display(),
                error
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
            "Deletion aborted.",
        )?;
        removed_instances += count;
        staged.push((source_file, temp, converted_path));
    }

    if staged.is_empty() {
        return Err(crate::gettext!(
            "In tiles {} found no uses of {}. Update check and repeat.",
            tile_label,
            asset_path
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
                return Err(crate::gettext!(
                    "Deleted entries: {}; successful changed files: {}. Error in {}: {}.",
                    removed_instances,
                    replaced_files,
                    source_file.display(),
                    error
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
        .map_err(|error| crate::gettext!("Couldn't check the route folder: {}", error))?;
    let mut source_files = HashSet::with_capacity(uses.len());
    for usage in uses {
        let source = usage.source_file.clone();
        if source.route_name(route_path).is_none() {
            return Err(crate::gettext!(
                "File for {} located off the route: {}",
                action_genitive,
                source.display()
            ));
        }
        if let RouteFile::Path(path) = &source {
            let canonical_source = path.canonicalize().map_err(|error| {
                crate::gettext!("Failed to check route file{}: {}", source.display(), error)
            })?;
            if !canonical_source.starts_with(&canonical_route) || !canonical_source.is_file() {
                return Err(crate::gettext!(
                    "File for {} located off the route of unavailable: {}",
                    action_genitive,
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
        crate::gettext!(
            "Failed to prerare changed BIN {}: {}",
            source_file.display(),
            error
        )
    })?;
    if bytes.len() < 1024 {
        return Err(crate::gettext!(
            "Generation error: file size abnormally small ({} bytes). {}",
            bytes.len(), abort_suffix
        ));
    }
    let mut validation_reader = SerzReader::new(&bytes);
    if let Err(error) = validation_reader.parse() {
        return Err(crate::gettext!(
            "Serialization error: generated binary is corrupted: {}", error
        ));
    }
    let converted_path = temp_dir.join(output_name);
    fs::write(&converted_path, &bytes).map_err(|error| {
        crate::gettext!(
            "Failed to prepare changed BIN {}: {}",
            source_file.display(),
            error
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
        .map_err(|error| crate::gettext!("Failed to check new asset: {}", error))?;
    let assets_root = assets_path
        .canonicalize()
        .map_err(|error| crate::gettext!("Failed to check Assets folder: {}", error))?;
    let replacement_relative = replacement_path
        .strip_prefix(&assets_root)
        .map_err(|_| crate::gettext!("New object must be inside Assets folder."))?;
    if !replacement_path.is_file()
        || !replacement_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
    {
        return Err(crate::gettext!("To replace, select an existing asset BIN file."));
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
        return Err(crate::gettext!("Path of new object must have format Provider/Product/....bin."));
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
        return Err(crate::gettext!("No usage locations were found for the missing asset."));
    }

    let asset_type = classify_route_asset_type(old_asset, uses);
    let replacement_xml = read_serz_xml(&replacement_path)
        .ok()
        .or_else(|| fs::read_to_string(&replacement_path).ok());
    let root_tag = replacement_xml.as_deref().and_then(first_blueprint_tag);

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
                return Err(crate::gettext!(
                    "Type of selected asset mismatched with type of missing asset."
                ));
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
                return Err(crate::gettext!(
                    "Type of selected asset mismatched with type of missing asset."
                ));
            }
        }
        RouteAssetType::Road => {
            if let Some(tag) = &root_tag
                && tag != "cRoadSectionBlueprint"
            {
                return Err(crate::gettext!(
                    "Type of selected asset mismatched with type of missing asset."
                ));
            }
        }
        RouteAssetType::TrackRules => {
            return Err(crate::gettext!(
                "You can't change the track infrastructure parts through the program!"
            ));
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
        return Err(crate::gettext!(
            "You can't change the track infrastructure parts through the program!"
        ));
    }

    let source_files = collect_and_validate_source_files(route_path, uses, &crate::gettext!("replacements"))?;

    println!("[Replacement] Asset replaced: {old_asset} -> {new_asset}");

    let (old_provider, old_product, old_blueprint) = asset_blueprint_ref(old_asset);

    let mut staged = Vec::new();
    let mut replaced_instances = 0;
    for source_file in source_files {
        println!("[Replacement] Processing file: {}", source_file.display());
        let is_binary = source_file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
        if !is_binary
            && !source_file
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
        {
            return Err(crate::gettext!(
                "Unknown route file format: {}",
                source_file.display()
            ));
        }

        let temp = create_temporary_work_directory(&source_work_parent(&source_file, route_path))?;
        let file_path = source_file.materialize(temp.path())?;

        if is_binary {
            let bytes = fs::read(&file_path).map_err(|error| {
                crate::gettext!(
                    "Failed to read {} before replace: {}",
                    source_file.display(),
                    error
                )
            })?;
            let mut reader = SerzReader::new(&bytes);
            let mut root = reader.parse().map_err(|error| {
                crate::gettext!(
                    "Failed to read {} before replace: {}",
                    source_file.display(),
                    error
                )
            })?;
            println!(
                "[Replacement Debug] Searching: Provider='{old_provider}', Product='{old_product}', Blueprint='{old_blueprint}'"
            );
            println!(
                "[Replacement Debug] Replacing with: Provider='{replacement_provider}', Product='{replacement_product}', Blueprint='{replacement_blueprint}'"
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
                "Replacement cancelled, original not modified.",
            )?;
            println!(
                "[Replacement] Serialized {serialized_len} bytes for {}",
                source_file.display()
            );
            replaced_instances += count;
            staged.push((source_file, temp, converted_path));
        } else {
            let xml_path = temp.path().join("source.xml");
            fs::copy(&file_path, &xml_path).map_err(|error| {
                crate::gettext!(
                    "Failed to prepare {} before replace: {}",
                    source_file.display(),
                    error
                )
            })?;

            let mut xml = fs::read_to_string(&xml_path).map_err(|error| {
                crate::gettext!("Failed to read XML for {}: {}", source_file.display(), error)
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
                crate::gettext!(
                    "Replacement in {} created broken XML: {}",
                    source_file.display(),
                    error
                )
            })?;
            fs::write(&xml_path, xml).map_err(|error| {
                crate::gettext!(
                    "Failed to write temporal XML for {}: {}",
                    source_file.display(),
                    error
                )
            })?;

            replaced_instances += instances;
            staged.push((source_file, temp, xml_path));
        }
    }

    if staged.is_empty() {
        return Err(crate::gettext!(
            "No usage of {old_asset} found in the source code of the route. Update check and repeat."
        ));
    }

    let mut backups = Vec::new();
    let mut transaction = FileTransaction::new();
    let mut replaced_files = 0;
    for (source_file, _temp, converted_path) in staged {
        println!("[Replacement] Installed file: {}", source_file.display());
        match install_route_file(route_path, &source_file, &converted_path) {
            Ok(backup) => {
                if let Some(backup_path) = backup {
                    println!("[Replacement] Backup created: {}", backup_path.display());
                    transaction.record(source_file.install_target(route_path), backup_path.clone());
                    backups.push(backup_path);
                }
                replaced_files += 1;
            }
            Err(error) => {
                transaction.rollback();
                return Err(crate::gettext!(
                    "Replaced entries: {}; changed files: {}. Error in {}: {}. Created backups: {}",
                    replaced_instances,
                    replaced_files,
                    source_file.display(),
                    error,
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
