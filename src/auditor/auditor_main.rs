use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock, RwLock};

use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::common::archive::ApArchiveCache;
use crate::common::fs_utils::{normalize_path, strip_shader_prefix};

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
            Self::Geometry => "Geometry",
            Self::Texture => "Texture",
            Self::Animation => "Animation",
            Self::Script => "Script",
            Self::Sound => "Sound",
            Self::ChildBlueprint => "Child object",
            Self::Unused => "Unused",
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
    pub dependency_graph: DependencyGraph,
}

#[derive(Debug, Clone, Default)]
pub struct FullAssetGraph {
    pub files_on_disk: HashSet<PathBuf>,
    pub forward_edges: HashMap<PathBuf, HashSet<PathBuf>>,
    pub missing_files: HashSet<PathBuf>,
    pub dead_files: HashSet<PathBuf>,
}

/// Потокобезопасный кэш содержимого директорий и папок провайдеров.
/// Исключает повторные `fs::read_dir` и `stat` при проверке сотен кандидатов.
struct DirCache {
    dirs: RwLock<HashMap<PathBuf, Arc<HashSet<String>>>>,
    providers: HashSet<String>,
}

impl DirCache {
    fn new(assets_root: &Path) -> Self {
        let mut providers = HashSet::new();
        if let Ok(entries) = fs::read_dir(assets_root) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    providers.insert(entry.file_name().to_string_lossy().to_ascii_lowercase());
                }
            }
        }
        Self {
            dirs: RwLock::new(HashMap::new()),
            providers,
        }
    }

    #[inline]
    fn has_provider(&self, name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('.')
            && self.providers.contains(&name.to_ascii_lowercase())
    }

    fn dir_entries(&self, dir: &Path) -> Arc<HashSet<String>> {
        if let Ok(guard) = self.dirs.read()
            && let Some(set) = guard.get(dir)
        {
            return Arc::clone(set);
        }

        let mut files = HashSet::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    let name = entry.file_name();
                    let stripped = strip_shader_prefix(&name.to_string_lossy());
                    files.insert(stripped.to_ascii_lowercase());
                }
            }
        }

        let arc = Arc::new(files);
        if let Ok(mut guard) = self.dirs.write() {
            guard.entry(dir.to_path_buf()).or_insert_with(|| Arc::clone(&arc));
        }
        arc
    }

    fn loose_path_exists(&self, path: &Path) -> bool {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        let target = strip_shader_prefix(&name.to_string_lossy()).to_ascii_lowercase();
        self.dir_entries(parent).contains(&target)
    }
}

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
static_tag_regex!(re_uncoupled_geometry, "UncoupledGeometry");
static_tag_regex!(re_coupled_geometry, "CoupledGeometry");
static_tag_regex!(re_receiving_geometry, "ReceivingGeometry");

fn re_hex_bracket() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[0-9a-fA-F]{2}\]").unwrap())
}

#[inline]
fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    let b = s.as_bytes();
    let suf = suffix.as_bytes();
    b.len()
        .checked_sub(suf.len())
        .is_some_and(|start| b[start..].eq_ignore_ascii_case(suf))
}

#[inline]
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    let b = s.as_bytes();
    let pre = prefix.as_bytes();
    b.len() >= pre.len() && b[..pre.len()].eq_ignore_ascii_case(pre)
}

#[inline]
fn contains_ignore_ascii_case(s: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    s.as_bytes()
        .windows(n.len())
        .any(|w| w.eq_ignore_ascii_case(n))
}

#[inline]
fn path_lower_key(path: &Path) -> PathBuf {
    PathBuf::from(normalize_path(path).to_string_lossy().to_lowercase())
}

#[inline]
fn push_unique_path(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

#[inline]
fn insert_unique_path(list: &mut Vec<PathBuf>, set: &mut HashSet<PathBuf>, path: PathBuf) {
    if set.insert(path.clone()) {
        list.push(path);
    }
}

fn tag_text_cached(xml: &str, re: &Regex) -> Option<String> {
    re.captures(xml)
        .and_then(|c| c.get(1))
        .map(|m| unescape_xml_entities(m.as_str().trim()))
        .filter(|s| !s.is_empty())
}

fn strip_tags(s: &str) -> Cow<'_, str> {
    if !s.contains('<') {
        return Cow::Borrowed(s);
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    re.replace_all(s, "")
}

#[inline]
fn is_numeric_token(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.parse::<f64>().is_ok()
}

fn is_blacklisted_sound_tag(tag: &str) -> bool {
    const BLACKLIST: &[&str] = &[
        "innervisualsoundradius",
        "outervisualsoundradius",
        "highestlevel_1ishighest",
        "lowestlevel_10islowest",
        "volume",
        "pitch",
        "mindistance",
        "maxdistance",
        "loop",
        "priority",
    ];
    BLACKLIST.iter().any(|b| tag.eq_ignore_ascii_case(b))
        || contains_ignore_ascii_case(tag, "editorrenderdata")
}

pub fn strip_lod_and_shader_prefix(raw: &str) -> String {
    if let Some(pos) = raw.rfind(['\\', '/']) {
        let (dir, file) = raw.split_at(pos + 1);
        if file.starts_with('[')
            && let Some(end) = file.find(']')
        {
            return format!("{dir}{}", &file[end + 1..]);
        }
    } else if raw.starts_with('[')
        && let Some(end) = raw.find(']')
    {
        return raw[end + 1..].to_string();
    }
    raw.to_string()
}

pub fn unescape_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut decoded = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(ampersand) = remaining.find('&') {
        decoded.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand + 1..];
        let Some(semicolon) = remaining.find(';') else {
            decoded.push('&');
            decoded.push_str(remaining);
            remaining = "";
            break;
        };
        let entity = &remaining[..semicolon];
        let character = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            value if value.starts_with('#') => {
                value[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(ch) = character {
            decoded.push(ch);
        } else {
            decoded.push('&');
            decoded.push_str(entity);
            decoded.push(';');
        }
        remaining = &remaining[semicolon + 1..];
    }
    decoded.push_str(remaining);
    decoded
}

fn clean_xml_first_line(raw: &str) -> String {
    let stripped = strip_tags(raw);
    let unescaped = unescape_xml_entities(stripped.trim());
    unescaped.lines().next().unwrap_or("").trim().to_string()
}

pub fn extract_bin_references(xml: &str, product_root: &Path) -> Vec<(AssetResourceKind, String)> {
    let mut out = Vec::new();

    // Геометрии
    static RE_ANY_GEOMETRY_ID: OnceLock<Regex> = OnceLock::new();
    let re_geo = RE_ANY_GEOMETRY_ID.get_or_init(|| {
        Regex::new(r"(?is)<([a-zA-Z0-9_]*GeometryID)(?:\s[^>]*)?>(.*?)</([a-zA-Z0-9_]*GeometryID)>")
            .unwrap()
    });
    for cap in re_geo.captures_iter(xml) {
        let open = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let close = cap.get(3).map(|m| m.as_str()).unwrap_or("");
        if !open.eq_ignore_ascii_case(close) {
            continue;
        }
        if let Some(val) = cap.get(2) {
            let clean = clean_xml_first_line(val.as_str());
            if !clean.is_empty() && !is_numeric_token(&clean) {
                out.push((AssetResourceKind::Geometry, clean));
            }
        }
    }

    for re in [
        re_uncoupled_geometry(),
        re_coupled_geometry(),
        re_receiving_geometry(),
    ] {
        for value in re.captures_iter(xml).filter_map(|c| c.get(1)) {
            let raw = clean_xml_first_line(value.as_str());
            if !raw.is_empty() && !is_numeric_token(&raw) {
                out.push((AssetResourceKind::Geometry, raw));
            }
        }
    }

    for re in [
        re_cross_section_id(),
        re_middle_section(),
        re_start_section(),
        re_end_section(),
        re_cross_section(),
    ] {
        for value in re.captures_iter(xml).filter_map(|c| c.get(1)) {
            let raw = clean_xml_first_line(value.as_str());
            if raw.is_empty() || is_numeric_token(&raw) {
                continue;
            }
            let raw = if raw.contains('.') || ends_with_ignore_ascii_case(&raw, ".xsec") {
                raw
            } else {
                format!("{raw}.xsec")
            };
            out.push((AssetResourceKind::Geometry, raw));
        }
    }

    // Анимации и звуки (только листовые теги без вложенных `<` — исключает бэктрекинг)
    {
        static LEAF_TAG_RE: OnceLock<Regex> = OnceLock::new();
        let re = LEAF_TAG_RE.get_or_init(|| {
            Regex::new(r"<([A-Za-z0-9_]+)[^>]*>([^<]+)</([A-Za-z0-9_]+)>").unwrap()
        });
        for cap in re.captures_iter(xml) {
            let tag = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let close = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            if !tag.eq_ignore_ascii_case(close) || is_blacklisted_sound_tag(tag) {
                continue;
            }
            let is_anim = ends_with_ignore_ascii_case(tag, "animname")
                || ends_with_ignore_ascii_case(tag, "animationname");
            let is_sound = !is_anim
                && (contains_ignore_ascii_case(tag, "sound")
                    || contains_ignore_ascii_case(tag, "audio")
                    || contains_ignore_ascii_case(tag, "proxy"));
            if !is_anim && !is_sound {
                continue;
            }
            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let clean = clean_xml_first_line(inner);
            if clean.is_empty() || is_numeric_token(&clean) {
                continue;
            }
            let kind = if is_anim {
                AssetResourceKind::Animation
            } else {
                AssetResourceKind::Sound
            };
            out.push((kind, clean));
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

    // Дочерние блюпринты
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
            let Some((prov, prod)) = provider_product_from_root(product_root) else {
                continue;
            };
            provider = prov;
            product = prod;
        }
        blueprint = blueprint.trim().to_string();
        if ends_with_ignore_ascii_case(&blueprint, ".xml") {
            blueprint.truncate(blueprint.len() - 4);
            blueprint.push_str(".bin");
        } else if !ends_with_ignore_ascii_case(&blueprint, ".bin")
            && !ends_with_ignore_ascii_case(&blueprint, ".geopcdx")
            && !ends_with_ignore_ascii_case(&blueprint, ".tgpcdx")
        {
            blueprint.push_str(".bin");
        }
        if !is_numeric_token(&blueprint) {
            out.push((
                AssetResourceKind::ChildBlueprint,
                format!("{provider}\\{product}\\{blueprint}"),
            ));
        }
    }

    // Текстуры террейна (cMixTexDescriptor)
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
                            out.push((AssetResourceKind::Texture, format!("{raw}_{suffix}")));
                        }
                    } else {
                        out.push((AssetResourceKind::Texture, raw));
                    }
                }
            }
        }
    }

    // Текстуры процедурной флоры
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

fn provider_product_from_root(product_root: &Path) -> Option<(String, String)> {
    let comps: Vec<_> = product_root.components().collect();
    let after_assets = comps
        .iter()
        .rposition(|c| c.as_os_str().eq_ignore_ascii_case("Assets"))
        .map(|idx| &comps[idx + 1..])
        .unwrap_or(&[]);
    let (prov, prod) = if after_assets.len() >= 2 {
        (
            after_assets[0].as_os_str().to_string_lossy().into_owned(),
            after_assets[1].as_os_str().to_string_lossy().into_owned(),
        )
    } else if comps.len() >= 2 {
        (
            comps[comps.len() - 2]
                .as_os_str()
                .to_string_lossy()
                .into_owned(),
            comps[comps.len() - 1]
                .as_os_str()
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        return None;
    };
    if prov.is_empty() || prod.is_empty() {
        None
    } else {
        Some((prov, prod))
    }
}

#[inline]
fn is_valid_texture_path(raw: &str) -> bool {
    !raw.is_empty() && !raw.contains(['<', '>', '\n', '\r', '\t'])
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
    !stem.is_empty()
        && !stem
            .chars()
            .all(|c| c.is_whitespace() || c.is_ascii_digit() || c == '[' || c == ']')
}

fn material_has_valid_texture(inner: &str) -> bool {
    for cap in re_texture_by_name().captures_iter(inner) {
        let block_inner = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for ecap in re_e_tag().captures_iter(block_inner) {
            let raw = unescape_xml_entities(ecap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
                return true;
            }
        }
        let raw = unescape_xml_entities(block_inner.trim());
        if !raw.contains('<') && is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
            return true;
        }
    }
    for re in [re_texture_name(), re_texture()] {
        for cap in re.captures_iter(inner) {
            let raw = unescape_xml_entities(cap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
                return true;
            }
        }
    }
    false
}

fn filter_material_blocks(xml: &str) -> Cow<'_, str> {
    let has_hc = contains_ignore_ascii_case(xml, "cHcExplicitStateMaterial");
    let has_dx = contains_ignore_ascii_case(xml, "cEffectMaterialDx");
    if !has_hc && !has_dx {
        return Cow::Borrowed(xml);
    }

    let filter_one = |caps: &regex::Captures| -> String {
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
    };

    let mut result = Cow::Borrowed(xml);
    if has_hc {
        result = Cow::Owned(re_hc_material().replace_all(&result, filter_one).into_owned());
    }
    if has_dx {
        result = Cow::Owned(re_dx_material().replace_all(&result, filter_one).into_owned());
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
            if is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
                out.push(normalize_texture_reference(&raw));
            }
        }
        let raw = unescape_xml_entities(inner.trim());
        if !raw.contains('<') && is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
            out.push(normalize_texture_reference(&raw));
        }
    }
    for re in [re_texture_name(), re_texture()] {
        for cap in re.captures_iter(&filtered) {
            let raw = unescape_xml_entities(cap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
            if is_valid_texture_path(&raw) && has_valid_texture_stem(&raw) {
                out.push(normalize_texture_reference(&raw));
            }
        }
    }
    let mut seen = HashSet::with_capacity(out.len());
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
    if ends_with_ignore_ascii_case(&stripped, ".tgpcdx") {
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
        let mut comps = rel.components();
        return match (comps.next(), comps.next()) {
            (Some(c0), Some(c1)) => assets_root.join(c0).join(c1),
            (Some(c0), None) => assets_root.join(c0),
            (None, _) => assets_root.to_path_buf(),
        };
    }
    dir.to_path_buf()
}

fn ensure_extension(raw: &str, kind: AssetResourceKind) -> String {
    const KNOWN_EXTS: &[&str] = &[
        ".geopcdx",
        ".tgpcdx",
        ".bin",
        ".xsec",
        ".lua",
        ".out",
        ".outz",
        ".ia",
        ".ban",
        ".xml",
        ".dav",
        ".proxybin",
        ".proxyxml",
    ];
    if KNOWN_EXTS
        .iter()
        .any(|ext| ends_with_ignore_ascii_case(raw, ext))
    {
        return raw.to_string();
    }
    match kind {
        AssetResourceKind::Texture => format!("{raw}.TgPcDx"),
        AssetResourceKind::Geometry => format!("{raw}.GeoPcDx"),
        AssetResourceKind::Animation => format!("{raw}.ban"),
        AssetResourceKind::ChildBlueprint => format!("{raw}.bin"),
        _ => raw.to_string(),
    }
}

fn candidate_relatives(raw: &str, kind: AssetResourceKind) -> Vec<String> {
    let raw_norm = raw.replace('/', "\\");
    let stripped = strip_lod_and_shader_prefix(&raw_norm);
    let mut bases = Vec::with_capacity(2);
    if stripped != raw_norm {
        bases.push(stripped);
        bases.push(raw_norm);
    } else {
        bases.push(raw_norm);
    }

    let mut out = Vec::new();
    for base in bases {
        let cleaned = if base.contains('/') {
            base.replace('/', "\\")
        } else {
            base
        };
        match kind {
            AssetResourceKind::Script => {
                let stem = if ends_with_ignore_ascii_case(&cleaned, ".lua")
                    || ends_with_ignore_ascii_case(&cleaned, ".out")
                    || ends_with_ignore_ascii_case(&cleaned, ".outz")
                {
                    match cleaned.rfind('.') {
                        Some(i) => &cleaned[..i],
                        None => &cleaned,
                    }
                } else {
                    &cleaned
                };
                out.push(format!("{stem}.out"));
                out.push(format!("{stem}.lua"));
            }
            AssetResourceKind::Texture => {
                let with_ext = ensure_extension(&cleaned, kind);
                let needs_textures_prefix = !starts_with_ignore_ascii_case(&with_ext, "textures\\");
                out.push(with_ext.clone());
                if needs_textures_prefix {
                    out.push(format!("textures\\{with_ext}"));
                }
            }
            AssetResourceKind::Animation => {
                let stem = if ends_with_ignore_ascii_case(&cleaned, ".ia")
                    || ends_with_ignore_ascii_case(&cleaned, ".ban")
                {
                    match cleaned.rfind('.') {
                        Some(i) => &cleaned[..i],
                        None => &cleaned,
                    }
                } else {
                    &cleaned
                };
                out.push(format!("{stem}.ban"));
                out.push(format!("{stem}.ia"));
                out.push(cleaned);
            }
            AssetResourceKind::ChildBlueprint => {
                if ends_with_ignore_ascii_case(&cleaned, ".bin")
                    || ends_with_ignore_ascii_case(&cleaned, ".xml")
                    || ends_with_ignore_ascii_case(&cleaned, ".geopcdx")
                    || ends_with_ignore_ascii_case(&cleaned, ".tgpcdx")
                    || ends_with_ignore_ascii_case(&cleaned, ".xsec")
                {
                    out.push(cleaned);
                } else {
                    out.push(format!("{cleaned}.bin"));
                    out.push(format!("{cleaned}.xml"));
                }
            }
            AssetResourceKind::Sound => {
                if ends_with_ignore_ascii_case(&cleaned, ".proxybin")
                    || ends_with_ignore_ascii_case(&cleaned, ".proxyxml")
                    || ends_with_ignore_ascii_case(&cleaned, ".bin")
                    || ends_with_ignore_ascii_case(&cleaned, ".xml")
                    || ends_with_ignore_ascii_case(&cleaned, ".dav")
                {
                    out.push(cleaned);
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

    let mut seen = HashSet::with_capacity(out.len());
    out.retain(|s| seen.insert(s.clone()));
    out
}

fn is_provider_under_assets(first: &str, assets_root: &Path, dir_cache: Option<&DirCache>) -> bool {
    if first.is_empty() || first.starts_with('.') {
        return false;
    }
    match dir_cache {
        Some(cache) => cache.has_provider(first),
        None => assets_root.join(first).is_dir(),
    }
}

fn is_global_asset_ref(
    norm: &str,
    kind: AssetResourceKind,
    assets_root: &Path,
    dir_cache: Option<&DirCache>,
) -> bool {
    if kind == AssetResourceKind::Texture {
        return false;
    }
    let clean = norm.trim_start_matches(['/', '\\']);
    if clean.starts_with('.') {
        return false;
    }
    if starts_with_ignore_ascii_case(clean, "assets/")
        || starts_with_ignore_ascii_case(clean, "assets\\")
    {
        return true;
    }
    let mut parts = clean.split(['/', '\\']).filter(|p| !p.is_empty());
    let (Some(p1), Some(_p2)) = (parts.next(), parts.next()) else {
        return false;
    };
    if p1 == "." || p1 == ".." || p1.starts_with('.') {
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
    KNOWN_PROVIDERS.iter().any(|k| p1.eq_ignore_ascii_case(k))
        || is_provider_under_assets(p1, assets_root, dir_cache)
}

fn normalize_asset_ref(raw: &str) -> String {
    let mut norm = raw.replace('\\', "/");
    if starts_with_ignore_ascii_case(&norm, "assets/") {
        norm.drain(.."assets/".len());
    }
    strip_shader_prefix(&norm).replace('/', "\\")
}

fn candidate_paths(
    raw: &str,
    parent: &Path,
    product_root: &Path,
    assets_root: &Path,
    kind: AssetResourceKind,
    dir_cache: Option<&DirCache>,
) -> Vec<PathBuf> {
    let norm = normalize_asset_ref(raw);
    let rels = candidate_relatives(&norm, kind);
    let mut cands = Vec::with_capacity(rels.len() * 3);

    for rel in &rels {
        let first = rel.split(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("");
        let provider_under_assets = is_provider_under_assets(first, assets_root, dir_cache);

        if kind == AssetResourceKind::Texture {
            if provider_under_assets {
                cands.push(normalize_path(&assets_root.join(rel)));
                cands.push(normalize_path(&parent.join(rel)));
                cands.push(normalize_path(&product_root.join(rel)));
            } else {
                cands.push(normalize_path(&parent.join(rel)));
                cands.push(normalize_path(&product_root.join(rel)));
                cands.push(normalize_path(&assets_root.join(rel)));
            }
        } else {
            let is_relative = rel.starts_with(".\\")
                || rel.starts_with("..\\")
                || rel.starts_with("./")
                || rel.starts_with("../");
            let local_subfolder = [
                "cab",
                "cabs",
                "textures",
                "texture",
                "audio",
                "sound",
                "sounds",
                "scripts",
                "script",
                "anim",
                "anims",
                "animation",
                "animations",
            ]
            .iter()
            .any(|s| first.eq_ignore_ascii_case(s));

            if is_relative {
                cands.push(normalize_path(&parent.join(rel)));
                cands.push(normalize_path(&product_root.join(rel)));
            } else if provider_under_assets {
                cands.push(normalize_path(&assets_root.join(rel)));
            } else if local_subfolder
                || !loose_path_exists_cached(&assets_root.join(rel), dir_cache)
            {
                cands.push(normalize_path(&parent.join(rel)));
                cands.push(normalize_path(&product_root.join(rel)));
                cands.push(normalize_path(&assets_root.join(rel)));
            } else {
                cands.push(normalize_path(&assets_root.join(rel)));
                cands.push(normalize_path(&parent.join(rel)));
                cands.push(normalize_path(&product_root.join(rel)));
            }
        }
    }

    let mut seen = HashSet::with_capacity(cands.len());
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
    let target = strip_shader_prefix(&name.to_string_lossy());
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let entry_name = entry.file_name();
            if strip_shader_prefix(&entry_name.to_string_lossy()).eq_ignore_ascii_case(&target) {
                return true;
            }
        }
    }
    false
}

#[inline]
fn loose_path_exists_cached(path: &Path, dir_cache: Option<&DirCache>) -> bool {
    match dir_cache {
        Some(cache) => cache.loose_path_exists(path),
        None => loose_path_exists(path),
    }
}

fn ap_exists(
    raw: &str,
    parent: &Path,
    product_root: &Path,
    assets_root: &Path,
    ap_cache: &ApArchiveCache,
    kind: AssetResourceKind,
    dir_cache: Option<&DirCache>,
) -> bool {
    let norm = normalize_asset_ref(raw);
    let first = norm
        .split(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let is_global = is_global_asset_ref(&norm, kind, assets_root, dir_cache)
        || is_provider_under_assets(first, assets_root, dir_cache);

    for rel in candidate_relatives(&norm, kind) {
        if is_global {
            let parts: Vec<&str> = rel.split('\\').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 3 {
                let product_dir = assets_root.join(parts[0]).join(parts[1]);
                let inner = parts[2..].join("/");
                if ap_cache.lookup(&product_dir, &inner) {
                    return true;
                }
            }
        } else {
            let target_loose = normalize_path(&parent.join(&rel));
            if let Ok(rel_from_prod) = target_loose.strip_prefix(product_root) {
                let inner = rel_from_prod.to_string_lossy().replace('\\', "/");
                if ap_cache.lookup(product_root, &inner) {
                    return true;
                }
            }
            let inner_direct = rel.trim_start_matches(".\\").replace('\\', "/");
            if ap_cache.lookup(product_root, &inner_direct) {
                return true;
            }
        }
    }
    false
}

fn register_texture_siblings_with(
    target: &Path,
    dir_cache: Option<&DirCache>,
    mut add_path: impl FnMut(PathBuf),
) {
    let (Some(dir), Some(stem_os)) = (target.parent(), target.file_stem()) else {
        return;
    };
    let stem = stem_os.to_string_lossy();
    const SEASON_SUFFIXES: [&str; 4] = ["_Wi", "_Su", "_Sp", "_Au"];
    let base: &str = if SEASON_SUFFIXES
        .iter()
        .any(|suf| ends_with_ignore_ascii_case(&stem, suf))
    {
        &stem[..stem.len().saturating_sub(3)]
    } else {
        &stem
    };
    for suf in SEASON_SUFFIXES {
        let seasonal = dir.join(format!("{base}{suf}.TgPcDx"));
        if loose_path_exists_cached(&seasonal, dir_cache) {
            add_path(normalize_path(&seasonal));
        }
    }
    if ends_with_ignore_ascii_case(&stem, "_nm") {
        let base_nm = &stem[..stem.len().saturating_sub(3)];
        let nmc = dir.join(format!("{base_nm}_nmc.TgPcDx"));
        if loose_path_exists_cached(&nmc, dir_cache) {
            add_path(normalize_path(&nmc));
        }
    } else if ends_with_ignore_ascii_case(&stem, "_nmc") {
        let base_nmc = &stem[..stem.len().saturating_sub(4)];
        let nm = dir.join(format!("{base_nmc}_nm.TgPcDx"));
        if loose_path_exists_cached(&nm, dir_cache) {
            add_path(normalize_path(&nm));
        }
    }
}

fn find_assets_root(start_path: &Path) -> Option<PathBuf> {
    start_path
        .ancestors()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("Assets"))
        })
        .map(Path::to_path_buf)
}

pub fn extract_raw_bin_references(bytes: &[u8], bin_path: &Path, outgoing: &mut Vec<PathBuf>) {
    extract_raw_bin_references_cached(bytes, bin_path, outgoing, None);
}

fn extract_raw_bin_references_cached(
    bytes: &[u8],
    bin_path: &Path,
    outgoing: &mut Vec<PathBuf>,
    dir_cache: Option<&DirCache>,
) {
    let mut outgoing_set: HashSet<PathBuf> = outgoing.iter().cloned().collect();
    for ext in ["GeoPcDx", "XSec"] {
        let paired = bin_path.with_extension(ext);
        if loose_path_exists_cached(&paired, dir_cache) {
            insert_unique_path(outgoing, &mut outgoing_set, normalize_path(&paired));
        }
    }
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
    let re_hex = re_hex_bracket();

    let mut try_register = |rel_str: &str| {
        let clean = rel_str.trim_matches(['\0', '"', '\'']).replace('/', "\\");
        let clean = clean.trim_start_matches('\\');
        let clean = re_hex.replace_all(clean, "");
        if clean.is_empty() {
            return;
        }
        let is_tgpcdx = ends_with_ignore_ascii_case(&clean, ".tgpcdx");
        if let Some(root) = &assets_root {
            let candidate = root.join(clean.as_ref());
            if loose_path_exists_cached(&candidate, dir_cache) {
                if is_tgpcdx {
                    register_texture_siblings_with(&candidate, dir_cache, |p| {
                        insert_unique_path(outgoing, &mut outgoing_set, p);
                    });
                }
                insert_unique_path(outgoing, &mut outgoing_set, normalize_path(&candidate));
                return;
            }
        }
        if let Some(dir) = parent_dir {
            let candidate = dir.join(clean.as_ref());
            if loose_path_exists_cached(&candidate, dir_cache) {
                if is_tgpcdx {
                    register_texture_siblings_with(&candidate, dir_cache, |p| {
                        insert_unique_path(outgoing, &mut outgoing_set, p);
                    });
                }
                insert_unique_path(outgoing, &mut outgoing_set, normalize_path(&candidate));
            }
        }
    };

    for mat in re_ext.find_iter(&lossy_text) {
        let s = mat.as_str();
        if ends_with_ignore_ascii_case(s, ".xml") {
            let base = &s[..s.len() - 4];
            try_register(&format!("{base}.bin"));
            try_register(&format!("{base}.GeoPcDx"));
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
            try_register(&format!("{prefix}{model}.GeoPcDx"));
            try_register(&format!("{prefix}{model}.XSec"));
            try_register(&format!("{prefix}{model}.TgPcDx"));
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
    let mut outgoing_set: HashSet<PathBuf> = outgoing.iter().cloned().collect();
    extract_xml_texture_id_refs_cached(
        xml,
        current_file_path,
        outgoing,
        &mut outgoing_set,
        broken,
        seen,
        parent_file,
        ap_cache,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn extract_xml_texture_id_refs_cached(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
    outgoing_set: &mut HashSet<PathBuf>,
    broken: &mut Vec<BrokenResourceRef>,
    seen: &mut HashMap<(AssetResourceKind, PathBuf, PathBuf), usize>,
    parent_file: &Path,
    ap_cache: &ApArchiveCache,
    dir_cache: Option<&DirCache>,
) {
    let assets_root = find_assets_root(current_file_path);
    let parent_dir = current_file_path.parent();
    let product_root = product_root_for(
        current_file_path,
        assets_root.as_deref().unwrap_or(Path::new("")),
    );

    static RE_TEXTURE_ID: OnceLock<Regex> = OnceLock::new();
    let re_tex = RE_TEXTURE_ID.get_or_init(|| {
        Regex::new(r"(?i)<[a-z0-9_]*(?:TextureID|TextureNormalID|NormalMapID|NormalID|TextureSlot)[^>]*>([^<]+)</")
            .unwrap()
    });
    let re_hex = re_hex_bracket();

    for cap in re_tex.captures_iter(xml) {
        let raw = cap[1].trim().replace('/', "\\");
        let raw = raw.trim_start_matches('\\');
        if raw.is_empty() {
            continue;
        }

        let clean_rel = re_hex.replace_all(raw, "").trim().to_string();
        if clean_rel.is_empty() || is_numeric_token(&clean_rel) {
            continue;
        }

        let file_rel = if ends_with_ignore_ascii_case(&clean_rel, ".tgpcdx") {
            clean_rel
        } else {
            format!("{clean_rel}.TgPcDx")
        };

        let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);
        if let Some(root) = &assets_root {
            candidates.push(normalize_path(&root.join(&file_rel)));
        }
        candidates.push(normalize_path(&product_root.join(&file_rel)));
        if !starts_with_ignore_ascii_case(&file_rel, "textures\\") {
            candidates.push(normalize_path(
                &product_root.join("textures").join(&file_rel),
            ));
        }
        if let Some(p_dir) = parent_dir {
            candidates.push(normalize_path(&p_dir.join(&file_rel)));
        }
        let mut seen_cands = HashSet::with_capacity(candidates.len());
        candidates.retain(|p| seen_cands.insert(p.clone()));

        let expected = assets_root
            .as_ref()
            .map(|r| r.join(&file_rel))
            .unwrap_or_else(|| PathBuf::from(&file_rel));

        if let Some(candidate) = candidates
            .iter()
            .find(|c| loose_path_exists_cached(c, dir_cache))
        {
            register_texture_siblings_with(candidate, dir_cache, |p| {
                insert_unique_path(outgoing, outgoing_set, p);
            });
            insert_unique_path(outgoing, outgoing_set, normalize_path(candidate));
        } else if !ap_exists(
            &file_rel,
            parent_dir.unwrap_or(Path::new("")),
            &product_root,
            assets_root.as_deref().unwrap_or(Path::new("")),
            ap_cache,
            AssetResourceKind::Texture,
            dir_cache,
        ) {
            record_broken_ref(
                AssetResourceKind::Texture,
                expected,
                parent_file,
                broken,
                seen,
            );
        }
    }
}

pub fn extract_xml_cross_section_refs(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
) {
    let mut outgoing_set: HashSet<PathBuf> = outgoing.iter().cloned().collect();
    extract_xml_cross_section_refs_cached(
        xml,
        current_file_path,
        outgoing,
        &mut outgoing_set,
        None,
    );
}

fn extract_xml_cross_section_refs_cached(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
    outgoing_set: &mut HashSet<PathBuf>,
    dir_cache: Option<&DirCache>,
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

        let clean = if let (Some(b_open), Some(b_close)) = (raw.find('['), raw.find(']'))
            && b_close >= b_open
        {
            format!("{}{}", &raw[..b_open], &raw[b_close + 1..])
        } else {
            raw.to_string()
        };

        let base = if ends_with_ignore_ascii_case(&clean, ".xsec") {
            &clean[..clean.len() - 5]
        } else if ends_with_ignore_ascii_case(&clean, ".geopcdx") {
            &clean[..clean.len() - 8]
        } else {
            &clean
        };

        let mut check_and_add = |candidate: PathBuf| {
            if loose_path_exists_cached(&candidate, dir_cache) {
                insert_unique_path(outgoing, outgoing_set, normalize_path(&candidate));
            }
        };

        if let Some(root) = &assets_root {
            check_and_add(root.join(format!("{base}.XSec")));
            check_and_add(root.join(format!("{base}.GeoPcDx")));
        }
        if let Some(p_dir) = parent_dir {
            check_and_add(p_dir.join(format!("{base}.XSec")));
            check_and_add(p_dir.join(format!("{base}.GeoPcDx")));
        }
    }
}

pub fn extract_absolute_blueprint_refs(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
) {
    let mut outgoing_set: HashSet<PathBuf> = outgoing.iter().cloned().collect();
    extract_absolute_blueprint_refs_cached(
        xml,
        current_file_path,
        outgoing,
        &mut outgoing_set,
        None,
    );
}

fn extract_absolute_blueprint_refs_cached(
    xml: &str,
    current_file_path: &Path,
    outgoing: &mut Vec<PathBuf>,
    outgoing_set: &mut HashSet<PathBuf>,
    dir_cache: Option<&DirCache>,
) {
    let assets_root = find_assets_root(current_file_path);

    for cap in re_blueprint_lib().captures_iter(xml) {
        let block = &cap[1];
        let Some(bp_mat) = re_blueprint_id().captures(block) else {
            continue;
        };
        let raw_bp = bp_mat[1].trim().replace('/', "\\");
        let raw_bp = raw_bp.trim_start_matches('\\');
        if raw_bp.is_empty() {
            continue;
        }

        let provider = re_provider()
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim())
            .unwrap_or("");
        let product = re_product()
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim())
            .unwrap_or("");

        let base_rel = if ends_with_ignore_ascii_case(raw_bp, ".xml") {
            &raw_bp[..raw_bp.len() - 4]
        } else {
            raw_bp
        };

        let base_dir = if !provider.is_empty() && !product.is_empty() {
            assets_root
                .as_ref()
                .map(|root| root.join(provider).join(product))
        } else {
            current_file_path.parent().map(Path::to_path_buf)
        };

        let Some(target_dir) = base_dir else { continue };

        for ext in ["bin", "GeoPcDx", "XSec"] {
            let candidate = target_dir.join(format!("{base_rel}.{ext}"));
            if loose_path_exists_cached(&candidate, dir_cache) {
                insert_unique_path(outgoing, outgoing_set, normalize_path(&candidate));
            }
        }
    }
}

/// Identifies common multi-language vehicle category directories.
pub fn is_vehicle_category_folder(name: &str) -> bool {
    const FOLDERS: &[&str] = &[
        "railvehicles",
        "elloks",
        "eloks",
        "dieselloks",
        "dampfloks",
        "personenwagen",
        "personenw",
        "gueterwagen",
        "guterwagen",
        "triebwagen",
        "steuerwagen",
        "rollingstock",
        "wagons",
        "triebzuege",
        "triebzug",
        "passagierwagen",
        "personenverkehr",
        "tender",
        "tenderloks",
        "cabview",
        "cabviews",
        "passview",
        "passengers",
    ];
    FOLDERS.iter().any(|f| name.eq_ignore_ascii_case(f))
}

/// Identifies dynamic Train Simulator Classic runtime assets that are resolved
/// procedurally (numbering digits, EBuLa/FIS timetables, passenger randomization,
/// destination displays) and therefore lack static references.
fn is_dynamic_runtime_asset(path: &Path) -> bool {
    const DYNAMIC_DIRS: &[&str] = &[
        "primarynumbers",
        "secondarynumbers",
        "digits",
        "numbers",
        "ebula",
        "fahrplan",
        "timetable",
        "matrix",
        "zzt",
        "zst",
        "destination",
        "destinations",
    ];
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| DYNAMIC_DIRS.iter().any(|d| s.eq_ignore_ascii_case(d)))
    })
}

/// Resolves the component root folder containing the script by walking up the directory tree
/// until it finds a recognized category folder, a parent directory hosting a .bin blueprint,
/// or the package boundary.
pub fn find_scripted_component_root(script_path: &Path) -> Option<PathBuf> {
    let mut curr = script_path.parent();
    let mut candidate_root = None;

    while let Some(dir) = curr {
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if dir_name.eq_ignore_ascii_case("Assets") {
            break;
        }
        if let Some(parent) = dir.parent()
            && let Some(p_name) = parent.file_name().and_then(|n| n.to_str())
            && is_vehicle_category_folder(p_name)
        {
            return Some(dir.to_path_buf());
        }
        if let Ok(entries) = fs::read_dir(dir) {
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

    candidate_root.or_else(|| script_path.parent().map(Path::to_path_buf))
}

fn record_broken_ref(
    kind: AssetResourceKind,
    expected: PathBuf,
    parent_file: &Path,
    broken: &mut Vec<BrokenResourceRef>,
    seen: &mut HashMap<(AssetResourceKind, PathBuf, PathBuf), usize>,
) {
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

#[allow(clippy::too_many_arguments)]
fn resolve_and_record_ref(
    raw: &str,
    kind: AssetResourceKind,
    parent: &Path,
    product_root: &Path,
    assets_root: &Path,
    default_base: &Path,
    parent_file: &Path,
    ap_cache: &ApArchiveCache,
    dir_cache: &DirCache,
    outgoing: &mut Vec<PathBuf>,
    outgoing_set: &mut HashSet<PathBuf>,
    broken: &mut Vec<BrokenResourceRef>,
    seen: &mut HashMap<(AssetResourceKind, PathBuf, PathBuf), usize>,
) {
    let cands = candidate_paths(
        raw,
        parent,
        product_root,
        assets_root,
        kind,
        Some(dir_cache),
    );
    let expected = cands
        .first()
        .cloned()
        .unwrap_or_else(|| default_base.join(raw));

    let matched_loose = cands.iter().find(|p| dir_cache.loose_path_exists(p));
    let exists = matched_loose.is_some()
        || ap_exists(
            raw,
            parent,
            product_root,
            assets_root,
            ap_cache,
            kind,
            Some(dir_cache),
        );

    let resolved = matched_loose.unwrap_or(&expected);
    insert_unique_path(outgoing, outgoing_set, normalize_path(resolved));

    if kind == AssetResourceKind::Texture
        && let Some(loose_target) = matched_loose
    {
        register_texture_siblings_with(loose_target, Some(dir_cache), |p| {
            insert_unique_path(outgoing, outgoing_set, p);
        });
    }

    if !exists {
        record_broken_ref(kind, expected, parent_file, broken, seen);
    }
}

fn audit_one_file(
    file: &Path,
    folder: &Path,
    assets_root: &Path,
    ap_cache: &ApArchiveCache,
    dir_cache: &DirCache,
) -> FileAuditOutcome {
    let xml = match crate::assets::read_serz_xml(file) {
        Ok(x) => x,
        Err(_) => match fs::read(file) {
            Ok(bytes) if bytes.starts_with(b"<?xm") => String::from_utf8_lossy(&bytes).into_owned(),
            Ok(bytes) => {
                let mut outgoing = Vec::new();
                if file
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("bin"))
                {
                    extract_raw_bin_references_cached(&bytes, file, &mut outgoing, Some(dir_cache));
                }
                return FileAuditOutcome {
                    parent: file.to_path_buf(),
                    broken: Vec::new(),
                    outgoing,
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

    let mut broken: Vec<BrokenResourceRef> = Vec::new();
    let mut seen: HashMap<(AssetResourceKind, PathBuf, PathBuf), usize> = HashMap::new();
    let mut outgoing: Vec<PathBuf> = Vec::new();
    let mut outgoing_set: HashSet<PathBuf> = HashSet::new();
    extract_absolute_blueprint_refs_cached(
        &xml,
        file,
        &mut outgoing,
        &mut outgoing_set,
        Some(dir_cache),
    );
    extract_xml_cross_section_refs_cached(
        &xml,
        file,
        &mut outgoing,
        &mut outgoing_set,
        Some(dir_cache),
    );

    let parent = file.parent().unwrap_or(folder);
    let product_root = product_root_for(file, assets_root);
    let parent_file = file.to_path_buf();

    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("bin") {
        extract_xml_texture_id_refs_cached(
            &xml,
            file,
            &mut outgoing,
            &mut outgoing_set,
            &mut broken,
            &mut seen,
            &parent_file,
            ap_cache,
            Some(dir_cache),
        );
        for (kind, raw) in extract_bin_references(&xml, &product_root) {
            resolve_and_record_ref(
                &raw,
                kind,
                parent,
                &product_root,
                assets_root,
                assets_root,
                &parent_file,
                ap_cache,
                dir_cache,
                &mut outgoing,
                &mut outgoing_set,
                &mut broken,
                &mut seen,
            );
        }
    } else if ext.eq_ignore_ascii_case("geopcdx") {
        for raw in extract_geo_texture_references(&xml) {
            resolve_and_record_ref(
                &raw,
                AssetResourceKind::Texture,
                parent,
                &product_root,
                assets_root,
                parent,
                &parent_file,
                ap_cache,
                dir_cache,
                &mut outgoing,
                &mut outgoing_set,
                &mut broken,
                &mut seen,
            );
        }
    } else if ext.eq_ignore_ascii_case("xsec") {
        for raw in extract_xsec_texture_references(&xml) {
            resolve_and_record_ref(
                &raw,
                AssetResourceKind::Texture,
                parent,
                &product_root,
                assets_root,
                parent,
                &parent_file,
                ap_cache,
                dir_cache,
                &mut outgoing,
                &mut outgoing_set,
                &mut broken,
                &mut seen,
            );
        }
    }

    FileAuditOutcome {
        parent: parent_file,
        broken,
        outgoing,
    }
}

fn is_not_preload(entry: &walkdir::DirEntry) -> bool {
    !entry
        .file_name()
        .to_str()
        .is_some_and(|n| n.eq_ignore_ascii_case("preload"))
}

fn record_script_root(
    script_path: &Path,
    cache: &mut HashMap<PathBuf, Option<PathBuf>>,
    scripted_roots: &mut HashSet<PathBuf>,
) {
    let Some(parent) = script_path.parent() else {
        return;
    };
    let root = cache
        .entry(parent.to_path_buf())
        .or_insert_with(|| find_scripted_component_root(script_path).map(|r| normalize_path(&r)));
    if let Some(root) = root {
        scripted_roots.insert(root.clone());
    }
}

fn insert_outcome_edges(
    outcome: &FileAuditOutcome,
    outgoing_map: &mut HashMap<PathBuf, HashSet<PathBuf>>,
    incoming_map: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) {
    let parent_norm = path_lower_key(&outcome.parent);
    let set = outgoing_map.entry(parent_norm.clone()).or_default();
    for t in &outcome.outgoing {
        let tn = path_lower_key(t);
        set.insert(tn.clone());
        incoming_map
            .entry(tn)
            .or_default()
            .insert(parent_norm.clone());
    }
}

pub fn audit_folders_with_progress(
    target_folders: &[PathBuf],
    assets_root: &Path,
    max_concurrent: usize,
    ap_cache: &ApArchiveCache,
    progress: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (AuditReport, FullAssetGraph) {
    let dir_cache = DirCache::new(assets_root);

    let mut files: Vec<PathBuf> = Vec::new();
    let mut file_set: HashSet<PathBuf> = HashSet::new();
    let mut texture_files: Vec<PathBuf> = Vec::new();
    let mut texture_set: HashSet<PathBuf> = HashSet::new();
    let mut scripted_component_roots: HashSet<PathBuf> = HashSet::new();
    let mut script_root_cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();

    for folder_path in target_folders {
        for entry in walkdir::WalkDir::new(folder_path)
            .into_iter()
            .filter_entry(is_not_preload)
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if ext.eq_ignore_ascii_case("lua") || ext.eq_ignore_ascii_case("out") {
                record_script_root(path, &mut script_root_cache, &mut scripted_component_roots);
            } else if ext.eq_ignore_ascii_case("bin")
                || ext.eq_ignore_ascii_case("geopcdx")
                || ext.eq_ignore_ascii_case("xsec")
            {
                let owned = path.to_path_buf();
                if file_set.insert(owned.clone()) {
                    files.push(owned);
                }
            } else if ext.eq_ignore_ascii_case("tgpcdx") {
                let owned = path.to_path_buf();
                if texture_set.insert(owned.clone()) {
                    texture_files.push(owned);
                }
            }
        }
    }

    // Дополнительные .bin/.geopcdx/.xsec в рамках продукта для полного графа зависимостей
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
        for entry in walkdir::WalkDir::new(&product_root)
            .into_iter()
            .filter_entry(is_not_preload)
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if (ext.eq_ignore_ascii_case("bin")
                || ext.eq_ignore_ascii_case("geopcdx")
                || ext.eq_ignore_ascii_case("xsec"))
                && !target_folders.iter().any(|tf| path.starts_with(tf))
            {
                let owned = path.to_path_buf();
                if extra_set.insert(owned.clone()) {
                    extra_files.push(owned);
                }
            } else if ext.eq_ignore_ascii_case("lua") || ext.eq_ignore_ascii_case("out") {
                record_script_root(path, &mut script_root_cache, &mut scripted_component_roots);
            }
        }
    }

    let threads = max_concurrent.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let processed = Arc::new(AtomicUsize::new(0));
    let total_files = files.len();

    let results: Vec<FileAuditOutcome> = pool.install(|| {
        files
            .par_iter()
            .map(|f| {
                let r = audit_one_file(f, assets_root, assets_root, ap_cache, &dir_cache);
                let done = processed.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                if let Some(cb) = &progress
                    && total_files > 0
                    && (done == total_files || done.is_multiple_of(10))
                {
                    cb(done, total_files);
                }
                r
            })
            .collect()
    });

    let mut broken_all: Vec<BrokenResourceRef> = Vec::new();
    let mut outgoing_map: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let mut incoming_map: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    for r in results {
        insert_outcome_edges(&r, &mut outgoing_map, &mut incoming_map);
        broken_all.extend(r.broken);
    }

    let extra_outcomes: Vec<FileAuditOutcome> = pool.install(|| {
        extra_files
            .par_iter()
            .map(|f| audit_one_file(f, assets_root, assets_root, ap_cache, &dir_cache))
            .collect()
    });
    for outcome in &extra_outcomes {
        insert_outcome_edges(outcome, &mut outgoing_map, &mut incoming_map);
    }

    // Регистронезависимая дедупликация битых ссылок
    let mut deduped: Vec<BrokenResourceRef> = Vec::with_capacity(broken_all.len());
    let mut seen: HashMap<(AssetResourceKind, PathBuf), usize> = HashMap::new();
    for mut b in broken_all {
        let key = (b.kind, path_lower_key(&b.expected_path));
        if let Some(&idx) = seen.get(&key) {
            deduped[idx].count += b.count;
            for p in b.all_parents {
                if !deduped[idx].all_parents.contains(&p) {
                    deduped[idx].all_parents.push(p);
                }
            }
            if deduped[idx].parent_file.as_os_str().is_empty() {
                deduped[idx].parent_file = deduped[idx]
                    .all_parents
                    .first()
                    .cloned()
                    .unwrap_or_default();
            }
        } else {
            seen.insert(key, deduped.len());
            if b.all_parents.is_empty() {
                b.all_parents = vec![b.parent_file.clone()];
            }
            if b.parent_file.as_os_str().is_empty() {
                b.parent_file = b.all_parents.first().cloned().unwrap_or_default();
            }
            deduped.push(b);
        }
    }

    // Достижимость от живых корней (.bin) — Mark-and-Sweep
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for f in files.iter().chain(extra_files.iter()) {
        if f.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("bin"))
        {
            let nf = path_lower_key(f);
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

    // Поиск неиспользуемых ресурсов (сирот)
    let scripted_roots_lower: Vec<PathBuf> = scripted_component_roots
        .iter()
        .map(|root| PathBuf::from(root.to_string_lossy().to_lowercase()))
        .collect();

    for f in files.iter().chain(texture_files.iter()) {
        let Some(ext) = f.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let is_geo_or_tex =
            ext.eq_ignore_ascii_case("geopcdx") || ext.eq_ignore_ascii_case("tgpcdx");
        if !is_geo_or_tex && !ext.eq_ignore_ascii_case("xsec") {
            continue;
        }
        let nf = path_lower_key(f);
        if visited.contains(&nf) {
            continue;
        }
        let product_root = product_root_for(f, assets_root);
        let rel_product = f
            .strip_prefix(&product_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| f.to_string_lossy().replace('\\', "/"));
        if ap_cache.lookup(&product_root, &rel_product) {
            continue;
        }
        if is_geo_or_tex && scripted_roots_lower.iter().any(|root| nf.starts_with(root)) {
            continue;
        }
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

    let files_on_disk: HashSet<PathBuf> = files
        .iter()
        .chain(texture_files.iter())
        .chain(extra_files.iter())
        .map(|f| path_lower_key(f))
        .collect();

    let mut missing_files: HashSet<PathBuf> = HashSet::new();
    let mut dead_files: HashSet<PathBuf> = HashSet::new();
    for b in &deduped {
        let ep = path_lower_key(&b.expected_path);
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

    (
        AuditReport {
            broken_references: deduped,
            dependency_graph: DependencyGraph {
                outgoing: outgoing_map,
                incoming: incoming_map,
            },
        },
        full_graph,
    )
}

pub fn filter_items(items: &mut Vec<BrokenResourceRef>, filter: Option<AssetResourceKind>) {
    if let Some(kind) = filter {
        items.retain(|item| item.kind == kind);
    }
}

fn cmp_path_ignore_ascii_case(a: &Path, b: &Path) -> Ordering {
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

pub fn sort_items(items: &mut [BrokenResourceRef], col: usize, asc: bool) {
    items.sort_by(|a, b| {
        let cmp = match col {
            0 => (a.kind as usize).cmp(&(b.kind as usize)),
            1 => cmp_path_ignore_ascii_case(&a.expected_path, &b.expected_path),
            _ => cmp_path_ignore_ascii_case(&a.parent_file, &b.parent_file),
        };
        if asc { cmp } else { cmp.reverse() }
    });
}
