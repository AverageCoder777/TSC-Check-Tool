//! Таблица инвентаря ассетов: список используемых объектов маршрута.\
//! Действия, запрошенные строками таблицы инвентаря.

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
