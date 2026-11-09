use slint::ComponentHandle;
use slint::Model;
use slint::Weak;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::SettingsWindow;

pub struct SettingsController {
    pub window: SettingsWindow,
    pub weak: Weak<SettingsWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
}

/// Сохраняет текущие значения из окна настроек в конфиг и состояние приложения.
/// Вызывается и при нажатии «Сохранить», и при закрытии окна крестиком.
fn save_ui_settings(
    app: Arc<Mutex<crate::app::CheckerApp>>,
    weak: slint::Weak<SettingsWindow>,
) -> Result<(), String> {
    let sw = weak
        .upgrade()
        .ok_or_else(|| "Окно настроек недоступно".to_owned())?;
    let limit = sw.get_serz_process_limit() as usize;
    let locoswap_path = sw.get_locoswap_path().to_string();
    let game_path = sw.get_game_path().to_string();
    let thumbnails = sw.get_route_thumbnails_enabled();
    let hdd = sw.get_route_scan_on_hdd();
    let hide_scenarios = sw.get_hide_quick_drive_free_roam();
    let lang_idx = sw.get_language_index() as usize;
    let langs = ["ru", "en", "de"];
    let language = langs.get(lang_idx).copied().unwrap_or("ru").to_owned();

    let mut config = crate::settings::AppConfig::load();
    config.game_path = (!game_path.is_empty()).then_some(PathBuf::from(&game_path));
    config.locoswap_path = (!locoswap_path.is_empty()).then_some(PathBuf::from(&locoswap_path));
    config.serz_process_limit = limit;
    config.route_thumbnails_enabled = thumbnails;
    config.route_scan_on_hdd = hdd;
    config.hide_quick_drive_free_roam = hide_scenarios;
    config.language = language.clone();
    config.save()?;

    if let Ok(mut app) = app.lock() {
        let path_changed = app.rw_path != game_path;
        app.rw_path = game_path.clone();
        app.locoswap_path = locoswap_path.clone();
        app.serz_process_limit = limit;
        app.route_thumbnails_enabled = thumbnails;
        app.route_scan_on_hdd = hdd;
        app.hide_quick_drive_free_roam = hide_scenarios;
        app.language = language.clone();

        crate::settings::apply_saved_language(&language);
        if path_changed {
            app.start_route_scan();
        }
        crate::schedule_ui_sync();
    }
    Ok(())
}

impl SettingsController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = SettingsWindow::new().unwrap();
        let weak = window.as_weak();
        Self { window, weak, app }
    }

    /// Синхронизирует текущие настройки приложения с компонентами окна Slint.
    pub fn sync_from_app(&self) {
        let Ok(app) = self.app.lock() else { return };
        let available = crate::settings::maximum_serz_processes();
        let all_presets: [usize; 9] = [1, 2, 4, 6, 8, 12, 16, 24, 32];
        let presets: Vec<slint::SharedString> = all_presets
            .iter()
            .filter(|p| **p <= available)
            .map(|p| p.to_string().into())
            .collect();
        let mut current = app.serz_process_limit.clamp(1, available);
        if !presets.iter().any(|p| *p == current.to_string()) {
            current = presets
                .iter()
                .filter(|p| p.parse::<usize>().map(|v| v <= current).unwrap_or(false))
                .map(|p| p.parse::<usize>().unwrap_or(1))
                .max()
                .unwrap_or(1);
        }
        let index = presets
            .iter()
            .position(|p| *p == current.to_string())
            .unwrap_or(0);

        self.window.set_serz_process_limit(current as i32);
        self.window.set_serz_process_limit_max(available as i32);
        self.window
            .set_serz_presets(slint::ModelRc::new(slint::VecModel::from(presets)));
        self.window.set_serz_process_limit_index(index as i32);
        self.window
            .set_locoswap_path(app.locoswap_path.clone().into());
        self.window
            .set_route_thumbnails_enabled(app.route_thumbnails_enabled);
        self.window.set_route_scan_on_hdd(app.route_scan_on_hdd);
        self.window
            .set_hide_quick_drive_free_roam(app.hide_quick_drive_free_roam);
        self.window.set_game_path(app.rw_path.clone().into());

        let langs = ["ru", "en", "de"];
        self.window
            .set_language_options(slint::ModelRc::new(slint::VecModel::from(
                langs
                    .iter()
                    .map(|s| (*s).to_string().into())
                    .collect::<Vec<slint::SharedString>>(),
            )));
        let lang_idx = langs.iter().position(|&l| l == app.language).unwrap_or(0);
        self.window.set_language_index(lang_idx as i32);
    }

    /// Открывает окно настроек с гарантированной предварительной синхронизацией.
    pub fn show(&self) {
        self.sync_from_app();
        let _ = self.window.show();
    }

    pub fn bind(&self) {
        self.sync_from_app();

        let settings_handle = self.weak.clone();
        let settings_handle3 = self.weak.clone();
        let settings_handle4 = self.weak.clone();
        let settings_handle5 = self.weak.clone();

        // Выбор пути к LocoSwap
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_pick_locoswap(move || {
            let Some(sw) = settings_handle.upgrade() else {
                return;
            };
            let current = sw.get_locoswap_path().to_string();
            let mut dialog = rfd::FileDialog::new()
                .set_title("Выберите LocoSwap.exe")
                .add_filter("Исполняемый файл (*.exe)", &["exe"]);
            if !current.is_empty() {
                let p = std::path::PathBuf::from(&current);
                if let Some(parent) = p.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_file() {
                sw.set_locoswap_path(path.to_string_lossy().to_string().into());
                let _ = save_ui_settings(app.clone(), weak.clone());
            }
        });

        // Выбор папки с игрой RailWorks
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_pick_game(move || {
            let Some(sw) = settings_handle3.upgrade() else {
                return;
            };
            let current = sw.get_game_path().to_string();
            let mut dialog = rfd::FileDialog::new().set_title("Выберите папку с игрой RailWorks");
            if !current.is_empty() {
                let p = std::path::PathBuf::from(&current);
                if p.is_dir() {
                    dialog = dialog.set_directory(&p);
                } else if let Some(parent) = p.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_folder() {
                sw.set_game_path(path.to_string_lossy().to_string().into());
                let _ = save_ui_settings(app.clone(), weak.clone());
            }
        });

        // Выбор языка
        let app = self.app.clone();
        self.window.on_language_selected(move |lang| {
            let lang = lang.to_string();
            if let Ok(mut app) = app.lock() {
                app.language = lang.clone();
            }
            crate::settings::apply_saved_language(&lang);
            let _ = crate::settings::save_language(&lang);
            if let Some(sw) = settings_handle5.upgrade() {
                let langs = ["ru", "en", "de"];
                let idx = langs.iter().position(|&l| l == lang).unwrap_or(0);
                sw.set_language_index(idx as i32);
            }
        });

        // Выбор пресета процессов serz
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_serz_preset_selected(move |i| {
            let Some(sw) = settings_handle4.upgrade() else {
                return;
            };
            let presets = sw.get_serz_presets();
            if let Some(item) = presets.row_data(i as usize)
                && let Ok(limit) = item.to_string().parse::<usize>()
            {
                sw.set_serz_process_limit(limit as i32);
                if let Ok(mut app) = app.lock() {
                    app.serz_process_limit = limit;
                }
                let _ = save_ui_settings(app.clone(), weak.clone());
            }
        });

        // Сохранение настроек при любом изменении (включая закрытие окна крестиком)
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_settings_changed(move || {
            let app = app.clone();
            let weak = weak.clone();
            let _ = save_ui_settings(app, weak);
        });
    }
}
