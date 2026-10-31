//! egui application state, windows, and user interactions.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

enum CheckMessage {
    Progress(String),
    Finished(Result<crate::domain::CheckResult, String>),
}

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
}

type InventoryScanOutcome = (Vec<crate::domain::UmlautAsset>, Vec<String>);

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
    pub(crate) check_log: String,
    pub(crate) progress_pending: Option<String>,
    pub(crate) last_progress_log: Option<Instant>,
    pub(crate) last_progress_text: Option<String>,
    pub(crate) incomplete_folders: Vec<crate::domain::IncompleteRouteFolder>,
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
    pub(crate) asset_replacement_queue: Vec<(crate::domain::MissingAsset, PathBuf, String)>,
    asset_replacement_feedback: Option<Result<String, String>>,
    pub(crate) route_thumbnails_enabled: bool,
    pub(crate) serz_process_limit: usize,
    pub(crate) route_scan_on_hdd: bool,
    pub(crate) hide_quick_drive_free_roam: bool,
    pub(crate) locoswap_path: String,
    pub(crate) language: String,
    pub(crate) asset_inventory_state: Arc<Mutex<AssetInventoryWindowState>>,
    pub(crate) inventory_action_tx: Sender<InventoryAction>,
    inventory_action_rx: Receiver<InventoryAction>,
    inventory_scan: Option<Receiver<Result<InventoryScanOutcome, String>>>,
    pub(crate) missing_assets_popup: Option<Vec<String>>,
    pub(crate) missing_assets_shown: bool,
    pub(crate) inventory_refresh_pending: bool,
    pub(crate) inventory_replacement_pending_check: bool,
}

impl Default for CheckerApp {
    fn default() -> Self {
        let rw_path = crate::settings::load_last_game_path();
        let (ignored_route_ids, mut status) = match crate::settings::load_ignored_route_ids() {
            Ok(ids) => (
                ids,
                "Укажите путь к RailWorks и просканируйте маршруты.".to_owned(),
            ),
            Err(error) => (HashSet::new(), error),
        };
        let (route_thumbnails_enabled, settings_warning) =
            match crate::settings::load_route_thumbnail_setting() {
                Ok(enabled) => (enabled, None),
                Err(error) => (false, Some(error)),
            };
        let (serz_process_limit, serz_settings_warning) =
            match crate::settings::load_serz_process_limit() {
                Ok(limit) => (limit, None),
                Err(error) => (
                    DEFAULT_CONCURRENT_SERZ.min(crate::settings::maximum_serz_processes()),
                    Some(error),
                ),
            };
        let (route_scan_on_hdd, route_scan_hdd_warning) =
            match crate::settings::load_route_scan_on_hdd() {
                Ok(value) => (value, None),
                Err(error) => (false, Some(error)),
            };
        let (hide_quick_drive_free_roam, hide_scenarios_warning) =
            match crate::settings::load_hide_quick_drive_free_roam() {
                Ok(value) => (value, None),
                Err(error) => (false, Some(error)),
            };
        let language = crate::settings::load_language();
        let (inventory_action_tx, inventory_action_rx) = mpsc::channel();
        if let Some(warning) = settings_warning {
            status.push(' ');
            status.push_str(&warning);
        }
        if let Some(warning) = serz_settings_warning {
            status.push(' ');
            status.push_str(&warning);
        }
        if let Some(warning) = route_scan_hdd_warning {
            status.push(' ');
            status.push_str(&warning);
        }
        if let Some(warning) = hide_scenarios_warning {
            status.push(' ');
            status.push_str(&warning);
        }
        Self {
            rw_path,
            routes: Vec::new(),
            incomplete_folders: Vec::new(),
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
            check_log: String::new(),
            progress_pending: None,
            last_progress_log: None,
            last_progress_text: None,
            route_scan: None,
            check: None,
            asset_replacement: None,
            asset_replacement_queue: Vec::new(),
            asset_replacement_feedback: None,
            route_thumbnails_enabled,
            serz_process_limit,
            route_scan_on_hdd,
            hide_quick_drive_free_roam,
            locoswap_path: crate::settings::load_locoswap_path(),
            language,
            asset_inventory_state: Arc::new(Mutex::new(AssetInventoryWindowState::default())),
            inventory_action_tx,
            inventory_action_rx,
            inventory_scan: None,
            missing_assets_popup: None,
            missing_assets_shown: false,
            inventory_refresh_pending: false,
            inventory_replacement_pending_check: false,
        }
    }
}

impl CheckerApp {
    pub(crate) fn open_asset_inventory(&mut self) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        match self.asset_inventory_state.lock() {
            Ok(mut state) => {
                state.assets = result.asset_inventory.clone();
                state.umlaut_assets = result.umlaut_assets.clone();
                state.search.clear();
                state.show_umlauts = false;
                state.show_tiles = true;
                state.error = None;
                state.open = true;
            }
            Err(error) => self.status = format!("Не удалось открыть инвентарь ассетов: {error}"),
        }
    }

    fn start_inventory_scan(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.status = "Сначала выполните проверку маршрута.".to_owned();
            return;
        };
        if self.inventory_scan.is_some() {
            return;
        }
        let inventory = result.asset_inventory.clone();
        let assets_path = PathBuf::from(self.rw_path.trim()).join("Assets");
        let serz_path = crate::assets::preferred_serz_path(Path::new(self.rw_path.trim()));
        let max_concurrent_serz = self.serz_process_limit;
        let (tx, rx) = mpsc::channel();
        self.inventory_scan = Some(rx);
        self.status = "Проверка ассетов маршрута на умляуты...".to_owned();
        thread::spawn(move || {
            let result = crate::assets::scan_umlaut_assets(
                &inventory,
                &assets_path,
                &serz_path,
                max_concurrent_serz,
            );
            let _ = tx.send(result);
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
        let Some(route) = self
            .selected_route
            .and_then(|index| self.routes.get(index))
            .cloned()
        else {
            return;
        };
        let rw_path = PathBuf::from(self.rw_path.trim());
        let assets_path = rw_path.join("Assets");
        let serz_path = crate::assets::preferred_serz_path(&rw_path);
        let (tx, rx) = mpsc::channel();
        self.asset_replacement = Some(rx);
        self.asset_replacement_feedback = None;
        self.status = format!("Подготовка замены ассета {}...", missing.path);
        thread::spawn(move || {
            let missing_path = missing.path.clone();
            let replacement = if transliterate {
                match crate::assets::repair_umlaut_asset_chain(
                    &route.path,
                    &assets_path,
                    &serz_path,
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
                &serz_path,
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
                let mut entry = crate::domain::AssetReplacementLogEntry {
                    timestamp_unix_seconds: 0,
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
                match crate::settings::asset_replacement_log_file()
                    .ok_or_else(|| {
                        "Windows APPDATA не определена; журнал замен не сохранён.".to_owned()
                    })
                    .and_then(|path| {
                        crate::settings::append_asset_replacement_log(&path, &mut entry)
                            .map(|()| path)
                    }) {
                    Ok(path) => report.log_file = Some(path),
                    Err(error) => report.log_error = Some(error),
                }
                report
            });
            let _ = tx.send((missing_path, result));
        });
    }

    pub(crate) fn start_next_asset_replacement(&mut self) {
        if self.asset_replacement.is_some() {
            return;
        }
        if let Some((missing, replacement, replacement_relative)) =
            self.asset_replacement_queue.pop()
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
        let root = PathBuf::from(self.rw_path.trim());
        let (tx, rx) = mpsc::channel();
        self.route_scan = Some(rx);
        self.routes.clear();
        self.selected_route = None;
        if !preserve_incomplete {
            self.incomplete_folders.clear();
            self.selected_incomplete = None;
        }
        self.result = None;
        self.status = "Сканирование маршрутов...".to_owned();
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
        });
    }

    pub(crate) fn start_check(&mut self) {
        let Some(route) = self
            .selected_route
            .and_then(|index| self.routes.get(index))
            .cloned()
        else {
            return;
        };
        let rw_path = PathBuf::from(self.rw_path.trim());
        let assets_path = rw_path.join("Assets");
        let serz_path = crate::assets::preferred_serz_path(&rw_path);
        let ignore_scenarios = self.ignore_scenarios;
        let max_concurrent_serz = self.serz_process_limit;
        let (tx, rx) = mpsc::channel();
        self.check = Some(rx);
        self.result = None;
        self.missing_assets_popup = None;
        self.missing_assets_shown = false;
        self.check_log.clear();
        self.progress_pending = None;
        self.last_progress_log = None;
        self.last_progress_text = None;
        if let Ok(mut state) = self.asset_inventory_state.lock() {
            state.open = false;
            state.assets.clear();
            state.umlaut_assets.clear();
            state.scanning_umlauts = false;
        }
        self.status = format!("Проверка маршрута «{}»...", route.name);
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = crate::assets::check_route(
                &route.path,
                &assets_path,
                &serz_path,
                ignore_scenarios,
                max_concurrent_serz,
                |message| {
                    let _ = progress_tx.send(CheckMessage::Progress(message.to_owned()));
                },
            );
            let _ = tx.send(CheckMessage::Finished(result));
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
        self.scenarios = crate::scenario::read_route_scenarios(
            &route.path,
            &crate::assets::preferred_serz_path(Path::new(self.rw_path.trim())),
        );
        self.scenarios_route = Some(route.id.clone());
        self.selected_scenario = None;
        self.selected_consist = None;
        self.selected_vehicle = None;
        self.scenario_consist_info = None;
        self.scenario_vehicle_files = None;
    }

    fn flush_progress_log(&mut self, force: bool) {
        let Some(pending) = self.progress_pending.clone() else {
            return;
        };
        let due = force
            || self
                .last_progress_log
                .is_none_or(|instant| instant.elapsed() >= Duration::from_millis(200));
        if !due {
            return;
        }
        if let Some(previous) = self.last_progress_text.take() {
            let marker = format!("{previous}\n");
            if self.check_log.ends_with(&marker) {
                self.check_log.truncate(self.check_log.len() - marker.len());
            }
        }
        self.check_log.push_str(&pending);
        self.check_log.push('\n');
        self.last_progress_text = Some(pending);
        self.last_progress_log = Some(Instant::now());
        self.progress_pending = None;
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
                    self.status = if errors.is_empty() {
                        format!("Проверка умляутов завершена. Найдено: {}.", assets.len())
                    } else {
                        format!(
                            "Проверка завершена: найдено ассетов {}, ошибок анализа {}.",
                            assets.len(),
                            errors.len()
                        )
                    };
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
                    self.status = error;
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
                    self.inventory_refresh_pending = true;
                    if !report.backups.is_empty() {
                        self.status = format!("Создано резервных копий: {}.", report.backups.len());
                    }
                    if remaining_assets == Some((0, 0)) {
                        self.status = "Все отсутствующие ассеты обработаны. Выполняется итоговая проверка маршрута...".to_owned();
                        self.start_check();
                    } else if let Some((remaining, umlauts)) = remaining_assets {
                        self.status = format!(
                            "Замена завершена. Осталось отсутствующих ассетов: {remaining}; ассетов с умляутами: {umlauts}."
                        );
                    }
                }
                Err(error) => {
                    self.status = error.clone();
                    self.asset_replacement_feedback = Some(Err(error));
                }
            }
        }
        let was_last_inventory_replacement = self.asset_replacement_queue.is_empty();
        self.start_next_asset_replacement();
        if was_last_inventory_replacement && self.inventory_replacement_pending_check {
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
                    self.status = format!(
                        "Найдено маршрутов: {}; неполных папок: {}.",
                        routes.routes.len(),
                        routes.incomplete_folders.len()
                    );
                    self.status.push_str(source);
                    if let Some(notice) = outcome.notice {
                        self.status.push(' ');
                        self.status.push_str(&notice);
                    }
                    self.routes = routes.routes;
                    self.incomplete_folders = routes.incomplete_folders;
                    self.selected_incomplete = (!self.incomplete_folders.is_empty()).then_some(0);
                    self.selected_route = (!self.routes.is_empty()).then_some(0);
                }
                Err(error) => self.status = error,
            }
        }

        let check_message = self.check.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(message) => Some(Ok(message)),
            Err(TryRecvError::Disconnected) => Some(Err(
                "Не удалось получить результат проверки маршрута.".to_owned(),
            )),
            Err(TryRecvError::Empty) => None,
        });
        if let Some(message) = check_message {
            match message {
                Ok(CheckMessage::Progress(status)) => {
                    if status.starts_with("Анализ файла:") || status.starts_with("Анализ файлов:")
                    {
                        self.progress_pending = Some(status);
                    } else {
                        self.flush_progress_log(true);
                        self.check_log.push_str(&status);
                        self.check_log.push('\n');
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
                            self.status = if summary.is_empty() {
                                "Проверка завершена: ошибки не найдены.".to_owned()
                            } else {
                                format!("Проверка завершена: {}.", summary.join(", "))
                            };
                            self.flush_progress_log(true);
                            for issue in &result.structure_issues {
                                self.check_log
                                    .push_str(&format!("Проблема структуры: {issue}\n"));
                            }
                            for error in &result.conversion_errors {
                                self.check_log
                                    .push_str(&format!("Ошибка Serz/XML: {error}\n"));
                            }
                            let inventory = result.asset_inventory.clone();
                            self.result = Some(result);
                            if let Ok(mut state) = self.asset_inventory_state.lock()
                                && state.open
                            {
                                state.assets = inventory;
                            }
                            self.inventory_refresh_pending = true;
                        }
                        Err(error) => self.status = error,
                    }
                }
                Err(error) => {
                    self.check = None;
                    self.status = error;
                }
            }
        }
    }
}
