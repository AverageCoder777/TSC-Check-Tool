use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

/// Канал запросов на асинхронную загрузку иконок (UI → фоновый поток).
static ICON_LOAD_TX: OnceLock<mpsc::Sender<PathBuf>> = OnceLock::new();
/// Канал результатов загрузки иконок (фоновый поток → UI): (путь, сырые байты файла).
static ICON_RESULT_RX: OnceLock<Arc<Mutex<mpsc::Receiver<(PathBuf, Vec<u8>)>>>> = OnceLock::new();

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
    static IN_FLIGHT_ICONS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// Инициализирует каналы и запускает фоновый поток чтения иконок.
pub(crate) fn init() {
    let (icon_load_tx, icon_load_rx) = mpsc::channel::<PathBuf>();
    let (icon_result_tx, icon_result_rx) = mpsc::channel::<(PathBuf, Vec<u8>)>();
    ICON_LOAD_TX.set(icon_load_tx).ok();
    ICON_RESULT_RX
        .set(Arc::new(Mutex::new(icon_result_rx)))
        .ok();

    thread::spawn(move || {
        while let Ok(path) = icon_load_rx.recv() {
            // При ошибке чтения отправляем пустой буфер, чтобы путь закрылся в кэше
            // и не запрашивался бесконечно.
            let data = std::fs::read(&path).unwrap_or_default();
            if icon_result_tx.send((path, data)).is_ok() {
                crate::schedule_ui_sync();
            }
        }
    });
}

pub(crate) fn clear_route_icon_cache() {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
    IN_FLIGHT_ICONS.with(|set| set.borrow_mut().clear());
}

pub(crate) fn try_get_route_icon(icon_path: &Path) -> Option<slint::Image> {
    ROUTE_ICON_CACHE.with(|cache| cache.borrow().get(icon_path).cloned())
}

pub(crate) fn request_route_icon_load(icon_path: &Path) {
    let already_handled = IN_FLIGHT_ICONS.with(|set| set.borrow().contains(icon_path))
        || ROUTE_ICON_CACHE.with(|cache| cache.borrow().contains_key(icon_path));
    if already_handled {
        return;
    }
    IN_FLIGHT_ICONS.with(|set| set.borrow_mut().insert(icon_path.to_path_buf()));
    if let Some(tx) = ICON_LOAD_TX.get() {
        let _ = tx.send(icon_path.to_path_buf());
    }
}

/// Вычитывает готовые иконки из канала в UI-потоке и кладёт их в кэш.
/// Возвращает `true`, если появилась хотя бы одна новая иконка.
pub(crate) fn drain_loaded_icons() -> bool {
    let mut updated = false;
    if let Some(rx_arc) = ICON_RESULT_RX.get()
        && let Ok(rx) = rx_arc.lock()
    {
        while let Ok((path, data)) = rx.try_recv() {
            let image = decode_icon_bytes(&data);
            ROUTE_ICON_CACHE.with(|cache| {
                cache.borrow_mut().insert(path.clone(), image);
            });
            IN_FLIGHT_ICONS.with(|set| set.borrow_mut().remove(&path));
            updated = true;
        }
    }
    updated
}

fn decode_icon_bytes(data: &[u8]) -> slint::Image {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        slint::Image::load_from_data(data, Some("png")).unwrap_or_default()
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        slint::Image::load_from_data(data, Some("jpeg")).unwrap_or_default()
    } else {
        slint::Image::default()
    }
}
