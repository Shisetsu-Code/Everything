//! A maintenance lease covers an entire scan, not merely individual SQLite transactions.
use anyhow::{Context, Result};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct WriterLease {
    _file: File,
}

impl WriterLease {
    pub(crate) fn acquire(database: &Path) -> Result<Self> {
        let parent = database.parent().context("El índice no tiene carpeta")?;
        std::fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(database))?;
        file.try_lock().with_context(|| format!("El índice {} ya está abierto por otro escritor. Cierre la otra ventana o espere a que termine es index/import; las búsquedas de consola siguen disponibles.", database.display()))?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn lock_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".writer.lock");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{database::Database, model::SearchRequest};

    #[test]
    fn second_writer_is_rejected_but_readers_work_and_lock_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        let first = WriterLease::acquire(&path).unwrap();
        let db = Database::open(&path).unwrap();
        assert!(
            WriterLease::acquire(&path).is_err(),
            "two writers would race scan generations"
        );
        assert_eq!(db.search(&SearchRequest::default()).unwrap().total, 0);
        drop(first);
        assert!(WriterLease::acquire(&path).is_ok());
    }
}
