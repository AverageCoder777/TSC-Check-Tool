use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;

type LoadRequest = (u64, PathBuf);
type LoadedIcon = (u64, PathBuf, Vec<u8>);

static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);
static ICON_LOAD_TX: OnceLock<mpsc::Sender<LoadRequest>> = OnceLock::new();
static ICON_RESULT_RX: OnceLock<Mutex<mpsc::Receiver<LoadedIcon>>> = OnceLock::new();

thread_local! {
    static ROUTE_ICON_CACHE: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
    static IN_FLIGHT_ICONS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// Инициализирует каналы и запускает фоновый поток чтения иконок.
pub(crate) fn init() {
    if ICON_LOAD_TX.get().is_some() {
        return;
    }
    let (icon_load_tx, icon_load_rx) = mpsc::channel::<LoadRequest>();
    let (icon_result_tx, icon_result_rx) = mpsc::channel::<LoadedIcon>();
    if ICON_LOAD_TX.set(icon_load_tx).is_err() {
        return;
    }
    let _ = ICON_RESULT_RX.set(Mutex::new(icon_result_rx));

    thread::spawn(move || {
        while let Ok((generation, path)) = icon_load_rx.recv() {
            if generation != CACHE_GENERATION.load(Ordering::Relaxed) {
                continue;
            }
            let data = fs::read(&path).unwrap_or_default();
            if generation != CACHE_GENERATION.load(Ordering::Relaxed) {
                continue;
            }
            if icon_result_tx.send((generation, path, data)).is_ok() {
                crate::schedule_ui_sync();
            }
        }
    });
}

pub(crate) fn clear_route_icon_cache() {
    CACHE_GENERATION.fetch_add(1, Ordering::Relaxed);
    ROUTE_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
    IN_FLIGHT_ICONS.with(|set| set.borrow_mut().clear());
    if let Some(rx_mutex) = ICON_RESULT_RX.get()
        && let Ok(rx) = rx_mutex.lock()
    {
        while rx.try_recv().is_ok() {}
    }
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
    let Some(tx) = ICON_LOAD_TX.get() else {
        return;
    };
    let generation = CACHE_GENERATION.load(Ordering::Relaxed);
    let owned = icon_path.to_path_buf();
    if tx.send((generation, owned.clone())).is_ok() {
        IN_FLIGHT_ICONS.with(|set| set.borrow_mut().insert(owned));
    }
}

/// Вычитывает готовые иконки из канала в UI-потоке и кладёт их в кэш.
/// Возвращает `true`, если появилась хотя бы одна новая иконка.
pub(crate) fn drain_loaded_icons() -> bool {
    let current_gen = CACHE_GENERATION.load(Ordering::Relaxed);
    let mut updated = false;
    if let Some(rx_mutex) = ICON_RESULT_RX.get()
        && let Ok(rx) = rx_mutex.lock()
    {
        while let Ok((generation, path, data)) = rx.try_recv() {
            if generation != current_gen {
                continue;
            }
            let image = decode_icon_bytes(&data);
            IN_FLIGHT_ICONS.with(|set| set.borrow_mut().remove(&path));
            ROUTE_ICON_CACHE.with(|cache| {
                cache.borrow_mut().insert(path, image);
            });
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
