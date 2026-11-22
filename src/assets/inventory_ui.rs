use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use slint::{ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::app::app_controller::AssetInventoryWindowState;
use crate::assets::replacement::is_safe_for_global_replacement;
use crate::common::domain::{ReferencedAsset, TileKind};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum InventorySortColumn {
    #[default]
    Path,
    Usage,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct InventorySort {
    pub(crate) column: InventorySortColumn,
    pub(crate) descending: bool,
}

pub fn has_suspicious_chars(value: &str) -> bool {
    !value.is_ascii()
}

pub(crate) fn asset_directory(assets_path: &Path, asset_path: &str) -> PathBuf {
    let normalized: Cow<'_, str> = if asset_path.contains('/') {
        Cow::Owned(asset_path.replace('/', "\\"))
    } else {
        Cow::Borrowed(asset_path)
    };
    match Path::new(normalized.as_ref()).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => assets_path.join(parent),
        _ => assets_path.to_path_buf(),
    }
}

pub(crate) fn existing_directory_upwards(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
}

pub fn build_inventory_rows(
    state: &AssetInventoryWindowState,
    show_missing: bool,
) -> (
    Vec<ModelRc<StandardListViewItem>>,
    Vec<bool>,
    Vec<bool>,
    Vec<usize>,
) {
    let search = state.search.trim().to_lowercase();
    let mut matched: Vec<(&ReferencedAsset, usize, usize)> = state
        .assets
        .iter()
        .enumerate()
        .filter(|(_, asset)| matches_filters(asset, show_missing, state.umlaut_filter, &search))
        .map(|(index, asset)| {
            let usage_count: usize = asset.uses.iter().map(|u| u.count as usize).sum();
            (asset, usage_count, index)
        })
        .collect();

    match state.sort.column {
        InventorySortColumn::Path => {
            if state.sort.descending {
                matched.sort_unstable_by(|(a, _, _), (b, _, _)| b.path.cmp(&a.path));
            } else {
                matched.sort_unstable_by(|(a, _, _), (b, _, _)| a.path.cmp(&b.path));
            }
        }
        InventorySortColumn::Usage => {
            if state.sort.descending {
                matched.sort_by(|(_, count_a, _), (_, count_b, _)| count_b.cmp(count_a));
            } else {
                matched.sort_by_key(|(_, count_a, _)| *count_a);
            }
        }
    }

    let len = matched.len();
    let mut rows = Vec::with_capacity(len);
    let mut safe_flags = Vec::with_capacity(len);
    let mut delete_flags = Vec::with_capacity(len);
    let mut filtered_indices = Vec::with_capacity(len);

    let mut by_tile: BTreeMap<&str, usize> = BTreeMap::new();
    let mut count_buf = String::with_capacity(16);
    let mut tiles_buf = String::with_capacity(128);

    for (asset, usage_count, original_index) in matched {
        count_buf.clear();
        let _ = write!(count_buf, "{usage_count}");

        format_tiles(asset, &mut by_tile, &mut tiles_buf);

        let row = vec![
            list_item(SharedString::from(asset.path.as_str())),
            list_item(SharedString::from(count_buf.as_str())),
            list_item(SharedString::from(tiles_buf.as_str())),
        ];
        rows.push(ModelRc::new(VecModel::from(row)));
        safe_flags.push(is_safe_for_global_replacement(&asset.path, &asset.uses));
        delete_flags.push(can_delete_asset(asset));
        filtered_indices.push(original_index);
    }

    (rows, safe_flags, delete_flags, filtered_indices)
}

fn matches_filters(
    asset: &ReferencedAsset,
    show_missing: bool,
    umlaut_filter: bool,
    search: &str,
) -> bool {
    if show_missing && !asset.missing {
        return false;
    }
    if umlaut_filter {
        let suspicious = has_suspicious_chars(&asset.path)
            || asset.uses.iter().any(|u| {
                has_suspicious_chars(&u.tile) || has_suspicious_chars(&u.source_file_search)
            });
        if !suspicious {
            return false;
        }
    }
    if !search.is_empty() {
        let matches = asset.path_search.contains(search)
            || asset
                .uses
                .iter()
                .any(|u| u.tile_search.contains(search) || u.source_file_search.contains(search));
        if !matches {
            return false;
        }
    }
    true
}

fn can_delete_asset(asset: &ReferencedAsset) -> bool {
    !asset.uses.is_empty()
        && asset.uses.iter().all(|u| {
            matches!(
                u.tile_kind,
                TileKind::Scenery | TileKind::Loft | TileKind::Road
            )
        })
}

fn format_tiles<'a>(
    asset: &'a ReferencedAsset,
    by_tile: &mut BTreeMap<&'a str, usize>,
    out: &mut String,
) {
    by_tile.clear();
    for u in &asset.uses {
        *by_tile.entry(u.tile.as_str()).or_insert(0) += u.count as usize;
    }

    out.clear();
    for (i, (&tile, &count)) in by_tile.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let clean_tile = tile.trim_start_matches("тайл ");
        let _ = write!(out, "{clean_tile} ({count})");
    }
}

fn list_item(text: SharedString) -> StandardListViewItem {
    let mut item = StandardListViewItem::default();
    item.text = text;
    item
}
