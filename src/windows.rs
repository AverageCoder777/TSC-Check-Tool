//! Windows shell integrations such as Explorer and the Recycle Bin.

use std::path::Path;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use std::process::Command;

pub(crate) fn open_in_explorer(target_path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
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

pub(crate) fn open_in_file_explorer(path: &Path) -> Result<(), String> {
    open_in_explorer(path)
}
