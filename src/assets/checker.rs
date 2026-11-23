use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use rayon::iter::*;
use regex::Regex;
use zip::ZipArchive;

use crate::assets::{
    analysis_cache::*, fuzzy::*, reference::*, replacement::asset_key,
    resolver::resolve_asset_path, scanner::*, types::classify_route_asset_type,
    umlaut::scan_umlaut_assets,
};
use crate::common::domain::*;
use crate::common::serz::{SerzNode, SerzReader};
use crate::common::translit::*;

pub type ApCache = HashMap<PathBuf, HashSet<String>>;
type ZipCache = HashMap<PathBuf, ZipArchive<fs::File>>;

pub struct FileAnalysis {
    from_cache: bool,
    file: RouteFile,
    stamp: Option<FileStamp>,
    uses: Vec<(String, u32, AssetKind)>,
    error: Option<String>,
}

/// Сравнивает имя SERZ-узла с эталоном через `-`, считая `::` эквивалентным `-` (без аллокаций).
fn name_matches_dash(name: &str, expected_dash: &str) -> bool {
    let mut name_parts = name.split("::").flat_map(|p| p.split('-'));
    let mut exp_parts = expected_dash.split('-');
    loop {
        match (name_parts.next(), exp_parts.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn analyze_route_file_with_stamp(
    route_file: &RouteFile,
    stamp: Option<FileStamp>,
    zip_cache: &mut ZipCache,
) -> FileAnalysis {
    let is_binary = route_file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));

    let bytes = match route_file {
        RouteFile::Path(file_path) => {
            if is_binary {
                if fs::metadata(file_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
                    < 256
                {
                    return FileAnalysis {
                        from_cache: false,
                        file: route_file.clone(),
                        stamp,
                        uses: Vec::new(),
                        error: None,
                    };
                }
                if !may_contain_asset_reference(file_path) {
                    return FileAnalysis {
                        from_cache: false,
                        file: route_file.clone(),
                        stamp,
                        uses: Vec::new(),
                        error: None,
                    };
                }
            }
            match fs::read(file_path) {
                Ok(data) => data,
                Err(error) => {
                    return FileAnalysis {
                        from_cache: false,
                        file: route_file.clone(),
                        stamp,
                        uses: Vec::new(),
                        error: Some(format!(
                            "{}: failed to read file: {error}",
                            file_path.display()
                        )),
                    };
                }
            }
        }
        RouteFile::Archive { archive, name, .. } => {
            let min_size = is_binary.then_some(256);
            match read_archive_member_bytes(archive, name, zip_cache, min_size) {
                Ok(Some(data)) => {
                    if is_binary && !may_contain_asset_reference_bytes(&data) {
                        return FileAnalysis {
                            from_cache: false,
                            file: route_file.clone(),
                            stamp,
                            uses: Vec::new(),
                            error: None,
                        };
                    }
                    data
                }
                Ok(None) => {
                    return FileAnalysis {
                        from_cache: false,
                        file: route_file.clone(),
                        stamp,
                        uses: Vec::new(),
                        error: None,
                    };
                }
                Err(error) => {
                    return FileAnalysis {
                        from_cache: false,
                        file: route_file.clone(),
                        stamp,
                        uses: Vec::new(),
                        error: Some(error),
                    };
                }
            }
        }
    };

    if is_binary {
        let mut reader = SerzReader::new(&bytes);
        let node = match reader.parse() {
            Ok(node) => node,
            Err(error) => {
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    stamp,
                    uses: Vec::new(),
                    error: Some(format!("{}: {error}", route_file.display())),
                };
            }
        };
        return FileAnalysis {
            from_cache: false,
            file: route_file.clone(),
            stamp,
            uses: extract_asset_references_from_serz(&node),
            error: None,
        };
    }

    let xml_text = crate::routes::decode_xml_bytes(&bytes);
    let references = extract_asset_references_with_locations(&xml_text);

    FileAnalysis {
        from_cache: false,
        file: route_file.clone(),
        stamp,
        uses: count_asset_uses(references),
        error: None,
    }
}

/// Обходит дерево `SerzNode` напрямую и извлекает ссылки на ассеты,
/// без промежуточной сериализации в XML. Результат идентичен
/// `extract_asset_references_with_locations`, но без аллокации большой строки.
pub(crate) fn extract_asset_references_from_serz(node: &SerzNode) -> Vec<(String, u32, AssetKind)> {
    let mut raw_refs: Vec<(String, AssetKind)> = Vec::new();
    let mut context: Option<AssetKind> = None;
    walk_serz_for_blueprints(node, &mut raw_refs, &mut context);
    count_asset_uses_iter(raw_refs)
}

/// Рекурсивный обход дерева SerzNode. Контекст (транспорт/декор/лофт)
/// определяется ближайшим ранее встреченным элементом контекста и сохраняется
/// до перезаписи другим контекстным элементом — аналогично линейному сканированию XML.
fn walk_serz_for_blueprints(
    node: &SerzNode,
    refs: &mut Vec<(String, AssetKind)>,
    context: &mut Option<AssetKind>,
) {
    if let SerzNode::Element { name, children, .. } = node {
        let prev_context = *context;

        // Устанавливаем контекст при входе в соответствующий элемент.
        if name == "iRailVehicleBlueprint" {
            *context = Some(AssetKind::Vehicle);
        } else if name == "SceneryRenderBlueprint" {
            *context = Some(AssetKind::Scenery);
        } else if name == "RenderBlueprint" {
            *context = Some(AssetKind::Loft);
        }

        // Блок абсолютного блюпринта: извлекаем Provider/Product/BlueprintID.
        if name_matches_dash(name, "iBlueprintLibrary-cAbsoluteBlueprintID") {
            if let Some((provider, product, blueprint)) = extract_blueprint_ids(children) {
                let kind = context.unwrap_or(AssetKind::Other);
                if let Some(path) = asset_relative_path(&provider, &product, &blueprint) {
                    refs.push((path, kind));
                }
            }
        } else {
            // Запасной вариант: «голые» подряд идущие свойства
            // Provider → Product → BlueprintID без обёртки.
            check_bare_blueprint_children(children, refs, context.unwrap_or(AssetKind::Other));
        }

        for child in children {
            walk_serz_for_blueprints(child, refs, context);
        }

        *context = prev_context;
    }
}

/// Извлекает (Provider, Product, BlueprintID) из потомков элемента
/// `iBlueprintLibrary-cAbsoluteBlueprintID`, повторяя логику из
/// `serz::replace_blueprint_references` / `collect_from_absolute_blueprint_id`.
fn extract_blueprint_ids(children: &[SerzNode]) -> Option<(String, String, String)> {
    let mut provider: Option<String> = None;
    let mut product: Option<String> = None;
    let mut blueprint: Option<String> = None;

    for child in children {
        match child {
            // BlueprintSetID → iBlueprintLibrary-cBlueprintSetID с Provider/Product.
            SerzNode::Element {
                name,
                children: bs_children,
                ..
            } if name == "BlueprintSetID" => {
                for bs_child in bs_children {
                    if let SerzNode::Element {
                        name: ibs_name,
                        children: ibs_children,
                        ..
                    } = bs_child
                        && name_matches_dash(ibs_name, "iBlueprintLibrary-cBlueprintSetID")
                    {
                        for prop in ibs_children {
                            if let SerzNode::Property { name, value, .. } = prop {
                                if provider.is_none() && name == "Provider" {
                                    provider = Some(decode_xml_text(value.trim()));
                                }
                                if product.is_none() && name == "Product" {
                                    product = Some(decode_xml_text(value.trim()));
                                }
                            }
                        }
                    }
                }
            }
            // BlueprintID как прямое свойство текущего блока.
            SerzNode::Property { name, value, .. }
                if blueprint.is_none() && name == "BlueprintID" =>
            {
                blueprint = Some(decode_xml_text(value.trim()));
            }
            _ => {}
        }
    }

    let (provider, product, blueprint) = (provider?, product?, blueprint?);
    if provider.is_empty() || product.is_empty() || blueprint.is_empty() {
        return None;
    }
    Some((provider, product, blueprint))
}

/// Проверяет прямых потомков элемента на последовательность
/// Provider → Product → BlueprintID (без обёртки iBlueprintLibrary-cAbsoluteBlueprintID).
fn check_bare_blueprint_children(
    children: &[SerzNode],
    refs: &mut Vec<(String, AssetKind)>,
    kind: AssetKind,
) {
    let mut i = 0;
    while i + 2 < children.len() {
        if let (
            SerzNode::Property {
                name: n1,
                value: v1,
                ..
            },
            SerzNode::Property {
                name: n2,
                value: v2,
                ..
            },
            SerzNode::Property {
                name: n3,
                value: v3,
                ..
            },
        ) = (&children[i], &children[i + 1], &children[i + 2])
            && n1 == "Provider"
            && n2 == "Product"
            && n3 == "BlueprintID"
        {
            let provider = decode_xml_text(v1.trim());
            let product = decode_xml_text(v2.trim());
            let blueprint = decode_xml_text(v3.trim());
            if let Some(path) = asset_relative_path(&provider, &product, &blueprint) {
                refs.push((path, kind));
            }
            i += 3;
            continue;
        }
        i += 1;
    }
}

/// Считает вхождения пар (путь, тип), сохраняя порядок первого появления.
fn count_asset_uses_iter(
    refs: impl IntoIterator<Item = (String, AssetKind)>,
) -> Vec<(String, u32, AssetKind)> {
    struct Entry {
        count: u32,
        kind: AssetKind,
    }

    let iter = refs.into_iter();
    let (lower_bound, _) = iter.size_hint();
    let mut order = Vec::with_capacity(lower_bound);
    let mut map: HashMap<String, Entry> = HashMap::with_capacity(lower_bound);

    for (path, kind) in iter {
        match map.get_mut(&path) {
            Some(entry) => entry.count += 1,
            None => {
                order.push(path.clone());
                map.insert(path, Entry { count: 1, kind });
            }
        }
    }

    order
        .into_iter()
        .filter_map(|path| {
            map.remove(&path)
                .map(|entry| (path, entry.count, entry.kind))
        })
        .collect()
}

/// Считает, сколько раз каждый ассет встречается в файле.
///
/// Порядок первого появления сохраняется: по нему собирается список
/// использований в инвентаре. Вместе с ассетом запоминается, что он из себя
/// представляет: объект маршрута или файл состава.
pub fn count_asset_uses(references: Vec<AssetReference>) -> Vec<(String, u32, AssetKind)> {
    count_asset_uses_iter(references.into_iter().map(|r| (r.path, r.kind)))
}

/// Может ли файл содержать ссылку на ассет.
///
/// Serz хранит имена элементов обычными строками, поэтому файл без подстроки
/// `BlueprintID` не содержит ни одной ссылки на ассет: его конвертация в XML
/// не нужна. При неудачном чтении файл считается подозрительным и проверяется
/// обычным порядком.
pub fn may_contain_asset_reference(file_path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(file_path) else {
        return true;
    };
    let marker = BLUEPRINT_ID_MARKER.as_bytes();
    if marker.is_empty() {
        return true;
    }
    let overlap = marker.len().saturating_sub(1);
    const CHUNK_SIZE: usize = 64 * 1024;

    let mut buf = vec![0u8; overlap + CHUNK_SIZE];
    let mut tail_len = 0;

    loop {
        let n = match file.read(&mut buf[tail_len..tail_len + CHUNK_SIZE]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return true,
        };

        let active_len = tail_len + n;
        if memchr::memmem::find(&buf[..active_len], marker).is_some() {
            return true;
        }

        if active_len >= overlap {
            buf.copy_within(active_len - overlap..active_len, 0);
            tail_len = overlap;
        } else {
            tail_len = active_len;
        }
    }
    false
}

pub(crate) fn check_route<F>(
    route_path: &Path,
    assets_path: &Path,
    ignore_scenarios: bool,
    max_concurrent_serz: usize,
    report_progress: F,
) -> Result<CheckResult, String>
where
    F: Fn(&str) + Sync,
{
    report_progress("Scanning route files...");
    let collected_route_files = collect_route_files(route_path, ignore_scenarios)?;
    let mut structure_issues =
        validate_route_structure(route_path, assets_path, &collected_route_files);
    let mut route_files: Vec<RouteFile> = collected_route_files
        .into_iter()
        .map(RouteFile::Path)
        .collect();
    let outside: HashSet<String> = route_files
        .iter()
        .filter_map(|file| file.route_name(route_path).map(|name| name.to_lowercase()))
        .collect();
    for member in collect_archive_members(route_path, ignore_scenarios)? {
        if let Some(name) = member.route_name(route_path)
            && outside.contains(&name.to_lowercase())
        {
            continue;
        }
        route_files.push(member);
    }
    let is_tracks_bin = |route_file: &RouteFile| {
        route_file
            .route_name(route_path)
            .is_some_and(|name| name.eq_ignore_ascii_case("Networks/Tracks.bin"))
    };
    route_files.sort_by_key(|a| !is_tracks_bin(a));
    let all_files = route_files;
    let normal_binary_count = all_files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        })
        .count();
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(max_concurrent_serz.max(1))
        .min(normal_binary_count.max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
        .map_err(|error| format!("Failed to create file check pool: {error}"))?;
    let mut cache = AnalysisCache::load(route_path);
    let total_files = all_files.len();
    let processed = AtomicUsize::new(0);

    if total_files > 0 {
        report_progress(&format!(
            "Analyzing files. 1 out of {total_files}; threads: {worker_count}",
        ));
    }

    let analyses: Vec<FileAnalysis> = pool.install(|| {
        all_files
            .par_iter()
            .map_init(ZipCache::new, |zip_cache, route_file| {
                let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
                if done.is_multiple_of(32) || done == total_files {
                    report_progress(&format!(
                        "Analyzing files. {done} out of {total_files}; threads: {worker_count}",
                    ));
                }

                let name = route_file.route_name(route_path);
                let stamp = route_file.stamp();
                if let (Some(name), Some(stamp)) = (&name, stamp)
                    && let Some(cached) = cache.get(name, stamp)
                {
                    return FileAnalysis {
                        from_cache: true,
                        file: route_file.clone(),
                        stamp: Some(stamp),
                        uses: cached.uses.clone(),
                        error: None,
                    };
                }
                analyze_route_file_with_stamp(route_file, stamp, zip_cache)
            })
            .collect()
    });

    let mut asset_uses: HashMap<String, Vec<AssetUse>> = HashMap::new();
    // Написание пути, как оно встретилось в маршруте: ключ в asset_uses приведён
    // к нижнему регистру, потому что игра регистр не различает.
    let mut asset_names: HashMap<String, String> = HashMap::new();
    let mut conversion_errors = Vec::new();

    for analysis in analyses {
        if !analysis.from_cache
            && analysis.error.is_none()
            && let Some(name) = analysis.file.route_name(route_path)
            && let Some(stamp) = analysis.stamp
        {
            cache.put(
                name,
                CachedFile {
                    stamp,
                    uses: analysis.uses.clone(),
                },
            );
        }
        let tile = route_tile_label(route_path, &analysis.file);
        let tile_search = tile.to_lowercase();
        let source_file_search = analysis.file.display().to_lowercase();
        for (asset, count, kind) in analysis.uses {
            let key = asset_key(&asset);
            asset_names
                .entry(key.clone())
                .or_insert_with(|| asset.clone());
            asset_uses.entry(key).or_default().push(AssetUse {
                source_file: analysis.file.clone(),
                tile: tile.clone(),
                tile_kind: analysis.file.tile_kind(),
                kind,
                count,
                tile_search: tile_search.clone(),
                source_file_search: source_file_search.clone(),
            });
        }
        if let Some(error) = analysis.error {
            conversion_errors.push(format!(
                "{} — {error}",
                analysis
                    .file
                    .route_name(route_path)
                    .unwrap_or_else(|| analysis.file.display())
            ));
        }
    }
    conversion_errors.sort();

    // Удаляем из кэша файлы, которых в маршруте больше нет, и сохраняем кэш.
    let current_files: HashMap<String, ()> = all_files
        .iter()
        .filter_map(|path| path.route_name(route_path))
        .map(|name| (name, ()))
        .collect();
    cache.retain(&current_files);
    if let Err(error) = cache.save(route_path) {
        structure_issues.push(format!("Failed to save check cache: {error}"));
    }

    report_progress(&format!(
        "Unique links: {}. Searching for .ap arcives...",
        asset_uses.len(),
    ));
    let product_dirs: HashSet<PathBuf> = asset_uses
        .keys()
        .filter_map(|relative_asset| {
            let mut parts = relative_asset.split('/');
            let (Some(provider), Some(product), Some(_)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            let exact = assets_path.join(relative_asset.replace('/', "\\"));
            if exact.is_file() {
                return None;
            }
            Some(assets_path.join(provider).join(product))
        })
        .collect();

    let (ap_cache, archive_errors) = scan_product_ap_archives(&product_dirs, &pool)?;
    structure_issues.extend(archive_errors);

    report_progress("Checking for umlauts...");
    let inventory_seed: Vec<_> = asset_uses
        .iter()
        .map(|(path, uses)| {
            let display_path = asset_names
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.clone());
            let path_search = display_path.to_lowercase();
            ReferencedAsset {
                path: display_path,
                uses: uses.clone(),
                missing: false,
                path_search,
            }
        })
        .collect();
    let (umlaut_assets, umlaut_errors) =
        scan_umlaut_assets(&inventory_seed, assets_path, max_concurrent_serz)?;
    conversion_errors.extend(umlaut_errors);
    conversion_errors.sort();
    let mut route_properties_warnings: Vec<String> = Vec::new();
    if let Ok(route_properties_text) = read_route_properties_text(route_path) {
        let declared_pairs: HashSet<(String, String)> = parse_preload_pairs(&route_properties_text)
            .into_iter()
            .map(|(p, pr)| (p.to_lowercase(), pr.to_lowercase()))
            .collect();
        const SKIPPED_PROVIDER_PRODUCT: &[(&str, &str)] = &[("kuju", "railsimulatorcore")];
        let mut used_pairs: HashSet<(String, String)> = HashSet::new();
        for relative_asset in asset_uses.keys() {
            let mut parts = relative_asset.split('/');
            if let (Some(p0), Some(p1)) = (parts.next(), parts.next()) {
                let provider = p0.to_lowercase();
                let product = p1.to_lowercase();
                if !provider.is_empty()
                    && !product.is_empty()
                    && !SKIPPED_PROVIDER_PRODUCT.contains(&(provider.as_str(), product.as_str()))
                {
                    used_pairs.insert((provider, product));
                }
            }
        }
        let mut missing: Vec<(String, String)> = used_pairs
            .into_iter()
            .filter(|pair| !declared_pairs.contains(pair))
            .collect();
        missing.sort();
        for (provider, product) in missing {
            route_properties_warnings.push(format!(
                "Provider/product «{provider} / {product}» used on route, but not declared in RouteProperties.xml.",
            ));
        }
    }

    let (missing_assets, asset_inventory) = resolve_missing_and_corrupted(
        asset_uses,
        &asset_names,
        &ap_cache,
        assets_path,
        &mut structure_issues,
    );

    Ok(CheckResult {
        missing_assets,
        asset_inventory,
        umlaut_assets,
        conversion_errors,
        structure_issues,
        route_properties_warnings,
    })
}

/// Проверяет, есть ли RouteProperties.xml либо на диске, либо внутри
/// любого .ap-архива в папке маршрута.
fn route_properties_exists(route_path: &Path) -> bool {
    if route_path.join("RouteProperties.xml").is_file() {
        return true;
    }
    let Ok(entries) = fs::read_dir(route_path) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_ap_archive(&path) {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let Ok(archive) = ZipArchive::new(file) else {
            continue;
        };
        for raw_name in archive.file_names().flatten() {
            if raw_name.ends_with(['/', '\\']) {
                continue;
            }
            let base = raw_name.rsplit(['/', '\\']).next().unwrap_or_default();
            if base.eq_ignore_ascii_case("RouteProperties.xml") {
                return true;
            }
        }
    }
    false
}

/// Читает RouteProperties.xml с диска или из .ap-архива, декодируя
/// UTF-8/UTF-16 (с BOM) через crate::routes::decode_xml_bytes.
fn read_route_properties_text(route_path: &Path) -> Result<String, String> {
    let properties_path = route_path.join("RouteProperties.xml");
    if let Ok(data) = fs::read(&properties_path) {
        return Ok(crate::routes::decode_xml_bytes(&data));
    }
    let entries = fs::read_dir(route_path)
        .map_err(|error| format!("failed to read {}: {error}", route_path.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Read failure {}: {error}", route_path.display()))?;
        let archive_path = entry.path();
        if !is_ap_archive(&archive_path) {
            continue;
        }
        let Ok(file) = fs::File::open(&archive_path) else {
            continue;
        };
        let Ok(mut archive) = ZipArchive::new(file) else {
            continue;
        };
        let found_name = archive
            .file_names()
            .flatten()
            .find(|raw_name| {
                !raw_name.ends_with(['/', '\\'])
                    && raw_name
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or_default()
                        .eq_ignore_ascii_case("RouteProperties.xml")
            })
            .map(|name| name.into_owned());

        if let Some(member_name) = found_name
            && let Ok(mut member) = archive.by_name(&member_name)
        {
            let mut bytes = Vec::new();
            if member.read_to_end(&mut bytes).is_ok() {
                return Ok(crate::routes::decode_xml_bytes(&bytes));
            }
        }
    }
    Err("RouteProperties.xml not found".to_owned())
}

/// Извлекает пары (Provider, Product) из блоков
/// `<RBlueprintSetPreLoad>...</RBlueprintSetPreLoad>`.
fn parse_preload_pairs(xml: &str) -> HashSet<(String, String)> {
    static RE_PRELOAD: OnceLock<Regex> = OnceLock::new();
    static RE_PROVIDER: OnceLock<Regex> = OnceLock::new();
    static RE_PRODUCT: OnceLock<Regex> = OnceLock::new();

    let preload_re = RE_PRELOAD.get_or_init(|| {
        Regex::new(r"(?is)<RBlueprintSetPreLoad\b[^>]*>(.*?)</RBlueprintSetPreLoad\s*>")
            .expect("valid regex")
    });
    let provider_re = RE_PROVIDER.get_or_init(|| {
        Regex::new(r"(?is)<Provider\b[^>]*>(.*?)</Provider\s*>").expect("valid regex")
    });
    let product_re = RE_PRODUCT.get_or_init(|| {
        Regex::new(r"(?is)<Product\b[^>]*>(.*?)</Product\s*>").expect("valid regex")
    });

    let mut pairs = HashSet::new();
    for preload_cap in preload_re.captures_iter(xml) {
        let block = preload_cap.get(1).map(|c| c.as_str()).unwrap_or("");
        let providers = provider_re
            .captures_iter(block)
            .filter_map(|c| c.get(1))
            .map(|m| decode_xml_text(m.as_str().trim()));
        let products = product_re
            .captures_iter(block)
            .filter_map(|c| c.get(1))
            .map(|m| decode_xml_text(m.as_str().trim()));
        for (p, pr) in providers.zip(products) {
            if !p.is_empty() && !pr.is_empty() {
                pairs.insert((p, pr));
            }
        }
    }
    pairs
}

pub(crate) fn validate_route_structure(
    route_path: &Path,
    assets_path: &Path,
    files: &[PathBuf],
) -> Vec<String> {
    let mut issues = Vec::new();
    if !route_properties_exists(route_path) {
        issues.push("RouteProperties.xml not found".to_owned());
    }
    if !assets_path.is_dir() {
        issues.push("Assets folder not found".to_owned());
    }
    if files.is_empty() {
        issues.push("no any .bin или .xml for check".to_owned());
    }
    if let Ok(directories) = unexpected_route_directories(route_path)
        && !directories.is_empty()
    {
        issues.push(format!(
            "unexpected route folders: {}",
            directories.join(", ")
        ));
    }
    if let Ok(leftover) = leftover_temporary_work_directories(route_path)
        && !leftover.is_empty()
    {
        issues.push(format!(
            "route folder stil have temp program folders (can be deleted): {}",
            leftover.join(", ")
        ));
    }
    issues
}

struct ProductApScan {
    dir: PathBuf,
    files: HashSet<String>,
    errors: Vec<String>,
}

pub(crate) fn scan_product_ap_archives(
    product_dirs: &HashSet<PathBuf>,
    pool: &rayon::ThreadPool,
) -> Result<(ApCache, Vec<String>), String> {
    let scans: Vec<ProductApScan> = pool.install(|| {
        product_dirs
            .par_iter()
            .map(|dir| scan_single_product_dir(dir))
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut ap_cache = HashMap::with_capacity(scans.len());
    let mut archive_errors = Vec::new();
    for scan in scans {
        ap_cache.insert(scan.dir, scan.files);
        archive_errors.extend(scan.errors);
    }
    archive_errors.sort();
    archive_errors.dedup();

    Ok((ap_cache, archive_errors))
}

fn scan_single_product_dir(product_dir: &Path) -> Result<ProductApScan, String> {
    let mut files = HashSet::new();
    let mut errors = Vec::new();

    let entries = match fs::read_dir(product_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProductApScan {
                dir: product_dir.to_path_buf(),
                files,
                errors,
            });
        }
        Err(error) => {
            return Err(format!("Failed to read {}: {error}", product_dir.display()));
        }
    };

    for archive_path in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_ap_archive(path))
    {
        if let Err(error) = read_ap_archive_files(&archive_path, &mut files) {
            errors.push(error);
        }
    }

    Ok(ProductApScan {
        dir: product_dir.to_path_buf(),
        files,
        errors,
    })
}

fn is_ap_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
}

fn read_ap_archive_files(archive_path: &Path, files: &mut HashSet<String>) -> Result<(), String> {
    let archive_file = fs::File::open(archive_path)
        .map_err(|error| format!("Failed to open archive {}: {error}", archive_path.display()))?;
    let archive = ZipArchive::new(archive_file)
        .map_err(|error| format!("Failed to read archive {}: {error}", archive_path.display()))?;

    for name in archive.file_names().flatten() {
        files.insert(name.replace('/', "\\").trim().to_lowercase());
    }
    Ok(())
}

fn asset_exists_in_ap_cache(assets_path: &Path, relative_asset: &str, ap_cache: &ApCache) -> bool {
    let parts: Vec<&str> = relative_asset.split('/').collect();
    if parts.len() < 3 {
        return false;
    }
    let product_dir = assets_path.join(parts[0]).join(parts[1]);
    let Some(files) = ap_cache.get(&product_dir) else {
        return false;
    };

    let inner_path = parts[2..].join("\\").to_lowercase();
    let product_path = parts[1..].join("\\").to_lowercase();
    let full_path = relative_asset.replace('/', "\\").to_lowercase();

    let mut variants = Vec::with_capacity(18);
    for base in [inner_path, product_path, full_path] {
        variants.push(replace_broken_characters(&base, "ss"));
        variants.push(replace_broken_characters(&base, "ß"));
        variants.push(base);
    }

    let base_len = variants.len();
    for i in 0..base_len {
        let variant = &variants[i];
        if !variant.ends_with(".tgpcdx")
            && !variant.ends_with(".geopcdx")
            && !variant.ends_with(".bin")
        {
            variants.push(format!("{variant}.tgpcdx"));
        }
    }

    variants
        .iter()
        .any(|variant| files.contains(variant.trim()))
}

pub(crate) fn resolve_missing_and_corrupted(
    asset_uses: HashMap<String, Vec<AssetUse>>,
    asset_names: &HashMap<String, String>,
    ap_cache: &ApCache,
    assets_path: &Path,
    structure_issues: &mut Vec<String>,
) -> (Vec<MissingAsset>, Vec<ReferencedAsset>) {
    let mut asset_inventory = Vec::with_capacity(asset_uses.len());
    let mut missing_assets = Vec::new();

    for (relative_asset, uses) in asset_uses {
        if uses.iter().any(|usage| usage.kind.is_vehicle()) {
            continue;
        }
        let display_path = asset_names
            .get(&relative_asset)
            .cloned()
            .unwrap_or_else(|| relative_asset.clone());
        let full_asset_path = match resolve_asset_path(assets_path, &relative_asset) {
            Ok(path) => path,
            Err(error) => {
                structure_issues.push(error);
                None
            }
        };

        let (is_in_archive, resolved_disk_path, suggested_repair, is_corrupted) =
            if let Some(exact_path) = full_asset_path {
                (false, Some(exact_path), None, false)
            } else if asset_exists_in_ap_cache(assets_path, &relative_asset, ap_cache) {
                (true, None, None, false)
            } else if let Some((fuzzy_disk_path, relative_repair)) =
                fuzzy_resolve_disk_path(assets_path, &relative_asset)
            {
                let query_corrupted = has_corrupted_or_nonascii(&relative_asset);
                let umlaut_folded_equivalent = !relative_asset.is_ascii()
                    && normalize_fuzzy_segment(&relative_asset)
                        == normalize_fuzzy_segment(&relative_repair);
                if query_corrupted || umlaut_folded_equivalent {
                    (false, Some(fuzzy_disk_path), Some(relative_repair), true)
                } else {
                    (false, None, None, false)
                }
            } else {
                (false, None, None, false)
            };

        let is_missing = !is_in_archive && resolved_disk_path.is_none() && !is_corrupted;
        if is_missing || is_corrupted {
            missing_assets.push(MissingAsset {
                path: display_path.clone(),
                uses: uses.clone(),
                suggested_repair,
                is_corrupted_link: is_corrupted,
                asset_type: classify_route_asset_type(&display_path, &uses),
            });
        }
        let path_search = display_path.to_lowercase();
        asset_inventory.push(ReferencedAsset {
            path: display_path,
            uses,
            missing: is_missing,
            path_search,
        });
    }

    missing_assets.sort_by(|left, right| left.path.cmp(&right.path));
    asset_inventory.sort_by(|left, right| left.path.cmp(&right.path));
    (missing_assets, asset_inventory)
}

fn may_contain_asset_reference_bytes(bytes: &[u8]) -> bool {
    let marker = BLUEPRINT_ID_MARKER.as_bytes();
    marker.is_empty() || memchr::memmem::find(bytes, marker).is_some()
}

fn read_archive_member_bytes(
    archive_path: &Path,
    member_name: &str,
    zip_cache: &mut ZipCache,
    skip_if_smaller_than: Option<u64>,
) -> Result<Option<Vec<u8>>, String> {
    let zip = match zip_cache.entry(archive_path.to_path_buf()) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let file = fs::File::open(archive_path).map_err(|error| {
                format!("Failed to open archive {}: {error}", archive_path.display())
            })?;
            let archive = ZipArchive::new(file).map_err(|error| {
                format!("Failed to read arcive {}: {error}", archive_path.display())
            })?;
            entry.insert(archive)
        }
    };

    // collect_archive_members сохраняет точное имя записи из архива, поэтому by_name работает за O(1).
    let mut member = zip.by_name(member_name).map_err(|error| {
        format!(
            "Cannot find {member_name} в {}: {error}",
            archive_path.display()
        )
    })?;

    if let Some(min_size) = skip_if_smaller_than
        && member.size() < min_size
    {
        return Ok(None);
    }

    let mut bytes = Vec::with_capacity(member.size().min(64 * 1024 * 1024) as usize);
    member.read_to_end(&mut bytes).map_err(|error| {
        format!(
            "Failed to read {member_name} from {}: {error}",
            archive_path.display()
        )
    })?;
    Ok(Some(bytes))
}
