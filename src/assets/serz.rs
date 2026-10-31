use super::*;

pub(crate) fn preferred_serz_path(rw_path: &Path) -> PathBuf {
    let serz64 = rw_path.join("serz64.exe");
    if serz64.is_file() {
        serz64
    } else {
        rw_path.join("serz.exe")
    }
}

pub(crate) fn fallback_serz_path(serz_path: &Path) -> Option<PathBuf> {
    if !serz_path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("serz64.exe"))
    {
        return None;
    }
    let fallback = serz_path.with_file_name("serz.exe");
    fallback.is_file().then_some(fallback)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SerzError {
    #[error("serz process failed: {0}")]
    Process(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<String> for SerzError {
    fn from(value: String) -> Self {
        Self::Process(value)
    }
}

impl From<SerzError> for String {
    fn from(value: SerzError) -> Self {
        value.to_string()
    }
}



#[derive(Debug, thiserror::Error)]
pub(crate) enum AssetError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl From<String> for AssetError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<AssetError> for String {
    fn from(value: AssetError) -> Self {
        value.to_string()
    }
}
pub(crate) struct SerzTool {
    pub(crate) primary: PathBuf,
    pub(crate) fallback: Option<PathBuf>,
}

impl SerzTool {
    pub(crate) fn new(serz_path: &Path) -> Self {
        Self {
            primary: serz_path.to_path_buf(),
            fallback: fallback_serz_path(serz_path),
        }
    }

    pub(crate) fn bin_to_xml(&self, input: &Path, output: &Path) -> Result<(), SerzError> {
        convert_bin_to_xml(&self.primary, input, output).or_else(|primary_error| {
            let Some(fallback) = &self.fallback else {
                return Err(primary_error);
            };
            convert_bin_to_xml(fallback, input, output).map_err(|fallback_error| {
                SerzError::from(format!(
                    "{primary_error}; повторная попытка через {} не удалась: {fallback_error}",
                    fallback.display()
                ))
            })
        })
    }

    pub(crate) fn xml_to_bin(&self, input: &Path, output: &Path) -> Result<(), SerzError> {
        convert_xml_to_bin(&self.primary, input, output).or_else(|primary_error| {
            let Some(fallback) = &self.fallback else {
                return Err(primary_error);
            };
            convert_xml_to_bin(fallback, input, output).map_err(|fallback_error| {
                SerzError::from(format!(
                    "{primary_error}; повторная попытка через {} не удалась: {fallback_error}",
                    fallback.display()
                ))
            })
        })
    }
}

pub(crate) fn find_serz_path(target: &Path) -> Option<PathBuf> {
    for ancestor in target.ancestors() {
        if ancestor
            .file_name()
            .map(|n| n.eq_ignore_ascii_case("Assets"))
            .unwrap_or(false)
            && let Some(root) = ancestor.parent()
        {
            for name in ["serz64.exe", "serz.exe"] {
                let p = root.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    for mut dir in [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|x| x.to_path_buf())),
    ]
    .into_iter()
    .flatten()
    {
        loop {
            for name in ["serz64.exe", "serz.exe"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
            if !dir.pop() {
                break;
            }
        }
    }
    None
}

pub(crate) fn read_serz_xml(serz_path: &Path, asset_path: &Path) -> Result<String, SerzError> {
    let temp = tempdir().map_err(|error| format!("Не удалось создать временную папку: {error}"))?;
    let xml_path = temp.path().join("asset.xml");
let conversion = SerzTool::new(serz_path).bin_to_xml(asset_path, &xml_path);
    conversion?;
    let bytes = fs::read(&xml_path)
        .map_err(|error| format!("Не удалось прочитать XML {}: {error}", xml_path.display()))?;
    Ok(decode_xml_bytes(&bytes))
}

pub(crate) fn decode_xml_bytes(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\u{FFFD}')
    {
        return text.to_owned();
    }
    let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
    decoded.into_owned()
}

/// Запускает serz.exe с таймаутом, чтобы зависший дочерний процесс не
/// блокировал пул потоков навсегда.
pub fn serz_output(serz_path: &Path, args: &[String]) -> Result<std::process::Output, SerzError> {
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let parent = serz_path
        .parent()
        .ok_or_else(|| format!("Некорректный путь к Serz: {}", serz_path.display()))?;
    let mut cmd = Command::new(serz_path);
    cmd.current_dir(parent)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .map_err(|error| format!("не удалось запустить serz.exe: {error}"))?;
    let child = Arc::new(Mutex::new(Some(child)));
    let waiter_child = child.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut guard = waiter_child.lock().unwrap();
        if let Some(child) = guard.take() {
            let _ = tx.send(child.wait_with_output());
        }
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(SerzError::Process(format!("не удалось дождаться serz.exe: {error}"))),
        Err(_) => {
            if let Some(mut child) = child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err("serz.exe завис и был остановлен по таймауту".to_owned().into())
        }
    }
}

pub(crate) fn convert_bin_to_xml(
    serz_path: &Path,
    bin_path: &Path,
    xml_path: &Path,
) -> Result<(), SerzError> {
    if xml_path.exists() {
        fs::remove_file(xml_path).map_err(|error| {
            format!(
                "не удалось удалить предыдущий временный XML {}: {error}",
                xml_path.display()
            )
        })?;
    }
    let mut args = vec![legacy_windows_path(bin_path).display().to_string()];
    args.push(format!("/xml:{}", legacy_windows_path(xml_path).display()));
    let output = serz_output(serz_path, &args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_serz_process(output.status.success(), &stdout, &stderr)
        .map_err(|error| format!("{error} (код выхода {})", output.status))?;
    let metadata = fs::metadata(xml_path).map_err(|error| {
        format!(
            "Serz завершился успешно, но выходной XML {} недоступен: {error}",
            xml_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "Serz завершился успешно, но не создал непустой XML-файл: {}",
            xml_path.display()
        ).into());
    }
    if !has_complete_xml_tail(xml_path)? {
        return Err(format!(
            "Serz создал незавершённый XML для {}. Файл использует формат, который не поддерживает конвертация в XML.",
            bin_path.display()
        ).into());
    }
    Ok(())
}

/// Проверяет, что XML-файл закрыт корневым элементом: Serz возвращает обрывок разметки
/// для некоторых устаревших форматов файлов маршрута.
pub fn has_complete_xml_tail(xml_path: &Path) -> Result<bool, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(xml_path).map_err(|error| {
        format!(
            "не удалось прочитать {} после конвертации: {error}",
            xml_path.display()
        )
    })?;
    let length = file.seek(SeekFrom::End(0)).map_err(|error| {
        format!(
            "не удалось определить размер {}: {error}",
            xml_path.display()
        )
    })?;
    let read_size = length.min(64);
    file.seek(SeekFrom::End(-(read_size as i64)))
        .map_err(|error| format!("не удалось прочитать {}: {error}", xml_path.display()))?;
    let mut tail = vec![0u8; read_size as usize];
    file.read_exact(&mut tail)
        .map_err(|error| format!("не удалось прочитать {}: {error}", xml_path.display()))?;
    Ok(tail
        .iter()
        .rev()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'>'))
}

pub(crate) fn convert_xml_to_bin(
    serz_path: &Path,
    xml_path: &Path,
    target_asset_path: &Path,
) -> Result<(), SerzError> {
    let staging_output = xml_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("staging_output.bin");
    let _ = fs::remove_file(&staging_output);

    let mut args = vec![legacy_windows_path(xml_path).display().to_string()];
    args.push(format!(
        "/bin:{}",
        legacy_windows_path(&staging_output).display()
    ));
    println!(
        "[Serz Exec] Serz: {:?}, XML: {:?}, Staging BIN: {:?}, Target: {:?}",
        serz_path, xml_path, staging_output, target_asset_path
    );
    let output = serz_output(serz_path, &args)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("[Serz Output] stdout: {}, stderr: {}", stdout, stderr);
    validate_serz_process(output.status.success(), &stdout, &stderr).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!("{error} (код выхода {})", output.status)
    })?;

    let metadata = fs::metadata(&staging_output).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!(
            "Serz завершился успешно, но выходной бинарный файл {} недоступен: {error}",
            staging_output.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        let _ = fs::remove_file(&staging_output);
        return Err(format!(
            "Serz не сгенерировал данные или вернул пустой файл: {}",
            staging_output.display()
        ).into());
    }

    fs::copy(&staging_output, target_asset_path).map_err(|error| {
        let _ = fs::remove_file(&staging_output);
        format!(
            "Не удалось скопировать скомпилированный BIN в {}: {error}",
            target_asset_path.display()
        )
    })?;
    let _ = fs::remove_file(&staging_output);
    Ok(())
}

pub(crate) fn validate_serz_process(
    status_success: bool,
    stdout: &str,
    stderr: &str,
) -> Result<(), SerzError> {
    if !status_success {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_owned()
        } else if stdout.trim().is_empty() {
            "Serz завершился с ошибкой без диагностического сообщения.".to_owned()
        } else {
            stdout.trim().to_owned()
        };
        return Err(detail.into());
    }
    if !stderr.trim().is_empty() {
        return Err(format!("Serz сообщил об ошибке: {}", stderr.trim()).into());
    }
    Ok(())
}

pub(crate) fn legacy_windows_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
        const UNC_PREFIX: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];

        let path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if let Some(path_wide) = path_wide.strip_prefix(&VERBATIM_PREFIX) {
            if let Some(unc_path) = path_wide.strip_prefix(&UNC_PREFIX) {
                let mut legacy_unc_path = vec![b'\\' as u16, b'\\' as u16];
                legacy_unc_path.extend_from_slice(unc_path);
                return PathBuf::from(std::ffi::OsString::from_wide(&legacy_unc_path));
            }
            return PathBuf::from(std::ffi::OsString::from_wide(path_wide));
        }
    }
    path.to_path_buf()
}

pub(crate) fn validate_xml_document(xml: &str) -> Result<(), String> {
    for event in xml::reader::EventReader::new(xml.as_bytes()) {
        event.map_err(|error| format!("Некорректный XML: {error}"))?;
    }
    Ok(())
}

pub(crate) fn install_converted_asset(
    source_path: &Path,
    converted_path: &Path,
) -> Result<PathBuf, String> {
    let backup_path = next_backup_path(source_path)?;
    fs::rename(source_path, &backup_path).map_err(|error| {
        format!(
            "Не удалось сохранить резервную копию {}: {error}",
            backup_path.display()
        )
    })?;
    if let Err(error) = fs::rename(converted_path, source_path) {
        return match fs::rename(&backup_path, source_path) {
            Ok(()) => Err(format!(
                "Не удалось записать новый файл: {error}. Исходник восстановлен."
            )),
            Err(restore_error) => Err(format!(
                "Не удалось записать новый файл: {error}. Не удалось восстановить исходник из {}: {restore_error}",
                backup_path.display()
            )),
        };
    }
    Ok(backup_path)
}

pub(crate) fn next_backup_path(source_path: &Path) -> Result<PathBuf, String> {
    let file_name = source_path
        .file_name()
        .ok_or_else(|| format!("Некорректный путь к файлу: {}", source_path.display()))?;
    for index in 1_u64.. {
        let mut backup_name = file_name.to_os_string();
        backup_name.push(format!(".bak{index}"));
        let backup_path = source_path.with_file_name(backup_name);
        if !backup_path.exists() {
            return Ok(backup_path);
        }
    }
    Err(format!(
        "Не удалось подобрать имя резервной копии для {}",
        source_path.display()
    ))
}

pub fn first_blueprint_tag(xml: &str) -> Option<String> {
    let regex = Regex::new(r"<(c[A-Za-z0-9]+Blueprint)").ok()?;
    regex
        .captures(xml)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

