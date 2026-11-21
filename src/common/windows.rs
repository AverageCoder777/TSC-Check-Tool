//! Windows shell integrations such as Explorer and the Recycle Bin.

#[cfg(windows)]
use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
};

/// Запуск дочернего процесса без консольного окна.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
pub(crate) fn open_in_explorer(target_path: &Path) {
    let path = target_path.to_path_buf();
    thread::spawn(move || {
        let (display_path, spawn_result) = if path.is_file() {
            let normalized = normalize_explorer_path(&path);
            let res = Command::new("explorer.exe")
                .creation_flags(CREATE_NO_WINDOW)
                .raw_arg(format!("/select,\"{normalized}\""))
                .spawn();
            (path, res)
        } else {
            let mut current = path.as_path();
            while !current.exists() {
                match current.parent() {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
            let normalized = PathBuf::from(normalize_explorer_path(current));
            let res = Command::new("explorer.exe")
                .creation_flags(CREATE_NO_WINDOW)
                .arg(&normalized)
                .spawn();
            (normalized, res)
        };

        if let Err(error) = spawn_result {
            eprintln!(
                "Не удалось открыть {} в Проводнике: {error}",
                display_path.display()
            );
        }
    });
}

#[cfg(windows)]
fn normalize_explorer_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    stripped.replace('/', "\\")
}
