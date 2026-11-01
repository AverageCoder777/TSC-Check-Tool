//! Таблица инвентаря ассетов: список используемых объектов маршрута.

use std::collections::BTreeMap;
use std::fmt::Write;
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
    let search = state.search.trim().to_lowercase();

    // 1. Сначала фильтруем элементы и один раз вычисляем суммарное число использований
    let mut matched: Vec<(&crate::domain::ReferencedAsset, usize)> = Vec::new();
    for a in &state.assets {
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
        let usage_count: usize = a.uses.iter().map(|u| u.count as usize).sum();
        matched.push((a, usage_count));
    }

    // 2. Сортируем только отфильтрованные элементы по готовым числовым значениям
    match state.sort.column {
        InventorySortColumn::Path => {
            if state.sort.descending {
                matched.sort_by(|(a, _), (b, _)| b.path.cmp(&a.path));
            } else {
                matched.sort_by(|(a, _), (b, _)| a.path.cmp(&b.path));
            }
        }
        InventorySortColumn::Usage => {
            if state.sort.descending {
                matched.sort_by(|(_, count_a), (_, count_b)| count_b.cmp(count_a));
            } else {
                matched.sort_by(|(_, count_a), (_, count_b)| count_a.cmp(count_b));
            }
        }
    }

    // 3. Формируем строки для Slint UI
    let mut rows = Vec::with_capacity(matched.len());
    let mut safe_flags = Vec::with_capacity(matched.len());

    for (a, usage_count) in matched {
        // Заимствуем срезы &str в BTreeMap без аллокаций строк
        let mut by_tile: BTreeMap<&str, usize> = BTreeMap::new();
        for u in &a.uses {
            *by_tile.entry(u.tile.as_str()).or_insert(0) += u.count as usize;
        }

        // Собираем строку описания тайлов в один буфер
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
    }

    (rows, safe_flags)
}