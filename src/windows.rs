//! Windows shell integrations such as Explorer and the Recycle Bin.

#[cfg(target_os = "windows")]
use std::{os::windows::process::CommandExt, path::Path, process::Command};

#[cfg(target_os = "windows")]
pub(crate) fn open_in_explorer(target_path: &Path) -> Result<(), String> {
    {
        if target_path.is_file() {
            Command::new("explorer")
                .raw_arg(format!("/select,\"{}\"", target_path.to_string_lossy()))
                .spawn()
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "Не удалось открыть {} в Проводнике: {error}",
                        target_path.display()
                    )
                })
        } else {
            let mut current = target_path.to_path_buf();
            while !current.exists() {
                match current.parent() {
                    Some(parent) => current = parent.to_path_buf(),
                    None => break,
                }
            }
            Command::new("explorer")
                .raw_arg(format!("\"{}\"", current.to_string_lossy()))
                .spawn()
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "Не удалось открыть {} в Проводнике: {error}",
                        current.display()
                    )
                })
        }
    }
}

#[cfg(windows)]
pub(crate) use open_in_explorer as open_in_file_explorer;
