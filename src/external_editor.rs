//! External editor (VS Code / system default) integration with two-way sync.

use crate::assets::{SerzTool, read_serz_xml};
use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

pub fn open_and_sync_with_vscode(
    binary_asset_path: &Path,
    serz_path: &Path,
    status_callback: impl Fn(Result<String, String>) + Send + Sync + 'static,
) -> Result<(), String> {
    let sync_dir = resolve_sync_dir(binary_asset_path)?;
    let temp_xml_path = generate_temp_xml_path(&sync_dir, binary_asset_path);

    let xml = read_serz_xml(serz_path, binary_asset_path).map_err(|e| e.to_string())?;
    fs::write(&temp_xml_path, &xml)
        .map_err(|e| format!("Не удалось записать {}: {e}", temp_xml_path.display()))?;

    // Запуск редактора без блокировки UI
    let _ = launch_editor(&temp_xml_path, false);

    let binary_path = binary_asset_path.to_path_buf();
    let serz = serz_path.to_path_buf();
    let xml_path = temp_xml_path;
    let cb = Arc::new(status_callback);
    let was_modified = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::new(AtomicBool::new(false));

    thread::spawn(move || {
        run_watcher(
            &xml_path,
            &binary_path,
            &serz,
            cb,
            was_modified,
            stop_signal,
        );
    });

    Ok(())
}

fn resolve_sync_dir(target: &Path) -> Result<PathBuf, String> {
    let dir = if let Some(railworks_root) = target.ancestors().find(|p| p.join("Assets").is_dir()) {
        railworks_root.join(".tsc_sync")
    } else {
        std::env::temp_dir().join("tsc_sync")
    };
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать {}: {e}", dir.display()))?;
    Ok(dir)
}

fn generate_temp_xml_path(sync_dir: &Path, binary_path: &Path) -> PathBuf {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let stem = binary_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("asset");
    let ext = binary_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin");

    // Хэш родительского каталога исключает коллизии имен при открытии одноименных ассетов из разных папок
    let parent_hash = binary_path
        .parent()
        .map(|p| {
            let mut hasher = DefaultHasher::new();
            p.to_string_lossy().hash(&mut hasher);
            format!("{:08x}", hasher.finish() as u32)
        })
        .unwrap_or_default();

    sync_dir.join(format!("{stem}_{parent_hash}.{ext}.xml"))
}

pub fn find_vscode_path() -> Option<PathBuf> {
    // 1. Поиск прямого исполняемого файла Code.exe (предпочтительно для обхода cmd.exe)
    for base in [
        std::env::var("LOCALAPPDATA").ok(),
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let code_exe = PathBuf::from(&base)
            .join("Programs")
            .join("Microsoft VS Code")
            .join("Code.exe");
        if code_exe.is_file() {
            return Some(code_exe);
        }
        let code_exe_root = PathBuf::from(&base)
            .join("Microsoft VS Code")
            .join("Code.exe");
        if code_exe_root.is_file() {
            return Some(code_exe_root);
        }
    }

    // 2. Поиск code.cmd
    for base in [
        std::env::var("LOCALAPPDATA").ok(),
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let p = PathBuf::from(base)
            .join("Programs")
            .join("Microsoft VS Code")
            .join("bin")
            .join("code.cmd");
        if p.is_file() {
            return Some(p);
        }
    }

    // 3. Поиск в PATH
    if let Ok(output) = Command::new("cmd").args(["/C", "where", "code"]).output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let p = PathBuf::from(line.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn launch_editor(xml_path: &Path, wait: bool) -> Result<std::process::Child, String> {
    if let Some(editor_path) = find_vscode_path() {
        let is_exe = editor_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"));

        let mut cmd = if is_exe {
            let mut c = Command::new(&editor_path);
            if wait {
                c.arg("--wait");
            }
            c.arg("-r").arg("-g").arg(xml_path);
            c
        } else {
            // Запуск .cmd пакета через cmd.exe с защитой от разрыва кавычек пробелами
            let mut c = Command::new("cmd.exe");
            let wait_flag = if wait { "--wait " } else { "" };
            #[cfg(windows)]
            c.raw_arg(format!(
                "/S /C \"\"{}\" {wait_flag}-r -g \"{}\"\"",
                editor_path.display(),
                xml_path.display()
            ));
            #[cfg(not(windows))]
            c.arg(format!(
                "/S /C \"\"{}\" {wait_flag}-r -g \"{}\"\"",
                editor_path.display(),
                xml_path.display()
            ));
            c
        };

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        return cmd
            .spawn()
            .map_err(|e| format!("Не удалось запустить VS Code: {e}"));
    }

    // Fallback: Блокнот Windows
    let mut c = Command::new("notepad.exe");
    c.arg(xml_path);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c.spawn()
        .map_err(|e| format!("Не удалось запустить Блокнот: {e}"))
}

pub fn create_backup_file(target_bin: &Path) -> Result<Option<PathBuf>, String> {
    if !target_bin.exists() {
        return Ok(None);
    }
    let file_name = target_bin
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "asset.bin".to_string());

    let mut backup_path = target_bin.with_file_name(format!("{file_name}.bak"));
    if backup_path.exists() {
        let mut i = 1;
        loop {
            backup_path = target_bin.with_file_name(format!("{file_name}.bak{i}"));
            if !backup_path.exists() {
                break;
            }
            i += 1;
        }
    }
    fs::copy(target_bin, &backup_path).map_err(|e| {
        format!(
            "Не удалось создать резервную копию {}: {e}",
            backup_path.display()
        )
    })?;
    println!("[Backup] Создана резервная копия: {:?}", backup_path);
    Ok(Some(backup_path))
}

pub fn show_native_error(title: &str, message: &str) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
}

pub fn run_headless(target: &Path) -> Result<(), String> {
    let serz_path = match crate::assets::find_serz_path(target) {
        Some(p) => p,
        None => {
            let err = "serz.exe не найден".to_string();
            show_native_error("Ошибка", &err);
            return Err(err);
        }
    };
    let sync_dir = resolve_sync_dir(target)?;
    let temp_xml_path = generate_temp_xml_path(&sync_dir, target);

    let xml = read_serz_xml(&serz_path, target)
        .inspect_err(|e| {
            show_native_error("Ошибка", &e.to_string());
        })
        .map_err(|e| e.to_string())?;

    let initial_xml = xml.clone();
    fs::write(&temp_xml_path, &xml).map_err(|e| {
        let err = format!("Не удалось записать {}: {e}", temp_xml_path.display());
        show_native_error("Ошибка", &err);
        err
    })?;

    let was_modified = Arc::new(AtomicBool::new(false));
    let stop_watcher = Arc::new(AtomicBool::new(false));

    // Запуск фонового watcher
    let xml_w = temp_xml_path.clone();
    let bin_w = target.to_path_buf();
    let serz_w = serz_path.clone();
    let was_modified_w = was_modified.clone();
    let stop_watcher_w = stop_watcher.clone();

    let watcher_thread = thread::spawn(move || {
        run_watcher(
            &xml_w,
            &bin_w,
            &serz_w,
            Arc::new(|res| match res {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("{e}"),
            }),
            was_modified_w,
            stop_watcher_w,
        );
    });

    // Ожидание закрытия окна редактора
    let mut child =
        launch_editor(&temp_xml_path, true).inspect_err(|e| show_native_error("Ошибка", e))?;
    let _ = child.wait();

    // Останавливаем watcher и дожидаемся завершения его потока
    stop_watcher.store(true, Ordering::SeqCst);
    let _ = watcher_thread.join();

    // Финальная сверка и компиляция
    let final_xml = fs::read_to_string(&temp_xml_path).unwrap_or_default();
    let has_changes = was_modified.load(Ordering::SeqCst) || (final_xml != initial_xml);

    if !has_changes {
        println!("[Headless] Файл закрыт без изменений. Компиляция пропущена.");
        let _ = fs::remove_file(&temp_xml_path);
        return Ok(());
    }

    // Гарантируем наличие резервной копии перед перезаписью
    let _ = create_backup_file(target);

    let conv = SerzTool::new(&serz_path).xml_to_bin(&temp_xml_path, target);
    if let Err(e) = conv {
        show_native_error("Ошибка конвертации Serz", &e.to_string());
        return Err(e.to_string());
    }

    let _ = fs::remove_file(&temp_xml_path);
    let _ = fs::remove_dir(&sync_dir); // Удалит только если пуста
    Ok(())
}

fn run_watcher(
    xml_path: &Path,
    binary_path: &Path,
    serz_path: &Path,
    cb: Arc<dyn Fn(Result<String, String>) + Send + Sync>,
    was_modified: Arc<AtomicBool>,
    stop_signal: Arc<AtomicBool>,
) {
    use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = match RecommendedWatcher::new(tx, Config::default()) {
        Ok(w) => w,
        Err(e) => {
            cb(Err(format!("Watcher init failed: {e}")));
            return;
        }
    };

    let watch_dir = xml_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
        cb(Err(format!("Watcher watch failed: {e}")));
        return;
    }

    let watched_name = xml_path
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut backup_created = false;

    loop {
        if stop_signal.load(Ordering::Relaxed) || !xml_path.exists() {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                let is_target = event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase() == watched_name)
                        .unwrap_or(false)
                });
                if !is_target {
                    continue;
                }

                let is_relevant = matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
                );
                if !is_relevant {
                    continue;
                }

                // Корректный динамический Debounce (400 мс тишины)
                let mut deadline = Instant::now() + Duration::from_millis(400);
                while Instant::now() < deadline {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    if let Ok(Ok(e)) = rx.recv_timeout(timeout.min(Duration::from_millis(50))) {
                        let rel = e.paths.iter().any(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().to_lowercase() == watched_name)
                                .unwrap_or(false)
                        });
                        if rel {
                            deadline = Instant::now() + Duration::from_millis(400);
                        }
                    }
                }

                // Чтение с повторными попытками
                let mut xml_data: Option<String> = None;
                for _ in 0..15 {
                    match fs::read_to_string(xml_path) {
                        Ok(data) if !data.trim().is_empty() => {
                            xml_data = Some(data);
                            break;
                        }
                        _ => thread::sleep(Duration::from_millis(80)),
                    }
                }

                let Some(_) = xml_data else {
                    continue;
                };

                was_modified.store(true, Ordering::SeqCst);

                if !backup_created {
                    let _ = create_backup_file(binary_path);
                    backup_created = true;
                }

                println!(
                    "[Sync] Сохранение обнаружено! Компиляция {} -> {}",
                    xml_path.display(),
                    binary_path.display()
                );

                let serz = SerzTool::new(serz_path);
                match serz.xml_to_bin(xml_path, binary_path) {
                    Ok(()) => {
                        println!(
                            "[Sync] ✓ Успешно скомпилировано в {}",
                            binary_path.display()
                        );
                        cb(Ok("✓ Изменения скомпилированы в BIN".to_string()));
                    }
                    Err(e) => {
                        eprintln!("[Sync] ✕ Ошибка serz.exe: {e:?}");
                        cb(Err(format!("✕ Ошибка Serz: {e}")));
                    }
                }
            }
            Ok(Err(e)) => {
                cb(Err(format!("Watcher error: {e}")));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
