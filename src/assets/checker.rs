use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use walkdir::WalkDir;

pub struct FileAnalysis {
    /// Разбор взят из кэша: файл не менялся с прошлой проверки.
    from_cache: bool,
    file: crate::domain::RouteFile,
    /// Сколько раз каждый ассет встречается в файле.
    uses: Vec<(String, u32, crate::domain::AssetKind)>,
    error: Option<String>,
}

pub fn analyze_route_file(
    route_file: &crate::domain::RouteFile,
    index: usize,
    temp_dir: &Path,
    serz_path: &Path,
) -> FileAnalysis {
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
    let xml_data = if is_binary {
        if fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            < 256
        {
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
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: None,
            };
        }
        let xml_path = temp_dir.join(format!("{index:08}.xml"));
let conversion_result = SerzTool::new(serz_path).bin_to_xml(&file_path, &xml_path);
        if let Err(error) = conversion_result {
            return FileAnalysis {
                from_cache: false,
                file: route_file.clone(),
                uses: Vec::new(),
                error: Some(format!("{}: {error}", file_path.display())),
            };
        }
        match fs::read(xml_path) {
            Ok(data) => data,
            Err(error) => {
                return FileAnalysis {
                    from_cache: false,
                    file: route_file.clone(),
                    uses: Vec::new(),
                    error: Some(format!(
                        "{}: не удалось прочитать результат Serz: {error}",
                        file_path.display()
                    )),
                };
            }
        }
    } else {
        match fs::read(&file_path) {
            Ok(data) => data,
            Err(error) => {
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
        }
    };

    let xml_text = decode_xml_bytes(&xml_data);

    let references = crate::reference::extract_asset_references_with_locations(&xml_text);

    FileAnalysis {
        from_cache: false,
        file: route_file.clone(),
        uses: count_asset_uses(references),
        error: None,
    }
}

/// Считает, сколько раз каждый ассет встречается в файле.
///
/// Порядок первого появления сохраняется: по нему собирается список
/// использований в инвентаре. Вместе с ассетом запоминается, что он из себя
/// представляет: объект маршрута или файл состава.
pub fn count_asset_uses(
    references: Vec<crate::reference::AssetReference>,
) -> Vec<(String, u32, crate::domain::AssetKind)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut kinds: HashMap<String, crate::domain::AssetKind> = HashMap::new();
    for reference in references {
        let counter = counts.entry(reference.path.clone()).or_insert_with(|| {
            order.push(reference.path.clone());
            kinds.insert(reference.path.clone(), reference.kind);
            0
        });
        *counter += 1;
    }
    order
        .into_iter()
        .map(|path| {
            let count = counts.get(&path).copied().unwrap_or_default();
            let kind = kinds
                .get(&path)
                .copied()
                .unwrap_or(crate::domain::AssetKind::Other);
            (path, count, kind)
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
    let Ok(data) = fs::read(file_path) else {
        return true;
    };
    data.windows(crate::translit::BLUEPRINT_ID_MARKER.len())
        .any(|window| window == crate::translit::BLUEPRINT_ID_MARKER.as_bytes())
}

/// Ставит изменённый файл маршрута на место.
///
/// Для файла из архива .ap копия кладётся рядом с архивом: игра читает сначала
/// архив, а потом файлы рядом с ним, поэтому изменённая версия перекроет
/// оригинальную, а сам архив останется нетронутым.
pub fn install_route_file(
    route_path: &Path,
    source_file: &crate::domain::RouteFile,
    converted_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let target = source_file.install_target(route_path);
    // У записи из архива оригинала на диске нет: резервная копия не нужна, а
    // изменённая версия просто кладётся рядом с архивом.
    let original = match source_file {
        crate::domain::RouteFile::Path(path) => Some(path.clone()),
        crate::domain::RouteFile::Archive { .. } => None,
    };
    if let Some(original) = original
        && target == original
    {
        return install_converted_asset(&original, converted_path).map(Some);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Не удалось создать папку {}: {error}", parent.display()))?;
    }
    fs::copy(converted_path, &target)
        .map_err(|error| format!("Не удалось записать {}: {error}", target.display()))?;
    Ok(None)
}

pub(crate) fn check_route<F>(
    route_path: &Path,
    assets_path: &Path,
    serz_path: &Path,
    ignore_scenarios: bool,
    max_concurrent_serz: usize,
    report_progress: F,
) -> Result<crate::domain::CheckResult, String>
where
    F: Fn(&str) + Sync,
{
    if !serz_path.is_file() {
        return Err(format!(
            "Не найден serz.exe: {}. Выберите корневую папку RailWorks.",
            serz_path.display()
        ));
    }

    report_progress("Сканирование файлов маршрута...");
    let collected_route_files = collect_route_files(route_path, ignore_scenarios)?;

    let mut structure_issues = Vec::new();
    if !route_path.join("RouteProperties.xml").is_file() {
        structure_issues.push("не найден RouteProperties.xml".to_owned());
    }
    if !assets_path.is_dir() {
        structure_issues.push("не найдена папка Assets".to_owned());
    }
    if collected_route_files.is_empty() {
        structure_issues.push("не найдено файлов .bin или .xml для проверки".to_owned());
    }
    let unexpected_directories = unexpected_route_directories(route_path)?;
    if !unexpected_directories.is_empty() {
        structure_issues.push(format!(
            "неизвестные папки маршрута не проверялись: {}",
            unexpected_directories.join(", ")
        ));
    }
    let leftover_work_directories = leftover_temporary_work_directories(route_path)?;
    if !leftover_work_directories.is_empty() {
        structure_issues.push(format!(
            "в маршруте остались рабочие каталоги программы (можно удалить): {}",
            leftover_work_directories.join(", ")
        ));
    }

    // Основные файлы маршрута могут быть упакованы в .ap...
    let mut route_files: Vec<crate::domain::RouteFile> = collected_route_files
        .into_iter()
        .map(crate::domain::RouteFile::Path)
        .collect();
    let outside: Vec<String> = route_files
        .iter()
        .filter_map(|file| file.route_name(route_path))
        .collect();
    for member in collect_archive_members(route_path)? {
        let name = member.route_name(route_path).unwrap_or_default();
        if outside
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
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
    route_files.sort_by_key(|a| is_tracks_bin(a));
    let normal_files: Vec<crate::domain::RouteFile> = route_files
        .iter()
        .filter(|file| !is_tracks_bin(file))
        .cloned()
        .collect();
    let heavy_files: Vec<crate::domain::RouteFile> = route_files
        .iter()
        .filter(|file| is_tracks_bin(file))
        .cloned()
        .collect();
    // LPT: тяжёлые файлы (Tracks.bin) ставим первыми, чтобы пул не простаивал.
    let mut all_files = heavy_files;
    all_files.extend(normal_files.iter().cloned());

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

    let mut analyses = Vec::with_capacity(route_files.len());
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
                .map(|(offset, route_file)| {
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
                    analyze_route_file(route_file, start_index + offset, temp_dir.path(), serz_path)
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
        for (asset, count, kind) in analysis.uses {
            // Игра читает пути без учёта регистра, поэтому ссылки, отличающиеся
            // регистром, относятся к одному ассету.
            let key = asset_key(&asset);
            asset_names
                .entry(key.clone())
                .or_insert_with(|| asset.clone());
            let uses = asset_uses.entry(key).or_default();
            for _ in 0..count {
                uses.push(crate::domain::AssetUse {
                    source_file: analysis.file.clone(),
                    tile: tile.clone(),
                    tile_kind: analysis.file.tile_kind(),
                    kind,
                    tile_search: tile.to_lowercase(),
                    source_file_search: analysis.file.display().to_lowercase(),
                });
            }
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
    let current_files: HashMap<String, ()> = route_files
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

    let archive_results: Result<Vec<(PathBuf, HashSet<String>, Vec<String>)>, String> = pool
        .install(|| {
            product_dirs
                .par_iter()
                .map(|product_dir| {
                    let mut files = HashSet::new();
                    let mut errors = Vec::new();
                    match fs::read_dir(product_dir) {
                        Ok(entries) => {
                            for entry in entries {
                                let archive_path = match entry {
                                    Ok(entry) => entry.path(),
                                    Err(error) => {
                                        errors.push(format!(
                                            "Ошибка при чтении {}: {error}",
                                            product_dir.display()
                                        ));
                                        continue;
                                    }
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
                                        errors.push(format!(
                                            "Не удалось открыть {}: {error}",
                                            archive_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                let archive = match ZipArchive::new(archive_file) {
                                    Ok(archive) => archive,
                                    Err(error) => {
                                        errors.push(format!(
                                            "Не удалось прочитать архив ассетов {}: {error}",
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
                    Ok((product_dir.clone(), files, errors))
                })
                .collect()
        });
    let archive_results = archive_results?;
    for (product_dir, files, errors) in archive_results {
        structure_issues.extend(errors);
        ap_cache.insert(product_dir, files);
    }

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
        scan_umlaut_assets(&inventory_seed, assets_path, serz_path, max_concurrent_serz)?;
    conversion_errors.extend(umlaut_errors);
    conversion_errors.sort();

    // Проверки подключённых в RouteProperties.xml продуктов здесь нет и не
    // должно быть: новый провайдер с продуктом добавляют как раз для замены
    // ассетов — в кастомном бинарнике ссылки ведут на геометрию из другой
    // папки, и её продукт подключают к маршруту, даже если прямых ссылок на
    // него в файлах маршрута нет.

    let mut asset_inventory = Vec::with_capacity(asset_uses.len());
    let mut missing_assets = Vec::new();
    for (relative_asset, uses) in asset_uses {
        // Файлы составов (поездов) при проверке маршрута не учитываются: игра
        // ссылается на них отдельно от объектов маршрута, а проверять их нужно
        // вместе со сценариями.
        if uses.iter().any(|usage| usage.kind.is_vehicle()) {
            continue;
        }
        // Показываем путь так, как он записан в маршруте, но ищем файл без
        // учёта регистра: игра регистр не различает.
        let display_path = asset_names
            .get(&relative_asset)
            .cloned()
            .unwrap_or_else(|| relative_asset.clone());
        let full_asset_path = match resolve_asset_path(assets_path, &relative_asset) {
            Ok(path) => path,
            Err(error) => {
                structure_issues.push(format!(
                    "Не удалось однозначно сопоставить ассет {display_path}: {error}"
                ));
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
            // Texture slot prefixes: ensure a trailing extension for image refs.
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
                // Exists inside .ap archive - perfectly valid!
                (None, None, false)
            } else if let Some((fuzzy_disk_path, relative_repair)) =
                fuzzy_resolve_disk_path(assets_path, &relative_asset)
            {
                // Fuzzy candidates are only auto-repairable corrupted/encoding variants.
                // Clean ASCII names that merely look similar are genuinely missing.
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
                // Genuinely missing
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

    Ok(crate::domain::CheckResult {
        missing_assets,
        asset_inventory,
        umlaut_assets,
        conversion_errors,
        structure_issues,
    })
}

