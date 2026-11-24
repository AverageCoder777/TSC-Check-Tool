//! External editor (VS Code / system default) integration with two-way sync.

use std::ffi::OsString;
use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::SystemTime;

use crate::assets::read_serz_xml;
use crate::common::serz::{
    SerzNode, SerzReader, compile_tracks_xml_to_bin, encode_tgpcdx_to_vec, encode_to_vec,
    tracks_bin_to_xml,
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn is_tracks_bin(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Tracks.bin"))
}

fn has_extension_ignore_ascii_case(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

fn uses_structured_serz_xml(path: &Path) -> bool {
    is_tracks_bin(path)
        || has_extension_ignore_ascii_case(path, "GeoPcDx")
        || has_extension_ignore_ascii_case(path, "TgPcDx")
}

fn compile_xml_for_target(target_bin: &Path, xml: &str) -> Result<Vec<u8>, String> {
    if is_tracks_bin(target_bin) {
        compile_tracks_xml_to_bin(xml)
    } else if uses_structured_serz_xml(target_bin) {
        let bytes = compile_tracks_xml_to_bin(xml)?;
        if has_extension_ignore_ascii_case(target_bin, "TgPcDx") {
            let mut reader = SerzReader::new(&bytes);
            let root = reader
                .parse()
                .map_err(|error| format!("Failed to check deserialized TgPcDx: {error}"))?;
            encode_tgpcdx_to_vec(&root, reader.preamble.as_ref())
        } else {
            Ok(bytes)
        }
    } else {
        let (node, preamble) = SerzNode::from_xml_str(xml)?;
        encode_to_vec(&node, preamble.as_ref())
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
    thread::spawn(move || {
        let result = run_sync_loop(&binary_path, &temp_xml_path, &initial_xml, "ExternalEditor");
        status_callback(result);
    });
    Ok(())
}

pub fn run_headless(target: &Path) -> Result<(), String> {
    let temp_xml_path = generate_temp_xml_path(target);
    let initial_xml = resolve_temp_xml(target, &temp_xml_path)
        .inspect_err(|e| show_native_error(&crate::gettext!("Error"), e))?;
    run_sync_loop(target, &temp_xml_path, &initial_xml, "Headless").map(|_| ())
}

fn run_sync_loop(
    binary_path: &Path,
    xml_path: &Path,
    initial_xml: &str,
    log_tag: &str,
) -> Result<String, String> {
    loop {
        // Запуск редактора с блокирующим ожиданием закрытия окна/вкладки.
        match launch_editor(xml_path, true) {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => {
                let err = format!("Failed to open editor: {e}");
                show_native_error(&crate::gettext!("Launch error"), &err);
                return Err(err);
            }
        }

        if !xml_path.exists() {
            let msg = "File has been deleted. Sync cancelled.".to_string();
            println!("[{log_tag}] {msg}");
            return Ok(msg);
        }

        let final_xml = match fs::read_to_string(xml_path) {
            Ok(s) => s,
            Err(e) => {
                let err = format!("Failed to read {}: {e}", xml_path.display());
                show_native_error(&crate::gettext!("Read error"), &err);
                return Err(err);
            }
        };

        if final_xml == initial_xml || final_xml.trim().is_empty() {
            let _ = fs::remove_file(xml_path);
            let msg = "Closed without saving".to_string();
            println!("[{log_tag}] {msg}");
            return Ok(msg);
        }

        match compile_xml_for_target(binary_path, &final_xml) {
            Ok(bytes) => {
                if bytes.len() < 10 {
                    let err = format!(
                        "Write error: binary file too short ({} bytes).",
                        bytes.len()
                    );
                    show_native_error(&crate::gettext!("Write error"), &err);
                    return Err(err);
                }
                let mut validation_reader = SerzReader::new(&bytes);
                if let Err(e) = validation_reader.parse() {
                    let err = format!("Serialization error: generated binary is corrupted: {e}");
                    show_native_error(&crate::gettext!("Write error"), &err);
                    return Err(err);
                }
                let backup = create_backup_file(binary_path).ok().flatten();
                if let Err(e) = write_binary_atomic(binary_path, &bytes) {
                    if let Some(backup_path) = backup
                        && !binary_path.exists()
                    {
                        let _ = fs::rename(&backup_path, binary_path);
                    }
                    let err = format!("Error writing to binary: {e}");
                    show_native_error(&crate::gettext!("Write error"), &err);
                    return Err(err);
                }
                let _ = fs::remove_file(xml_path);
                let msg = "Successfully imported to binary".to_string();
                println!("[{log_tag}] {msg}");
                return Ok(msg);
            }
            Err(err) => {
                if ask_retry_on_xml_error(&err) {
                    continue;
                }
                return Err(format!(
                    "File saved as XML ({}), binary without changes.",
                    xml_path.display()
                ));
            }
        }
    }
}

fn ask_retry_on_xml_error(err: &str) -> bool {
    let prompt = crate::gettext!(
        "An XML syntax error has been detected:\n{0}\n\nOpen the editor again to fix the error?\n(If you click 'No', the file will remain saved as XML next to the original, and the binary will not be affected)",
        err
    );
    rfd::MessageDialog::new()
        .set_title(crate::gettext!("XML syntax error"))
        .set_description(&prompt)
        .set_buttons(rfd::MessageButtons::YesNo)
        .set_level(rfd::MessageLevel::Warning)
        .show()
        == rfd::MessageDialogResult::Yes
}

fn resolve_temp_xml(binary_asset_path: &Path, temp_xml_path: &Path) -> Result<String, String> {
    let regenerate = match (fs::metadata(temp_xml_path), fs::metadata(binary_asset_path)) {
        (Ok(xml_meta), Ok(bin_meta)) => {
            let xml_mtime = xml_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let bin_mtime = bin_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            bin_mtime > xml_mtime
        }
        (Err(_), _) => true,
        (_, Err(e)) => return Err(e.to_string()),
    };

    if regenerate {
        let xml = if uses_structured_serz_xml(binary_asset_path) {
            let raw = fs::read(binary_asset_path)
                .map_err(|e| format!("Failed to read {}: {e}", binary_asset_path.display()))?;
            tracks_bin_to_xml(&raw)?
        } else {
            read_serz_xml(binary_asset_path).map_err(|e| e.to_string())?
        };
        fs::write(temp_xml_path, &xml)
            .map_err(|e| format!("Failed to write {}: {e}", temp_xml_path.display()))?;
        Ok(xml)
    } else {
        fs::read_to_string(temp_xml_path)
            .map_err(|e| format!("Failed to read {}: {e}", temp_xml_path.display()))
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

fn generate_temp_xml_path(binary_path: &Path) -> PathBuf {
    append_suffix(binary_path, ".xml")
}

pub fn find_vscode_path() -> Option<PathBuf> {
    let bases: Vec<PathBuf> = ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(|var| std::env::var_os(var).map(PathBuf::from))
        .collect();

    for base in &bases {
        let code_exe = base
            .join("Programs")
            .join("Microsoft VS Code")
            .join("Code.exe");
        if code_exe.is_file() {
            return Some(code_exe);
        }
        let code_exe_root = base.join("Microsoft VS Code").join("Code.exe");
        if code_exe_root.is_file() {
            return Some(code_exe_root);
        }
    }

    for base in &bases {
        let code_cmd = base
            .join("Programs")
            .join("Microsoft VS Code")
            .join("bin")
            .join("code.cmd");
        if code_cmd.is_file() {
            return Some(code_cmd);
        }
    }

    let mut where_cmd = Command::new("cmd");
    where_cmd.args(["/C", "where", "code"]);
    #[cfg(windows)]
    where_cmd.creation_flags(CREATE_NO_WINDOW);

    if let Ok(output) = where_cmd.output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let p = PathBuf::from(line.trim());
            if p.is_file()
                && (has_extension_ignore_ascii_case(&p, "exe")
                    || has_extension_ignore_ascii_case(&p, "cmd")
                    || has_extension_ignore_ascii_case(&p, "bat"))
            {
                return Some(p);
            }
        }
    }
    None
}

pub fn launch_editor(xml_path: &Path, wait: bool) -> Result<Child, String> {
    if let Some(editor_path) = find_vscode_path() {
        let mut cmd = if has_extension_ignore_ascii_case(&editor_path, "exe") {
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
        cmd.creation_flags(CREATE_NO_WINDOW);
        return cmd
            .spawn()
            .map_err(|e| format!("Failed to launch VS Code: {e}"));
    }

    let mut c = Command::new("notepad.exe");
    c.arg(xml_path);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c.spawn()
        .map_err(|e| format!("Failed to launch Notepad: {e}"))
}

pub fn write_binary_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    if target.parent().is_none() {
        return Err(format!("Incorrect file path: {}", target.display()));
    }
    let temp_path = append_suffix(target, ".tmp");
    fs::write(&temp_path, bytes)
        .map_err(|error| format!("Failed to write temp file {}: {error}", temp_path.display()))?;
    fs::rename(&temp_path, target).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!("Failed to install {}: {error}", target.display())
    })?;
    Ok(())
}

pub fn create_backup_file(target_bin: &Path) -> Result<Option<PathBuf>, String> {
    if !target_bin.exists() {
        return Ok(None);
    }
    let mut backup_path = append_suffix(target_bin, ".bak");
    if backup_path.exists() {
        let mut i = 1u32;
        loop {
            backup_path = append_suffix(target_bin, &format!(".bak{i}"));
            if !backup_path.exists() {
                break;
            }
            i += 1;
        }
    }
    fs::copy(target_bin, &backup_path)
        .map_err(|e| format!("Failed to create backup {}: {e}", backup_path.display()))?;
    println!("[Backup] Backup created: {backup_path:?}");
    Ok(Some(backup_path))
}

pub fn show_native_error(title: &str, message: &str) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[cfg(test)]
#[path = "../tests/external_editor.rs"]
mod tests;
