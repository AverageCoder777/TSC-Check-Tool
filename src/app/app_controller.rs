//! Application state, background tasks, and Slint UI synchronization.
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender, TryRecvError},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app::settings::{
    AppConfig, append_asset_replacement_log, asset_replacement_log_file, load_ignored_route_ids,
    load_last_game_path, maximum_serz_processes, route_scan_max_threads,
};
use crate::assets::inventory_ui::InventorySort;
use crate::common::domain::{
    AssetReplacementLogEntry, AssetReplacementLogUse, AssetReplacementReport, CheckResult,
    IncompleteRouteFolder, MissingAsset, ReferencedAsset, Route, RouteScanOutcome, UmlautAsset,
};
use crate::common::fs_utils::clean_rw_path;
use crate::common::translit::CharacterFixes;
use crate::routes::clear_route_icon_cache;
use crate::scenarios::read_route_scenarios;

enum CheckMessage {
    Progress(String),
    Finished(Result<CheckResult, String>),
}

#[derive(Default)]
pub(crate) struct AssetInventoryWindowState {
    open: bool,
    pub(crate) search: String,
    show_umlauts: bool,
    show_tiles: bool,
    scanning_umlauts: bool,
    error: Option<String>,
    pub(crate) assets: Vec<ReferencedAsset>,
    umlaut_assets: Vec<UmlautAsset>,
    pub(crate) sort: InventorySort,
    pub(crate) filtered_indices: Vec<usize>,
    pub(crate) umlaut_filter: bool,
}

type InventoryScanOutcome = (Vec<UmlautAsset>, Vec<String>);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LogLevel {
    Info = 0,
    Warning = 1,
    Error = 2,
    Success = 3,
}

#[derive(Clone)]
pub(crate) struct LogEntry {
    pub(crate) text: String,
    pub(crate) level: LogLevel,
}

/// Жёсткий лимит записей в кольцевом буфере лога для UI.
const MAX_LOG_ENTRIES: usize = 1500;

pub(crate) struct CheckerApp {
    pub(crate) rw_path: String,
    pub(crate) routes: Vec<Route>,
    pub(crate) selected_route: Option<usize>,
    pub(crate) scenarios: Vec<crate::scenarios::RouteScenario>,
    scenarios_route: Option<String>,
    pub(crate) selected_scenario: Option<usize>,
    pub(crate) selected_consist: Option<usize>,
    pub(crate) selected_vehicle: Option<usize>,
    pub(crate) scenario_window_open: bool,
    pub(crate) scenario_consist_info: Option<Vec<(String, bool)>>,
    pub(crate) scenario_vehicle_files: Option<Vec<Vec<bool>>>,
    pub(crate) search_query: String,
    ignore_scenarios: bool,
    pub(crate) result: Option<CheckResult>,
    pub(crate) status: String,
    pub(crate) check_log: VecDeque<LogEntry>,
    pub(crate) log_tx: Option<Sender<LogEntry>>,
    pub(crate) progress_pending: Option<String>,
    pub(crate) last_progress_log: Option<Instant>,
    pub(crate) has_ephemeral_progress: bool,
    pub(crate) incomplete_folders: Vec<IncompleteRouteFolder>,
    pub(crate) show_incomplete_warning: bool,
    pub(crate) ignore_incomplete_warning: bool,
    selected_incomplete: Option<usize>,
    pub(crate) ignored_route_ids: HashSet<String>,
    route_scan: Option<Receiver<Result<RouteScanOutcome, String>>>,
    check: Option<Receiver<CheckMessage>>,
    pub(crate) asset_replacement:
        Option<Receiver<(String, Result<AssetReplacementReport, String>)>>,
    pub(crate) asset_replacement_queue: VecDeque<(MissingAsset, PathBuf, String)>,
    asset_replacement_feedback: Option<Result<String, String>>,
    pub(crate) route_thumbnails_enabled: bool,
    pub(crate) serz_process_limit: usize,
    pub(crate) route_scan_on_hdd: bool,
    pub(crate) hide_quick_drive_free_roam: bool,
    pub(crate) locoswap_path: String,
    pub(crate) language: String,
    pub(crate) asset_inventory_state: Arc<Mutex<AssetInventoryWindowState>>,
    inventory_scan: Option<Receiver<Result<InventoryScanOutcome, String>>>,
    pub(crate) missing_assets_popup: Option<Vec<String>>,
    pub(crate) missing_assets_shown: bool,
    pub(crate) route_properties_warnings_shown: bool,
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
        let config = AppConfig::load();
        let available = maximum_serz_processes();
        let rw_path = config
            .game_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(load_last_game_path);
        let (ignored_route_ids, status) = match load_ignored_route_ids() {
            Ok(ids) => (ids, crate::gettext!("Set RailWorks path and scan routes.")),
            Err(error) => (HashSet::new(), error),
        };
        Self {
            rw_path,
            routes: Vec::new(),
            incomplete_folders: Vec::new(),
            show_incomplete_warning: false,
            ignore_incomplete_warning: config.ignore_incomplete_warning,
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
            check_log: VecDeque::new(),
            progress_pending: None,
            last_progress_log: None,
            has_ephemeral_progress: false,
            route_scan: None,
            check: None,
            asset_replacement: None,
            asset_replacement_queue: VecDeque::new(),
            asset_replacement_feedback: None,
            route_thumbnails_enabled: config.route_thumbnails_enabled,
            serz_process_limit: config.serz_process_limit.clamp(1, available),
            route_scan_on_hdd: config.route_scan_on_hdd,
            hide_quick_drive_free_roam: config.hide_quick_drive_free_roam,
            locoswap_path: config
                .locoswap_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            language: config.language,
            asset_inventory_state: Arc::new(Mutex::new(AssetInventoryWindowState::default())),
            inventory_scan: None,
            missing_assets_popup: None,
            missing_assets_shown: false,
            route_properties_warnings_shown: false,
            inventory_refresh_pending: false,
            inventory_replacement_pending_check: false,
            synced_search_query: String::new(),
            routes_revision: 0,
            synced_routes_revision: 0,
            synced_ignored_count: 0,
            synced_incomplete_count: 0,
            synced_thumbnails: false,
            log_tx: None,
        }
    }
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
                crate::gettext!("Failed to open asset inventory: {}", error),
                LogLevel::Error,
            );
        }
    }

    pub(crate) fn start_asset_replacement(
        &mut self,
        missing: MissingAsset,
        replacement: PathBuf,
        transliterate: bool,
        replacement_relative: String,
        fixes: CharacterFixes,
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
        let assets_path = self.clean_rw_path().join("Assets");
        let (tx, rx) = mpsc::channel();
        self.asset_replacement = Some(rx);
        self.asset_replacement_feedback = None;
        self.log_event(
            crate::gettext!("Preparing asset replacement {}...", missing.path),
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
                            Err(crate::gettext!(
                                "Couldn't rename new asset to English names: {}",
                                error
                            )),
                        ));
                        crate::schedule_ui_sync();
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
                let mut entry = AssetReplacementLogEntry {
                    timestamp_unix_seconds,
                    operation: "Replacing missing asset".to_owned(),
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
                        .map(|usage| AssetReplacementLogUse {
                            tile: usage.tile.clone(),
                            source_file: usage.source_file.display(),
                        })
                        .collect(),
                    backups: report.backups.clone(),
                };
                let log_path = asset_replacement_log_file();
                match append_asset_replacement_log(&log_path, &mut entry) {
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
        clear_route_icon_cache();
        if !preserve_incomplete {
            self.incomplete_folders.clear();
            self.selected_incomplete = None;
        }
        self.result = None;
        self.log_event("Scanning routes".to_owned(), LogLevel::Info);
        crate::schedule_ui_sync();
        let ignored_route_ids = self.ignored_route_ids.clone();
        let max_threads = route_scan_max_threads(self.serz_process_limit, self.route_scan_on_hdd);
        let log_tx = self.log_tx.clone();
        thread::spawn(move || {
            let result = crate::routes::scan_routes_with_cache(
                &root,
                &ignored_route_ids,
                allow_cache,
                max_threads,
                log_tx.as_ref(),
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
        let assets_path = self.clean_rw_path().join("Assets");
        let ignore_scenarios = self.ignore_scenarios;
        let max_concurrent_serz = self.serz_process_limit;
        let (tx, rx) = mpsc::channel();
        self.check = Some(rx);
        self.result = None;
        self.missing_assets_popup = None;
        self.missing_assets_shown = false;
        self.route_properties_warnings_shown = false;
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
            crate::gettext!("Checking route «{}»", route.name),
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
        self.scenarios = read_route_scenarios(&route.path);
        self.scenarios_route = Some(route.id.clone());
        self.selected_scenario = None;
        self.selected_consist = None;
        self.selected_vehicle = None;
        self.scenario_consist_info = None;
        self.scenario_vehicle_files = None;
    }

    fn push_log_entry(&mut self, entry: LogEntry) {
        self.check_log.push_front(entry);
        if self.check_log.len() > MAX_LOG_ENTRIES {
            self.check_log.pop_back();
        }
    }

    fn pop_ephemeral_progress(&mut self) {
        if self.has_ephemeral_progress {
            self.check_log.pop_front();
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
        self.push_log_entry(LogEntry {
            text: format!("[{}] {pending}", current_timestamp()),
            level: LogLevel::Info,
        });
        self.has_ephemeral_progress = true;
        self.last_progress_log = Some(Instant::now());
    }

    pub(crate) fn log_event(&mut self, message: impl Into<String>, level: LogLevel) {
        self.pop_ephemeral_progress();
        let message = message.into();
        self.status = message.clone();
        let new_entry = LogEntry {
            text: format!("[{}] {message}", current_timestamp()),
            level,
        };
        if let Some(tx) = &self.log_tx {
            let _ = tx.send(new_entry.clone());
        }
        self.push_log_entry(new_entry);
    }

    pub(crate) fn is_checking(&self) -> bool {
        self.check.is_some()
    }

    pub(crate) fn receive_background_results(&mut self) {
        self.flush_progress_log(false);
        self.poll_inventory_scan();
        self.poll_asset_replacement();
        self.poll_route_scan();
        self.poll_check_messages();
    }

    fn poll_inventory_scan(&mut self) {
        let Some(result) = poll_receiver(
            &mut self.inventory_scan,
            &crate::gettext!("Failed to obtain umlauts check result."),
        ) else {
            return;
        };
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
                let (message, level) = if errors.is_empty() {
                    (
                        crate::gettext!("Umlauts check completed. Found: {}.", assets.len()),
                        LogLevel::Success,
                    )
                } else {
                    (
                        crate::gettext!(
                            "Check completed. Found: {}, analysis errors: {}.",
                            assets.len(),
                            errors.len()
                        ),
                        LogLevel::Warning,
                    )
                };
                self.log_event(message, level);
                if let Some(error) = window_error {
                    self.status.push_str(&crate::gettext!(
                        "Failed to update inventory window status: {}",
                        error
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

    fn poll_asset_replacement(&mut self) {
        let received = self
            .asset_replacement
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some((
                    String::new(),
                    Err(crate::gettext!(
                        "Failed to obtain asset replacement result."
                    )),
                )),
                Err(TryRecvError::Empty) => None,
            });
        if let Some((missing_path, result)) = received {
            self.asset_replacement = None;
            match result {
                Ok(report) => {
                    let log_message = report.log_file.as_ref().map_or_else(
                        || {
                            report.log_error.as_ref().map_or_else(
                                || crate::gettext!("Replacement log is unavailable."),
                                |error| crate::gettext!("LOG IS NOT RECORDED: {}", error),
                            )
                        },
                        |path| crate::gettext!("Log: {}", path.display()),
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
                        self.missing_assets_popup = (!paths.is_empty()).then_some(paths);
                    }
                    self.missing_assets_shown = false;
                    self.asset_replacement_feedback = Some(Ok(crate::gettext!(
                        "Replaced placements: {}; edited tiles: {}. {}",
                        report.replaced_instances,
                        report.replaced_files,
                        log_message
                    )));
                    self.log_event(
                        crate::gettext!(
                            "Replacing object {} -> {} on route ({} in total)",
                            missing_path,
                            report.replacement_asset,
                            report.replaced_instances
                        ),
                        LogLevel::Info,
                    );
                    self.inventory_refresh_pending = true;
                    if !report.backups.is_empty() {
                        self.log_event(
                            crate::ngettext!(
                                "Created {0} backup.",
                                "Created {0} backups.",
                                report.backups.len(),
                                report.backups.len()
                            ),
                            LogLevel::Info,
                        );
                    }
                    if remaining_assets == Some((0, 0)) {
                        self.log_event(
                            crate::gettext!(
                                "All missing assets have been processed. The final route check is being performed..."
                            ),
                            LogLevel::Info,
                        );
                        self.inventory_replacement_pending_check = false;
                        self.start_check();
                    } else if let Some((remaining, umlauts)) = remaining_assets {
                        self.log_event(
                            crate::gettext!(
                                "Replacement completed. Missing assets remaining: {0}; assets with umlauts: {1}.",
                                remaining,
                                umlauts
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
    }

    fn poll_route_scan(&mut self) {
        let Some(result) = poll_receiver(
            &mut self.route_scan,
            &crate::gettext!("Failed to obtain scanning routes result"),
        ) else {
            return;
        };
        match result {
            Ok(outcome) => {
                let routes = outcome.result;
                let source = if outcome.from_cache {
                    crate::gettext!(" (loaded from cache)")
                } else {
                    crate::gettext!(" (full scan)")
                };
                self.log_event(
                    crate::gettext!(
                        "Routes found: {}; incomplete routes: {}.",
                        routes.routes.len(),
                        routes.incomplete_folders.len()
                    ),
                    LogLevel::Info,
                );
                self.status.push_str(&source);
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

    fn poll_check_messages(&mut self) {
        while let Some(rx) = self.check.as_ref() {
            let check_message = match rx.try_recv() {
                Ok(message) => Ok(message),
                Err(TryRecvError::Disconnected) => {
                    Err(crate::gettext!("Failed to obtain route checking result."))
                }
                Err(TryRecvError::Empty) => break,
            };
            match check_message {
                Ok(CheckMessage::Progress(status)) => {
                    if status.starts_with("File analysis:") || status.starts_with("Files analysis:")
                    {
                        self.progress_pending = Some(status);
                    } else {
                        self.progress_pending = None;
                        self.pop_ephemeral_progress();
                        self.push_log_entry(LogEntry {
                            text: format!("[{}] {status}", current_timestamp()),
                            level: LogLevel::Info,
                        });
                    }
                }
                Ok(CheckMessage::Finished(result)) => {
                    self.check = None;
                    self.progress_pending = None;
                    self.pop_ephemeral_progress();
                    match result {
                        Ok(result) => {
                            let mut summary = Vec::new();
                            if !result.missing_assets.is_empty() {
                                summary.push(crate::gettext!(
                                    "missing assets count: {}",
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
                                summary.push(crate::gettext!(
                                    "Serz/XML errors count: {}",
                                    result.conversion_errors.len()
                                ));
                            }
                            if !result.structure_issues.is_empty() {
                                summary.push(crate::gettext!(
                                    "structural problems: {}",
                                    result.structure_issues.len()
                                ));
                            }
                            self.log_event(
                                if summary.is_empty() {
                                    crate::gettext!("Check completed: no errors.")
                                } else {
                                    crate::gettext!("Check completed: {}.", summary.join(", "))
                                },
                                LogLevel::Info,
                            );
                            let timestamp = current_timestamp();
                            for issue in &result.structure_issues {
                                self.push_log_entry(LogEntry {
                                    text: format!("[{timestamp}] Structure error: {issue}"),
                                    level: LogLevel::Warning,
                                });
                            }
                            for error in &result.conversion_errors {
                                self.push_log_entry(LogEntry {
                                    text: format!("[{timestamp}] Serz/XML error: {error}"),
                                    level: LogLevel::Error,
                                });
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
                    self.progress_pending = None;
                    self.log_event(error, LogLevel::Error);
                }
            }
        }
    }
}

fn current_timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn poll_receiver<T>(
    slot: &mut Option<Receiver<Result<T, String>>>,
    disconnect_msg: &str,
) -> Option<Result<T, String>> {
    let result = slot.as_ref().and_then(|rx| match rx.try_recv() {
        Ok(value) => Some(value),
        Err(TryRecvError::Disconnected) => Some(Err(disconnect_msg.to_owned())),
        Err(TryRecvError::Empty) => None,
    });
    if result.is_some() {
        *slot = None;
    }
    result
}
