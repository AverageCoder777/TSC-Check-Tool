//! External editor (VS Code / system default) integration with two-way sync.

use crate::assets::read_serz_xml;
use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

fn is_tracks_bin(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Tracks.bin"))
}

fn uses_structured_serz_xml(path: &Path) -> bool {
    is_tracks_bin(path)
        || path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("GeoPcDx") || extension.eq_ignore_ascii_case("TgPcDx")
        })
}

fn compile_xml_for_target(target_bin: &Path, xml: &str) -> Result<Vec<u8>, String> {
    if is_tracks_bin(target_bin) {
        crate::serz::compile_tracks_xml_to_bin(xml)
    } else if uses_structured_serz_xml(target_bin) {
        let bytes = crate::serz::compile_tracks_xml_to_bin(xml)?;
        if target_bin
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("TgPcDx"))
        {
            let mut reader = crate::serz::SerzReader::new(&bytes);
            let root = reader
                .parse()
                .map_err(|error| format!("Не удалось проверить сериализованный TgPcDx: {error}"))?;
            crate::serz::encode_tgpcdx_to_vec(&root, reader.preamble.as_ref())
        } else {
            Ok(bytes)
        }
    } else {
        let (node, preamble) = crate::serz::SerzNode::from_xml_str(xml)?;
        crate::serz::encode_to_vec(&node, preamble.as_ref())
    }
}

pub fn open_and_sync_with_vscode(
    binary_asset_path: &Path,
    status_callback: impl Fn(Result<String, String>) + Send + Sync + 'static,
) -> Result<(), String> {
    let temp_xml_path = generate_temp_xml_path(binary_asset_path);
    // Разрешаем временный XML с проверкой mtime: перегенерируем, если .bin новее или .bin.xml отсутствует.
    let initial_xml = resolve_temp_xml(binary_asset_path, &temp_xml_path)?;

    let binary_path = binary_asset_path.to_path_buf();
    let xml_path = temp_xml_path.clone();

    thread::spawn(move || {
        let initial_state = initial_xml;
        loop {
            // Запуск редактора с блокирующим ожиданием закрытия окна/вкладки
            let launch_res = launch_editor(&xml_path, true);
            match launch_res {
                Ok(mut child) => {
                    let _ = child.wait();
                }
                Err(e) => {
                    let err = format!("Не удалось запустить редактор: {e}");
                    show_native_error("Ошибка запуска", &err);
                    status_callback(Err(err));
                    return;
                }
            }

            if !xml_path.exists() {
                let msg = "Файл был удалён. Синхронизация отменена.".to_string();
                println!("[ExternalEditor] {msg}");
                status_callback(Ok(msg));
                return;
            }

            let final_xml = match fs::read_to_string(&xml_path) {
                Ok(s) => s,
                Err(e) => {
                    let err = format!("Не удалось прочитать {}: {e}", xml_path.display());
                    show_native_error("Ошибка чтения", &err);
                    status_callback(Err(err));
                    return;
                }
            };

            if final_xml == initial_state {
                let _ = fs::remove_file(&xml_path);
                let msg = "Файл закрыт без изменений".to_string();
                println!("[ExternalEditor] {msg}");
                status_callback(Ok(msg));
                return;
            }

            match compile_xml_for_target(&binary_path, &final_xml) {
                Ok(bytes) => {
                    if bytes.len() < 10 {
                        let err = format!(
                            "Ошибка записи: бинарный файл слишком короткий ({} байт).",
                            bytes.len()
                        );
                        show_native_error("Ошибка записи", &err);
                        status_callback(Err(err));
                        return;
                    }
                    let mut validation_reader = crate::serz::SerzReader::new(&bytes);
                    if let Err(e) = validation_reader.parse() {
                        let err =
                            format!("Ошибка сериализации: сгенерированный бинарник поврежден: {e}");
                        show_native_error("Ошибка записи", &err);
                        status_callback(Err(err));
                        return;
                    }
                    let backup = create_backup_file(&binary_path).ok().flatten();
                    if let Err(e) = write_binary_atomic(&binary_path, &bytes) {
                        if let Some(backup_path) = backup {
                            let _ = fs::rename(&backup_path, &binary_path);
                        }
                        let err = format!("Ошибка записи в бинарник: {e}");
                        show_native_error("Ошибка записи", &err);
                        status_callback(Err(err));
                        return;
                    }
                    let _ = fs::remove_file(&xml_path);
                    let msg = "✓ Изменения успешно скомпилированы в BIN".to_string();
                    println!("[ExternalEditor] {msg}");
                    status_callback(Ok(msg));
                    return;
                }
                Err(err) => {
                    let prompt = format!(
                        "Обнаружена ошибка синтаксиса XML:\n{}\n\nОткрыть редактор снова для исправления ошибки?\n(При нажатии 'Нет' файл останется сохранённым как XML рядом с оригиналом, а бинарник затронут не будет)",
                        err
                    );
                    let retry = rfd::MessageDialog::new()
                        .set_title("Ошибка синтаксиса XML")
                        .set_description(&prompt)
                        .set_buttons(rfd::MessageButtons::YesNo)
                        .set_level(rfd::MessageLevel::Warning)
                        .show();
                    if retry == rfd::MessageDialogResult::Yes {
                        continue;
                    } else {
                        let msg = format!(
                            "Файл сохранён как XML ({}), бинарник не изменён.",
                            xml_path.display()
                        );
                        status_callback(Err(msg));
                        return;
                    }
                }
            }
        }
    });
    Ok(())
}

fn resolve_temp_xml(binary_asset_path: &Path, temp_xml_path: &Path) -> Result<String, String> {
    let regenerate = match (fs::metadata(temp_xml_path), fs::metadata(binary_asset_path)) {
        (Ok(xml_meta), Ok(bin_meta)) => {
            let xml_mtime = xml_meta
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let bin_mtime = bin_meta
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            bin_mtime > xml_mtime
        }
        (Err(_), _) => true,
        (_, Err(e)) => return Err(e.to_string()),
    };

    if regenerate {
        let xml = if uses_structured_serz_xml(binary_asset_path) {
            let raw = fs::read(binary_asset_path).map_err(|e| {
                format!("Не удалось прочитать {}: {e}", binary_asset_path.display())
            })?;
            crate::serz::tracks_bin_to_xml(&raw)?
        } else {
            read_serz_xml(binary_asset_path).map_err(|e| e.to_string())?
        };
        fs::write(temp_xml_path, &xml)
            .map_err(|e| format!("Не удалось записать {}: {e}", temp_xml_path.display()))?;
        Ok(xml)
    } else {
        fs::read_to_string(temp_xml_path)
            .map_err(|e| format!("Не удалось прочитать {}: {e}", temp_xml_path.display()))
    }
}

fn generate_temp_xml_path(binary_path: &Path) -> PathBuf {
    let file_name = binary_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("asset.bin");
    binary_path.with_file_name(format!("{file_name}.xml"))
}

pub fn find_vscode_path() -> Option<PathBuf> {
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
            let mut c = Command::new("cmd.exe");
            let wait_flag = if wait { "--wait " } else { "" };
            #[cfg(windows)]
            c.raw_arg(format!(
                "/S /C \"\"{}\" {wait_flag}-r -g \"{}\"\"",
                editor_path.display(),
                xml_path.display()
            ));
            c
        };
        #[cfg(windows)]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        return cmd
            .spawn()
            .map_err(|e| format!("Не удалось запустить VS Code: {e}"));
    }
    let mut c = Command::new("notepad.exe");
    c.arg(xml_path);
    #[cfg(windows)]
    c.creation_flags(0x08000000);
    c.spawn()
        .map_err(|e| format!("Не удалось запустить Блокнот: {e}"))
}

pub fn write_binary_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("Некорректный путь к файлу: {}", target.display()))?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "asset.bin".to_owned());
    let temp_path = parent.join(format!("{file_name}.tmp"));
    fs::write(&temp_path, bytes).map_err(|error| {
        format!(
            "Не удалось записать временный файл {}: {error}",
            temp_path.display()
        )
    })?;
    if target.exists() {
        fs::remove_file(target).map_err(|error| {
            let _ = fs::remove_file(&temp_path);
            format!("Не удалось подготовить {}: {error}", target.display())
        })?;
    }
    fs::rename(&temp_path, target).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!("Не удалось установить {}: {error}", target.display())
    })?;
    Ok(())
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
    let temp_xml_path = generate_temp_xml_path(target);
    let initial_xml =
        resolve_temp_xml(target, &temp_xml_path).inspect_err(|e| show_native_error("Ошибка", e))?;
    loop {
        let mut child =
            launch_editor(&temp_xml_path, true).inspect_err(|e| show_native_error("Ошибка", e))?;
        let _ = child.wait();
        if !temp_xml_path.exists() {
            println!("[Headless] Файл удалён. Завершение.");
            return Ok(());
        }
        let final_xml = fs::read_to_string(&temp_xml_path).unwrap_or_default();
        if final_xml == initial_xml || final_xml.trim().is_empty() {
            println!("[Headless] Файл закрыт без изменений. Компиляция пропущена.");
            let _ = fs::remove_file(&temp_xml_path);
            return Ok(());
        }
        match compile_xml_for_target(target, &final_xml) {
            Ok(bytes) => {
                if bytes.len() < 10 {
                    show_native_error(
                        "Ошибка записи",
                        &format!("Бинарный файл слишком короткий ({} байт).", bytes.len()),
                    );
                    return Err("Бинарный файл слишком короткий".to_string());
                }
                let mut validation_reader = crate::serz::SerzReader::new(&bytes);
                if let Err(e) = validation_reader.parse() {
                    show_native_error(
                        "Ошибка записи",
                        &format!("Сгенерированный бинарник поврежден: {e}"),
                    );
                    return Err(e);
                }
                let backup = create_backup_file(target).ok().flatten();
                if let Err(e) = write_binary_atomic(target, &bytes) {
                    if let Some(backup_path) = backup {
                        let _ = fs::rename(&backup_path, target);
                    }
                    show_native_error("Ошибка записи", &e);
                    return Err(e);
                }
                let _ = fs::remove_file(&temp_xml_path);
                return Ok(());
            }
            Err(e) => {
                let prompt = format!(
                    "Обнаружена ошибка синтаксиса XML:\n{}\n\nОткрыть редактор снова для исправления ошибки?\n(При 'Нет' исходный бинарник останется без изменений)",
                    e
                );
                let retry = rfd::MessageDialog::new()
                    .set_title("Ошибка синтаксиса XML")
                    .set_description(&prompt)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .set_level(rfd::MessageLevel::Warning)
                    .show();
                if retry == rfd::MessageDialogResult::Yes {
                    continue;
                } else {
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serz::{SerzNode, SerzPreamble};

    fn texture_node(blob: Vec<u8>) -> SerzNode {
        SerzNode::Element {
            name: "cHcTexture".into(),
            id: 1,
            version: 1,
            children: vec![
                SerzNode::Property {
                    name: "Name".into(),
                    type_name: "cDeltaString".into(),
                    value: "OldTexture".into(),
                    alt_encoding: None,
                },
                SerzNode::Element {
                    name: "cHcTextureData".into(),
                    id: 2,
                    version: 1,
                    children: vec![SerzNode::Element {
                        name: "Mip".into(),
                        id: 3,
                        version: 1,
                        children: vec![SerzNode::Element {
                            name: "cHcImageDx".into(),
                            id: 4,
                            version: 1,
                            children: vec![SerzNode::Blob { data: blob }],
                        }],
                    }],
                },
            ],
        }
    }

    fn find_property<'a>(node: &'a SerzNode, property_name: &str) -> Option<&'a str> {
        match node {
            SerzNode::Element { children, .. } => children
                .iter()
                .find_map(|child| find_property(child, property_name)),
            SerzNode::Property { name, value, .. } if name == property_name => Some(value),
            _ => None,
        }
    }

    fn collect_blobs<'a>(node: &'a SerzNode, blobs: &mut Vec<&'a [u8]>) {
        match node {
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_blobs(child, blobs);
                }
            }
            SerzNode::Blob { data } => blobs.push(data),
            _ => {}
        }
    }

    #[test]
    fn texture_editor_xml_round_trip_preserves_binary_blob() {
        let blob = vec![0xFF, 0xAA, 0x80, 0x42, 0x00, 0xFE, 0xFF, 0x01];
        let preamble = SerzPreamble {
            flag: 0x01,
            val: 0x1234_5678,
        };
        let original =
            crate::serz::encode_tgpcdx_to_vec(&texture_node(blob.clone()), Some(&preamble))
                .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("texture.TgPcDx");
        let temp_xml = generate_temp_xml_path(&target);
        fs::write(&target, &original).unwrap();

        let xml = resolve_temp_xml(&target, &temp_xml).unwrap();
        let edited_xml = xml.replace("OldTexture", "RenamedTexture");
        assert_ne!(xml, edited_xml);
        let saved = compile_xml_for_target(&target, &edited_xml).unwrap();
        let invalid_blob_size = edited_xml.replace(
            &format!("d:size=\"{}\"", blob.len()),
            &format!("d:size=\"{}\"", blob.len() + 1),
        );
        assert!(compile_xml_for_target(&target, &invalid_blob_size).is_err());

        assert_eq!(&saved[..10], b"SERZ\0\0\x01\0\xFF\x43");
        let root = crate::serz::validate_tgpcdx_binary(&saved, Some(&[blob.len()])).unwrap();
        assert_eq!(find_property(&root, "Name"), Some("RenamedTexture"));
        let mut saved_blobs = Vec::new();
        collect_blobs(&root, &mut saved_blobs);
        assert_eq!(saved_blobs, vec![blob.as_slice()]);
        let parsed_preamble = {
            let mut reader = crate::serz::SerzReader::new(&saved);
            reader.parse().unwrap();
            reader.preamble.unwrap()
        };
        assert_eq!(parsed_preamble, preamble);
    }

    #[test]
    fn geometry_editor_xml_round_trip_preserves_binary_blob() {
        let blob = vec![0xFF, 0xAA, 0x80, 0x42, 0x00, 0xFE, 0xFF, 0x01];
        let preamble = SerzPreamble {
            flag: 0x01,
            val: 0x1234_5678,
        };
        let original =
            crate::serz::encode_to_vec(&texture_node(blob.clone()), Some(&preamble)).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.GeoPcDx");
        let temp_xml = generate_temp_xml_path(&target);
        fs::write(&target, &original).unwrap();

        let xml = resolve_temp_xml(&target, &temp_xml).unwrap();
        let edited_xml = xml.replace("OldTexture", "RenamedTexture");
        let saved = compile_xml_for_target(&target, &edited_xml).unwrap();

        let mut reader = crate::serz::SerzReader::new(&saved);
        let root = reader.parse().unwrap();
        assert_eq!(find_property(&root, "Name"), Some("RenamedTexture"));
        let mut saved_blobs = Vec::new();
        collect_blobs(&root, &mut saved_blobs);
        assert_eq!(saved_blobs, vec![blob.as_slice()]);
        assert_eq!(reader.preamble.as_ref(), Some(&preamble));
    }
}
