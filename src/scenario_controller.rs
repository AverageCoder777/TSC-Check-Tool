use slint::ComponentHandle;
use slint::Weak;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::{Path, PathBuf};
use std::collections::HashMap;

use crate::ScenarioWindow;

pub struct ScenarioController {
    pub window: ScenarioWindow,
    pub weak: Weak<ScenarioWindow>,
    pub app: Arc<Mutex<crate::app::CheckerApp>>,
    pub scenario_generation: Arc<AtomicU64>,
}

impl ScenarioController {
    pub fn new(app: Arc<Mutex<crate::app::CheckerApp>>) -> Self {
        let window = ScenarioWindow::new().unwrap();
        let weak = window.as_weak();
        Self { window, weak, app, scenario_generation: Arc::new(AtomicU64::new(0)) }
    }

    pub fn bind(&self, ui: &crate::MainWindow) {
        let scenario_handle = self.weak.clone();
        let scenario_handle2 = self.weak.clone();
        let ui_handle3 = ui.as_weak();
    let app_for_scenario = self.app.clone();
    let scenario_generation = self.scenario_generation.clone();
    ui.on_scenario_selected(move |idx| {
        let generation_id = scenario_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app_for_scenario.clone();
        let scenario_generation = scenario_generation.clone();
        let ui_handle = ui_handle3.clone();
        let scenario_handle = scenario_handle.clone();
        std::thread::spawn(move || {
            // 1. Быстро забираем данные под замком, затем сразу отпускаем его.
            let (scen_idx, scen_items, scenario, rw_path, route_path, consist_names, consist_player, need_info) = {
                let mut app = match app.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                app.refresh_scenarios();
                let hide = app.hide_quick_drive_free_roam;
                let filtered_indices: Vec<usize> = app
                    .scenarios
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type)
                    })
                    .map(|(i, _)| i)
                    .collect();
                let ui_idx = idx as usize;
                let Some(&scen_idx) = filtered_indices.get(ui_idx) else {
                    return;
                };
                if scen_idx >= app.scenarios.len() {
                    return;
                }
                crate::scenario::load_scenario_metadata(&mut app.scenarios[scen_idx]);

                let scen_items: Vec<crate::ScenarioItem> = app
                    .scenarios
                    .iter()
                    .filter(|s| !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type))
                    .map(|s| crate::ScenarioItem {
                        name: s.name.clone().into(),
                        scenario_type: s.scenario_type.clone().into(),
                    })
                    .collect();

                let needs_load = app.scenarios[scen_idx].consists.is_none();
                if needs_load {
                    let scenario_path = app.scenarios[scen_idx].path.clone();
                    let serz_path =
                        crate::assets::preferred_serz_path(&app.clean_rw_path());
                    let consists = crate::scenario::load_consists(&scenario_path, &serz_path);
                    app.scenarios[scen_idx].consists = Some(consists);
                }

                let scenario = app.scenarios[scen_idx].clone();
                let rw_path = app.rw_path.clone();
                let route_path = app
                    .selected_route
                    .and_then(|i| app.routes.get(i))
                    .map(|r| r.path.clone())
                    .unwrap_or_default();

                let consist_names: Vec<slint::SharedString> = scenario
                    .consists
                    .as_ref()
                    .map(|c| c.iter().map(|x| x.name.clone().into()).collect())
                    .unwrap_or_default();
                let consist_player: Vec<bool> = scenario
                    .consists
                    .as_ref()
                    .map(|c| c.iter().map(|x| x.is_player).collect())
                    .unwrap_or_default();

                let need_info = app.scenario_consist_info.is_none()
                    || app
                        .scenario_consist_info
                        .as_ref()
                        .map(|v| v.len())
                        .unwrap_or(0)
                        != consist_names.len();

                (scen_idx, scen_items, scenario, rw_path, route_path, consist_names, consist_player, need_info)
            };

            // 2. Тяжелая работа БЕЗ удержания глобального мьютекса.
            let (computed_info, computed_vf) = if need_info {
                let mut info = Vec::new();
                let mut vf = Vec::new();
                if let Some(consists) = scenario.consists.as_ref() {
                    let (i, v) =
                        crate::scenario::analyze_consists_parallel(consists, &rw_path);
                    info = i;
                    vf = v;
                }
                (Some(info), Some(vf))
            } else {
                (None, None)
            };

            let serz_path = crate::assets::preferred_serz_path(&crate::app::clean_rw_path(&rw_path));
            let weather_name = crate::scenario::weather_display_name(
                &crate::app::clean_rw_path(&rw_path),
                std::path::Path::new(&route_path),
                &serz_path,
                &scenario.weather,
            );

            if scenario_generation.load(Ordering::SeqCst) != generation_id {
                return;
            }
            // 3. Быстро сохраняем результат под замком.
            let (vehicle_files, consist_missing, consist_complete) = {
                let mut app = match app.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if need_info {
                    if let (Some(info), Some(vf)) = (computed_info, computed_vf) {
                        app.scenario_consist_info = Some(info);
                        app.scenario_vehicle_files = Some(vf);
                    }
                }
                let vehicle_files = app.scenario_vehicle_files.clone().unwrap_or_default();
                app.selected_scenario = Some(scen_idx);
                app.selected_consist = Some(0);
                app.scenario_window_open = true;

                let consist_missing: Vec<bool> = app
                    .scenario_consist_info
                    .as_ref()
                    .map(|info| info.iter().map(|(_, ok)| !*ok).collect())
                    .unwrap_or_default();
                let consist_complete: Vec<bool> = app
                    .scenario_consist_info
                    .as_ref()
                    .map(|info| info.iter().map(|(_, ok)| *ok).collect())
                    .unwrap_or_default();

                (vehicle_files, consist_missing, consist_complete)
            };

            let vehicles: Vec<slint::SharedString> = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .map(|cc| {
                    cc.vehicles
                        .iter()
                        .map(|v| v.name.to_string().into())
                        .collect()
                })
                .unwrap_or_default();
            let missing: Vec<bool> = vehicle_files.first().cloned().unwrap_or_default();
            let paths: Vec<slint::SharedString> = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .map(|cc| {
                    cc.vehicles
                        .iter()
                        .map(|v| v.blueprint.clone().into())
                        .collect()
                })
                .unwrap_or_default();

            let selected_vehicle_info = if let Some(v) = scenario
                .consists
                .as_ref()
                .and_then(|c| c.first())
                .and_then(|cc| cc.vehicles.first())
            {
                format!(
                    "Поставщик: {}\nДополнение: {}\nФайл: {}\nНомер: {}",
                    v.provider,
                    v.product,
                    v.blueprint,
                    v.number.clone().unwrap_or_else(|| "—".to_owned())
                )
            } else {
                "Выберите единицу техники".to_owned()
            };

            let scenario_name = scenario.name.clone();
            let scenario_title = format!(
                "Сценарий: {}",
                if scenario.name.trim().is_empty() {
                    "Без названия"
                } else {
                    scenario.name.as_str()
                }
            );
            let fallback = |value: &str| -> String {
                if value.trim().is_empty() {
                    "Не указано".to_owned()
                } else {
                    value.to_owned()
                }
            };
            let scenario_author = fallback(&scenario.author);
            let scenario_season = fallback(&scenario.season);
            let scenario_start_time = fallback(&scenario.start_time);
            let scenario_weather = fallback(&weather_name);

            let ui_selected_scenario = idx as i32;

            if scenario_generation.load(Ordering::SeqCst) != generation_id {
                return;
            }
            let result = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_handle.upgrade() else {
                    eprintln!("scenario UI handle dropped before update");
                    return;
                };
                ui.set_scenario_items(slint::ModelRc::from(scen_items.as_slice()));
                ui.set_selected_scenario(ui_selected_scenario);

                let Some(sw) = scenario_handle.upgrade() else {
                    eprintln!("scenario window handle dropped before update");
                    return;
                };
                sw.set_scenario_name(scenario_name.into());
                sw.set_scenario_title(scenario_title.into());
                sw.set_scenario_author(scenario_author.into());
                sw.set_scenario_season(scenario_season.into());
                sw.set_scenario_start_time(scenario_start_time.into());
                sw.set_scenario_weather(scenario_weather.into());
                sw.set_consist_names(slint::ModelRc::from(consist_names.as_slice()));
                sw.set_consist_player(slint::ModelRc::from(consist_player.as_slice()));
                sw.set_consist_missing(slint::ModelRc::from(consist_missing.as_slice()));
                sw.set_consist_complete(slint::ModelRc::from(consist_complete.as_slice()));
                sw.set_vehicle_names(slint::ModelRc::from(vehicles.as_slice()));
                sw.set_vehicle_missing(slint::ModelRc::from(missing.as_slice()));
                sw.set_vehicle_paths(slint::ModelRc::from(paths.as_slice()));
                sw.set_selected_vehicle_info(selected_vehicle_info.into());
                sw.set_selected_consist(0);
                if let Err(err) = sw.show() {
                    eprintln!("failed to show scenario window: {err}");
                }
            });
            if let Err(err) = result {
                eprintln!("failed to invoke scenario UI update: {err}");
            }
        });
    });

    let app = self.app.clone();
        self.window.on_consist_selected(move |idx| {
        if let Ok(mut app) = app.lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let i = idx as usize;
            if i >= consists.len() {
                return;
            }
            let c = &consists[i];
            let vehicles: Vec<slint::SharedString> = c
                .vehicles
                .iter()
                .map(|v| format!("{} - {}", v.provider, v.name).into())
                .collect();
            let missing: Vec<bool> = app
                .scenario_vehicle_files
                .as_ref()
                .and_then(|vf| vf.get(i))
                .cloned()
                .unwrap_or_default();
            let paths: Vec<slint::SharedString> = c
                .vehicles
                .iter()
                .map(|v| v.blueprint.clone().into())
                .collect();
            app.selected_consist = Some(i);
            if let Some(sw) = scenario_handle2.upgrade() {
                sw.set_vehicle_names(slint::ModelRc::new(slint::VecModel::from(vehicles)));
                sw.set_vehicle_missing(slint::ModelRc::new(slint::VecModel::from(missing)));
                sw.set_vehicle_paths(slint::ModelRc::new(slint::VecModel::from(paths)));
                sw.set_selected_consist(idx);
            }
        }
    });
    let ui_handle_close_scenario = ui.as_weak();
    let app = self.app.clone();
        self.window.on_open_scenario_folder(move || {
        if let Ok(app) = app.lock()
            && let Some(idx) = app.selected_scenario
            && let Some(scenario) = app.scenarios.get(idx)
        {
            let _ = crate::windows::open_in_file_explorer(&scenario.path);
        }
    });
    let scenario_handle_close = self.weak.clone();
    let app = self.app.clone();
        self.window.on_close_scenario(move || {
        if let Some(sw) = scenario_handle_close.upgrade() {
            let _ = sw.hide();
        }
        let ui_handle = ui_handle_close_scenario.clone();
        if let Ok(mut app) = app.lock() {
            app.scenario_window_open = false;
            let hide = app.hide_quick_drive_free_roam;
            let scen: Vec<crate::ScenarioItem> = app
                .scenarios
                .iter()
                .filter(|s| !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type))
                .map(|s| crate::ScenarioItem {
                    name: s.name.clone().into(),
                    scenario_type: s.scenario_type.clone().into(),
                })
                .collect();
            let selected_ui = app.selected_scenario.and_then(|si| {
                app.scenarios
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        !hide || !crate::scenario::is_hidden_scenario_type(&s.scenario_type)
                    })
                    .position(|(i, _)| i == si)
            });
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_scenario_items(slint::ModelRc::new(slint::VecModel::from(scen)));
                ui.set_selected_scenario(selected_ui.map(|i| i as i32).unwrap_or(-1));
            }
        }
    });
    let app = self.app.clone();
        self.window.on_open_locoswap(move || {
        if let Ok(app) = app.lock() {
            let path = app.locoswap_path.clone();
            if !path.is_empty() && Path::new(&path).is_file() {
                let mut cmd = std::process::Command::new(&path);
                if let Some(parent) = Path::new(&path).parent() {
                    cmd.current_dir(parent);
                }
                if let Some(idx) = app.selected_scenario
                    && let Some(scenario) = app.scenarios.get(idx)
                {
                    cmd.arg(&scenario.path);
                }
                let _ = cmd.spawn();
            }
        }
    });
    let app = self.app.clone();
        self.window.on_open_locoswap_consist(move |idx| {
        if let Ok(app) = app.lock() {
            let locoswap_path = app.locoswap_path.clone();
            if locoswap_path.is_empty() || !Path::new(&locoswap_path).is_file() {
                return;
            }
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consist_index = scenario
                .consists
                .as_ref()
                .and_then(|consists| consists.get(idx as usize))
                .map(|consist| consist.index)
                .unwrap_or(idx as usize);
            let mut cmd = std::process::Command::new(&locoswap_path);
            if let Some(parent) = Path::new(&locoswap_path).parent() {
                cmd.current_dir(parent);
            }
            let _ = cmd
                .arg(&scenario.path)
                .arg("--consist")
                .arg(consist_index.to_string())
                .arg("--vehicle")
                .arg("-1")
                .spawn();
        }
    });
    let scenario_handle_preview = self.weak.clone();
    let app = self.app.clone();
        self.window.on_vehicle_selected(move |idx| {
        if let Ok(mut app) = app.lock() {
            app.selected_vehicle = Some(idx as usize);
            let rw_path = app.clean_rw_path();
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consists
                .get(consist_idx)
                .and_then(|c| c.vehicles.get(idx as usize))
            {
                Some(v) => v.clone(),
                None => return,
            };
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                &rw_path,
                &route_path,
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            let exists = crate::blueprint::vehicle_blueprint_exists(
                &rw_path,
                &vehicle.provider,
                &vehicle.product,
                &vehicle.blueprint,
            );
            let path_text = if exists {
                if vehicle.provider.trim().is_empty() || vehicle.product.trim().is_empty() {
                    vehicle.blueprint.clone()
                } else {
                    format!(
                        "Assets\\{}\\{}\\{}",
                        vehicle.provider.trim(),
                        vehicle.product.trim(),
                        vehicle.blueprint.trim().replace('/', "\\")
                    )
                }
            } else {
                String::new()
            };
            let image =
                load_vehicle_preview_image(&rw_path, &route_path, &vehicle, resolved.as_ref());
            if let Some(sw) = scenario_handle_preview.upgrade() {
                if let Some(idx) = path_text.find("Assets") {
                    sw.set_selected_vehicle_path(path_text[idx..].into());
                } else {
                    sw.set_selected_vehicle_path(path_text.into());
                }
                sw.set_selected_vehicle_image(image);
                let details = format!(
                    "Поставщик: {}\nДополнение: {}\nФайл: {}\nНомер: {}",
                    vehicle.provider,
                    vehicle.product,
                    vehicle.blueprint,
                    vehicle.number.clone().unwrap_or_else(|| "—".to_owned())
                );
                sw.set_selected_vehicle_info(details.into());
            }
        }
    });
    let app = self.app.clone();
        self.window.on_open_selected_vehicle_path(move || {
        if let Ok(app) = app.lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let vehicle_idx = match app.selected_vehicle {
                Some(i) => i,
                None => return,
            };
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s.clone(),
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let consist = match consists.get(consist_idx) {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consist.vehicles.get(vehicle_idx) {
                Some(v) => v.clone(),
                None => return,
            };
            let rw_path = app.rw_path.clone();
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                &crate::app::clean_rw_path(&rw_path),
                Path::new(&route_path),
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            if let Some(path) = resolved {
                if path.is_file() {
                    let _ = crate::windows::open_in_explorer(&path);
                } else {
                    let mut cur = path.as_path();
                    while !cur.exists() {
                        match cur.parent() {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                    let _ = crate::windows::open_in_file_explorer(cur);
                }
            }
        }
    });
    let app = self.app.clone();
        self.window.on_open_vehicle_path(move |idx| {
        if let Ok(app) = app.lock() {
            let scen_idx = match app.selected_scenario {
                Some(i) => i,
                None => return,
            };
            let consist_idx = app.selected_consist.unwrap_or(0);
            let scenario = match app.scenarios.get(scen_idx) {
                Some(s) => s,
                None => return,
            };
            let consists = match scenario.consists.as_ref() {
                Some(c) => c,
                None => return,
            };
            let consist = match consists.get(consist_idx) {
                Some(c) => c,
                None => return,
            };
            let vehicle = match consist.vehicles.get(idx as usize) {
                Some(v) => v,
                None => return,
            };
            let rw_path = app.rw_path.clone();
            let route_path = app
                .selected_route
                .and_then(|i| app.routes.get(i))
                .map(|r| r.path.clone())
                .unwrap_or_default();
            let resolved = crate::blueprint::resolve_blueprint_file_cached_free(
                &crate::app::clean_rw_path(&rw_path),
                Path::new(&route_path),
                &vehicle.blueprint,
                &vehicle.provider,
                &vehicle.product,
            );
            if let Some(path) = resolved {
                if path.is_file() {
                    let _ = crate::windows::open_in_explorer(&path);
                } else {
                    let mut cur = path.as_path();
                    while !cur.exists() {
                        match cur.parent() {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                    let _ = crate::windows::open_in_file_explorer(cur);
                }
            } else {
                // Если блюпринт не найден, ничего не открываем.
            }
        }
    });

    }
}

pub fn load_vehicle_preview_image(
    rw_path: &Path,
    route_path: &Path,
    vehicle: &crate::scenario::ScenarioVehicle,
    resolved: Option<&PathBuf>,
) -> slint::Image {
    let resolved = resolved.cloned().or_else(|| {
        crate::blueprint::resolve_blueprint_file_cached_free(
            rw_path,
            route_path,
            &vehicle.blueprint,
            &vehicle.provider,
            &vehicle.product,
        )
    });
    let Some(path) = resolved else {
        return slint::Image::default();
    };

    // Обычный файл на диске: картинка лежит рядом в LocoInformation/image.png.
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ap"))
    {
        let image_path = path
            .parent()
            .unwrap_or(path.as_path())
            .join("LocoInformation")
            .join("image.png");
        if image_path.is_file() {
            if let Ok(data) = std::fs::read(&image_path)
                && let Ok(image) = slint::Image::load_from_data(&data, Some("png"))
            {
                return image;
            }
            return slint::Image::load_from_path(&image_path).unwrap_or_default();
        }
        return slint::Image::default();
    }

    // Архив .ap: ищем запись блюпринта, затем соседнюю LocoInformation/image.png.
    let Ok(file) = std::fs::File::open(&path) else {
        return slint::Image::default();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return slint::Image::default();
    };

    let stem = Path::new(&vehicle.blueprint)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| vehicle.blueprint.clone());
    let mut base_dir = String::new();
    let mut found = false;
    // Один проход по архиву: ищем блюпринт и сразу строим карту имён записей.
    let mut entry_index_by_name: HashMap<String, usize> = HashMap::with_capacity(zip.len());
    for index in 0..zip.len() {
        let Ok(entry) = zip.by_index(index) else {
            continue;
        };
        let name = entry.name().unwrap_or_default().to_string();
        entry_index_by_name.insert(name.to_lowercase(), index);
        if !found {
            let entry_stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if entry_stem.eq_ignore_ascii_case(&stem) {
                base_dir = name
                    .rsplit_once('/')
                    .or_else(|| name.rsplit_once('\\'))
                    .map(|(dir, _)| dir.to_owned())
                    .unwrap_or_default();
                found = true;
            }
        }
    }
    if !found {
        return slint::Image::default();
    }

    let image_name = if base_dir.is_empty() {
        "LocoInformation/image.png".to_owned()
    } else {
        format!(
            "{}/LocoInformation/image.png",
            base_dir.trim_end_matches('/')
        )
    };

    if let Some(index) = entry_index_by_name.get(&image_name.to_lowercase()) {
        let Ok(mut entry) = zip.by_index(*index) else {
            return slint::Image::default();
        };
        let mut data = Vec::new();
        if std::io::Read::read_to_end(&mut entry, &mut data).is_ok()
            && let Ok(image) = slint::Image::load_from_data(&data, Some("png"))
        {
            return image;
        }
    }
    slint::Image::default()
}
