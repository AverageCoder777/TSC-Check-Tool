//! Windows shell integrations such as Explorer and the Recycle Bin.

#[cfg(target_os = "windows")]
use std::{os::windows::process::CommandExt, path::Path, process::Command, thread};

/// Запуск дочернего процесса без консольного окна.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(target_os = "windows")]
pub(crate) fn open_in_explorer(target_path: &Path) {
    let path = target_path.to_path_buf();
    thread::spawn(move || {
        let result = if path.is_file() {
            Command::new("explorer")
                .creation_flags(CREATE_NO_WINDOW)
                .raw_arg(format!("/select,\"{}\"", path.to_string_lossy()))
                .spawn()
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "Не удалось открыть {} в Проводнике: {error}",
                        path.display()
                    )
                })
        } else {
            let mut current = path;
            while !current.exists() {
                match current.parent() {
                    Some(parent) => current = parent.to_path_buf(),
                    None => break,
                }
            }
            Command::new("explorer")
                .creation_flags(CREATE_NO_WINDOW)
                .raw_arg(format!("\"{}\"", current.to_string_lossy()))
                .spawn()
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "Не удалось открыть {} в Проводнике: {error}",
                        current.display()
                    )
                })
        };
        if let Err(msg) = result {
            eprintln!("{msg}");
        }
    });
}

#[cfg(windows)]
pub(crate) use open_in_explorer as open_in_file_explorer;
