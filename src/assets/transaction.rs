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
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (original, backup) in &self.records {
            if backup.exists() {
                let _ = fs::rename(backup, original);
            }
        }
    }
}

pub struct StagedUmlautFile {
    /// Исходный файл: запись архива .ap или файл на диске.
    pub source: crate::domain::RouteFile,
    /// Куда класть результат.
    pub target: PathBuf,
    pub staged: PathBuf,
    pub _temp: tempfile::TempDir,
}

