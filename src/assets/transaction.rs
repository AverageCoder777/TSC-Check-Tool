use super::*;

#[derive(Default)]
pub(crate) struct FileTransaction {
    records: Vec<(PathBuf, PathBuf)>,
    committed: bool,
}

impl FileTransaction {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&mut self, original: PathBuf, backup: PathBuf) {
        self.records.push((original, backup));
    }

    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }

    pub(crate) fn rollback(&mut self) {
        if self.committed {
            return;
        }
        for (original, backup) in self.records.iter().rev() {
            if backup.exists() {
                if original.exists() {
                    let _ = fs::remove_file(original);
                }
                let _ = fs::rename(backup, original);
            }
        }
        self.committed = true;
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        self.rollback();
    }
}

pub(crate) fn install_converted_asset(
    source_path: &Path,
    converted_path: &Path,
) -> Result<PathBuf, String> {
    let backup_path = next_backup_path(source_path)?;
    println!("[Backup] Создаётся резервная копия: {}", backup_path.display());
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

pub fn stage_serz_xml(source: &Path, target: &Path, xml: &str) -> Result<StagedUmlautFile, String> {
    let parent = source
        .parent()
        .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?;
    let temp = create_temporary_work_directory(parent)?;
    let xml_path = temp.path().join("transliterated.xml");
    fs::write(&xml_path, xml)
        .map_err(|error| format!("Не удалось записать временный XML: {error}"))?;
    // Нативная сериализация XML -> BIN в памяти.
    let (node, preamble) = crate::serz::SerzNode::from_xml_str(xml)?;
    let bytes = crate::serz::encode_to_vec(&node, preamble.as_ref()).map_err(|error| {
        format!(
            "Не удалось подготовить изменённый файл {}: {error}",
            source.display()
        )
    })?;
    let staged = temp.path().join("transliterated.bin");
    fs::write(&staged, bytes).map_err(|error| {
        format!(
            "Не удалось подготовить изменённый файл {}: {error}",
            source.display()
        )
    })?;
    Ok(StagedUmlautFile {
        source: crate::domain::RouteFile::Path(source.to_path_buf()),
        target: target.to_path_buf(),
        staged,
        _temp: temp,
    })
}

pub fn stage_renamed_file(source: &Path, target: &Path) -> Result<StagedUmlautFile, String> {
    let parent = source
        .parent()
        .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?;
    let temp = create_temporary_work_directory(parent)?;
    let staged = temp.path().join(
        source
            .file_name()
            .ok_or_else(|| format!("Некорректный путь к ассету: {}", source.display()))?,
    );
    fs::copy(source, &staged).map_err(|error| {
        format!(
            "Не удалось подготовить переименование {}: {error}",
            source.display()
        )
    })?;
    Ok(StagedUmlautFile {
        source: crate::domain::RouteFile::Path(source.to_path_buf()),
        target: target.to_path_buf(),
        staged,
        _temp: temp,
    })
}

pub fn install_staged_file(staged: &StagedUmlautFile) -> Result<(), String> {
    match &staged.source {
        crate::domain::RouteFile::Path(_) => fs::rename(&staged.staged, &staged.target)
            .map_err(|error| format!("Не удалось установить {}: {error}", staged.target.display())),
        crate::domain::RouteFile::Archive { .. } => {
            if let Some(parent) = staged.target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("Не удалось создать папку {}: {error}", parent.display())
                })?;
            }
            fs::copy(&staged.staged, &staged.target)
                .map(|_| ())
                .map_err(|error| {
                    format!("Не удалось записать {}: {error}", staged.target.display())
                })
        }
    }
}

pub fn install_route_file(
    route_path: &Path,
    source_file: &crate::domain::RouteFile,
    converted_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let target = source_file.install_target(route_path);
    let original = match source_file {
        crate::domain::RouteFile::Path(path) => Some(path.clone()),
        crate::domain::RouteFile::Archive { .. } => None,
    };
    if let Some(original) = original
        && target == original
    {
        return install_converted_asset(&original, converted_path).map(Some);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Не удалось создать папку {}: {error}", parent.display()))?;
    }
    let backup = if target.exists() {
        let backup_path = next_backup_path(&target)?;
        fs::copy(&target, &backup_path).map_err(|error| {
            format!(
                "Не удалось создать резервную копию {}: {error}",
                backup_path.display()
            )
        })?;
        println!("[Backup] Создана резервная копия: {}", backup_path.display());
        Some(backup_path)
    } else {
        None
    };
    fs::copy(converted_path, &target)
        .map_err(|error| format!("Не удалось записать {}: {error}", target.display()))?;
    Ok(backup)
}
