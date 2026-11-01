//! Таблица инвентаря ассетов: список используемых объектов маршрута.\
//! Действия, запрошенные строками таблицы инвентаря.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Столбец, по которому отсортирована таблица инвентаря.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum InventorySortColumn {
    /// По названию ассета.
    #[default]
    Path,
    /// По числу использований.
    Usage,
}

/// Сортировка таблицы инвентаря.
#[derive(Clone, Copy, Default)]
pub(crate) struct InventorySort {
    /// Столбец, по которому идёт сортировка.
    pub(crate) column: InventorySortColumn,
    /// Сортировка по убыванию.
    pub(crate) descending: bool,
}

/// Сортирует строки таблицы инвентаря.
/// Ширины столбцов таблицы инвентаря.
///
/// Считаются один раз для шапки и для строк, чтобы подписи и данные стояли
/// в одних и тех же местах.
/// Измеряет ширину текста в стиле мелкого шрифта.
pub(crate) fn asset_directory(assets_path: &Path, asset_path: &str) -> PathBuf {
    let normalized = asset_path.replace('/', "\\");
    let relative = Path::new(&normalized);
    match relative.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => assets_path.join(parent),
        _ => assets_path.to_path_buf(),
    }
}

/// Ближайшая существующая папка по пути: если нужной папки нет, поднимаемся выше.
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
) -> (Vec<slint::ModelRc<slint::StandardListViewItem>>, Vec<bool>) {
    let mut assets: Vec<&crate::domain::ReferencedAsset> = state.assets.iter().collect();
    match state.sort.column {
        InventorySortColumn::Path => {
            if state.sort.descending {
                assets.sort_by(|a, b| b.path.cmp(&a.path));
            } else {
                assets.sort_by(|a, b| a.path.cmp(&b.path));
            }
        }
        InventorySortColumn::Usage => {
            if state.sort.descending {
                assets.sort_by_key(|a| std::cmp::Reverse(a.uses.iter().map(|u| u.count as usize).sum::<usize>()));
            } else {
                assets.sort_by_key(|a| a.uses.iter().map(|u| u.count as usize).sum::<usize>());
            }
        }
    }
    let search = state.search.to_lowercase();
    let mut rows = Vec::new();
    let mut safe_flags = Vec::new();
    for a in assets {
        let usage_count: usize = a.uses.iter().map(|u| u.count as usize).sum();
        if show_missing && !a.missing {
            continue;
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
        let mut by_tile: BTreeMap<String, usize> = BTreeMap::new();
        for u in a.uses.iter() {
            *by_tile.entry(u.tile.clone()).or_insert(0) += u.count as usize;
        }
        let tiles: String = by_tile
            .iter()
            .map(|(tile, count)| format!("{} ({})", tile.trim_start_matches("тайл "), count))
            .collect::<Vec<_>>()
            .join(", ");
        let path_text = a.path.clone();

        let mut item0 = slint::StandardListViewItem::default();
        item0.text = path_text.as_str().into();
        let mut item1 = slint::StandardListViewItem::default();
        item1.text = usage_count.to_string().as_str().into();
        let mut item2 = slint::StandardListViewItem::default();
        item2.text = tiles.as_str().into();
        let row_items = vec![item0, item1, item2];
        let row: slint::ModelRc<slint::StandardListViewItem> =
            slint::ModelRc::new(slint::VecModel::from(row_items));
        rows.push(row);
        safe_flags.push(crate::assets::is_safe_for_global_replacement(
            &a.path, &a.uses,
        ));
    }
    (rows, safe_flags)
}
