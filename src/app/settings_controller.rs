use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::SettingsWindow;
use crate::app::app_controller::{CheckerApp, LogLevel};
use crate::app::settings::*;

const SUPPORTED_LANGUAGES: [&str; 3] = ["ru", "en", "de"];
const SERZ_PRESETS: [usize; 9] = [1, 2, 4, 6, 8, 12, 16, 24, 32];

pub struct SettingsController {
    pub window: SettingsWindow,
    pub weak: Weak<SettingsWindow>,
    pub app: Arc<Mutex<CheckerApp>>,
}

/// Сохраняет текущие значения из окна настроек в конфиг и состояние приложения.
/// Вызывается и при нажатии «Сохранить», и при закрытии окна крестиком.
fn save_ui_settings(
    app: &Arc<Mutex<CheckerApp>>,
    weak: &Weak<SettingsWindow>,
) -> Result<(), String> {
    let sw = weak
        .upgrade()
        .ok_or_else(|| "Окно настроек недоступно".to_owned())?;
    let limit = sw.get_serz_process_limit().max(1) as usize;
    let locoswap_path = sw.get_locoswap_path().to_string();
    let game_path = sw.get_game_path().to_string();
    let thumbnails = sw.get_route_thumbnails_enabled();
    let hdd = sw.get_route_scan_on_hdd();
    let hide_scenarios = sw.get_hide_quick_drive_free_roam();
    let language = usize::try_from(sw.get_language_index())
        .ok()
        .and_then(|idx| SUPPORTED_LANGUAGES.get(idx))
        .copied()
        .unwrap_or(SUPPORTED_LANGUAGES[0])
        .to_owned();

    let mut config = AppConfig::load();
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
        app.rw_path = game_path;
        app.locoswap_path = locoswap_path;
        app.serz_process_limit = limit;
        app.route_thumbnails_enabled = thumbnails;
        app.route_scan_on_hdd = hdd;
        app.hide_quick_drive_free_roam = hide_scenarios;
        app.language = language.clone();

        apply_saved_language(&language);
        app.log_event("Settings saved.".to_owned(), LogLevel::Info);
        if path_changed {
            app.start_route_scan();
        }
        crate::schedule_ui_sync();
    }
    Ok(())
}

impl SettingsController {
    pub fn new(app: Arc<Mutex<CheckerApp>>) -> Self {
        let window = SettingsWindow::new().unwrap();
        let weak = window.as_weak();
        Self { window, weak, app }
    }

    /// Синхронизирует текущие настройки приложения с компонентами окна Slint.
    pub fn sync_from_app(&self) {
        let Ok(app) = self.app.lock() else { return };
        let available = maximum_serz_processes();
        let available_presets: Vec<usize> = SERZ_PRESETS
            .iter()
            .copied()
            .filter(|&p| p <= available)
            .collect();

        let clamped = app.serz_process_limit.clamp(1, available);
        let (index, current) = available_presets
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &p)| p <= clamped)
            .map(|(i, &p)| (i, p))
            .unwrap_or((0, 1));

        let preset_labels: Vec<SharedString> = available_presets
            .iter()
            .map(|p| p.to_string().into())
            .collect();

        self.window.set_serz_process_limit(current as i32);
        self.window.set_serz_process_limit_max(available as i32);
        self.window
            .set_serz_presets(ModelRc::new(VecModel::from(preset_labels)));
        self.window.set_serz_process_limit_index(index as i32);
        self.window
            .set_locoswap_path(app.locoswap_path.as_str().into());
        self.window
            .set_route_thumbnails_enabled(app.route_thumbnails_enabled);
        self.window.set_route_scan_on_hdd(app.route_scan_on_hdd);
        self.window
            .set_hide_quick_drive_free_roam(app.hide_quick_drive_free_roam);
        self.window.set_game_path(app.rw_path.as_str().into());

        let lang_options: Vec<SharedString> = SUPPORTED_LANGUAGES
            .iter()
            .map(|&s| SharedString::from(s))
            .collect();
        self.window
            .set_language_options(ModelRc::new(VecModel::from(lang_options)));
        self.window
            .set_language_index(language_index(&app.language));
    }

    /// Открывает окно настроек с гарантированной предварительной синхронизацией.
    pub fn show(&self) {
        self.sync_from_app();
        let _ = self.window.show();
    }

    pub fn bind(&self) {
        self.sync_from_app();
        self.bind_pick_locoswap();
        self.bind_pick_game();
        self.bind_language();
        self.bind_serz_preset();
        self.bind_settings_changed();
    }

    fn bind_pick_locoswap(&self) {
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_pick_locoswap(move || {
            let Some(sw) = weak.upgrade() else { return };
            let current = sw.get_locoswap_path().to_string();
            let mut dialog = rfd::FileDialog::new()
                .set_title("Select LocoSwap.exe")
                .add_filter("Executable (*.exe)", &["exe"]);
            if !current.is_empty()
                && let Some(parent) = PathBuf::from(&current).parent()
            {
                dialog = dialog.set_directory(parent);
            }
            if let Some(path) = dialog.pick_file() {
                sw.set_locoswap_path(path.to_string_lossy().as_ref().into());
                let _ = save_ui_settings(&app, &weak);
            }
        });
    }

    fn bind_pick_game(&self) {
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_pick_game(move || {
            let Some(sw) = weak.upgrade() else { return };
            let current = sw.get_game_path().to_string();
            let mut dialog = rfd::FileDialog::new().set_title("Select RailWorks folder");
            if !current.is_empty() {
                let p = PathBuf::from(&current);
                if p.is_dir() {
                    dialog = dialog.set_directory(&p);
                } else if let Some(parent) = p.parent() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_folder() {
                sw.set_game_path(path.to_string_lossy().as_ref().into());
                let _ = save_ui_settings(&app, &weak);
            }
        });
    }

    fn bind_language(&self) {
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_language_selected(move |lang| {
            let lang = lang.to_string();
            if let Ok(mut app) = app.lock() {
                app.language = lang.clone();
            }
            apply_saved_language(&lang);
            let _ = save_language(&lang);
            if let Some(sw) = weak.upgrade() {
                sw.set_language_index(language_index(&lang));
            }
        });
    }

    fn bind_serz_preset(&self) {
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_serz_preset_selected(move |i| {
            let Some(sw) = weak.upgrade() else { return };
            let Ok(idx) = usize::try_from(i) else { return };
            if let Some(item) = sw.get_serz_presets().row_data(idx)
                && let Ok(limit) = item.as_str().parse::<i32>()
            {
                sw.set_serz_process_limit(limit);
                let _ = save_ui_settings(&app, &weak);
            }
        });
    }

    fn bind_settings_changed(&self) {
        let app = self.app.clone();
        let weak = self.weak.clone();
        self.window.on_settings_changed(move || {
            let _ = save_ui_settings(&app, &weak);
        });
    }
}

fn language_index(lang: &str) -> i32 {
    SUPPORTED_LANGUAGES
        .iter()
        .position(|&l| l == lang)
        .unwrap_or(0) as i32
}
