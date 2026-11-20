//! Разрешение путей блюпринтов подвижного состава на диске.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[derive(Clone, PartialEq, Eq, Hash)]
struct BlueprintCacheKey {
    rw_path: PathBuf,
    route_path: PathBuf,
    blueprint: String,
    provider: String,
    product: String,
}

thread_local! {
    static BLUEPRINT_CACHE: RefCell<HashMap<BlueprintCacheKey, Option<PathBuf>>> =
        RefCell::new(HashMap::new());
}

/// Кэш оглавления архивов .ap для мгновенного поиска за O(1)
static AP_ARCHIVE_FILES: LazyLock<Mutex<HashMap<PathBuf, HashSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static AP_ARCHIVE_ENTRIES: LazyLock<Mutex<HashMap<PathBuf, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[inline]
fn normalize_archive_member(path: &str) -> String {
    path.replace('/', "\\").trim().to_ascii_lowercase()
}

/// Загружает и кэширует список записей архива .ap.
pub(crate) fn ap_archive_entries(path: &Path) -> Vec<String> {
    if let Ok(cache) = AP_ARCHIVE_ENTRIES.lock()
        && let Some(cached) = cache.get(path)
    {
        return cached.clone();
    }

    let mut entries = Vec::new();
    let mut file_set = HashSet::new();

    if let Ok(file) = fs::File::open(path)
        && let Ok(zip) = zip::ZipArchive::new(file)
    {
        for name_result in zip.file_names() {
            let Ok(name) = name_result else {
                continue;
            };
            let norm = normalize_archive_member(&name);
            if norm.is_empty() {
                continue;
            }

            file_set.insert(norm.clone());

            let ext = Path::new(&norm)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");

            if ext == "bin" || ext == "xml" {
                entries.push(name.into_owned());
            }
        }
    }

    entries.sort();
    entries.dedup();

    if let Ok(mut set_cache) = AP_ARCHIVE_FILES.lock() {
        set_cache.insert(path.to_path_buf(), file_set);
    }
    if let Ok(mut cache) = AP_ARCHIVE_ENTRIES.lock() {
        cache.insert(path.to_path_buf(), entries.clone());
    }

    entries
}

fn ap_archive_contains_member(archive_path: &Path, member_candidate: &str) -> bool {
    let target = normalize_archive_member(member_candidate);

    {
        if let Ok(cache) = AP_ARCHIVE_FILES.lock()
            && let Some(set) = cache.get(archive_path)
        {
            return set.contains(&target);
        }
    }

    // Если архива ещё нет в кэше — индексируем его
    let _ = ap_archive_entries(archive_path);

    if let Ok(cache) = AP_ARCHIVE_FILES.lock()
        && let Some(set) = cache.get(archive_path)
    {
        set.contains(&target)
    } else {
        false
    }
}

pub(crate) fn ap_archive_member_stems(path: &Path) -> Option<HashSet<String>> {
    let entries = ap_archive_entries(path);
    if entries.is_empty() && !path.is_file() {
        return None;
    }

    let mut seen = HashSet::with_capacity(entries.len());
    for member_name in entries {
        if let Some(stem) = Path::new(&member_name).file_stem() {
            seen.insert(stem.to_string_lossy().to_ascii_lowercase());
        }
    }
    Some(seen)
}

/// Кэшированная версия resolve_blueprint_file.
pub(crate) fn resolve_blueprint_file_cached_free(
    rw_path: &Path,
    route_path: &Path,
    blueprint: &str,
    provider: &str,
    product: &str,
) -> Option<PathBuf> {
    let key = BlueprintCacheKey {
        rw_path: rw_path.to_path_buf(),
        route_path: route_path.to_path_buf(),
        blueprint: blueprint.to_owned(),
        provider: provider.to_owned(),
        product: product.to_owned(),
    };

    BLUEPRINT_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(cached) = map.get(&key) {
            return cached.clone();
        }
        let found = resolve_blueprint_file(rw_path, route_path, blueprint, provider, product);
        map.insert(key, found.clone());
        found
    })
}

/// Строгая проверка существования блюпринта подвижного состава.
///
/// Проверяет точный путь в папке продукта на диске и в соответствующих .ap архивах
/// с оптимизированным $O(1)$ поиском в памяти.
pub(crate) fn vehicle_blueprint_exists(
    rw_path: &Path,
    provider: &str,
    product: &str,
    blueprint: &str,
) -> bool {
    let normalized = blueprint.trim().replace('/', "\\");
    if normalized.is_empty() || provider.trim().is_empty() || product.trim().is_empty() {
        return false;
    }

    let assets_path = rw_path.join("Assets");
    let product_dir = assets_path.join(provider.trim()).join(product.trim());

    let path_obj = Path::new(&normalized);
    let mut candidates = Vec::with_capacity(3);

    // В сценариях ссылки на блюпринты часто имеют расширение .xml, хотя на диске лежит .bin
    candidates.push(path_obj.with_extension("bin").to_string_lossy().to_string());
    candidates.push(path_obj.with_extension("xml").to_string_lossy().to_string());
    if !candidates.contains(&normalized) {
        candidates.push(normalized.clone());
    }

    // 1. Быстрая проверка распакованных файлов на диске
    for candidate in &candidates {
        if product_dir.join(candidate).is_file() {
            return true;
        }
    }

    // 2. Проверка в .ap архивах папки продукта по кэшу
    let Ok(entries) = fs::read_dir(&product_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let archive_path = entry.path();
        if !archive_path.is_file()
            || !archive_path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
        {
            continue;
        }

        for candidate in &candidates {
            if ap_archive_contains_member(&archive_path, candidate) {
                return true;
            }
        }
    }

    false
}

/// Разрешает ссылку на блюпринт состава в реальный файл на диске.
pub(crate) fn resolve_blueprint_file(
    rw_path: &Path,
    route_path: &Path,
    blueprint: &str,
    provider: &str,
    product: &str,
) -> Option<PathBuf> {
    let blueprint = blueprint.trim();
    if blueprint.is_empty() {
        return None;
    }

    let assets_path = rw_path.join("Assets");
    let normalized = blueprint.replace('/', "\\");
    let path_obj = Path::new(&normalized);

    let provider = provider.trim();
    let product = product.trim();
    let has_prod = !provider.is_empty() && !product.is_empty();

    let mut relatives = Vec::new();
    relatives.push(path_obj.to_path_buf());

    let orig_ext = path_obj.extension().and_then(|e| e.to_str()).unwrap_or("");
    for ext in ["bin", "GeoPcDx", "TgPcDx", "xml"] {
        if !ext.eq_ignore_ascii_case(orig_ext) {
            relatives.push(path_obj.with_extension(ext));
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    for rel in relatives {
        if has_prod {
            candidates.push(Path::new(provider).join(product).join(&rel));
        }
        candidates.push(rel);
    }

    // Приоритет проверки форматов: сначала .bin, затем геометрия и .xml
    candidates.sort_by_key(|candidate| {
        match candidate
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("bin") => 0usize,
            Some("geopcdx") => 1,
            Some("xsec") => 1,
            Some("tgpcdx") => 2,
            Some("xml") => 3,
            _ => 4,
        }
    });

    let roots = [&assets_path, route_path];
    for root in roots {
        for candidate in &candidates {
            let path = root.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    // Fallback: поиск по stem исключительно внутри каталога провайдера/продукта
    let file_stem = path_obj.file_stem()?.to_string_lossy().to_string();

    let mut search_roots: Vec<PathBuf> = Vec::new();
    if !provider.is_empty() {
        let provider_dir = assets_path.join(provider);
        if !product.is_empty() {
            let product_dir = provider_dir.join(product);
            if product_dir.is_dir() {
                search_roots.push(product_dir);
            }
        }
        if search_roots.is_empty() && provider_dir.is_dir() {
            search_roots.push(provider_dir);
        }
    }

    // Защита от зависания: никогда не сканируем всю директорию Assets целиком
    if search_roots.is_empty() {
        return None;
    }

    for root in &search_roots {
        // Ограничиваем глубину поиска для предотвращения зависаний
        if let Some(found) = walkdir::WalkDir::new(root)
            .max_depth(8)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .find(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.eq_ignore_ascii_case(&file_stem))
                    .unwrap_or(false)
            })
        {
            return Some(found.into_path());
        }

        // Проверка по архивам .ap
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file()
                    || !path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("ap"))
                {
                    continue;
                }
                if let Some(stems) = ap_archive_member_stems(&path)
                    && stems.contains(&file_stem.to_ascii_lowercase())
                {
                    return Some(path);
                }
            }
        }
    }

    None
}
