//! External editor (VS Code / system default) integration with two-way sync.

use crate::assets::read_serz_xml;
use std::fs;
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
    let sync_dir = if let Some(railworks_root) = binary_asset_path
        .ancestors()
        .find(|p| p.join("Assets").is_dir())
    {
        railworks_root.join(".tsc_sync")
    } else {
        std::env::temp_dir().join("tsc_sync")
    };
    fs::create_dir_all(&sync_dir)
        .map_err(|e| format!("Не удалось создать {}: {e}", sync_dir.display()))?;

    let file_stem = binary_asset_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("asset");
    let original_ext = binary_asset_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin");
    let temp_xml_path = sync_dir.join(format!("{}.{}.xml", file_stem, original_ext));

    let xml = read_serz_xml(serz_path, binary_asset_path).or_else(|err| {
        if let Some(fallback) = crate::assets::fallback_serz_path(serz_path) {
            read_serz_xml(&fallback, binary_asset_path).map_err(|e| format!("{err}; {e}"))
        } else {
            Err(err)
        }
    })?;
    fs::write(&temp_xml_path, &xml)
        .map_err(|e| format!("Не удалось записать {}: {e}", temp_xml_path.display()))?;

    // Launch editor
    let _ = launch_editor(&temp_xml_path, false);

    let binary_path = binary_asset_path.to_path_buf();
    let serz = serz_path.to_path_buf();
    let xml_path = temp_xml_path;
    let cb = Arc::new(status_callback);
    let was_modified = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        run_watcher(&xml_path, &binary_path, &serz, cb, was_modified);
    });

    Ok(())
}

pub fn find_vscode_path() -> Option<PathBuf> {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local)
            .join("Programs")
            .join("Microsoft VS Code")
            .join("bin")
            .join("code.cmd");
        if p.is_file() {
            return Some(p);
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            let p = PathBuf::from(pf)
                .join("Microsoft VS Code")
                .join("bin")
                .join("code.cmd");
            if p.is_file() {
                return Some(p);
            }
        }
    }
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
    if let Some(code) = find_vscode_path() {
        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmd.exe");
            if wait {
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.raw_arg(format!(
                        "/C call \"{}\" --wait -r -g \"{}\"",
                        code.display(),
                        xml_path.display()
                    ));
                }
            } else {
                cmd.arg("/C").arg(&code).arg("-r").arg("-g").arg(xml_path);
            }
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            return cmd
                .spawn()
                .map_err(|e| format!("Не удалось запустить VS Code: {e}"));
        } else {
            let mut c = Command::new(code);
            if wait {
                c.arg("--wait");
            }
            c.arg("-r").arg("-g").arg(xml_path);
            return c
                .spawn()
                .map_err(|e| format!("Не удалось запустить VS Code: {e}"));
        }
    }
    // Fallback: Notepad (never browser)
    if cfg!(target_os = "windows") {
        let mut c = Command::new("notepad.exe");
        c.arg(xml_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.spawn()
            .map_err(|e| format!("Не удалось запустить Блокнот: {e}"))
    } else {
        Command::new("notepad")
            .arg(xml_path)
            .spawn()
            .map_err(|e| format!("Не удалось запустить редактор: {e}"))
    }
}

pub fn create_backup_file(target_bin: &Path) -> Result<Option<PathBuf>, String> {
    if !target_bin.exists() {
        return Ok(None);
    }
    let file_name = target_bin
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "asset.bin".to_string());
    let mut backup_path = target_bin.with_file_name(format!("{}.bak", file_name));
    if backup_path.exists() {
        let mut i = 1;
        loop {
            backup_path = target_bin.with_file_name(format!("{}.bak{}", file_name, i));
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
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show('{message}', '{title}', [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Error)",
            message = message.replace('\'', "''"),
            title = title.replace('\'', "''")
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .creation_flags(0x08000000)
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("{title}: {message}");
    }
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
    let sync_dir =
        if let Some(railworks_root) = target.ancestors().find(|p| p.join("Assets").is_dir()) {
            railworks_root.join(".tsc_sync")
        } else {
            std::env::temp_dir().join("tsc_sync")
        };
    fs::create_dir_all(&sync_dir).map_err(|e| {
        let err = format!("Не удалось создать {}: {e}", sync_dir.display());
        show_native_error("Ошибка", &err);
        err
    })?;

    let file_stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("asset");
    let original_ext = target.extension().and_then(|s| s.to_str()).unwrap_or("bin");
    let temp_xml_path = sync_dir.join(format!("{}.{}.xml", file_stem, original_ext));

    let xml = read_serz_xml(&serz_path, target)
        .or_else(|err| {
            if let Some(fallback) = crate::assets::fallback_serz_path(&serz_path) {
                read_serz_xml(&fallback, target).map_err(|e| format!("{err}; {e}"))
            } else {
                Err(err)
            }
        })
        .inspect_err(|e| {
            show_native_error("Ошибка", e);
        })?;
    let initial_xml = xml.clone();
    fs::write(&temp_xml_path, &xml).map_err(|e| {
        let err = format!("Не удалось записать {}: {e}", temp_xml_path.display());
        show_native_error("Ошибка", &err);
        err
    })?;

    // background watcher
    let xml_w = temp_xml_path.clone();
    let bin_w = target.to_path_buf();
    let serz_w = serz_path.clone();
    let was_modified = Arc::new(AtomicBool::new(false));
    let was_modified_w = was_modified.clone();
    thread::spawn(move || {
        run_watcher(
            &xml_w,
            &bin_w,
            &serz_w,
            Arc::new(|res| match res {
                Ok(s) => println!("{}", s),
                Err(e) => eprintln!("{}", e),
            }),
            was_modified_w,
        );
    });

    let mut child =
        launch_editor(&temp_xml_path, true).inspect_err(|e| show_native_error("Ошибка", e))?;
    child.wait().map_err(|e| {
        let err = format!("Ошибка ожидания редактора: {e}");
        show_native_error("Ошибка", &err);
        err
    })?;

    // final compile
    let final_xml = fs::read_to_string(&temp_xml_path).unwrap_or_default();
    let has_changes = was_modified.load(Ordering::SeqCst) || (final_xml != initial_xml);
    if !has_changes {
        println!("[Headless] Файл закрыт без изменений. Компиляция пропущена.");
        let _ = fs::remove_file(&temp_xml_path);
        return Ok(());
    }

    let conv =
        crate::assets::convert_xml_to_bin(&serz_path, &temp_xml_path, target).or_else(|err| {
            if let Some(fallback) = crate::assets::fallback_serz_path(&serz_path) {
                crate::assets::convert_xml_to_bin(&fallback, &temp_xml_path, target)
                    .map_err(|e| format!("{err}; {e}"))
            } else {
                Err(err)
            }
        });
    if let Err(e) = conv {
        show_native_error("Ошибка конвертации Serz", &e);
        return Err(e);
    }
    let _ = fs::remove_file(&temp_xml_path);
    let _ = fs::remove_dir(&sync_dir);
    Ok(())
}

fn run_watcher(
    xml_path: &Path,
    binary_path: &Path,
    serz_path: &Path,
    cb: Arc<dyn Fn(Result<String, String>) + Send + Sync>,
    was_modified: Arc<AtomicBool>,
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
        .map(|s| s.to_os_string())
        .unwrap_or_default();

    let mut backup_created = false;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                let watched_str = watched_name.to_string_lossy().to_lowercase();
                let is_target = event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase() == watched_str)
                        .unwrap_or(false)
                });
                if !is_target {
                    continue;
                }
                let is_relevant_event = matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
                );
                if !is_relevant_event {
                    continue;
                }

                // 400ms debounce
                let deadline = Instant::now() + Duration::from_millis(400);
                while Instant::now() < deadline {
                    if let Ok(Ok(e)) = rx.recv_timeout(Duration::from_millis(50)) {
                        let rel = e.paths.iter().any(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().to_lowercase() == watched_str)
                                .unwrap_or(false)
                        });
                        if rel {
                            // keep draining
                        }
                    }
                }

                // retry read with backoff
                let mut xml_data: Option<String> = None;
                for attempt in 0..15 {
                    match fs::read_to_string(xml_path) {
                        Ok(data) if !data.trim().is_empty() => {
                            xml_data = Some(data);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if attempt == 14 {
                                eprintln!("[Sync] Не удалось прочитать XML после 15 попыток: {e}");
                            }
                        }
                    }
                    if xml_data.is_none() {
                        thread::sleep(Duration::from_millis(80));
                    }
                }
                let Some(xml) = xml_data else {
                    continue;
                };
                let _ = xml;

                was_modified.store(true, Ordering::SeqCst);

                // rely on serz.exe for validation
                if !backup_created {
                    let _ = create_backup_file(binary_path);
                    backup_created = true;
                }

                println!(
                    "[Sync] Сохранение обнаружено! Компиляция {} -> {}",
                    xml_path.display(),
                    binary_path.display()
                );
                let serz = serz_path.to_path_buf();
                let conversion = crate::assets::convert_xml_to_bin(&serz, xml_path, binary_path)
                    .or_else(|err| {
                        if let Some(fallback) = crate::assets::fallback_serz_path(&serz) {
                            crate::assets::convert_xml_to_bin(&fallback, xml_path, binary_path)
                                .map_err(|e| format!("{err}; {e}"))
                        } else {
                            Err(err)
                        }
                    });
                match conversion {
                    Ok(()) => {
                        println!(
                            "[Sync] ✓ Успешно скомпилировано в {}",
                            binary_path.display()
                        );
                        cb(Ok("✓ Изменения скомпилированы в BIN".to_string()));
                    }
                    Err(e) => {
                        eprintln!("[Sync] ✕ Ошибка serz.exe: {:?}", e);
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
