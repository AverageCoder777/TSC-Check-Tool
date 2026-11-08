use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

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
    value.chars().any(|c| !c.is_ascii() || matches!(c, '╞' | '¤' | '�'))
}

pub(crate) fn asset_directory(assets_path: &Path, asset_path: &str) -> PathBuf {
    let normalized = asset_path.replace('/', "\\");
    let relative = Path::new(&normalized);
    match relative.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => assets_path.join(parent),
        _ => assets_path.to_path_buf(),
    }
}

pub(crate) fn existing_directory_upwards(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.is_dir() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

pub fn build_inventory_rows(
    state: &crate::app::AssetInventoryWindowState,
    show_missing: bool,
) -> (Vec<slint::ModelRc<slint::StandardListViewItem>>, Vec<bool>, Vec<bool>, Vec<usize>) {
    let search = state.search.trim().to_lowercase();
    let mut matched: Vec<(&crate::domain::ReferencedAsset, usize, usize)> = Vec::new();
    for (index, a) in state.assets.iter().enumerate() {
        if show_missing && !a.missing {
            continue;
        }
        if state.umlaut_filter {
            let suspicious = crate::inventory_ui::has_suspicious_chars(&a.path)
                || a.uses.iter().any(|u| {
                    crate::inventory_ui::has_suspicious_chars(&u.tile)
                        || crate::inventory_ui::has_suspicious_chars(&u.source_file_search)
                });
            if !suspicious {
                continue;
            }
        }
        if !search.is_empty() {
            let matches = a.path_search.contains(&search)
                || a.uses.iter().any(|u| {
                    u.tile_search.contains(&search) || u.source_file_search.contains(&search)
                });
            if !matches {
                continue;
            }
        }
        let usage_count: usize = a.uses.iter().map(|u| u.count as usize).sum();
        matched.push((a, usage_count, index));
    }
    match state.sort.column {
        InventorySortColumn::Path => {
            if state.sort.descending {
                matched.sort_by(|(a, _, _), (b, _, _)| b.path.cmp(&a.path));
            } else {
                matched.sort_by(|(a, _, _), (b, _, _)| a.path.cmp(&b.path));
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
    let mut rows = Vec::with_capacity(matched.len());
    let mut safe_flags = Vec::with_capacity(matched.len());
    let mut delete_flags = Vec::with_capacity(matched.len());
    let mut filtered_indices = Vec::with_capacity(matched.len());
    for (a, usage_count, original_index) in matched {
        // Заимствуем срезы &str в BTreeMap без аллокаций строк
        let mut by_tile: BTreeMap<&str, usize> = BTreeMap::new();
        for u in &a.uses {
            *by_tile.entry(u.tile.as_str()).or_insert(0) += u.count as usize;
        }
        let mut tiles = String::new();
        for (i, (tile, count)) in by_tile.into_iter().enumerate() {
            if i > 0 {
                tiles.push_str(", ");
            }
            let clean_tile = tile.trim_start_matches("тайл ");
            let _ = write!(tiles, "{clean_tile} ({count})");
        }
        let mut item0 = slint::StandardListViewItem::default();
        item0.text = slint::SharedString::from(a.path.as_str());
        let mut item1 = slint::StandardListViewItem::default();
        item1.text = format!("{usage_count}").into();
        let mut item2 = slint::StandardListViewItem::default();
        item2.text = tiles.into();
        rows.push(slint::ModelRc::new(slint::VecModel::from(vec![
            item0, item1, item2,
        ])));
        safe_flags.push(crate::assets::is_safe_for_global_replacement(
            &a.path, &a.uses,
        ));
        delete_flags.push(a.uses.iter().all(|u| u.tile_kind == crate::domain::TileKind::Scenery));
        filtered_indices.push(original_index);
    }
    (rows, safe_flags, delete_flags, filtered_indices)
}
