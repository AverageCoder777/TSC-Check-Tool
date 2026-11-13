use super::*;
use std::collections::{HashMap, HashSet};

pub struct FileAnalysis {
    /// Разбор взят из кэша: файл не менялся с прошлой проверки.
    from_cache: bool,
    file: crate::domain::RouteFile,
    /// Сколько раз каждый ассет встречается в файле.
    uses: Vec<(String, u32, crate::domain::AssetKind)>,
    error: Option<String>,
}

pub(crate) fn analyze_route_file(
    route_file: &crate::domain::RouteFile,
    temp_dir: &Path,
) -> FileAnalysis {
    let is_archive = matches!(route_file, crate::domain::RouteFile::Archive { .. });
    // Запись архива читается через временный файл: конвертеру Serz нужен
    // настоящий файл на диске.
    let file_path = match route_file.materialize(temp_dir) {
        Ok(path) => path,
        Err(error) => {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: Some(error),
            };
        }
    };
    let is_binary = file_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"));
    // .bin файлы: разбираем SERZ-дерево напрямую, без сериализации в XML-строку
    // и без регулярных выражений по тексту.
    if is_binary {
        // Проверка размера: слишком маленький BIN не содержит ссылок на ассеты.
        if fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            < 256
        {
            if is_archive {
                let _ = fs::remove_file(&file_path);
            }
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: None,
            };
        }
        // Serz хранит имена элементов обычными строками, поэтому файл без
        // `BlueprintID` заведомо не содержит ссылок на ассеты. Такие файлы не
        // конвертируются: на больших маршрутах это основная часть работы.
        if !may_contain_asset_reference(&file_path) {
            if is_archive {
                let _ = fs::remove_file(&file_path);
            }
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: None,
            };
        }
        let xml_bytes = match std::fs::read(&file_path) {
            Ok(data) => data,
            Err(error) => {
                if is_archive {
                    let _ = fs::remove_file(&file_path);
                }
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    uses: Vec::new(),
                    error: Some(format!(
                        "{}: не удалось прочитать BIN: {error}",
                        file_path.display()
                    )),
                };
            }
        };
        let mut reader = crate::serz::SerzReader::new(&xml_bytes);
        let node = match reader.parse() {
            Ok(node) => node,
            Err(error) => {
                if is_archive {
                    let _ = fs::remove_file(&file_path);
                }
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    uses: Vec::new(),
                    error: Some(format!("{}: {error}", file_path.display())),
                };
            }
        };
        if is_archive {
            let _ = fs::remove_file(&file_path);
        }
        return FileAnalysis {
            from_cache: false,
            file: route_file.clone(),
            uses: extract_asset_references_from_serz(&node),
            error: None,
        };
    }

    // .xml файлы: оставляем существующий путь через текстовый XML.
    let xml_data = match fs::read(&file_path) {
        Ok(data) => data,
        Err(error) => {
            if is_archive {
                let _ = fs::remove_file(&file_path);
            }
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: Some(format!(
                    "{}: не удалось прочитать XML: {error}",
                    file_path.display()
                )),
            };
        }
    };

    if is_archive {
        let _ = fs::remove_file(&file_path);
    }
    let xml_text = decode_xml_bytes(&xml_data);

    let references = crate::reference::extract_asset_references_with_locations(&xml_text);

    FileAnalysis {
        from_cache: false,
        file: route_file.clone(),
        uses: count_asset_uses(references),
        error: None,
    }
}

/// Обходит дерево `SerzNode` напрямую и извлекает ссылки на ассеты,
/// без промежуточной сериализации в XML. Результат идентичен
/// `extract_asset_references_with_locations`, но без аллокации большой строки.
pub(crate) fn extract_asset_references_from_serz(
    node: &crate::serz::SerzNode,
) -> Vec<(String, u32, crate::domain::AssetKind)> {
    let mut raw_refs: Vec<(String, crate::domain::AssetKind)> = Vec::new();
    let mut context: Option<crate::domain::AssetKind> = None;
    walk_serz_for_blueprints(node, &mut raw_refs, &mut context);
    count_asset_uses_from_pairs(raw_refs)
}

/// Рекурсивный обход дерева SerzNode. Контекст (транспорт/декор/лофт)
/// определяется ближайшим ранее встреченным элементом контекста и сохраняется
/// до перезаписи другим контекстным элементом — аналогично линейному сканированию XML.
fn walk_serz_for_blueprints(
    node: &crate::serz::SerzNode,
    refs: &mut Vec<(String, crate::domain::AssetKind)>,
    context: &mut Option<crate::domain::AssetKind>,
) {
    if let crate::serz::SerzNode::Element { name, children, .. } = node {
        let clean = name.replace("::", "-");
        let prev_context = *context;

        // Устанавливаем контекст при входе в соответствующий элемент.
        if clean == "iRailVehicleBlueprint" {
            *context = Some(crate::domain::AssetKind::Vehicle);
        } else if clean == "SceneryRenderBlueprint" {
            *context = Some(crate::domain::AssetKind::Scenery);
        } else if clean == "RenderBlueprint" {
            *context = Some(crate::domain::AssetKind::Loft);
        }

        // Блок абсолютного блюпринта: извлекаем Provider/Product/BlueprintID.
        if clean == "iBlueprintLibrary-cAbsoluteBlueprintID" {
            if let Some((provider, product, blueprint)) = extract_blueprint_ids(children) {
                let kind = context.unwrap_or(crate::domain::AssetKind::Other);
                if let Some(path) = crate::reference::asset_relative_path(&provider, &product, &blueprint)
                {
                    refs.push((path, kind));
                }
            }
        } else {
            // Запасной вариант: «голые» подряд идущие свойства
            // Provider → Product → BlueprintID без обёртки.
            check_bare_blueprint_children(
                children,
                refs,
                context.unwrap_or(crate::domain::AssetKind::Other),
            );
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
fn extract_blueprint_ids(children: &[crate::serz::SerzNode]) -> Option<(String, String, String)> {
    let mut provider: Option<String> = None;
    let mut product: Option<String> = None;
    let mut blueprint: Option<String> = None;

    for child in children {
        // BlueprintSetID → iBlueprintLibrary-cBlueprintSetID с Provider/Product.
        if let crate::serz::SerzNode::Element { name, children: bs_children, .. } = child {
            let clean = name.replace("::", "-");
            if clean == "BlueprintSetID" {
                for bs_child in bs_children {
                    if let crate::serz::SerzNode::Element {
                        name: ibs_name,
                        children: ibs_children,
                        ..
                    } = bs_child
                    {
                        let ibs_clean = ibs_name.replace("::", "-");
                        if ibs_clean == "iBlueprintLibrary-cBlueprintSetID" {
                            for prop in ibs_children {
                                if let crate::serz::SerzNode::Property { name, value, .. } = prop {
                                    if provider.is_none() && name == "Provider" {
                                        provider =
                                            Some(crate::translit::decode_xml_text(value.trim()));
                                    }
                                    if product.is_none() && name == "Product" {
                                        product =
                                            Some(crate::translit::decode_xml_text(value.trim()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // BlueprintID как прямое свойство текущего блока.
        if let crate::serz::SerzNode::Property { name, value, .. } = child
            && blueprint.is_none()
            && name == "BlueprintID"
        {
            blueprint = Some(crate::translit::decode_xml_text(value.trim()));
        }
    }

    let provider = provider?;
    let product = product?;
    let blueprint = blueprint?;

    if provider.is_empty() || product.is_empty() || blueprint.is_empty() {
        return None;
    }

    Some((provider, product, blueprint))
}

/// Проверяет прямых потомков элемента на последовательность
/// Provider → Product → BlueprintID (без обёртки iBlueprintLibrary-cAbsoluteBlueprintID).
fn check_bare_blueprint_children(
    children: &[crate::serz::SerzNode],
    refs: &mut Vec<(String, crate::domain::AssetKind)>,
    kind: crate::domain::AssetKind,
) {
    let mut i = 0;
    while i < children.len() {
        if let crate::serz::SerzNode::Property { name, value, .. } = &children[i]
            && name == "Provider"
            && let Some(crate::serz::SerzNode::Property { name: n2, value: v2, .. }) =
                children.get(i + 1)
            && n2 == "Product"
            && let Some(crate::serz::SerzNode::Property { name: n3, value: v3, .. }) =
                children.get(i + 2)
            && n3 == "BlueprintID"
        {
            let provider = crate::translit::decode_xml_text(value.trim());
            let product = crate::translit::decode_xml_text(v2.trim());
            let blueprint = crate::translit::decode_xml_text(v3.trim());
            if let Some(path) =
                crate::reference::asset_relative_path(&provider, &product, &blueprint)
            {
                refs.push((path, kind));
            }
            i += 3;
            continue;
        }
        i += 1;
    }
}

/// Считает вхождения пар (путь, тип), сохраняя порядок первого появления.
fn count_asset_uses_from_pairs(
    refs: Vec<(String, crate::domain::AssetKind)>,
) -> Vec<(String, u32, crate::domain::AssetKind)> {
    struct Entry {
        count: u32,
        kind: crate::domain::AssetKind,
    }

    let mut order = Vec::new();
    let mut map: std::collections::HashMap<String, Entry> =
        std::collections::HashMap::with_capacity(refs.len());

    for (path, kind) in refs {
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
pub fn count_asset_uses(
    references: Vec<crate::reference::AssetReference>,
) -> Vec<(String, u32, crate::domain::AssetKind)> {
    struct Entry {
        count: u32,
        kind: crate::domain::AssetKind,
    }

    let mut order = Vec::new();
    let mut map: HashMap<String, Entry> = HashMap::with_capacity(references.len());

    for reference in references {
        match map.get_mut(&reference.path) {
            Some(entry) => {
                entry.count += 1;
            }
            None => {
                order.push(reference.path.clone());
                map.insert(
                    reference.path,
                    Entry {
                        count: 1,
                        kind: reference.kind,
                    },
                );
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

/// Может ли файл содержать ссылку на ассет.
///
/// Serz хранит имена элементов обычными строками, поэтому файл без подстроки
/// `BlueprintID` не содержит ни одной ссылки на ассет: его конвертация в XML
/// не нужна. При неудачном чтении файл считается подозрительным и проверяется
/// обычным порядком.
pub fn may_contain_asset_reference(file_path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(file_path) else {
        return true;
    };
    let marker = crate::translit::BLUEPRINT_ID_MARKER.as_bytes();
    if marker.is_empty() {
        return true;
    }
    let overlap = marker.len().saturating_sub(1);
    const CHUNK_SIZE: usize = 64 * 1024;

    // Буфер вмещает перекрытие с прошлого шага + новую порцию данных
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
            // Копируем хвост в самое начало буфера без аллокаций
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
) -> Result<crate::domain::CheckResult, String>
where
    F: Fn(&str) + Sync,
{
    report_progress("Сканирование файлов маршрута...");
    let collected_route_files = collect_route_files(route_path, ignore_scenarios)?;

    let mut structure_issues =
        validate_route_structure(route_path, assets_path, &collected_route_files);

    // Основные файлы маршрута могут быть упакованы в .ap...
    let mut route_files: Vec<crate::domain::RouteFile> = collected_route_files
        .into_iter()
        .map(crate::domain::RouteFile::Path)
        .collect();
    let outside: HashSet<String> = route_files
        .iter()
        .filter_map(|file| file.route_name(route_path).map(|name| name.to_lowercase()))
        .collect();
    for member in collect_archive_members(route_path, ignore_scenarios)? {
        if let Some(name) = member.route_name(route_path)
            && outside.contains(&name.to_lowercase())
        {
            // Такой файл лежит рядом с архивом и перекрывает запись архива.
            continue;
        }
        route_files.push(member);
    }

    // Тяжёлый глобальный файл сети путей проверяем отдельно и в самом конце:
    // его конвертация занимает минуты и не должна задерживать остальные файлы
    // и отчёты о ходе проверки.
    let is_tracks_bin = |route_file: &crate::domain::RouteFile| {
        route_file
            .route_name(route_path)
            .map(|name| name.eq_ignore_ascii_case("Networks/Tracks.bin"))
            .unwrap_or(false)
    };
    // Тяжелые файлы (Tracks.bin) ставим в начало, чтобы задействовать пул потоков сразу
    route_files.sort_by_key(|a| !is_tracks_bin(a));
    let all_files = route_files;

    let temp_dir = tempdir()
        .map_err(|error| format!("Не удалось создать временную папку для Serz: {error}"))?;

    // Архивы маршрута (.ap) не распаковываются целиком: записи materialize()
    // читаются из zip по требованию только для анализируемых .bin/.xml файлов.

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
        .map_err(|error| format!("Не удалось создать пул проверки файлов: {error}"))?;

    let mut analyses = Vec::with_capacity(all_files.len());
    // Кэш разбора лежит в папке маршрута: неизменившиеся файлы берутся из
    // него, а заново разбираются только те, что изменились или появились.
    let mut cache = crate::analysis_cache::AnalysisCache::load(route_path);
    for (chunk_index, chunk) in all_files.chunks(32).enumerate() {
        let start_index = chunk_index * 32;
        report_progress(&format!(
            "Анализ файлов: {} из {}; параллельных Serz-процессов: {}...",
            start_index + 1,
            all_files.len(),
            worker_count.min(normal_binary_count.max(1))
        ));
        let chunk_analyses: Vec<FileAnalysis> = pool.install(|| {
            chunk
                .par_iter()
                .enumerate()
                .map(|(_, route_file)| {
                    let name = route_file.route_name(route_path);
                    let stamp = route_file.stamp();
                    if let (Some(name), Some(stamp)) = (&name, stamp)
                        && let Some(cached) = cache.get(name, stamp)
                    {
                        return FileAnalysis {
                            from_cache: true,
                            file: route_file.clone(),
                            uses: cached.uses.clone(),
                            error: None,
                        };
                    }
                    analyze_route_file(route_file, temp_dir.path())
                })
                .collect()
        });
        analyses.extend(chunk_analyses);
    }
    let mut asset_uses: HashMap<String, Vec<crate::domain::AssetUse>> = HashMap::new();
    // Написание пути, как оно встретилось в маршруте: ключ в asset_uses приведён
    // к нижнему регистру, потому что игра регистр не различает.
    let mut asset_names: HashMap<String, String> = HashMap::new();
    let mut conversion_errors = Vec::new();
    for analysis in analyses {
        if !analysis.from_cache {
            // В кэш попадают только заново разобранные файлы: для остальных
            // прежняя запись и так актуальна.
            if let Some(name) = analysis.file.route_name(route_path)
                && let Some(stamp) = analysis.file.stamp()
            {
                cache.put(
                    name,
                    crate::analysis_cache::CachedFile {
                        stamp,
                        uses: analysis.uses.clone(),
                    },
                );
            }
        }
        let tile = crate::reference::route_tile_label(route_path, &analysis.file);
        let tile_search = tile.to_lowercase();
        let source_file_search = analysis.file.display().to_lowercase();
        for (asset, count, kind) in analysis.uses {
            // Игра читает пути без учёта регистра, поэтому ссылки, отличающиеся
            // регистром, относятся к одному ассету.
            let key = asset_key(&asset);
            asset_names
                .entry(key.clone())
                .or_insert_with(|| asset.clone());
            let uses = asset_uses.entry(key).or_default();
            uses.push(crate::domain::AssetUse {
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
        structure_issues.push(format!("Кэш разбора не сохранён: {error}"));
    }

    report_progress(&format!(
        "Уникальных ссылок: {}. Проверка файлов и архивов .ap...",
        asset_uses.len(),
    ));
    let mut ap_cache: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    // Уникальные папки продуктов, для которых нужно проверить .ap-архивы.
    // Читаем архивы параллельно: открытие большого .ap занимает секунды, и
    // последовательное чтение нескольких архивов задерживает всю проверку.
    let product_dirs: HashSet<PathBuf> = asset_uses
        .keys()
        .filter_map(|relative_asset| {
            let parts: Vec<&str> = relative_asset.split('/').collect();
            if parts.len() < 3 {
                return None;
            }
            // Если ассет уже есть на диске по точному пути, архив для него
            // проверять не нужно.
            let exact = assets_path.join(relative_asset.replace('/', "\\"));
            if exact.is_file() {
                return None;
            }
            Some(assets_path.join(parts[0]).join(parts[1]))
        })
        .collect();

    let (scanned, archive_errors) = scan_product_ap_archives(&product_dirs, &pool)?;
    ap_cache.extend(scanned);
    structure_issues.extend(archive_errors);

    report_progress("Проверка имён и строк ассетов на умляуты...");
    let inventory_seed: Vec<_> = asset_uses
        .iter()
        .map(|(path, uses)| crate::domain::ReferencedAsset {
            path: asset_names
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.clone()),
            uses: uses.clone(),
            missing: false,
            path_search: asset_names
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.clone())
                .to_lowercase(),
        })
        .collect();
    let (umlaut_assets, umlaut_errors) =
        scan_umlaut_assets(&inventory_seed, assets_path, max_concurrent_serz)?;
    conversion_errors.extend(umlaut_errors);
    conversion_errors.sort();

    // Проверки подключённых в RouteProperties.xml продуктов здесь нет и не
    // должно быть: новый провайдер с продуктом добавляют как раз для замены
    // ассетов — в кастомном бинарнике ссылки ведут на геометрию из другой
    // папки, и её продукт подключают к маршруту, даже если прямых ссылок на
    // него в файлах маршрута нет.

    let (missing_assets, asset_inventory) = resolve_missing_and_corrupted(
        asset_uses,
        &asset_names,
        &ap_cache,
        assets_path,
        &mut structure_issues,
    );

    Ok(crate::domain::CheckResult {
        missing_assets,
        asset_inventory,
        umlaut_assets,
        conversion_errors,
        structure_issues,
    })
}

pub type ApCache = HashMap<PathBuf, HashSet<String>>;

pub(crate) fn validate_route_structure(
    route_path: &Path,
    assets_path: &Path,
    files: &[PathBuf],
) -> Vec<String> {
    let mut issues = Vec::new();
    if !route_path.join("RouteProperties.xml").is_file() {
        issues.push("не найден RouteProperties.xml".to_owned());
    }
    if !assets_path.is_dir() {
        issues.push("не найдена папка Assets".to_owned());
    }
    if files.is_empty() {
        issues.push("не найдено файлов .bin или .xml для проверки".to_owned());
    }
    if let Ok(directories) = unexpected_route_directories(route_path)
        && !directories.is_empty()
    {
        issues.push(format!(
            "неизвестные папки маршрута не проверялись: {}",
            directories.join(", ")
        ));
    }
    if let Ok(leftover) = leftover_temporary_work_directories(route_path)
        && !leftover.is_empty()
    {
        issues.push(format!(
            "в маршруте остались рабочие каталоги программы (можно удалить): {}",
            leftover.join(", ")
        ));
    }
    issues
}

pub(crate) fn scan_product_ap_archives(
    product_dirs: &HashSet<PathBuf>,
    pool: &rayon::ThreadPool,
) -> Result<(ApCache, Vec<String>), String> {
    let dirs: Vec<PathBuf> = product_dirs.iter().cloned().collect();

    let results: Result<Vec<(PathBuf, HashSet<String>, Vec<String>)>, String> =
        pool.install(|| {
            dirs.par_iter()
                .map(|product_dir| {
                    let mut files = HashSet::new();
                    let mut archive_errors = Vec::new();
                    match fs::read_dir(product_dir) {
                        Ok(entries) => {
                            for entry in entries {
                                let archive_path = match entry {
                                    Ok(entry) => entry.path(),
                                    Err(_) => continue,
                                };
                                if !archive_path
                                    .extension()
                                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
                                {
                                    continue;
                                }
                                let archive_file = match fs::File::open(&archive_path) {
                                    Ok(file) => file,
                                    Err(error) => {
                                        archive_errors.push(format!(
                                            "Не удалось открыть архив {}: {error}",
                                            archive_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                let archive = match ZipArchive::new(archive_file) {
                                    Ok(archive) => archive,
                                    Err(error) => {
                                        archive_errors.push(format!(
                                            "Не удалось прочитать архив {}: {error}",
                                            archive_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                for name in archive.file_names().flatten() {
                                    files.insert(name.replace('/', "\\").trim().to_lowercase());
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!(
                                "Не удалось прочитать {}: {error}",
                                product_dir.display()
                            ));
                        }
                    }
                    Ok((product_dir.clone(), files, archive_errors))
                })
                .collect()
        });

    let results = results?;
    let mut ap_cache = HashMap::new();
    let mut archive_errors = Vec::new();
    for (product_dir, files, errors) in results {
        ap_cache.insert(product_dir, files);
        archive_errors.extend(errors);
    }
    archive_errors.sort();
    archive_errors.dedup();
    Ok((ap_cache, archive_errors))
}

pub(crate) fn resolve_missing_and_corrupted(
    asset_uses: HashMap<String, Vec<crate::domain::AssetUse>>,
    asset_names: &HashMap<String, String>,
    ap_cache: &ApCache,
    assets_path: &Path,
    structure_issues: &mut Vec<String>,
) -> (
    Vec<crate::domain::MissingAsset>,
    Vec<crate::domain::ReferencedAsset>,
) {
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
        let parts: Vec<&str> = relative_asset.split('/').collect();
        let in_archive = if parts.len() >= 3 {
            let product_dir = assets_path.join(parts[0]).join(parts[1]);
            let inner_path = parts[2..].join("\\").to_lowercase();
            let product_path = parts[1..].join("\\").to_lowercase();
            let full_path = relative_asset.replace('/', "\\").to_lowercase();
            let mut variants = vec![inner_path.clone(), product_path.clone(), full_path.clone()];
            for base in [inner_path, product_path, full_path] {
                variants.push(crate::translit::replace_broken_characters(&base, "ss"));
                variants.push(crate::translit::replace_broken_characters(&base, "ß"));
            }
            let mut texture_variants = Vec::new();
            for variant in &variants {
                if !variant.ends_with(".tgpcdx")
                    && !variant.ends_with(".geopcdx")
                    && !variant.ends_with(".bin")
                {
                    texture_variants.push(format!("{}.tgpcdx", variant));
                }
            }
            variants.extend(texture_variants);
            ap_cache.get(&product_dir).is_some_and(|files| {
                variants
                    .iter()
                    .any(|variant| files.contains(&variant.trim().to_lowercase()))
            })
        } else {
            false
        };
        let is_in_archive = in_archive;
        let (resolved_disk_path, suggested_repair, is_corrupted) =
            if let Some(exact_path) = full_asset_path {
                (Some(exact_path), None, false)
            } else if is_in_archive {
                (None, None, false)
            } else if let Some((fuzzy_disk_path, relative_repair)) =
                fuzzy_resolve_disk_path(assets_path, &relative_asset)
            {
                let query_corrupted = has_corrupted_or_nonascii(&relative_asset);
                let umlaut_folded_equivalent = !relative_asset.is_ascii()
                    && normalize_fuzzy_segment(&relative_asset)
                        == normalize_fuzzy_segment(&relative_repair);
                if query_corrupted || umlaut_folded_equivalent {
                    (Some(fuzzy_disk_path), Some(relative_repair), true)
                } else {
                    (None, None, false)
                }
            } else {
                (None, None, false)
            };
        let is_missing = !is_in_archive && resolved_disk_path.is_none() && !is_corrupted;
        if is_missing || is_corrupted {
            missing_assets.push(crate::domain::MissingAsset {
                path: display_path.clone(),
                uses: uses.clone(),
                suggested_repair,
                is_corrupted_link: is_corrupted,
                asset_type: classify_route_asset_type(&display_path, &uses),
            });
        }
        let path_search = display_path.to_lowercase();
        asset_inventory.push(crate::domain::ReferencedAsset {
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
