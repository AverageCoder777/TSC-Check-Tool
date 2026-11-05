use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssetResourceKind {
    Geometry,
    Texture,
    Animation,
    Script,
    Sound,
    ChildBlueprint,
    Unused,
}

impl AssetResourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Geometry => "Геометрия",
            Self::Texture => "Текстура",
            Self::Animation => "Анимация",
            Self::Script => "Скрипт",
            Self::Sound => "Звуки",
            Self::ChildBlueprint => "Дочерние объекты",
            Self::Unused => "Мёртвый ресурс",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrokenResourceRef {
    pub parent_file: PathBuf,
    pub all_parents: Vec<PathBuf>,
    pub expected_path: PathBuf,
    pub kind: AssetResourceKind,
    pub count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    pub outgoing: HashMap<PathBuf, HashSet<PathBuf>>,
    pub incoming: HashMap<PathBuf, HashSet<PathBuf>>,
}

#[derive(Debug, Clone)]
pub struct FileAuditOutcome {
    pub parent: PathBuf,
    pub broken: Vec<BrokenResourceRef>,
    pub outgoing: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct AuditReport {
    pub broken_references: Vec<BrokenResourceRef>,
    pub dependency_graph: DependencyGraph
}

/// Complete directory-wide asset dependency graph produced by an audit run.
#[derive(Debug, Clone, Default)]
pub struct FullAssetGraph {
    /// Every file found on disk during the scan (normalized, lowercase).
    pub files_on_disk: HashSet<PathBuf>,
    /// Forward dependency edges: source file -> set of files it depends on.
    pub forward_edges: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Referenced files that do not exist on disk.
    pub missing_files: HashSet<PathBuf>,
    /// Files present on disk but unused by anything (orphans).
    pub dead_files: HashSet<PathBuf>,
}

pub use crate::archive::ApArchiveCache;
pub use crate::fs_utils::normalize_path;

// Статические прекомпилированные регулярные выражения
macro_rules! static_tag_regex {
    ($name:ident, $tag:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| {
                Regex::new(&format!(
                    r"(?is)<{tag}(?:\s[^>]*)?>(.*?)</{tag}>",
                    tag = $tag
                ))
                .unwrap()
            })
        }
    };
}

static_tag_regex!(re_script_component, "ScriptComponent");
static_tag_regex!(re_path, "Path");
static_tag_regex!(re_blueprint_lib, "iBlueprintLibrary-cAbsoluteBlueprintID");
static_tag_regex!(re_provider, "Provider");
static_tag_regex!(re_product, "Product");
static_tag_regex!(re_blueprint_id, "BlueprintID");
static_tag_regex!(re_texture_by_name, "TextureByName");
static_tag_regex!(re_e_tag, "e");
static_tag_regex!(re_texture_name, "TextureName");
static_tag_regex!(re_texture, "Texture");
static_tag_regex!(re_hc_material, "cHcExplicitStateMaterial");
static_tag_regex!(re_dx_material, "cEffectMaterialDx");
static_tag_regex!(re_shader_name, "ShaderName");
static_tag_regex!(re_visible, "Visible");
static_tag_regex!(re_cross_section_id, "CrossSectionID");
static_tag_regex!(re_middle_section, "MiddleSection");
static_tag_regex!(re_start_section, "StartSection");
static_tag_regex!(re_end_section, "EndSection");
static_tag_regex!(re_cross_section, "CrossSection");

fn tag_text_cached(xml: &str, re: &Regex) -> Option<String> {
    re.captures(xml)
        .and_then(|c| c.get(1))
        .map(|m| unescape_xml_entities(m.as_str().trim()))
        .filter(|s| !s.is_empty())
}

fn strip_tags(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    re.replace_all(s, "").to_string()
}

#[inline]
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

pub fn strip_lod_and_shader_prefix(raw: &str) -> String {
    if let Some(pos) = raw.rfind(['\\', '/']) {
        let (dir, file) = raw.split_at(pos + 1);
        if file.starts_with('[')
            && let Some(end) = file.find(']')
        {
            return format!("{}{}", dir, &file[end + 1..]);
        }
    } else if raw.starts_with('[')
        && let Some(end) = raw.find(']')
    {
        return raw[end + 1..].to_string();
    }
    raw.to_string()
}

pub fn unescape_xml_entities(s: &str) -> String {
    crate::xml_scan::unescape_xml_entities(s)
}

pub fn extract_bin_references(xml: &str, product_root: &Path) -> Vec<(AssetResourceKind, String)> {
    let mut out = Vec::new();

    // Извлечение геометрий
    static RE_ANY_GEOMETRY_ID: OnceLock<Regex> = OnceLock::new();
    let re_geo = RE_ANY_GEOMETRY_ID.get_or_init(|| {
        Regex::new(r"(?is)<([a-zA-Z0-9_]*GeometryID)(?:\s[^>]*)?>(.*?)</([a-zA-Z0-9_]*GeometryID)>").unwrap()
    });
    for cap in re_geo.captures_iter(xml) {
        let open = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let close = cap.get(3).map(|m| m.as_str()).unwrap_or("");
        if !open.eq_ignore_ascii_case(close) {
            continue;
        }
        if let Some(val) = cap.get(2) {
            let raw = unescape_xml_entities(strip_tags(val.as_str()).trim());
            let clean = raw.lines().next().unwrap_or("").trim().to_string();
            if !clean.is_empty() && !is_numeric_token(&clean) {
                out.push((AssetResourceKind::Geometry, clean));
            }
        }
    }

    // Извлечение сечений (cross-section / loft) -> .xsec
    for re in [
        re_cross_section_id(),
        re_middle_section(),
        re_start_section(),
        re_end_section(),
        re_cross_section(),
    ] {
        for value in re.captures_iter(xml).filter_map(|c| c.get(1)) {
            let raw = unescape_xml_entities(strip_tags(value.as_str()).trim());
            let raw = raw.lines().next().unwrap_or("").trim().to_string();
            if raw.is_empty() || is_numeric_token(&raw) {
                continue;
            }
            let lower = raw.to_lowercase();
            let raw = if lower.contains('.') || lower.ends_with(".xsec") {
                raw
            } else {
                format!("{raw}.xsec")
            };
            out.push((AssetResourceKind::Geometry, raw));
        }
    }

    // Извлечение анимаций и звуков в один проход
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
            let is_anim = lower_tag.ends_with("animname") || lower_tag.ends_with("animationname");
            let is_sound = lower_tag.contains("sound")
                || lower_tag.contains("audio")
                || lower_tag.contains("proxy");

            if !is_anim && !is_sound {
                continue;
            }

            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if inner.contains('<') {
                continue;
            }

            let raw = unescape_xml_entities(strip_tags(inner).trim());
            let clean = raw.lines().next().unwrap_or("").trim().to_string();
            if clean.is_empty() || is_numeric_token(&clean) {
                continue;
            }

            if is_anim {
                out.push((AssetResourceKind::Animation, clean));
            } else if is_sound {
                out.push((AssetResourceKind::Sound, clean));
            }
        }
    }

    // Скрипты
    for sc in re_script_component()
        .captures_iter(xml)
        .filter_map(|c| c.get(1))
    {
        let inner = sc.as_str();
        let raw = re_path()
            .captures(inner)
            .and_then(|c| c.get(1))
            .map(|m| unescape_xml_entities(m.as_str().trim()))
            .unwrap_or_else(|| unescape_xml_entities(strip_tags(inner).trim()));
        if !raw.is_empty() && !is_numeric_token(&raw) {
            out.push((AssetResourceKind::Script, raw));
        }
    }

    // Дочерние объекты (Blueprints)
    for block in re_blueprint_lib()
        .captures_iter(xml)
        .filter_map(|c| c.get(1))
    {
        let inner = block.as_str();
        let mut provider = tag_text_cached(inner, re_provider()).unwrap_or_default();
        let mut product = tag_text_cached(inner, re_product()).unwrap_or_default();
        let mut blueprint = tag_text_cached(inner, re_blueprint_id()).unwrap_or_default();
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

        blueprint = blueprint.trim().to_string();
        if blueprint.to_lowercase().ends_with(".xml") {
            blueprint = format!("{}.bin", &blueprint[..blueprint.len() - 4]);
        } else if !blueprint.to_lowercase().ends_with(".bin")
            && !blueprint.to_lowercase().ends_with(".geopcdx")
            && !blueprint.to_lowercase().ends_with(".tgpcdx")
        {
            blueprint = format!("{blueprint}.bin");
        }

        let raw = format!("{provider}\\{product}\\{blueprint}");
        if !raw.is_empty() && !is_numeric_token(&blueprint) {
            out.push((AssetResourceKind::ChildBlueprint, raw));
        }
    }

    // Terrain textures (cMixTexDescriptor + Wang tiles)
    {
        static RE_MIX_TEX: OnceLock<Regex> = OnceLock::new();
        let re_mix_tex = RE_MIX_TEX.get_or_init(|| {
            Regex::new(r"(?s)<cMixTexDescriptor[\s>].*?</cMixTexDescriptor>").unwrap()
        });

        static RE_TERRAIN_BASE_TEX: OnceLock<Regex> = OnceLock::new();
        let re_terrain_base_tex = RE_TERRAIN_BASE_TEX.get_or_init(|| {
            Regex::new(r"<(?:BaseTextureName|BaseSpringTextureName|BaseSummerTextureName|BaseAutumnTextureName|BaseWinterTextureName)[^>]*>([^<]+)</").unwrap()
        });

        for desc in re_mix_tex.captures_iter(xml).filter_map(|c| c.get(0)) {
            let desc = desc.as_str();
            let is_wang = desc.contains(">eTrue<") || desc.contains(">true<");
            for cap in re_terrain_base_tex.captures_iter(desc) {
                if let Some(val) = cap.get(1) {
                    let raw = unescape_xml_entities(strip_tags(val.as_str()).trim());
                    if raw.is_empty() || is_numeric_token(&raw) {
                        continue;
                    }
                    if is_wang {
                        for suffix in ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'] {
                            out.push((AssetResourceKind::Texture, format!("{}_{}", raw, suffix)));
                        }
                    } else {
                        out.push((AssetResourceKind::Texture, raw));
                    }
                }
            }
        }
    }

    // Procedural flora textures
    {
        static RE_FLORA_TEX: OnceLock<Regex> = OnceLock::new();
        let re_flora = RE_FLORA_TEX
            .get_or_init(|| Regex::new(r"<ProceduralFloraTextureID[^>]*>([^<]+)</").unwrap());
        for cap in re_flora.captures_iter(xml) {
            if let Some(val) = cap.get(1) {
                let raw = unescape_xml_entities(strip_tags(val.as_str()).trim());
                if !raw.is_empty() && !is_numeric_token(&raw) {
                    out.push((AssetResourceKind::Texture, raw));
                }
            }
        }
    }

    out
}

pub use crate::fs_utils::strip_shader_prefix;

#[inline]
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
    for cap in re_texture_by_name().captures_iter(inner) {
        let block_inner = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for ecap in re_e_tag().captures_iter(block_inner) {
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
    for re in [re_texture_name(), re_texture()] {
        for cap in re.captures_iter(inner) {
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
    for re in [re_hc_material(), re_dx_material()] {
        result = re
            .replace_all(&result, |caps: &regex::Captures| {
                let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let shader = tag_text_cached(inner, re_shader_name()).unwrap_or_default();
                if shader.eq_ignore_ascii_case("Invisible") {
                    return String::new();
                }
                let visible = tag_text_cached(inner, re_visible()).unwrap_or_default();
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

    for cap in re_texture_by_name().captures_iter(&filtered) {
        let inner = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for ecap in re_e_tag().captures_iter(inner) {
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

    for re in [re_texture_name(), re_texture()] {
        for cap in re.captures_iter(&filtered) {
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

pub fn extract_xsec_texture_references(xml: &str) -> Vec<String> {
    static RE_XSEC_TEXTURE_BLOCK: OnceLock<Regex> = OnceLock::new();
    let re_block = RE_XSEC_TEXTURE_BLOCK
        .get_or_init(|| Regex::new(r"(?s)<Texture\b[^>]*>(.*?)</Texture>").unwrap());

    static RE_E_TAG_XSEC: OnceLock<Regex> = OnceLock::new();
    let re_e = RE_E_TAG_XSEC.get_or_init(|| Regex::new(r"<e\b[^>]*>([^<]+)</e>").unwrap());

    let mut out = Vec::new();
    for block in re_block.captures_iter(xml) {
        if let Some(content) = block.get(1) {
            for cap in re_e.captures_iter(content.as_str()) {
                if let Some(val) = cap.get(1) {
                    let raw = unescape_xml_entities(strip_tags(val.as_str()).trim());
                    if raw.is_empty() || raw == "[00]" || is_numeric_token(&raw) {
                        continue;
                    }
                    out.push(raw);
                }
            }
        }
    }
    out
}

pub fn normalize_texture_reference(raw: &str) -> String {
    let stripped = strip_shader_prefix(raw);
    let lower = stripped.to_lowercase();
    if lower.ends_with(".tgpcdx") {
        stripped
    } else {
        format!("{stripped}.TgPcDx")
    }
}

pub fn product_root_for(path: &Path, assets_root: &Path) -> PathBuf {
    let dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    if let Ok(rel) = dir.strip_prefix(assets_root) {
        let comps: Vec<_> = rel.components().collect();
        if comps.len() >= 2 {
            // Assets\Provider\Product\... -> Assets\Provider\Product
            return assets_root.join(comps[0]).join(comps[1]);
        } else if comps.len() == 1 {
            // Assets\Provider -> Assets\Provider (1-level umbrella)
            return assets_root.join(comps[0]);
        } else {
            return assets_root.to_path_buf();
        }
    }
    dir.to_path_buf()
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
            AssetResourceKind::Texture => format!("{raw}.TgPcDx"),
            AssetResourceKind::Geometry => format!("{raw}.GeoPcDx"),
            AssetResourceKind::Animation => format!("{raw}.ban"),
            AssetResourceKind::ChildBlueprint => {
                if raw.to_lowercase().ends_with(".xml") {
                    raw.to_string()
                } else {
                    format!("{raw}.bin")
                }
            }
            _ => raw.to_string(),
        }
    }
}

fn candidate_relatives(raw: &str, kind: AssetResourceKind) -> Vec<String> {
    let raw_norm = raw.replace('/', "\\");
    let stripped = strip_lod_and_shader_prefix(&raw_norm);

    let mut bases = vec![stripped.clone()];
    if stripped != raw_norm {
        bases.push(raw_norm.clone());
    }

    let mut out = Vec::new();
    for base in bases {
        let cleaned = base.replace('/', "\\");
        match kind {
            AssetResourceKind::Script => {
                let lower = cleaned.to_lowercase();
                let stripped = if lower.ends_with(".lua")
                    || lower.ends_with(".out")
                    || lower.ends_with(".outz")
                {
                    match cleaned.rfind('.') {
                        Some(i) => cleaned[..i].to_string(),
                        None => cleaned.clone(),
                    }
                } else {
                    cleaned.clone()
                };
                out.push(format!("{stripped}.out"));
                out.push(format!("{stripped}.lua"));
            }
            AssetResourceKind::Texture => {
                let with_ext = ensure_extension(&cleaned, kind);
                let mut v = vec![with_ext.clone()];
                if !with_ext.to_lowercase().starts_with("textures\\") {
                    v.push(format!("textures\\{with_ext}"));
                }
                out.extend(v);
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
                out.push(format!("{stripped}.ban"));
                out.push(format!("{stripped}.ia"));
                out.push(cleaned.clone());
            }
            AssetResourceKind::ChildBlueprint => {
                let lower = cleaned.to_lowercase();
                if lower.ends_with(".bin")
                    || lower.ends_with(".xml")
                    || lower.ends_with(".geopcdx")
                    || lower.ends_with(".tgpcdx")
                    || lower.ends_with(".xsec")
                {
                    out.push(cleaned.clone());
                } else {
                    out.push(format!("{cleaned}.bin"));
                    out.push(format!("{cleaned}.xml"));
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
                    out.push(cleaned.clone());
                } else {
                    out.push(format!("{cleaned}.proxybin"));
                    out.push(format!("{cleaned}.proxyxml"));
                    out.push(format!("{cleaned}.bin"));
                    out.push(format!("{cleaned}.xml"));
                    out.push(format!("{cleaned}.dav"));
                }
            }
            _ => {
                out.push(ensure_extension(&cleaned, kind));
            }
        }
    }

    let mut seen = HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    out
}

fn is_global_asset_ref(norm: &str, kind: AssetResourceKind, assets_root: &Path) -> bool {
    if kind == AssetResourceKind::Texture {
        return false;
    }

    let clean = norm.trim_start_matches('/').replace('\\', "/");
    let lower = clean.to_lowercase();

    if lower.starts_with('.') || lower.starts_with("..") {
        return false;
    }
    if lower.starts_with("assets/") {
        return true;
    }

    let parts: Vec<&str> = lower.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        let p1 = parts[0];
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
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let target = strip_shader_prefix(&name.to_string_lossy()).to_lowercase();
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if !is_file {
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

#[inline]
fn loose_exists(candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|p| loose_path_exists(p))
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

    for rel in candidate_relatives(&norm, kind) {
        let cleaned = rel.replace('/', "\\");

        if is_global {
            let parts: Vec<&str> = cleaned.split('\\').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 3 {
                let product_dir = assets_root.join(parts[0]).join(parts[1]);
                let inner = parts[2..].join("/");
                if ap_cache.lookup(&product_dir, &inner) {
                    return true;
                }
            }
        } else {
            let target_loose = normalize_path(&parent.join(&cleaned));
            if let Ok(rel_from_prod) = target_loose.strip_prefix(product_root) {
                let inner = rel_from_prod.to_string_lossy().replace('\\', "/");
                if ap_cache.lookup(product_root, &inner) {
                    return true;
                }
            }
            let inner_direct = cleaned.trim_start_matches(".\\").replace('\\', "/");
            if ap_cache.lookup(product_root, &inner_direct) {
                return true;
            }
        }
    }
    false
}

fn register_texture_siblings(target: &Path, outgoing: &mut Vec<PathBuf>) {
    let (Some(dir), Some(stem_os)) = (target.parent(), target.file_stem()) else {
        return;
    };
    let stem = stem_os.to_string_lossy();

    // Strip a trailing seasonal suffix (any case) to derive the base stem.
    const SEASON_SUFFIXES_LOWER: [&str; 4] = ["_wi", "_su", "_sp", "_au"];
    let base: &str = if SEASON_SUFFIXES_LOWER
        .iter()
        .any(|suf| stem.to_lowercase().ends_with(suf))
    {
        &stem[..stem.len().saturating_sub(3)]
    } else {
        &stem
    };

    // All seasonal suffix variants, covering every case convention used by assets.
    const SEASONAL_VARIANTS: [&str; 12] = [
        "_Wi", "_wi", "_WI", "_Su", "_su", "_SU", "_Sp", "_sp", "_SP", "_Au", "_au", "_AU",
    ];

    for suf in SEASONAL_VARIANTS {
        let seasonal = dir.join(format!("{}{}.TgPcDx", base, suf));
        if loose_path_exists(&seasonal) {
            let norm = crate::fs_utils::normalize_path(&seasonal);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }
    }

    // Normal-map companions.
    let lower_stem = stem.to_lowercase();
    if lower_stem.ends_with("_nm") {
        let base_nm = &stem[..stem.len().saturating_sub(3)];
        let nmc = dir.join(format!("{}_nmc.TgPcDx", base_nm));
        if loose_path_exists(&nmc) {
            let norm = crate::fs_utils::normalize_path(&nmc);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }
    } else if lower_stem.ends_with("_nmc") {
        let base_nmc = &stem[..stem.len().saturating_sub(4)];
        let nm = dir.join(format!("{}_nm.TgPcDx", base_nmc));
        if loose_path_exists(&nm) {
            let norm = crate::fs_utils::normalize_path(&nm);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }
    }
}

fn find_assets_root(start_path: &Path) -> Option<PathBuf> {
    let mut curr = start_path;
    loop {
        if let Some(name) = curr.file_name().and_then(|n| n.to_str())
            && name.eq_ignore_ascii_case("Assets")
        {
            return Some(curr.to_path_buf());
        }
        match curr.parent() {
            Some(p) => curr = p,
            None => break,
        }
    }
    None
}

pub fn extract_raw_bin_references(bytes: &[u8], bin_path: &Path, outgoing: &mut Vec<PathBuf>) {
    // 1. Same-Stem Safety Net
    for ext in &["GeoPcDx", "XSec"] {
        let paired = bin_path.with_extension(ext);
        if loose_path_exists(&paired) {
            let norm = crate::fs_utils::normalize_path(&paired);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }
    }

    // 2. Raw byte string regex matching
    let assets_root = find_assets_root(bin_path);
    let parent_dir = bin_path.parent();
    let lossy_text = String::from_utf8_lossy(bytes);

    static RE_EXT: OnceLock<Regex> = OnceLock::new();
    static RE_LOD: OnceLock<Regex> = OnceLock::new();

    let re_ext = RE_EXT.get_or_init(|| {
        Regex::new(r"(?i)[a-zA-Z0-9_\-\\/\[\]]+?\.(?:geopcdx|tgpcdx|xsec|bin|xml)").unwrap()
    });
    let re_lod = RE_LOD.get_or_init(|| {
        Regex::new(r"(?i)[a-zA-Z0-9_\-\\/]+?\[[0-9a-fA-F]{2}\][a-zA-Z0-9_\-]+").unwrap()
    });

    let mut try_register = |rel_str: &str| {
        let clean = rel_str
            .trim_matches(|c| c == '\0' || c == '"' || c == '\'')
            .replace('/', "\\");
        let clean = clean.trim_start_matches('\\');
        // Strip any [xx] hex seasonal/LOD bitmask prefixes.
        static RE_HEX_BRACKET: OnceLock<Regex> = OnceLock::new();
        let re_hex = RE_HEX_BRACKET.get_or_init(|| Regex::new(r"\[[0-9a-fA-F]{2}\]").unwrap());
        let clean = re_hex.replace_all(clean, "").to_string();
        if clean.is_empty() {
            return;
        }

        let is_tgpcdx = clean
            .rsplit(['\\', '/'])
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext.eq_ignore_ascii_case("tgpcdx"))
            .unwrap_or(false);

        // Test relative to Assets root
        if let Some(root) = &assets_root {
            let candidate = root.join(&clean);
            if loose_path_exists(&candidate) {
                if is_tgpcdx {
                    register_texture_siblings(&candidate, outgoing);
                }
                let norm = crate::fs_utils::normalize_path(&candidate);
                if !outgoing.contains(&norm) {
                    outgoing.push(norm);
                }
                return;
            }
        }

        // Test relative to parent directory
        if let Some(dir) = parent_dir {
            let candidate = dir.join(&clean);
            if loose_path_exists(&candidate) {
                if is_tgpcdx {
                    register_texture_siblings(&candidate, outgoing);
                }
                let norm = crate::fs_utils::normalize_path(&candidate);
                if !outgoing.contains(&norm) {
                    outgoing.push(norm);
                }
            }
        }
    };

    for mat in re_ext.find_iter(&lossy_text) {
        let s = mat.as_str();
        if s.to_ascii_lowercase().ends_with(".xml") {
            let base = &s[..s.len() - 4];
            try_register(&format!("{}.bin", base));
            try_register(&format!("{}.GeoPcDx", base));
        } else {
            try_register(s);
        }
    }

    for mat in re_lod.find_iter(&lossy_text) {
        let raw = mat.as_str();
        if let (Some(b_open), Some(b_close)) = (raw.find('['), raw.find(']'))
            && b_close > b_open
        {
            let prefix = &raw[..b_open];
            let model = &raw[b_close + 1..];
            try_register(&format!("{}{}.GeoPcDx", prefix, model));
            try_register(&format!("{}{}.XSec", prefix, model));
            try_register(&format!("{}{}.TgPcDx", prefix, model)); // <-- ADD THIS
        }
    }
}

pub fn extract_xml_texture_id_refs(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
    broken: &mut Vec<BrokenResourceRef>,
    seen: &mut HashMap<(AssetResourceKind, PathBuf, PathBuf), usize>,
    parent_file: &Path,
    ap_cache: &ApArchiveCache,
) {
    let assets_root = find_assets_root(current_file_path);
    let parent_dir = current_file_path.parent();
    let product_root = product_root_for(current_file_path, assets_root.as_deref().unwrap_or(Path::new("")));

    static RE_TEXTURE_ID: OnceLock<Regex> = OnceLock::new();
    let re_tex = RE_TEXTURE_ID.get_or_init(|| {
        Regex::new(r"(?i)<[a-z0-9_]*(?:TextureID|TextureNormalID|NormalMapID|NormalID|TextureSlot)[^>]*>([^<]+)</")
            .unwrap()
    });

    static RE_HEX_BRACKET: OnceLock<Regex> = OnceLock::new();
    let re_hex = RE_HEX_BRACKET.get_or_init(|| Regex::new(r"\[[0-9a-fA-F]{2}\]").unwrap());

    for cap in re_tex.captures_iter(xml) {
        let raw = cap[1].trim().replace('/', "\\");
        let raw = raw.trim_start_matches('\\');
        if raw.is_empty() {
            continue;
        }

        // Strip any [xx] hex seasonal/LOD bitmask prefixes.
        let clean_rel = re_hex.replace_all(raw, "").trim().to_string();
        if clean_rel.is_empty() || is_numeric_token(&clean_rel) {
            continue;
        }

        let file_rel = if clean_rel.to_ascii_lowercase().ends_with(".tgpcdx") {
            clean_rel
        } else {
            format!("{}.TgPcDx", clean_rel)
        };

        // Build candidate paths: relative to Assets root, product root, and blueprint directory.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(root) = &assets_root {
            candidates.push(crate::fs_utils::normalize_path(&root.join(&file_rel)));
        }
        candidates.push(crate::fs_utils::normalize_path(&product_root.join(&file_rel)));
        if !file_rel.to_ascii_lowercase().starts_with("textures\\") {
            candidates.push(crate::fs_utils::normalize_path(&product_root.join("textures").join(&file_rel)));
        }
        if let Some(p_dir) = parent_dir {
            candidates.push(crate::fs_utils::normalize_path(&p_dir.join(&file_rel)));
        }
        let mut seen_cands = HashSet::new();
        candidates.retain(|p| seen_cands.insert(p.clone()));

        let expected = assets_root
            .as_ref()
            .map(|r| r.join(&file_rel))
            .unwrap_or_else(|| PathBuf::from(&file_rel));

        let matched_candidate = candidates.iter().find(|c| loose_path_exists(c)).cloned();
        let exists = matched_candidate.is_some()
            || ap_exists(
                &file_rel,
                parent_dir.unwrap_or(Path::new("")),
                &product_root,
                assets_root.as_deref().unwrap_or(Path::new("")),
                ap_cache,
                AssetResourceKind::Texture,
            );

        if exists {
            if let Some(candidate) = matched_candidate {
                register_texture_siblings(&candidate, outgoing);
                let norm = crate::fs_utils::normalize_path(&candidate);
                if !outgoing.contains(&norm) {
                    outgoing.push(norm);
                }
            }
        } else {
            // Texture missing on disk and not in AP archive -> BrokenResourceRef.
            let kind = AssetResourceKind::Texture;
            let key = (kind, expected.clone(), parent_file.to_path_buf());
            if let Some(&idx) = seen.get(&key) {
                broken[idx].count += 1;
            } else {
                seen.insert(key, broken.len());
                    broken.push(BrokenResourceRef {
                        parent_file: parent_file.to_path_buf(),
                        all_parents: vec![parent_file.to_path_buf()],
                        expected_path: expected,
                        kind,
                        count: 1,
                    });
            }
        }
    }
}

pub fn extract_xml_cross_section_refs(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
) {
    let assets_root = find_assets_root(current_file_path);
    let parent_dir = current_file_path.parent();

    static RE_CS: OnceLock<Regex> = OnceLock::new();
    let re_cs = RE_CS.get_or_init(|| {
        Regex::new(r"<(?:CrossSectionID|CrossSectionCapID|CrossSectionStartCapID|CrossSectionEndCapID)[^>]*>([^<]+)</").unwrap()
    });

    for cap in re_cs.captures_iter(xml) {
        let raw = cap[1].trim().replace('/', "\\");
        let raw = raw.trim_start_matches('\\');
        if raw.is_empty() {
            continue;
        }

        // Strip [00] prefix if present
        let clean = if let (Some(b_open), Some(b_close)) = (raw.find('['), raw.find(']')) {
            if b_close >= b_open {
                format!("{}{}", &raw[..b_open], &raw[b_close + 1..])
            } else {
                raw.to_string()
            }
        } else {
            raw.to_string()
        };

        // If the path already has .xsec or .geopcdx, strip it to get the base stem
        let base = if clean.to_ascii_lowercase().ends_with(".xsec") {
            &clean[..clean.len() - 5]
        } else if clean.to_ascii_lowercase().ends_with(".geopcdx") {
            &clean[..clean.len() - 8]
        } else {
            &clean
        };

        let mut check_and_add = |candidate: PathBuf| {
            if loose_path_exists(&candidate) {
                let norm = crate::fs_utils::normalize_path(&candidate);
                if !outgoing.contains(&norm) {
                    register_texture_siblings(&candidate, outgoing);
                    outgoing.push(norm);
                }
            }
        };

        // 1. Check relative to Assets root
        if let Some(root) = &assets_root {
            check_and_add(root.join(format!("{}.XSec", base)));
            check_and_add(root.join(format!("{}.GeoPcDx", base)));
        }

        // 2. Check relative to current file's directory
        if let Some(p_dir) = parent_dir {
            check_and_add(p_dir.join(format!("{}.XSec", base)));
            check_and_add(p_dir.join(format!("{}.GeoPcDx", base)));
        }
    }
}

pub fn extract_absolute_blueprint_refs(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
) {
    let assets_root = find_assets_root(current_file_path);

    static RE_BLOCK: OnceLock<Regex> = OnceLock::new();
    static RE_PROV: OnceLock<Regex> = OnceLock::new();
    static RE_PROD: OnceLock<Regex> = OnceLock::new();
    static RE_BP: OnceLock<Regex> = OnceLock::new();

    let re_block = RE_BLOCK.get_or_init(|| {
        Regex::new(r"(?s)<iBlueprintLibrary-cAbsoluteBlueprintID>(.*?)</iBlueprintLibrary-cAbsoluteBlueprintID>").unwrap()
    });
    let re_prov = RE_PROV.get_or_init(|| Regex::new(r"<Provider[^>]*>([^<]*)</Provider>").unwrap());
    let re_prod = RE_PROD.get_or_init(|| Regex::new(r"<Product[^>]*>([^<]*)</Product>").unwrap());
    let re_bp =
        RE_BP.get_or_init(|| Regex::new(r"<BlueprintID[^>]*>([^<]+)</BlueprintID>").unwrap());

    for cap in re_block.captures_iter(xml) {
        let block = &cap[1];
        let Some(bp_mat) = re_bp.captures(block) else {
            continue;
        };
        let raw_bp = bp_mat[1].trim().replace('/', "\\");
        let raw_bp = raw_bp.trim_start_matches('\\');
        if raw_bp.is_empty() {
            continue;
        }

        let provider = re_prov
            .captures(block)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        let product = re_prod
            .captures(block)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();

        let base_rel = if raw_bp.to_ascii_lowercase().ends_with(".xml") {
            &raw_bp[..raw_bp.len() - 4]
        } else {
            raw_bp
        };

        let base_dir = if !provider.is_empty() && !product.is_empty() {
            assets_root
                .as_ref()
                .map(|root| root.join(provider).join(product))
        } else {
            current_file_path.parent().map(|p| p.to_path_buf())
        };

        let Some(target_dir) = base_dir else { continue };

        let bin_candidate = target_dir.join(format!("{}.bin", base_rel));
        if loose_path_exists(&bin_candidate) {
            let norm = crate::fs_utils::normalize_path(&bin_candidate);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }

        let geo_candidate = target_dir.join(format!("{}.GeoPcDx", base_rel));
        if loose_path_exists(&geo_candidate) {
            let norm = crate::fs_utils::normalize_path(&geo_candidate);
            if !outgoing.contains(&norm) {
                register_texture_siblings(&geo_candidate, outgoing);
                outgoing.push(norm);
            }
        }

        let xsec_candidate = target_dir.join(format!("{}.XSec", base_rel));
        if loose_path_exists(&xsec_candidate) {
            let norm = crate::fs_utils::normalize_path(&xsec_candidate);
            if !outgoing.contains(&norm) {
                outgoing.push(norm);
            }
        }
    }
}

/// Identifies common multi-language vehicle category directories.
pub fn is_vehicle_category_folder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "railvehicles"
            | "elloks"
            | "eloks"
            | "dieselloks"
            | "dampfloks"
            | "personenwagen"
            | "personenw"
            | "gueterwagen"
            | "guterwagen"
            | "triebwagen"
            | "steuerwagen"
            | "rollingstock"
            | "wagons"
            | "triebzuege"
            | "triebzug"
            | "passagierwagen"
            | "personenverkehr"
            | "tender"
            | "tenderloks"
            | "cabview"
            | "cabviews"
            | "passview"
            | "passengers"
    )
}

/// Identifies dynamic Train Simulator Classic runtime assets that are resolved
/// procedurally (numbering digits, EBuLa/FIS timetables, passenger randomization,
/// destination displays) and therefore lack static references.
fn is_dynamic_runtime_asset(path: &Path) -> bool {
    let lower_str = path.to_string_lossy().to_ascii_lowercase();

    // 1. Dynamic numbering digits (queried at runtime by cNumberingBlueprint)
    let has_numbering = path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        s == "primarynumbers" || s == "secondarynumbers" || s == "digits" || s == "numbers"
    });

    // 2. Electronic in-cab timetable systems (EBuLa / Fahrplan / FIS)
    let has_timetable = path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        s == "ebula" || s == "fahrplan" || s == "timetable" || s == "matrix" || s == "zzt" || s == "zst"
    });

    // 3. Scripted destination display textures
    let has_destination = lower_str.contains(r"\destination\")
        || lower_str.contains(r"/destination/")
        || lower_str.contains(r"\destinations\")
        || lower_str.contains(r"/destinations/");

    has_numbering || has_timetable || has_destination
}

/// Resolves the component root folder containing the script by walking up the directory tree
/// until it finds a recognized category folder, a parent directory hosting a .bin blueprint,
/// or the package boundary.
pub fn find_scripted_component_root(script_path: &Path) -> Option<PathBuf> {
    let mut curr = script_path.parent();
    let mut candidate_root = None;

    while let Some(dir) = curr {
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Stop traversal at Assets boundary
        if dir_name.eq_ignore_ascii_case("Assets") {
            break;
        }

        // If direct parent is a recognized category folder (e.g. Elloks/<LocoFolder>),
        // then `dir` is the vehicle root.
        if let Some(parent) = dir.parent()
            && let Some(p_name) = parent.file_name().and_then(|n| n.to_str())
            && is_vehicle_category_folder(p_name)
        {
            return Some(dir.to_path_buf());
        }

        // Check if `dir` contains a controlling blueprint (.bin)
        if let Ok(entries) = std::fs::read_dir(dir) {
            let has_blueprint = entries.flatten().any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
            });
            if has_blueprint {
                candidate_root = Some(dir.to_path_buf());
            }
        }

        curr = dir.parent();
    }

    candidate_root.or_else(|| script_path.parent().map(|p| p.to_path_buf()))
}

fn audit_one_file(
    file: &Path,
    folder: &Path,
    assets_root: &Path,
    ap_cache: &ApArchiveCache,
) -> FileAuditOutcome {
    let xml = match crate::assets::read_serz_xml(file) {
        Ok(x) => x,
        Err(_) => match fs::read(file) {
            Ok(bytes) if bytes.starts_with(b"<?xm") => String::from_utf8_lossy(&bytes).to_string(),
            Ok(bytes) => {
                let is_bin = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("bin"))
                    .unwrap_or(false);
                if is_bin {
                    let mut outgoing = Vec::new();
                    extract_raw_bin_references(&bytes, file, &mut outgoing);
                    return FileAuditOutcome {
                        parent: file.to_path_buf(),
                        broken: Vec::new(),
                        outgoing,
                    };
                }
                return FileAuditOutcome {
                    parent: file.to_path_buf(),
                    broken: Vec::new(),
                    outgoing: Vec::new(),
                };
            }
            _ => {
                return FileAuditOutcome {
                    parent: file.to_path_buf(),
                    broken: Vec::new(),
                    outgoing: Vec::new(),
                };
            }
        },
    };

    let mut out: Vec<BrokenResourceRef> = Vec::new();
    let mut seen: HashMap<(AssetResourceKind, PathBuf, PathBuf), usize> = HashMap::new();
    let mut outgoing: Vec<PathBuf> = Vec::new();
    extract_absolute_blueprint_refs(&xml, file, &mut outgoing);
    extract_xml_cross_section_refs(&xml, file, &mut outgoing);
    let parent = file.parent().unwrap_or(folder);
    let product_root = product_root_for(file, assets_root);
    let parent_file = file.to_path_buf();

    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "bin" {
        extract_xml_texture_id_refs(&xml, file, &mut outgoing, &mut out, &mut seen, &parent_file, ap_cache);
        for (kind, raw) in extract_bin_references(&xml, &product_root) {
            let cands = candidate_paths(&raw, parent, &product_root, assets_root, kind);
            let expected = cands
                .first()
                .cloned()
                .unwrap_or_else(|| assets_root.join(&raw));
            let exists = loose_exists(&cands)
                || ap_exists(&raw, parent, &product_root, assets_root, ap_cache, kind);
            let target = cands
                .iter()
                .find(|p| loose_path_exists(p))
                .unwrap_or(&expected);
            outgoing.push(crate::fs_utils::normalize_path(target));
            if kind == AssetResourceKind::Texture && loose_path_exists(target) {
                register_texture_siblings(target, &mut outgoing);
            }
            if !exists {
                let key = (kind, expected.clone(), parent_file.clone());
                if let Some(&idx) = seen.get(&key) {
                    out[idx].count += 1;
                } else {
                    seen.insert(key, out.len());
                    out.push(BrokenResourceRef {
                        parent_file: parent_file.clone(),
                        all_parents: vec![parent_file.clone()],
                        expected_path: expected,
                        kind,
                        count: 1,
                    });
                }
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
            let target = cands
                .iter()
                .find(|p| loose_path_exists(p))
                .unwrap_or(&expected);
            outgoing.push(crate::fs_utils::normalize_path(target));

            if loose_path_exists(target) {
                register_texture_siblings(target, &mut outgoing);
            }

            if !exists {
                let key = (
                    AssetResourceKind::Texture,
                    expected.clone(),
                    parent_file.clone(),
                );
                if let Some(&idx) = seen.get(&key) {
                    out[idx].count += 1;
                } else {
                    seen.insert(key, out.len());
                    out.push(BrokenResourceRef {
                        parent_file: parent_file.clone(),
                        all_parents: vec![parent_file.clone()],
                        expected_path: expected,
                        kind: AssetResourceKind::Texture,
                        count: 1,
                    });
                }
            }
        }
    } else if ext == "xsec" {
        for raw in extract_xsec_texture_references(&xml) {
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
            let target = cands
                .iter()
                .find(|p| loose_path_exists(p))
                .unwrap_or(&expected);
            outgoing.push(crate::fs_utils::normalize_path(target));

            if loose_path_exists(target) {
                register_texture_siblings(target, &mut outgoing);
            }

            if !exists {
                let key = (
                    AssetResourceKind::Texture,
                    expected.clone(),
                    parent_file.clone(),
                );
                if let Some(&idx) = seen.get(&key) {
                    out[idx].count += 1;
                } else {
                    seen.insert(key, out.len());
                    out.push(BrokenResourceRef {
                        parent_file: parent_file.clone(),
                        all_parents: vec![parent_file.clone()],
                        expected_path: expected,
                        kind: AssetResourceKind::Texture,
                        count: 1,
                    });
                }
            }
        }
    }

    FileAuditOutcome {
        parent: parent_file,
        broken: out,
        outgoing,
    }
}

pub fn audit_folders_with_progress(
    target_folders: &[PathBuf],
    assets_root: &Path,
    max_concurrent: usize,
    ap_cache: &ApArchiveCache,
    progress: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (AuditReport, FullAssetGraph) {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut file_set: HashSet<PathBuf> = HashSet::new();
    let mut texture_files: Vec<PathBuf> = Vec::new();
    let mut texture_set: HashSet<PathBuf> = HashSet::new();
    let mut scripted_component_roots: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();

    for folder_path in target_folders {
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
                    if ext == "lua" || ext == "out" {
                        if let Some(root) = find_scripted_component_root(path) {
                            scripted_component_roots
                                .insert(crate::fs_utils::normalize_path(&root));
                        }
                    } else if ext == "bin" || ext == "geopcdx" || ext == "xsec" {
                        if file_set.insert(path.to_path_buf()) {
                            files.push(path.to_path_buf());
                        }
                    } else if ext == "tgpcdx" && texture_set.insert(path.to_path_buf()) {
                        texture_files.push(path.to_path_buf());
                    }
                }
            }
        } else {
            let mut tmp_files = Vec::new();
            let mut tmp_textures = Vec::new();
            let mut tmp_scripts = std::collections::HashSet::new();
            collect_files(
                folder_path,
                &mut tmp_files,
                &mut tmp_textures,
                &mut tmp_scripts,
            );
            for p in tmp_files {
                if file_set.insert(p.clone()) {
                    files.push(p);
                }
            }
            for p in tmp_textures {
                if texture_set.insert(p.clone()) {
                    texture_files.push(p);
                }
            }
            for p in tmp_scripts {
                scripted_component_roots.insert(p);
            }
        }
    }

    // Collect additional .bin/.geopcdx across product roots for dependency scope
    let mut extra_files: Vec<PathBuf> = Vec::new();
    let mut extra_set: HashSet<PathBuf> = HashSet::new();
    let mut seen_products: HashSet<PathBuf> = HashSet::new();
    for folder_path in target_folders {
        let product_root = product_root_for(folder_path, assets_root);
        if !seen_products.insert(product_root.clone()) {
            continue;
        }
        let should_scan_extra = product_root != *folder_path
            && product_root != *assets_root
            && folder_path.starts_with(&product_root);
        if !should_scan_extra {
            continue;
        }
        let mut all_product_files = Vec::new();
        let mut dummy_textures = Vec::new();
        if let Ok(entries) = walkdir::WalkDir::new(&product_root)
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
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if (ext == "bin" || ext == "geopcdx" || ext == "xsec")
                        && !target_folders.iter().any(|tf| path.starts_with(tf))
                    {
                        all_product_files.push(path.to_path_buf());
                    } else if (ext == "lua" || ext == "out")
                        && let Some(root) = find_scripted_component_root(path)
                    {
                        scripted_component_roots
                            .insert(crate::fs_utils::normalize_path(&root));
                    }
                }
            }
        } else {
            collect_files(
                &product_root,
                &mut all_product_files,
                &mut dummy_textures,
                &mut scripted_component_roots,
            );
            all_product_files.retain(|p| !target_folders.iter().any(|tf| p.starts_with(tf)));
        }
        for p in all_product_files {
            if extra_set.insert(p.clone()) {
                extra_files.push(p);
            }
        }
    }

    let threads = max_concurrent.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_files = files.len();
    let progress_cb = progress.clone();

    let results: Vec<FileAuditOutcome> = pool.install(|| {
        files
            .par_iter()
            .map(|f| {
                let r = audit_one_file(f, assets_root, assets_root, ap_cache);
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

    // Aggregate broken references
    let mut broken_all: Vec<BrokenResourceRef> = Vec::new();
    for r in &results {
        broken_all.extend(r.broken.clone());
    }

    // Aggregate dependency graph
    let mut outgoing_map: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let mut incoming_map: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for r in &results {
        let parent_norm = std::path::PathBuf::from(
            crate::fs_utils::normalize_path(&r.parent)
                .to_string_lossy()
                .to_lowercase(),
        );
        let set = outgoing_map.entry(parent_norm.clone()).or_default();
        for t in &r.outgoing {
            let tn = std::path::PathBuf::from(
                crate::fs_utils::normalize_path(t)
                    .to_string_lossy()
                    .to_lowercase(),
            );
            set.insert(tn.clone());
            incoming_map
                .entry(tn)
                .or_default()
                .insert(parent_norm.clone());
        }
    }

    // Product-wide dependency scope: extract references from other files
    let extra_outcomes: Vec<FileAuditOutcome> = pool.install(|| {
        extra_files
            .par_iter()
            .map(|f| audit_one_file(f, assets_root, assets_root, ap_cache))
            .collect()
    });

    for outcome in extra_outcomes {
        let parent_norm = std::path::PathBuf::from(
            crate::fs_utils::normalize_path(&outcome.parent)
                .to_string_lossy()
                .to_lowercase(),
        );
        let set = outgoing_map.entry(parent_norm.clone()).or_default();
        for t in &outcome.outgoing {
            let tn = std::path::PathBuf::from(
                crate::fs_utils::normalize_path(t)
                    .to_string_lossy()
                    .to_lowercase(),
            );
            set.insert(tn.clone());
            incoming_map
                .entry(tn)
                .or_default()
                .insert(parent_norm.clone());
        }
    }

    // Dedup broken references
    let mut deduped: Vec<BrokenResourceRef> = Vec::with_capacity(broken_all.len());
    let mut seen: HashMap<(AssetResourceKind, PathBuf), usize> = HashMap::new();
    for b in broken_all {
        let key = (b.kind, b.expected_path.clone());
        if let Some(&idx) = seen.get(&key) {
            deduped[idx].count += b.count;
            for p in b.all_parents {
                if !deduped[idx].all_parents.contains(&p) {
                    deduped[idx].all_parents.push(p);
                }
            }
            if deduped[idx].parent_file.as_os_str().is_empty() {
                deduped[idx].parent_file =
                    deduped[idx].all_parents.first().cloned().unwrap_or_default();
            }
        } else {
            seen.insert(key, deduped.len());
            let mut b = b;
            if b.all_parents.is_empty() {
                b.all_parents = vec![b.parent_file.clone()];
            }
            if b.parent_file.as_os_str().is_empty() {
                b.parent_file = b.all_parents.first().cloned().unwrap_or_default();
            }
            deduped.push(b);
        }
    }

    // Reachability from live roots (Mark-and-Sweep)
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();

    // Live roots: all .bin blueprints in the scanned package (and dependency scope).
    for f in files.iter().chain(extra_files.iter()) {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if ext == "bin" {
            let nf = std::path::PathBuf::from(
                crate::fs_utils::normalize_path(f)
                    .to_string_lossy()
                    .to_lowercase(),
            );
            if visited.insert(nf.clone()) {
                queue.push_back(nf);
            }
        }
    }

    while let Some(cur) = queue.pop_front() {
        if let Some(targets) = outgoing_map.get(&cur) {
            for t in targets {
                if visited.insert(t.clone()) {
                    queue.push_back(t.clone());
                }
            }
        }
    }

    // Orphan detection
    for f in files.iter().chain(texture_files.iter()) {
        let nf = std::path::PathBuf::from(
            crate::fs_utils::normalize_path(f)
                .to_string_lossy()
                .to_lowercase(),
        );
        let in_ap = {
            let product_root = product_root_for(f, assets_root);
            let rel_product = f
                .strip_prefix(&product_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| f.to_string_lossy().replace('\\', "/"));
            ap_cache.lookup(&product_root, &rel_product)
        };
        if visited.contains(&nf) || in_ap {
            continue;
        }
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let is_orphan = matches!(ext.as_str(), "geopcdx" | "tgpcdx" | "xsec");
        if is_orphan {
            let is_shielded = (ext == "geopcdx" || ext == "tgpcdx")
                && scripted_component_roots.iter().any(|root| {
                    nf.starts_with(PathBuf::from(root.to_string_lossy().to_lowercase()))
                });
            if is_shielded {
                continue;
            }

            // Add dynamic runtime asset protection
            if is_dynamic_runtime_asset(f) {
                continue;
            }

            deduped.push(BrokenResourceRef {
                parent_file: f.clone(),
                all_parents: vec![f.clone()],
                expected_path: f.clone(),
                kind: AssetResourceKind::Unused,
                count: 1,
            });
        }
    }

    // Build full directory asset graph.
    let mut files_on_disk: HashSet<PathBuf> = HashSet::new();
    for f in files.iter().chain(texture_files.iter()).chain(extra_files.iter()) {
        files_on_disk.insert(std::path::PathBuf::from(
            crate::fs_utils::normalize_path(f)
                .to_string_lossy()
                .to_lowercase(),
        ));
    }

    let mut missing_files: HashSet<PathBuf> = HashSet::new();
    let mut dead_files: HashSet<PathBuf> = HashSet::new();
    for b in &deduped {
        let ep = std::path::PathBuf::from(
            crate::fs_utils::normalize_path(&b.expected_path)
                .to_string_lossy()
                .to_lowercase(),
        );
        if b.kind == AssetResourceKind::Unused {
            dead_files.insert(ep);
        } else {
            missing_files.insert(ep);
        }
    }

    let full_graph = FullAssetGraph {
        files_on_disk,
        forward_edges: outgoing_map.clone(),
        missing_files,
        dead_files,
    };

    (AuditReport {
        broken_references: deduped,
        dependency_graph: DependencyGraph {
            outgoing: outgoing_map,
            incoming: incoming_map,
        }
    }, full_graph)
}

fn collect_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    textures_out: &mut Vec<PathBuf>,
    scripted_roots: &mut std::collections::HashSet<PathBuf>,
) {
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
                collect_files(&path, out, textures_out, scripted_roots);
            } else if path.is_file() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "lua" || ext == "out" {
                    if let Some(root) = find_scripted_component_root(&path) {
                        scripted_roots.insert(crate::fs_utils::normalize_path(&root));
                    }
                } else if ext == "bin" || ext == "geopcdx" || ext == "xsec" {
                    out.push(path);
                } else if ext == "tgpcdx" {
                    textures_out.push(path);
                }
            }
        }
    }
}

pub fn filter_items(items: &mut Vec<BrokenResourceRef>, filter: &str) {
    let f = filter.trim().to_lowercase();
    let target_kind = match f.as_str() {
        "" | "all" | "все" => None,
        _ if f.contains("геометр") || f.contains("geometry") => {
            Some(AssetResourceKind::Geometry)
        }
        _ if f.contains("текстур") || f.contains("texture") => {
            Some(AssetResourceKind::Texture)
        }
        _ if f.contains("анимац") || f.contains("animation") => {
            Some(AssetResourceKind::Animation)
        }
        _ if f.contains("скрипт") || f.contains("script") => Some(AssetResourceKind::Script),
        _ if f.contains("звук")
            || f.contains("аудио")
            || f.contains("sound")
            || f.contains("audio") =>
        {
            Some(AssetResourceKind::Sound)
        }
        _ if f.contains("дочер") || f.contains("child") => {
            Some(AssetResourceKind::ChildBlueprint)
        }
        _ if f.contains("неиспольз")
            || f.contains("orphan")
            || f.contains("unused")
            || f.contains("dead")
            || f.contains("мертв") =>
        {
            Some(AssetResourceKind::Unused)
        }
        _ => None,
    };
    let Some(target_kind) = target_kind else {
        return;
    };
    items.retain(|b| b.kind == target_kind);
}

pub fn sort_items(items: &mut [BrokenResourceRef], col: usize, asc: bool) {
    items.sort_by(|a, b| {
        let cmp = match col {
            0 => (a.kind as usize).cmp(&(b.kind as usize)),
            1 => a
                .expected_path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.expected_path.to_string_lossy().to_lowercase()),
            _ => a
                .parent_file
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.parent_file.to_string_lossy().to_lowercase()),
        };
        if asc { cmp } else { cmp.reverse() }
    });
}
