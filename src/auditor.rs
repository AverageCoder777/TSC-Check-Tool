use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetResourceKind {
    Geometry,
    Texture,
    Animation,
    Script,
    Sound,
    ChildBlueprint,
}

#[derive(Debug, Clone)]
pub struct BrokenResourceRef {
    pub parent_file: PathBuf,
    pub expected_path: PathBuf,
    pub kind: AssetResourceKind,
}

#[derive(Debug, Default)]
pub struct AuditReport {
    pub broken_references: Vec<BrokenResourceRef>,
}

fn normalize_ap_path(value: &str) -> String {
    value.replace('/', "\\").trim().to_lowercase()
}

#[derive(Default)]
pub struct ApArchiveCache {
    pub inner: Mutex<HashMap<PathBuf, HashSet<String>>>,
}

impl ApArchiveCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ensure_indexed(&self, product_dir: &Path) {
        {
            let guard = match self.inner.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            if guard.contains_key(product_dir) {
                return;
            }
        }

        let mut files = HashSet::new();
        if let Ok(entries) = fs::read_dir(product_dir) {
            for entry in entries.flatten() {
                let archive_path = entry.path();
                if !archive_path.is_file() {
                    continue;
                }
                if !archive_path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ap"))
                {
                    continue;
                }
                let archive_file = match fs::File::open(&archive_path) {
                    Ok(file) => file,
                    Err(_) => continue,
                };
                let mut archive = match zip::ZipArchive::new(archive_file) {
                    Ok(archive) => archive,
                    Err(_) => continue,
                };
                for index in 0..archive.len() {
                    let entry = match archive.by_index(index) {
                        Ok(entry) => entry,
                        Err(_) => continue,
                    };
                    if entry.is_dir() {
                        continue;
                    }
                    let raw_name = match entry.name() {
                        Ok(name) => name.to_string(),
                        Err(_) => continue,
                    };
                    let name = raw_name.replace('/', "\\").trim().to_lowercase();
                    if name.is_empty() {
                        continue;
                    }
                    files.insert(name.clone());
                    let parts: Vec<&str> =
                        name.split('\\').filter(|part| !part.is_empty()).collect();
                    if parts.len() >= 3 {
                        files.insert(parts[2..].join("\\"));
                        files.insert(parts[1..].join("\\"));
                    } else if parts.len() == 2 {
                        files.insert(parts[1..].join("\\"));
                    }
                }
            }
        }

        if let Ok(mut guard) = self.inner.lock() {
            guard.entry(product_dir.to_path_buf()).or_insert(files);
        }
    }

    pub fn lookup(&self, product_dir: &Path, relative: &str) -> bool {
        self.ensure_indexed(product_dir);
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let target = normalize_ap_path(relative);

        for (key, set) in guard.iter() {
            if !key
                .to_string_lossy()
                .eq_ignore_ascii_case(&product_dir.to_string_lossy())
            {
                continue;
            }
            if set.iter().any(|member| normalize_ap_path(member) == target) {
                return true;
            }
        }
        false
    }
}

fn regex_tag(tag: &str) -> Regex {
    Regex::new(&format!(r"(?is)<{tag}(?:\s[^>]*)?>(.*?)</{tag}>")).unwrap()
}

fn tag_text(xml: &str, tag: &str) -> Option<String> {
    regex_tag(tag)
        .captures(xml)
        .and_then(|c| c.get(1))
        .map(|m| unescape_xml_entities(m.as_str().trim()))
        .filter(|s| !s.is_empty())
}

#[allow(dead_code)]
fn maybe_sound(raw: &str, fallback: AssetResourceKind) -> AssetResourceKind {
    // Tag-based classification wins over path heuristics: only explicit
    // sound-container extensions may promote a reference to Sound here.
    let lower = raw.to_lowercase();
    if lower.contains(".proxybin") || lower.contains(".proxyxml") || lower.contains(".dav") {
        AssetResourceKind::Sound
    } else {
        fallback
    }
}

fn strip_tags(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    re.replace_all(s, "").to_string()
}

fn is_numeric_token(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.parse::<f64>().is_ok()
}

fn is_blacklisted_sound_tag(tag: &str) -> bool {
    let lower = tag.to_lowercase();
    matches!(
        lower.as_str(),
        "innervisualsoundradius"
            | "outervisualsoundradius"
            | "highestlevel_1ishighest"
            | "lowestlevel_10islowest"
            | "volume"
            | "pitch"
            | "mindistance"
            | "maxdistance"
            | "loop"
            | "priority"
    ) || lower.contains("editorrenderdata")
}
pub fn unescape_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

pub fn extract_bin_references(xml: &str, product_root: &Path) -> Vec<(AssetResourceKind, String)> {
    let mut out = Vec::new();
    for tag in ["GeometryID", "ShadowGeometryID", "CollisionGeometryID"] {
        for value in regex_tag(tag).captures_iter(xml).filter_map(|c| c.get(1)) {
            let raw = unescape_xml_entities(value.as_str().trim());
            if !raw.is_empty() && !is_numeric_token(&raw) {
                out.push((AssetResourceKind::Geometry, raw));
            }
        }
    }

    // ВАЖНО: AnimationID — это лишь внутренний идентификатор (тег / алиас).
    // Реальный путь к файлу анимации ВСЕГДА лежит ТОЛЬКО в *AnimName!
    // Tag-based classification: any tag ending in "AnimName" resolves to .ban.
    {
        static TAG_RE: OnceLock<Regex> = OnceLock::new();
        let re = TAG_RE.get_or_init(|| {
            Regex::new(r"(?is)<([A-Za-z0-9_]+)[^>]*>(.*?)</([A-Za-z0-9_]+)>").unwrap()
        });
        for cap in re.captures_iter(xml) {
            let tag = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let close = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            if !tag.eq_ignore_ascii_case(close) {
                continue;
            }
            if is_blacklisted_sound_tag(tag) {
                continue;
            }
            if !tag.to_lowercase().ends_with("animname")
                && !tag.to_lowercase().ends_with("animationname")
            {
                continue;
            }
            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if inner.contains('<') {
                continue;
            }
            let raw = unescape_xml_entities(strip_tags(inner).trim());
            let clean = raw.lines().next().unwrap_or("").trim().to_string();
            if !clean.is_empty() && !is_numeric_token(&clean) {
                out.push((AssetResourceKind::Animation, clean));
            }
        }
    }

    for sc in regex_tag("ScriptComponent")
        .captures_iter(xml)
        .filter_map(|c| c.get(1))
    {
        let inner = sc.as_str();
        let raw = regex_tag("Path")
            .captures(inner)
            .and_then(|c| c.get(1))
            .map(|m| unescape_xml_entities(m.as_str().trim()))
            .unwrap_or_else(|| unescape_xml_entities(strip_tags(inner).trim()));
        if !raw.is_empty() && !is_numeric_token(&raw) {
            out.push((AssetResourceKind::Script, raw));
        }
    }

    // Sound references: only from sound-related tags, never from path substrings.
    {
        static TAG_RE: OnceLock<Regex> = OnceLock::new();
        let re = TAG_RE.get_or_init(|| {
            Regex::new(r"(?is)<([A-Za-z0-9_]+)[^>]*>(.*?)</([A-Za-z0-9_]+)>").unwrap()
        });
        for cap in re.captures_iter(xml) {
            let tag = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let close = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            if !tag.eq_ignore_ascii_case(close) {
                continue;
            }
            if is_blacklisted_sound_tag(tag) {
                continue;
            }
            let lower_tag = tag.to_lowercase();
            let is_sound_tag = lower_tag.contains("sound")
                || lower_tag.contains("audio")
                || lower_tag.contains("proxy");
            if !is_sound_tag {
                continue;
            }
            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if inner.contains('<') {
                continue;
            }
            let raw = unescape_xml_entities(strip_tags(inner).trim());
            let clean = raw.lines().next().unwrap_or("").trim().to_string();
            if !clean.is_empty() && !is_numeric_token(&clean) {
                out.push((AssetResourceKind::Sound, clean));
            }
        }
    }
    // Preserve explicit handling for known sound tags already covered above.

    for block in regex_tag("iBlueprintLibrary-cAbsoluteBlueprintID")
        .captures_iter(xml)
        .filter_map(|c| c.get(1))
    {
        let inner = block.as_str();
        let mut provider = tag_text(inner, "Provider").unwrap_or_default();
        let mut product = tag_text(inner, "Product").unwrap_or_default();
        let mut blueprint = tag_text(inner, "BlueprintID").unwrap_or_default();
        if blueprint.is_empty() {
            continue;
        }
        if provider.is_empty() || product.is_empty() {
            let comps: Vec<_> = product_root.components().collect();
            let after_assets = comps
                .iter()
                .rposition(|c| c.as_os_str().eq_ignore_ascii_case("Assets"))
                .map(|idx| &comps[idx + 1..])
                .unwrap_or(&[]);
            let (prov, prod) = if after_assets.len() >= 2 {
                (
                    after_assets[0].as_os_str().to_string_lossy().to_string(),
                    after_assets[1].as_os_str().to_string_lossy().to_string(),
                )
            } else if comps.len() >= 2 {
                (
                    comps[comps.len() - 2]
                        .as_os_str()
                        .to_string_lossy()
                        .to_string(),
                    comps[comps.len() - 1]
                        .as_os_str()
                        .to_string_lossy()
                        .to_string(),
                )
            } else {
                (String::new(), String::new())
            };
            if prov.is_empty() || prod.is_empty() {
                continue;
            }
            provider = prov;
            product = prod;
        }
        if provider.is_empty() || product.is_empty() {
            continue;
        }
        blueprint = blueprint.trim().to_string();
        if blueprint.to_lowercase().ends_with(".xml") {
            blueprint = format!("{}.bin", &blueprint[..blueprint.len() - 4]);
        } else if !blueprint.to_lowercase().ends_with(".bin")
            && !blueprint.to_lowercase().ends_with(".geopcdx")
            && !blueprint.to_lowercase().ends_with(".tgpcdx")
        {
            blueprint = format!("{}.bin", blueprint);
        }
        let raw = format!("{}\\{}\\{}", provider, product, blueprint);
        if !raw.is_empty() && !is_numeric_token(&blueprint) {
            out.push((AssetResourceKind::ChildBlueprint, raw));
        }
    }
    out
}

pub fn strip_shader_prefix(raw: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\[[0-9a-zA-Z_\-]+\]").unwrap());
    if let Some(idx) = raw.rfind(['\\', '/']) {
        let (head, tail) = raw.split_at(idx + 1);
        format!("{}{}", head, re.replace(tail, ""))
    } else {
        re.replace(raw, "").to_string()
    }
}

fn is_valid_texture_path(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.contains('<')
        && !raw.contains('>')
        && !raw.contains('\n')
        && !raw.contains('\r')
        && !raw.contains('\t')
}

fn has_valid_texture_stem(raw: &str) -> bool {
    let file = raw.rsplit(['\\', '/']).next().unwrap_or(raw);
    let stripped = strip_shader_prefix(file);
    let stem = stripped.trim();
    let stem = match stem.rfind('.') {
        Some(i) => &stem[..i],
        None => stem,
    };
    let stem = stem.trim();
    if stem.is_empty() {
        return false;
    }
    !stem
        .chars()
        .all(|c| c.is_whitespace() || c.is_ascii_digit() || c == '[' || c == ']')
}

fn material_has_valid_texture(inner: &str) -> bool {
    for cap in regex_tag("TextureByName").captures_iter(inner) {
        let block_inner = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for ecap in regex_tag("e").captures_iter(block_inner) {
            let raw = unescape_xml_entities(ecap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(raw.as_str()) && has_valid_texture_stem(raw.as_str()) {
                return true;
            }
        }
        let raw = unescape_xml_entities(block_inner.trim());
        if !raw.contains('<')
            && is_valid_texture_path(raw.as_str())
            && has_valid_texture_stem(raw.as_str())
        {
            return true;
        }
    }
    for tag in ["TextureName", "Texture"] {
        for cap in regex_tag(tag).captures_iter(inner) {
            let raw = unescape_xml_entities(cap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(raw.as_str()) && has_valid_texture_stem(raw.as_str()) {
                return true;
            }
        }
    }
    false
}

fn filter_material_blocks(xml: &str) -> String {
    let mut result = xml.to_string();
    for tag in ["cHcExplicitStateMaterial", "cEffectMaterialDx"] {
        let re = regex_tag(tag);
        result = re
            .replace_all(&result, |caps: &regex::Captures| {
                let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let shader = tag_text(inner, "ShaderName").unwrap_or_default();
                if shader.eq_ignore_ascii_case("Invisible") {
                    return String::new();
                }
                let visible = tag_text(inner, "Visible").unwrap_or_default();
                let visible_false =
                    visible.eq_ignore_ascii_case("0") || visible.eq_ignore_ascii_case("false");
                if visible_false && !material_has_valid_texture(inner) {
                    return String::new();
                }
                full.to_string()
            })
            .to_string();
    }
    result
}

pub fn extract_geo_texture_references(xml: &str) -> Vec<String> {
    let filtered = filter_material_blocks(xml);
    let mut out = Vec::new();

    for cap in regex_tag("TextureByName").captures_iter(&filtered) {
        let inner = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for ecap in regex_tag("e").captures_iter(inner) {
            let raw = unescape_xml_entities(ecap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(raw.as_str()) && has_valid_texture_stem(raw.as_str()) {
                out.push(normalize_texture_reference(raw.as_str()));
            }
        }
        let raw = unescape_xml_entities(inner.trim());
        if !raw.contains('<')
            && is_valid_texture_path(raw.as_str())
            && has_valid_texture_stem(raw.as_str())
        {
            out.push(normalize_texture_reference(raw.as_str()));
        }
    }

    for tag in ["TextureName", "Texture"] {
        for cap in regex_tag(tag).captures_iter(&filtered) {
            let raw = unescape_xml_entities(cap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(raw.as_str()) && has_valid_texture_stem(raw.as_str()) {
                out.push(normalize_texture_reference(raw.as_str()));
            }
        }
    }

    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

pub fn normalize_texture_reference(raw: &str) -> String {
    let stripped = strip_shader_prefix(raw);
    let lower = stripped.to_lowercase();
    if lower.ends_with(".tgpcdx") {
        stripped
    } else {
        format!("{}.TgPcDx", stripped)
    }
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(std::path::Component::Normal(_)) = out.last() {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    out.iter().collect()
}

fn product_root_for(file: &Path, assets_root: &Path) -> PathBuf {
    if let Ok(rel) = file.strip_prefix(assets_root) {
        let comps: Vec<_> = rel.components().collect();
        if comps.len() >= 2 {
            return assets_root.join(comps[0]).join(comps[1]);
        }
    }
    file.parent().unwrap_or(assets_root).to_path_buf()
}

fn ensure_extension(raw: &str, kind: AssetResourceKind) -> String {
    let lower = raw.to_lowercase();
    let has_ext = lower.ends_with(".geopcdx")
        || lower.ends_with(".tgpcdx")
        || lower.ends_with(".bin")
        || lower.ends_with(".xsec")
        || lower.ends_with(".lua")
        || lower.ends_with(".out")
        || lower.ends_with(".outz")
        || lower.ends_with(".ia")
        || lower.ends_with(".ban")
        || lower.ends_with(".xml")
        || lower.ends_with(".dav")
        || lower.ends_with(".proxybin")
        || lower.ends_with(".proxyxml");
    if has_ext {
        raw.to_string()
    } else {
        match kind {
            AssetResourceKind::Texture => format!("{}.TgPcDx", raw),
            AssetResourceKind::Geometry => format!("{}.GeoPcDx", raw),
            AssetResourceKind::Animation => format!("{}.ban", raw),
            AssetResourceKind::ChildBlueprint => {
                if raw.to_lowercase().ends_with(".xml") {
                    raw.to_string()
                } else {
                    format!("{}.bin", raw)
                }
            }
            _ => raw.to_string(),
        }
    }
}

fn candidate_relatives(raw: &str, kind: AssetResourceKind) -> Vec<String> {
    let cleaned = strip_shader_prefix(raw).replace('/', "\\");
    match kind {
        AssetResourceKind::Script => {
            let lower = cleaned.to_lowercase();
            let stripped =
                if lower.ends_with(".lua") || lower.ends_with(".out") || lower.ends_with(".outz") {
                    match cleaned.rfind('.') {
                        Some(i) => cleaned[..i].to_string(),
                        None => cleaned.clone(),
                    }
                } else {
                    cleaned
                };
            vec![format!("{}.out", stripped), format!("{}.lua", stripped)]
        }
        AssetResourceKind::Texture => {
            let with_ext = ensure_extension(&cleaned, kind);
            let mut v = vec![with_ext.clone()];
            if !with_ext.to_lowercase().starts_with("textures\\") {
                v.push(format!("textures\\{}", with_ext));
            }
            v
        }
        AssetResourceKind::Animation => {
            let lower = cleaned.to_lowercase();
            let stripped = if lower.ends_with(".ia") || lower.ends_with(".ban") {
                match cleaned.rfind('.') {
                    Some(i) => cleaned[..i].to_string(),
                    None => cleaned.clone(),
                }
            } else {
                cleaned.clone()
            };
            vec![
                format!("{}.ban", stripped),
                format!("{}.ia", stripped),
                cleaned,
            ]
        }
        AssetResourceKind::ChildBlueprint => {
            let lower = cleaned.to_lowercase();
            if lower.ends_with(".bin")
                || lower.ends_with(".xml")
                || lower.ends_with(".geopcdx")
                || lower.ends_with(".tgpcdx")
            {
                vec![cleaned]
            } else {
                vec![format!("{}.bin", cleaned), format!("{}.xml", cleaned)]
            }
        }
        AssetResourceKind::Sound => {
            let lower = cleaned.to_lowercase();
            if lower.ends_with(".proxybin")
                || lower.ends_with(".proxyxml")
                || lower.ends_with(".bin")
                || lower.ends_with(".xml")
                || lower.ends_with(".dav")
            {
                vec![cleaned]
            } else {
                vec![
                    format!("{}.proxybin", cleaned),
                    format!("{}.proxyxml", cleaned),
                    format!("{}.bin", cleaned),
                    format!("{}.xml", cleaned),
                    format!("{}.dav", cleaned),
                ]
            }
        }
        _ => vec![ensure_extension(&cleaned, kind)],
    }
}

/// Определяет, является ли ссылка глобальной (от корня Assets)
/// Определяет, является ли ссылка глобальной (от корня Assets)
fn is_global_asset_ref(norm: &str, kind: AssetResourceKind, assets_root: &Path) -> bool {
    // Текстуры в GeoPcDx НИКОГДА не бывают глобальными от корня Assets
    if kind == AssetResourceKind::Texture {
        return false;
    }

    let clean = norm.trim_start_matches('/').replace('\\', "/");
    let lower = clean.to_lowercase();

    // Относительные переходы never global
    if lower.starts_with('.') || lower.starts_with("..") {
        return false;
    }
    if lower.starts_with("assets/") {
        return true;
    }

    let parts: Vec<&str> = lower.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        let p1 = parts[0];
        // Защита от .. и .
        if p1 == ".." || p1 == "." {
            return false;
        }

        const KNOWN_PROVIDERS: &[&str] = &[
            "kuju",
            "dtg",
            "rsc",
            "3dzug",
            "virtualtracks",
            "justtrains",
            "rsslo",
            "railstudio",
            "shg",
            "vizzart",
            "tsg",
            "rw-0381",
            "ted",
            "kal000px",
        ];
        if KNOWN_PROVIDERS.contains(&p1) {
            return true;
        }

        // Проверяем реальную папку провайдера внутри Assets
        let prov_dir = assets_root.join(p1);
        if prov_dir.is_dir() && !p1.starts_with('.') {
            return true;
        }
    }
    false
}

fn candidate_paths(
    raw: &str,
    parent: &Path,
    product_root: &Path,
    assets_root: &Path,
    kind: AssetResourceKind,
) -> Vec<PathBuf> {
    let mut norm = raw.replace('\\', "/");
    if norm.to_lowercase().starts_with("assets/") {
        norm = norm["assets/".len()..].to_string();
    }
    let norm = strip_shader_prefix(&norm).replace('/', "\\");
    let rels = candidate_relatives(&norm, kind);

    let mut cands = Vec::new();
    if kind == AssetResourceKind::Texture {
        for rel in &rels {
            let rel_norm = rel.replace('/', "\\");
            let first = rel_norm
                .split(['\\', '/'])
                .find(|s| !s.is_empty())
                .unwrap_or("");
            let assets_first =
                !first.is_empty() && !first.starts_with('.') && assets_root.join(first).is_dir();
            if assets_first {
                cands.push(normalize_path(&assets_root.join(&rel_norm)));
                cands.push(normalize_path(&parent.join(&rel_norm)));
                cands.push(normalize_path(&product_root.join(&rel_norm)));
            } else {
                cands.push(normalize_path(&parent.join(&rel_norm)));
                cands.push(normalize_path(&product_root.join(&rel_norm)));
                cands.push(normalize_path(&assets_root.join(&rel_norm)));
            }
        }
    } else {
        for rel in &rels {
            let rel_norm = rel.replace('/', "\\");
            let is_relative = rel_norm.starts_with(".\\")
                || rel_norm.starts_with("..\\")
                || rel_norm.starts_with("./")
                || rel_norm.starts_with("../");
            let first_component = rel_norm
                .split(['\\', '/'])
                .find(|s| !s.is_empty())
                .unwrap_or("");
            let provider_under_assets = !first_component.is_empty()
                && !first_component.starts_with('.')
                && assets_root.join(first_component).is_dir();
            let assets_candidate_exists = assets_root.join(&rel_norm).exists();
            let local_subfolder = matches!(
                first_component.to_lowercase().as_str(),
                "cab"
                    | "cabs"
                    | "textures"
                    | "texture"
                    | "audio"
                    | "sound"
                    | "sounds"
                    | "scripts"
                    | "script"
                    | "anim"
                    | "anims"
                    | "animation"
                    | "animations"
            );

            if is_relative {
                cands.push(normalize_path(&parent.join(&rel_norm)));
                cands.push(normalize_path(&product_root.join(&rel_norm)));
            } else if provider_under_assets {
                cands.push(normalize_path(&assets_root.join(&rel_norm)));
            } else if local_subfolder || !assets_candidate_exists {
                cands.push(normalize_path(&parent.join(&rel_norm)));
                cands.push(normalize_path(&product_root.join(&rel_norm)));
                cands.push(normalize_path(&assets_root.join(&rel_norm)));
            } else {
                cands.push(normalize_path(&assets_root.join(&rel_norm)));
                cands.push(normalize_path(&parent.join(&rel_norm)));
                cands.push(normalize_path(&product_root.join(&rel_norm)));
            }
        }
    }

    let mut seen = HashSet::new();
    cands.retain(|p| seen.insert(p.clone()));
    cands
}

fn loose_path_exists(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    // Slot prefixes like [0d] may differ between reference and disk; scan the
    // same directory for a file whose prefix-stripped name matches.
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let target = strip_shader_prefix(&name.to_string_lossy()).to_lowercase();
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_file() {
                continue;
            }
            let entry_name = entry.file_name();
            if strip_shader_prefix(&entry_name.to_string_lossy())
                .to_lowercase()
                .eq_ignore_ascii_case(&target)
            {
                return true;
            }
        }
    }
    false
}

fn loose_exists(candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|p| loose_path_exists(p))
}

fn ap_member_matches(product_dir: &Path, relative: &str, ap_cache: &ApArchiveCache) -> bool {
    let target = normalize_ap_path(&strip_shader_prefix(relative));
    let target = target.trim_start_matches('\\').to_string();
    if let Ok(guard) = ap_cache.inner.lock() {
        for (key, set) in guard.iter() {
            if !key
                .to_string_lossy()
                .eq_ignore_ascii_case(&product_dir.to_string_lossy())
            {
                continue;
            }
            for member in set {
                let member = normalize_ap_path(&strip_shader_prefix(member));
                let member = member.trim_start_matches('\\').to_string();
                if member == target {
                    return true;
                }
            }
        }
    }
    false
}

fn ap_exists(
    raw: &str,
    parent: &Path,
    product_root: &Path,
    assets_root: &Path,
    ap_cache: &ApArchiveCache,
    kind: AssetResourceKind,
) -> bool {
    let mut norm = raw.replace('\\', "/");
    if norm.to_lowercase().starts_with("assets/") {
        norm = norm["assets/".len()..].to_string();
    }
    let norm = strip_shader_prefix(&norm).replace('/', "\\");
    let mut is_global = is_global_asset_ref(&norm, kind, assets_root);
    if kind == AssetResourceKind::Texture {
        let first = norm
            .split(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or("");
        if !is_global
            && !first.is_empty()
            && !first.starts_with('.')
            && assets_root.join(first).is_dir()
        {
            is_global = true;
        }
    } else {
        let first = norm
            .split(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or("");
        if !is_global
            && !first.is_empty()
            && !first.starts_with('.')
            && assets_root.join(first).is_dir()
        {
            is_global = true;
        }
    }

    for rel in candidate_relatives(&norm, kind) {
        let cleaned = rel.replace('/', "\\");

        if is_global {
            let parts: Vec<&str> = cleaned.split('\\').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 3 {
                let product_dir = assets_root.join(parts[0]).join(parts[1]);
                let inner = parts[2..].join("/");
                if ap_cache.lookup(&product_dir, &inner)
                    || ap_member_matches(&product_dir, &inner, ap_cache)
                {
                    return true;
                }
            }
        } else {
            let target_loose = normalize_path(&parent.join(&cleaned));
            if let Ok(rel_from_prod) = target_loose.strip_prefix(product_root) {
                let inner = rel_from_prod.to_string_lossy().replace('\\', "/");
                if ap_cache.lookup(product_root, &inner)
                    || ap_member_matches(product_root, &inner, ap_cache)
                {
                    return true;
                }
            }
            let inner_direct = cleaned.trim_start_matches(".\\").replace('\\', "/");
            if ap_cache.lookup(product_root, &inner_direct)
                || ap_member_matches(product_root, &inner_direct, ap_cache)
            {
                return true;
            }
        }
    }
    false
}

fn audit_one_file(
    file: &Path,
    folder: &Path,
    assets_root: &Path,
    serz_path: &Path,
    ap_cache: &ApArchiveCache,
) -> Vec<BrokenResourceRef> {
    let mut out = Vec::new();
    let xml = match crate::assets::read_serz_xml(serz_path, file) {
        Ok(x) => x,
        Err(_) => match fs::read(file) {
            Ok(bytes) if bytes.starts_with(b"<?xm") => String::from_utf8_lossy(&bytes).to_string(),
            _ => return out,
        },
    };

    let parent = file.parent().unwrap_or(folder);
    let product_root = product_root_for(file, assets_root);

    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "bin" {
        for (kind, raw) in extract_bin_references(&xml, &product_root) {
            let cands = candidate_paths(&raw, parent, &product_root, assets_root, kind);
            let expected = cands
                .first()
                .cloned()
                .unwrap_or_else(|| assets_root.join(&raw));
            let exists = loose_exists(&cands)
                || ap_exists(&raw, parent, &product_root, assets_root, ap_cache, kind);
            if !exists {
                out.push(BrokenResourceRef {
                    parent_file: file.to_path_buf(),
                    expected_path: expected,
                    kind,
                });
            }
        }
    } else if ext == "geopcdx" {
        for raw in extract_geo_texture_references(&xml) {
            let cands = candidate_paths(
                &raw,
                parent,
                &product_root,
                assets_root,
                AssetResourceKind::Texture,
            );
            let expected = cands.first().cloned().unwrap_or_else(|| parent.join(&raw));
            let exists = loose_exists(&cands)
                || ap_exists(
                    &raw,
                    parent,
                    &product_root,
                    assets_root,
                    ap_cache,
                    AssetResourceKind::Texture,
                );
            if !exists {
                out.push(BrokenResourceRef {
                    parent_file: file.to_path_buf(),
                    expected_path: expected,
                    kind: AssetResourceKind::Texture,
                });
            }
        }
    }

    out
}

pub fn audit_folder_with_progress(
    folder_path: &Path,
    assets_root: &Path,
    serz_path: &Path,
    max_concurrent: usize,
    ap_cache: &ApArchiveCache,
    progress: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> AuditReport {
    let mut files = Vec::new();
    if let Ok(entries) = walkdir::WalkDir::new(folder_path)
        .into_iter()
        .filter_entry(|e| {
            !e.path().components().any(|c| {
                c.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("preload")
            })
        })
        .collect::<Result<Vec<_>, _>>()
    {
        for entry in entries {
            let path = entry.path();
            if path.is_file() {
                let is_preload = path.components().any(|c| {
                    c.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case("preload")
                });
                if is_preload {
                    continue;
                }
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "bin" || ext == "geopcdx" {
                    files.push(path.to_path_buf());
                }
            }
        }
    } else {
        collect_files(folder_path, &mut files);
    }

    let threads = max_concurrent.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_files = files.len();
    let progress_cb = progress.clone();

    let results: Vec<Vec<BrokenResourceRef>> = pool.install(|| {
        files
            .par_iter()
            .map(|f| {
                let r = audit_one_file(f, folder_path, assets_root, serz_path, ap_cache);
                let done = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(cb) = &progress_cb
                    && total_files > 0
                    && (done == total_files || done.is_multiple_of(10))
                {
                    cb(done, total_files);
                }
                r
            })
            .collect()
    });

    let mut broken = Vec::new();
    for r in results {
        broken.extend(r);
    }

    AuditReport {
        broken_references: broken,
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.components().any(|c| {
                c.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("preload")
            }) {
                continue;
            }
            if path.is_dir() {
                collect_files(&path, out);
            } else if path.is_file() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "bin" || ext == "geopcdx" {
                    out.push(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_shader_prefix() {
        assert_eq!(strip_shader_prefix(r"textures\[00]body"), r"textures\body");
    }

    #[test]
    fn normalizes_texture_reference() {
        assert_eq!(
            normalize_texture_reference(r"textures\[00]body"),
            r"textures\body.TgPcDx"
        );
        assert_eq!(
            normalize_texture_reference(r"textures\body.TgPcDx"),
            r"textures\body.TgPcDx"
        );
    }

    #[test]
    fn extracts_bin_refs() {
        let xml = r#"
            <GeometryID>x\[00]mesh</GeometryID>
            <ShadowGeometryID>y\[01]shadow</ShadowGeometryID>
            <AnimationID>do01_l</AnimationID>
            <AnimationName>3DZUG\3dz_ICE4\RailVehicles\Electric\mg_bremse</AnimationName>
            <ScriptComponent>scripts\foo.lua</ScriptComponent>
            <iBlueprintLibrary-cAbsoluteBlueprintID>
                <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                    <Provider>Kuju</Provider>
                    <Product>RailSimulator</Product>
                </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                <BlueprintID>child\bar.xml</BlueprintID>
            </iBlueprintLibrary-cAbsoluteBlueprintID>
        "#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Geometry && r.contains("mesh"))
        );
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Animation && r.contains("mg_bremse"))
        );
        // Убеждаемся, что псевдо-путь AnimationID "do01_l" НЕ попал в ссылки:
        assert!(!refs.iter().any(|(_, r)| r == "do01_l"));
        assert!(refs.iter().any(|(k, _)| *k == AssetResourceKind::Script));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::ChildBlueprint
                    && r.contains("Kuju")
                    && r.contains("bar.bin"))
        );
    }

    #[test]
    fn sound_classification() {
        let xml = r#"<SoundName>audio\horn.wav</SoundName>"#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(refs.iter().any(|(k, _)| *k == AssetResourceKind::Sound));
    }

    #[test]
    fn extracts_geo_textures_strict() {
        let xml = r#"
            <cHcExplicitStateMaterial>
                0.000000 0.000000
                <EmissiveAsVec>1 2 3</EmissiveAsVec>
                textures\[0d]farn.TgPcDx
            </cHcExplicitStateMaterial>
            <TextureByName>
                <e d:type="cDeltaString">textures\[0d]farn.TgPcDx</e>
            </TextureByName>
            <TextureName>..\textures\[0d]vr_eiche.TgPcDx</TextureName>
            <Texture>foo</Texture>
        "#;
        let refs = extract_geo_texture_references(xml);
        assert_eq!(
            refs,
            vec![
                r"textures\farn.TgPcDx".to_string(),
                r"..\textures\vr_eiche.TgPcDx".to_string(),
                r"foo.TgPcDx".to_string(),
            ]
        );
    }

    #[test]
    fn resolves_provider_geometry_without_duplication() {
        let temp = std::env::temp_dir().join("tsc_audit_test");
        let assets = temp.join("Assets");
        let provider = assets.join("RWAustria_2");
        let source_dir = assets
            .join("RWAustria_2")
            .join("Austria_RailVehicles")
            .join("Electric")
            .join("GYSEV_470")
            .join("GYSEV_470_501");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&provider).unwrap();
        let raw =
            r"RWAustria_2\Austria_RailVehicles\Electric\GYSEV_470\GYSEV_470_501\[00]1116_Bleich";
        let cands = candidate_paths(
            raw,
            &source_dir,
            &assets.join("RWAustria_2").join("Austria_RailVehicles"),
            &assets,
            AssetResourceKind::Geometry,
        );
        let expected = assets.join(r"RWAustria_2\Austria_RailVehicles\Electric\GYSEV_470\GYSEV_470_501\1116_Bleich.GeoPcDx");
        assert_eq!(cands.first().unwrap(), &normalize_path(&expected));
        // ensure no duplicated source_dir prefix
        for c in &cands {
            assert!(!c.to_string_lossy().contains("GYSEV_470_501\\RWAustria_2"));
        }
    }

    #[test]
    fn inherits_empty_provider_product_in_child_blueprint() {
        let xml = r#"
            <iBlueprintLibrary-cAbsoluteBlueprintID>
                <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                    <Provider d:type="cDeltaString"></Provider>
                    <Product d:type="cDeltaString"></Product>
                </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                <BlueprintID d:type="cDeltaString">child\bar.xml</BlueprintID>
            </iBlueprintLibrary-cAbsoluteBlueprintID>
        "#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(refs.iter().any(|(k, r)| {
            *k == AssetResourceKind::ChildBlueprint && r == r"Kuju\RailSimulator\child\bar.bin"
        }));
    }

    #[test]
    fn detects_collision_geometry() {
        let xml = r#"<CollisionGeometryID>x\[00]mesh</CollisionGeometryID>"#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Geometry && r.contains("mesh"))
        );
    }

    #[test]
    fn script_candidates_prefer_out_then_lua() {
        let temp = std::env::temp_dir().join("tsc_script_candidate_test");
        let assets = temp.join("Assets");
        let dir = assets.join("Kuju").join("RailSimulator");
        std::fs::create_dir_all(&dir).unwrap();
        let cands = candidate_paths(
            r"scripts\foo",
            &dir,
            &dir,
            &assets,
            AssetResourceKind::Script,
        );
        assert_eq!(
            cands.first().unwrap(),
            &normalize_path(&dir.join(r"scripts\foo.out"))
        );
        assert!(
            cands
                .iter()
                .any(|p| p == &normalize_path(&dir.join(r"scripts\foo.lua")))
        );
    }

    #[test]
    fn ap_cache_lookup_is_case_insensitive() {
        let cache = ApArchiveCache::new();
        let product_dir = PathBuf::from("Assets/Kuju/RailSimulator");
        cache.inner.lock().unwrap().insert(
            product_dir.clone(),
            ["Textures\\Foo.TgPcDx".to_string()].into(),
        );
        assert!(cache.lookup(&product_dir, "textures\\foo.tgpcdx"));
    }

    #[test]
    fn filters_invisible_and_bare_slots() {
        let xml = r#"
            <cHcExplicitStateMaterial>
                <ShaderName d:type="cDeltaString">Invisible</ShaderName>
                <TextureByName>
                    <e d:type="cDeltaString">[00]</e>
                </TextureByName>
                <Visible d:type="bool">0</Visible>
            </cHcExplicitStateMaterial>
            <TextureByName>
                <e d:type="cDeltaString">textures\[00]wall</e>
                <e d:type="cDeltaString">[00]</e>
            </TextureByName>
        "#;
        let refs = extract_geo_texture_references(xml);
        assert_eq!(refs, vec![r"textures\wall.TgPcDx".to_string()]);
    }
}
