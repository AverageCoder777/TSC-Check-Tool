//! Application state, background tasks, and Slint UI synchronization.

use super::*;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

enum CheckMessage {
    Progress(String),
    Finished(Result<crate::domain::CheckResult, String>),
}

#[allow(dead_code)]
pub(crate) enum InventoryAction {
    ScanUmlauts,
}

#[derive(Default)]
pub(crate) struct AssetInventoryWindowState {
    open: bool,
    pub(crate) search: String,
    show_umlauts: bool,
    show_tiles: bool,
    scanning_umlauts: bool,
    error: Option<String>,
    pub(crate) assets: Vec<crate::domain::ReferencedAsset>,
    umlaut_assets: Vec<crate::domain::UmlautAsset>,
    pub(crate) sort: crate::inventory_ui::InventorySort,
    pub(crate) filtered_indices: Vec<usize>,
    pub(crate) umlaut_filter: bool,
}

type InventoryScanOutcome = (Vec<crate::domain::UmlautAsset>, Vec<String>);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LogLevel {
    Info = 0,
    Warning = 1,
    Error = 2,
    Success = 3,
}

pub(crate) struct LogEntry {
    pub(crate) text: String,
    pub(crate) level: LogLevel,
}

pub(crate) struct CheckerApp {
    pub(crate) rw_path: String,
    pub(crate) routes: Vec<crate::domain::Route>,
    pub(crate) selected_route: Option<usize>,
    pub(crate) scenarios: Vec<crate::scenario::RouteScenario>,
    scenarios_route: Option<String>,
    pub(crate) selected_scenario: Option<usize>,
    pub(crate) selected_consist: Option<usize>,
    pub(crate) selected_vehicle: Option<usize>,
    pub(crate) scenario_window_open: bool,
    pub(crate) scenario_consist_info: Option<Vec<(String, bool)>>,
    pub(crate) scenario_vehicle_files: Option<Vec<Vec<bool>>>,
    pub(crate) search_query: String,
    ignore_scenarios: bool,
    pub(crate) result: Option<crate::domain::CheckResult>,
    pub(crate) status: String,
    pub(crate) check_log: Vec<LogEntry>,
    pub(crate) progress_pending: Option<String>,
    pub(crate) last_progress_log: Option<Instant>,
    pub(crate) has_ephemeral_progress: bool,
    pub(crate) incomplete_folders: Vec<crate::domain::IncompleteRouteFolder>,
    pub(crate) show_incomplete_warning: bool,
    pub(crate) ignore_incomplete_warning: bool,
    selected_incomplete: Option<usize>,
    pub(crate) ignored_route_ids: HashSet<String>,
    route_scan: Option<Receiver<Result<crate::domain::RouteScanOutcome, String>>>,
    check: Option<Receiver<CheckMessage>>,
    pub(crate) asset_replacement: Option<
        Receiver<(
            String,
            Result<crate::domain::AssetReplacementReport, String>,
        )>,
    >,
    pub(crate) asset_replacement_queue: VecDeque<(crate::domain::MissingAsset, PathBuf, String)>,
    asset_replacement_feedback: Option<Result<String, String>>,
    pub(crate) route_thumbnails_enabled: bool,
    pub(crate) serz_process_limit: usize,
    pub(crate) route_scan_on_hdd: bool,
    pub(crate) hide_quick_drive_free_roam: bool,
    pub(crate) locoswap_path: String,
    pub(crate) language: String,
    pub(crate) asset_inventory_state: Arc<Mutex<AssetInventoryWindowState>>,
    #[allow(dead_code)]
    pub(crate) inventory_action_tx: Sender<InventoryAction>,
    #[allow(dead_code)]
    inventory_action_rx: Receiver<InventoryAction>,
    inventory_scan: Option<Receiver<Result<InventoryScanOutcome, String>>>,
    pub(crate) missing_assets_popup: Option<Vec<String>>,
    pub(crate) missing_assets_shown: bool,
    pub(crate) inventory_refresh_pending: bool,
    pub(crate) inventory_replacement_pending_check: bool,
    pub(crate) synced_search_query: String,
    pub(crate) routes_revision: u64,
    pub(crate) synced_routes_revision: u64,
    pub(crate) synced_ignored_count: usize,
    pub(crate) synced_incomplete_count: usize,
    pub(crate) synced_thumbnails: bool,
}

impl Default for CheckerApp {
    fn default() -> Self {
        let config = crate::settings::AppConfig::load();
        let available = crate::settings::maximum_serz_processes();
        let rw_path = config
            .game_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(crate::settings::load_last_game_path);
        let (ignored_route_ids, status) = match crate::settings::load_ignored_route_ids() {
            Ok(ids) => (
                ids,
                "Укажите путь к RailWorks и просканируйте маршруты.".to_owned(),
            ),
            Err(error) => (HashSet::new(), error),
        };
        let ignore_incomplete_warning = config.ignore_incomplete_warning;
        let language = config.language.clone();
        let serz_process_limit = config.serz_process_limit.clamp(1, available);
        let route_thumbnails_enabled = config.route_thumbnails_enabled;
        let route_scan_on_hdd = config.route_scan_on_hdd;
        let hide_quick_drive_free_roam = config.hide_quick_drive_free_roam;
        let (inventory_action_tx, inventory_action_rx) = mpsc::channel();
        Self {
            rw_path,
            routes: Vec::new(),
            incomplete_folders: Vec::new(),
            show_incomplete_warning: false,
            ignore_incomplete_warning,
            selected_incomplete: None,
            ignored_route_ids,
            selected_route: None,
            scenarios: Vec::new(),
            scenarios_route: None,
            selected_scenario: None,
            selected_consist: None,
            selected_vehicle: None,
            scenario_window_open: false,
            scenario_consist_info: None,
            scenario_vehicle_files: None,
            search_query: String::new(),
            ignore_scenarios: true,
            result: None,
            status,
            check_log: Vec::new(),
            progress_pending: None,
            last_progress_log: None,
            has_ephemeral_progress: false,
            route_scan: None,
            check: None,
            asset_replacement: None,
            asset_replacement_queue: VecDeque::new(),
            asset_replacement_feedback: None,
            route_thumbnails_enabled,
            serz_process_limit,
            route_scan_on_hdd,
            hide_quick_drive_free_roam,
            locoswap_path: config
                .locoswap_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            language,
            asset_inventory_state: Arc::new(Mutex::new(AssetInventoryWindowState::default())),
            inventory_action_tx,
            inventory_action_rx,
            inventory_scan: None,
            missing_assets_popup: None,
            missing_assets_shown: false,
            inventory_refresh_pending: false,
            inventory_replacement_pending_check: false,
            synced_search_query: String::new(),
            routes_revision: 0,
            synced_routes_revision: 0,
            synced_ignored_count: 0,
            synced_incomplete_count: 0,
            synced_thumbnails: false,
        }
    }
}

pub(crate) fn clean_rw_path(rw_path: &str) -> PathBuf {
    crate::fs_utils::clean_rw_path(rw_path)
}

impl CheckerApp {
    pub(crate) fn clean_rw_path(&self) -> PathBuf {
        clean_rw_path(&self.rw_path)
    }
    pub(crate) fn open_asset_inventory(&mut self) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let window_error = match self.asset_inventory_state.lock() {
            Ok(mut state) => {
                state.assets = result.asset_inventory.clone();
                state.umlaut_assets = result.umlaut_assets.clone();
                state.search.clear();
                state.show_umlauts = false;
                state.show_tiles = true;
                state.error = None;
                state.open = true;
                None
            }
            Err(error) => Some(error.to_string()),
        };
        if let Some(error) = window_error {
            self.log_event(
                format!("Не удалось открыть инвентарь ассетов: {error}"),
                LogLevel::Error,
            );
        }
    }
    fn start_inventory_scan(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.log_event(
                "Сначала выполните проверку маршрута.".to_owned(),
                LogLevel::Warning,
            );
            return;
        };
        if self.inventory_scan.is_some() {
            return;
        }
        let inventory = result.asset_inventory.clone();
        let assets_path = self.clean_rw_path().join("Assets");
        let max_concurrent_serz = self.serz_process_limit;
        let (tx, rx) = mpsc::channel();
        self.inventory_scan = Some(rx);
        self.log_event(
            "Проверка ассетов маршрута на умляуты...".to_owned(),
            LogLevel::Info,
        );
        thread::spawn(move || {
            let result =
                crate::assets::scan_umlaut_assets(&inventory, &assets_path, max_concurrent_serz);
            let _ = tx.send(result);
            crate::schedule_ui_sync();
        });
    }
    pub(crate) fn start_asset_replacement(
        &mut self,
        missing: crate::domain::MissingAsset,
        replacement: PathBuf,
        transliterate: bool,
        replacement_relative: String,
        fixes: crate::translit::CharacterFixes,
    ) {
        if self.asset_replacement.is_some() {
            return;
        }
        let Some(route) = self
            .selected_route
            .and_then(|index| self.routes.get(index))
            .cloned()
        else {
            return;
        };
        let rw_path = self.clean_rw_path();
        let assets_path = rw_path.join("Assets");
        let (tx, rx) = mpsc::channel();
        self.asset_replacement = Some(rx);
        self.asset_replacement_feedback = None;
        self.log_event(
            format!("Подготовка замены ассета {}...", missing.path),
            LogLevel::Info,
        );
        thread::spawn(move || {
            let missing_path = missing.path.clone();
            let replacement = if transliterate {
                match crate::assets::repair_umlaut_asset_chain(
                    &route.path,
                    &assets_path,
                    &replacement_relative,
                    &[],
                    &fixes,
                ) {
                    Ok(report) => assets_path.join(report.new_asset_path),
                    Err(error) => {
                        let _ = tx.send((
                            missing_path,
                            Err(format!(
                                "Не удалось переименовать новый ассет под английские имена: {error}"
                            )),
                        ));
                        return;
                    }
                }
            } else {
                replacement
            };
            let result = crate::assets::replace_missing_asset(
                &route.path,
                &assets_path,
                &missing.path,
                &replacement,
                &missing.uses,
            )
            .map(|mut report| {
                let mut original_files: Vec<_> = missing
                    .uses
                    .iter()
                    .map(|usage| usage.source_file.display())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                original_files.sort();
                let timestamp_unix_seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut entry = crate::domain::AssetReplacementLogEntry {
                    timestamp_unix_seconds,
                    operation: "Замена отсутствующего ассета".to_owned(),
                    route_id: Some(route.id.clone()),
                    route_name: Some(route.name.clone()),
                    route_path: Some(route.path.clone()),
                    original_files,
                    missing_asset: Some(missing.path.clone()),
                    replacement_asset: Some(report.replacement_asset.clone()),
                    replaced_files: report.replaced_files,
                    replaced_instances: report.replaced_instances,
                    uses: missing
                        .uses
                        .iter()
                        .map(|usage| crate::domain::AssetReplacementLogUse {
                            tile: usage.tile.clone(),
                            source_file: usage.source_file.display(),
                        })
                        .collect(),
                    backups: report.backups.clone(),
                };
                let log_path = crate::settings::asset_replacement_log_file();
                match crate::settings::append_asset_replacement_log(&log_path, &mut entry) {
                    Ok(()) => report.log_file = Some(log_path),
                    Err(error) => report.log_error = Some(error),
                }
                report
            });
            let _ = tx.send((missing_path, result));
            crate::schedule_ui_sync();
        });
    }
    pub(crate) fn start_next_asset_replacement(&mut self) {
        if self.asset_replacement.is_some() {
            return;
        }
        if let Some((missing, replacement, replacement_relative)) =
            self.asset_replacement_queue.pop_front()
        {
            self.start_asset_replacement(
                missing,
                replacement,
                false,
                replacement_relative,
                BTreeMap::new(),
            );
        }
    }
    pub(crate) fn start_route_scan(&mut self) {
        self.begin_route_scan(false, true);
    }
    fn begin_route_scan(&mut self, preserve_incomplete: bool, allow_cache: bool) {
        if self.route_scan.is_some() {
            return;
        }
        let root = self.clean_rw_path();
        let (tx, rx) = mpsc::channel();
        self.route_scan = Some(rx);
        self.routes.clear();
        self.selected_route = None;
        crate::clear_route_icon_cache();
        if !preserve_incomplete {
            self.incomplete_folders.clear();
            self.selected_incomplete = None;
        }
        self.result = None;
        self.log_event("Сканирование маршрутов...".to_owned(), LogLevel::Info);
        crate::schedule_ui_sync();
        let ignored_route_ids = self.ignored_route_ids.clone();
        let max_threads = crate::settings::route_scan_max_threads(
            self.serz_process_limit,
            self.route_scan_on_hdd,
        );
        thread::spawn(move || {
            let result = crate::routes::scan_routes_with_cache(
                &root,
                &ignored_route_ids,
                allow_cache,
                max_threads,
            );
            let _ = tx.send(result);
            crate::schedule_ui_sync();
        });
    }
    pub(crate) fn start_check(&mut self) {
        if self.is_checking() {
            return;
        }
        let Some(route) = self
            .selected_route
            .and_then(|index| self.routes.get(index))
            .cloned()
        else {
            return;
        };
        let rw_path = self.clean_rw_path();
        let assets_path = rw_path.join("Assets");
        let ignore_scenarios = self.ignore_scenarios;
        let max_concurrent_serz = self.serz_process_limit;
        let (tx, rx) = mpsc::channel();
        self.check = Some(rx);
        self.result = None;
        self.missing_assets_popup = None;
        self.missing_assets_shown = false;
        self.progress_pending = None;
        self.last_progress_log = None;
        self.has_ephemeral_progress = false;
        if let Ok(mut state) = self.asset_inventory_state.lock() {
            state.open = false;
            state.assets.clear();
            state.umlaut_assets.clear();
            state.scanning_umlauts = false;
        }
        self.log_event(
            format!("Проверка маршрута «{}»...", route.name),
            LogLevel::Info,
        );
        crate::schedule_ui_sync();
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = crate::assets::check_route(
                &route.path,
                &assets_path,
                ignore_scenarios,
                max_concurrent_serz,
                |message| {
                    let _ = progress_tx.send(CheckMessage::Progress(message.to_owned()));
                    crate::schedule_ui_sync();
                },
            );
            let _ = tx.send(CheckMessage::Finished(result));
            crate::schedule_ui_sync();
        });
    }
    pub(crate) fn refresh_scenarios(&mut self) {
        let Some(route) = self.selected_route.and_then(|index| self.routes.get(index)) else {
            self.scenarios.clear();
            self.scenarios_route = None;
            self.selected_scenario = None;
            return;
        };
        if self.scenarios_route.as_deref() == Some(route.id.as_str()) {
            return;
        }
        self.scenarios = crate::scenario::read_route_scenarios(&route.path);
        self.scenarios_route = Some(route.id.clone());
        self.selected_scenario = None;
        self.selected_consist = None;
        self.selected_vehicle = None;
        self.scenario_consist_info = None;
        self.scenario_vehicle_files = None;
    }
    fn pop_ephemeral_progress(&mut self) {
        if self.has_ephemeral_progress {
            self.check_log.remove(0);
            self.has_ephemeral_progress = false;
        }
    }
    fn flush_progress_log(&mut self, force: bool) {
        let Some(pending) = self.progress_pending.take() else {
            return;
        };
        let due = force
            || self
                .last_progress_log
                .is_none_or(|instant| instant.elapsed() >= Duration::from_millis(200));
        if !due {
            self.progress_pending = Some(pending);
            return;
        }
        self.pop_ephemeral_progress();
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.check_log.insert(
            0,
            LogEntry {
                text: format!("[{}] {}\n", timestamp, pending),
                level: LogLevel::Info,
            },
        );
        self.has_ephemeral_progress = true;
        self.last_progress_log = Some(Instant::now());
    }
    pub(crate) fn log_event(&mut self, message: impl Into<String>, level: LogLevel) {
        let message = message.into();
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.status = message.clone();
        let new_entry = LogEntry {
            text: format!("[{}] {}\n", timestamp, message),
            level,
        };
        self.check_log.insert(0, new_entry);
    }
    pub(crate) fn is_checking(&self) -> bool {
        self.check.is_some()
    }
    pub(crate) fn receive_background_results(&mut self) {
        while let Ok(InventoryAction::ScanUmlauts) = self.inventory_action_rx.try_recv() {
            self.start_inventory_scan();
        }
        self.flush_progress_log(false);
        let inventory_scan_result =
            self.inventory_scan
                .as_ref()
                .and_then(|rx| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => Some(Err(
                        "Не удалось получить результат проверки умляутов.".to_owned(),
                    )),
                    Err(TryRecvError::Empty) => None,
                });
        if let Some(result) = inventory_scan_result {
            self.inventory_scan = None;
            match result {
                Ok((assets, errors)) => {
                    if let Some(check_result) = self.result.as_mut() {
                        check_result.umlaut_assets = assets.clone();
                        for error in &errors {
                            if !check_result.conversion_errors.contains(error) {
                                check_result.conversion_errors.push(error.clone());
                            }
                        }
                        check_result.conversion_errors.sort();
                    }
                    let window_error = match self.asset_inventory_state.lock() {
                        Ok(mut state) => {
                            state.umlaut_assets = assets.clone();
                            state.scanning_umlauts = false;
                            state.show_umlauts = true;
                            state.error = if errors.is_empty() {
                                None
                            } else {
                                Some(errors.join("\n"))
                            };
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    };
                    self.log_event(
                        if errors.is_empty() {
                            format!("Проверка умляутов завершена. Найдено: {}.", assets.len())
                        } else {
                            format!(
                                "Проверка завершена: найдено ассетов {}, ошибок анализа {}.",
                                assets.len(),
                                errors.len()
                            )
                        },
                        if errors.is_empty() {
                            LogLevel::Success
                        } else {
                            LogLevel::Warning
                        },
                    );
                    if let Some(error) = window_error {
                        self.status.push_str(&format!(
                            " Не удалось обновить состояние окна инвентаря: {error}"
                        ));
                    }
                }
                Err(error) => {
                    if let Ok(mut state) = self.asset_inventory_state.lock() {
                        state.scanning_umlauts = false;
                        state.error = Some(error.clone());
                    }
                    self.log_event(error, LogLevel::Error);
                }
            }
        }
        let asset_replacement_result =
            self.asset_replacement
                .as_ref()
                .and_then(|rx| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => Some((
                        String::new(),
                        Err("Не удалось получить результат замены ассетов.".to_owned()),
                    )),
                    Err(TryRecvError::Empty) => None,
                });
        if let Some((missing_path, result)) = asset_replacement_result {
            self.asset_replacement = None;
            match result {
                Ok(report) => {
                    let log_message = report.log_file.as_ref().map_or_else(
                        || {
                            report.log_error.as_ref().map_or_else(
                                || "Журнал замен недоступен.".to_owned(),
                                |error| format!("ЖУРНАЛ НЕ ЗАПИСАН: {error}"),
                            )
                        },
                        |path| format!("Журнал: {}", path.display()),
                    );
                    let remaining_assets = self.result.as_mut().map(|check_result| {
                        check_result
                            .missing_assets
                            .retain(|asset| asset.path != missing_path);
                        (
                            check_result.missing_assets.len(),
                            check_result.umlaut_assets.len(),
                        )
                    });
                    if let Some(check_result) = self.result.as_ref() {
                        let paths: Vec<String> = check_result
                            .missing_assets
                            .iter()
                            .map(|a| a.path.clone())
                            .collect();
                        if paths.is_empty() {
                            self.missing_assets_popup = None;
                            self.missing_assets_shown = false;
                        } else {
                            self.missing_assets_popup = Some(paths);
                        }
                    }
                    self.missing_assets_shown = false;
                    self.asset_replacement_feedback = Some(Ok(format!(
                        "Заменено размещений: {}; изменено файлов тайлов: {}. {log_message}",
                        report.replaced_instances, report.replaced_files,
                    )));
                    self.log_event(
                        format!(
                            "Замена объекта {} -> {} в маршруте ({} вхождений)",
                            missing_path, report.replacement_asset, report.replaced_instances
                        ),
                        LogLevel::Info,
                    );
                    self.inventory_refresh_pending = true;
                    if !report.backups.is_empty() {
                        self.log_event(
                            format!("Создано резервных копий: {}.", report.backups.len()),
                            LogLevel::Info,
                        );
                    }
                    let all_done = remaining_assets == Some((0, 0));
                    if all_done {
                        self.log_event(
                        "Все отсутствующие ассеты обработаны. Выполняется итоговая проверка маршрута...".to_owned(),
                        LogLevel::Info,
                    );
                        self.inventory_replacement_pending_check = false;
                        self.start_check();
                    } else if let Some((remaining, umlauts)) = remaining_assets {
                        self.log_event(
                            format!(
                                "Замена завершена. Осталось отсутствующих ассетов: {remaining}; ассетов с умляутами: {umlauts}."
                            ),
                            LogLevel::Info,
                        );
                    }
                }
                Err(error) => {
                    self.log_event(error.clone(), LogLevel::Error);
                    self.asset_replacement_feedback = Some(Err(error));
                }
            }
        }
        let was_queue_empty_before = self.asset_replacement_queue.is_empty();
        self.start_next_asset_replacement();
        if was_queue_empty_before
            && self.inventory_replacement_pending_check
            && self.asset_replacement.is_none()
            && !self.is_checking()
        {
            self.inventory_replacement_pending_check = false;
            self.start_check();
        }
        let route_result = self.route_scan.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Disconnected) => Some(Err(
                "Не удалось получить результат сканирования маршрутов.".to_owned(),
            )),
            Err(TryRecvError::Empty) => None,
        });
        if let Some(result) = route_result {
            self.route_scan = None;
            match result {
                Ok(outcome) => {
                    let routes = outcome.result;
                    let source = if outcome.from_cache {
                        " (загружено из кэша)"
                    } else {
                        " (полное сканирование)"
                    };
                    self.log_event(
                        format!(
                            "Найдено маршрутов: {}; неполных папков: {}.",
                            routes.routes.len(),
                            routes.incomplete_folders.len()
                        ),
                        LogLevel::Info,
                    );
                    self.status.push_str(source);
                    if let Some(notice) = outcome.notice {
                        self.status.push(' ');
                        self.status.push_str(&notice);
                    }
                    self.routes = routes.routes;
                    self.incomplete_folders = routes.incomplete_folders;
                    self.routes_revision = self.routes_revision.wrapping_add(1);
                    self.selected_incomplete = (!self.incomplete_folders.is_empty()).then_some(0);
                    self.selected_route = (!self.routes.is_empty()).then_some(0);
                    if !self.incomplete_folders.is_empty() && !self.ignore_incomplete_warning {
                        self.show_incomplete_warning = true;
                    }
                }
                Err(error) => self.log_event(error, LogLevel::Error),
            }
        }
        while self.check.is_some() {
            let check_message = {
                let rx = self.check.as_ref().unwrap();
                match rx.try_recv() {
                    Ok(message) => Some(Ok(message)),
                    Err(TryRecvError::Disconnected) => Some(Err(
                        "Не удалось получить результат проверки маршрута.".to_owned(),
                    )),
                    Err(TryRecvError::Empty) => None,
                }
            };
            let Some(check_message) = check_message else {
                break;
            };
            match check_message {
                Ok(CheckMessage::Progress(status)) => {
                    if status.starts_with("Анализ файла:") || status.starts_with("Анализ файлов:")
                    {
                        self.progress_pending = Some(status);
                    } else {
                        self.flush_progress_log(true);
                        self.pop_ephemeral_progress();
                        self.check_log.insert(
                            0,
                            LogEntry {
                                text: format!(
                                    "[{}] {}\n",
                                    chrono::Local::now().format("%H:%M:%S"),
                                    status
                                ),
                                level: LogLevel::Info,
                            },
                        );
                    }
                }
                Ok(CheckMessage::Finished(result)) => {
                    self.check = None;
                    match result {
                        Ok(result) => {
                            let mut summary = Vec::new();
                            if !result.missing_assets.is_empty() {
                                summary.push(format!(
                                    "отсутствует ассетов: {}",
                                    result.missing_assets.len()
                                ));
                                let paths: Vec<String> = result
                                    .missing_assets
                                    .iter()
                                    .map(|a| a.path.clone())
                                    .collect();
                                self.missing_assets_popup = Some(paths);
                            }
                            if !result.conversion_errors.is_empty() {
                                summary.push(format!(
                                    "ошибок Serz/XML: {}",
                                    result.conversion_errors.len()
                                ));
                            }
                            if !result.structure_issues.is_empty() {
                                summary.push(format!(
                                    "проблем структуры: {}",
                                    result.structure_issues.len()
                                ));
                            }
                            self.log_event(
                                if summary.is_empty() {
                                    "Проверка завершена: ошибки не найдены.".to_owned()
                                } else {
                                    format!("Проверка завершена: {}.", summary.join(", "))
                                },
                                LogLevel::Info,
                            );
                            self.flush_progress_log(true);
                            self.pop_ephemeral_progress();
                            let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
                            for issue in &result.structure_issues {
                                self.check_log.insert(
                                    0,
                                    LogEntry {
                                        text: format!(
                                            "[{}] Проблема структуры: {issue}\n",
                                            timestamp
                                        ),
                                        level: LogLevel::Warning,
                                    },
                                );
                            }
                            for error in &result.conversion_errors {
                                self.check_log.insert(
                                    0,
                                    LogEntry {
                                        text: format!("[{}] Ошибка Serz/XML: {error}\n", timestamp),
                                        level: LogLevel::Error,
                                    },
                                );
                            }
                            if let Ok(mut state) = self.asset_inventory_state.lock()
                                && state.open
                            {
                                state.assets = result.asset_inventory.clone();
                            }
                            self.result = Some(result);
                            self.inventory_refresh_pending = true;
                        }
                        Err(error) => self.log_event(error, LogLevel::Error),
                    }
                }
                Err(error) => {
                    self.check = None;
                    self.log_event(error, LogLevel::Error);
                }
            }
        }
    }
}
