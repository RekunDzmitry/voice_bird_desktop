//! Exact attempt-scoped staging paths, prepared by model-store observations.
//!
//! Preparation failures are data, not download lifecycle transitions. A worker
//! reads only its own attempt's path or actionable error before fetching.

use std::ffi::OsString;
use std::path::PathBuf;

use rusqlite::OptionalExtension;

use super::{Database, Table};
use crate::picker::ModelEntry;
use crate::transcription_models::ModelStore;

pub struct ModelStagingTable;

impl Table for ModelStagingTable {
    const NAME: &'static str = "model_staging";
    const DEFINITION: &'static str = "\
        CREATE TABLE IF NOT EXISTS model_staging (
            model TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            path BLOB,
            error TEXT,
            PRIMARY KEY (model, attempt),
            CHECK ((path IS NOT NULL AND error IS NULL)
                OR (path IS NULL AND error IS NOT NULL))
        )";
}

/// Resolve the store's exact native path, retaining preparation failures.
/// An unchanged observation does not rewrite the row; other attempts are intact.
pub fn observe(
    db: &mut Database,
    store: &dyn ModelStore,
    entry: &ModelEntry,
    attempt: u32,
) -> rusqlite::Result<()> {
    let (path, error) = match store.staging_path(entry, attempt) {
        Ok(path) => (Some(encode_path(path)), None),
        Err(error) => (None, Some(error.to_string())),
    };
    db.conn_mut().execute(
        "INSERT INTO model_staging (model, attempt, path, error) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(model, attempt) DO UPDATE SET path = excluded.path, error = excluded.error
         WHERE model_staging.path IS NOT excluded.path
            OR model_staging.error IS NOT excluded.error",
        rusqlite::params![entry.id, attempt, path, error],
    )?;
    Ok(())
}

/// Read only the requested attempt; absent metadata never falls back to history.
pub fn get(
    db: &Database,
    model: &str,
    attempt: u32,
) -> rusqlite::Result<Option<Result<PathBuf, String>>> {
    db.conn_ref()
        .query_row(
            "SELECT path, error FROM model_staging WHERE model = ?1 AND attempt = ?2",
            rusqlite::params![model, attempt],
            |row| {
                let path: Option<Vec<u8>> = row.get(0)?;
                match path {
                    Some(path) => Ok(Ok(decode_path(path)?)),
                    None => Ok(Err(row.get(1)?)),
                }
            },
        )
        .optional()
}

// SQLite is machine-local. Preserve native OS paths, including non-UTF-8 names.
#[cfg(unix)]
fn encode_path(path: PathBuf) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    path.into_os_string().into_vec()
}

#[cfg(unix)]
fn decode_path(bytes: Vec<u8>) -> rusqlite::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn encode_path(path: PathBuf) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(windows)]
fn decode_path(bytes: Vec<u8>) -> rusqlite::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-16 staging path")),
        ));
    }
    let wide: Vec<_> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::db::downloads::CancelCheck;
    use crate::picker::CATALOG;
    use crate::testing::FixtureStore;
    use crate::transcription_models::DownloadError;
    use std::path::Path;

    struct PreparationStore;

    impl ModelStore for PreparationStore {
        fn is_available(&self, _: &ModelEntry) -> bool { false }

        fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError> {
            if attempt == 1 {
                Err(DownloadError::Io("cache directory is read-only".into()))
            } else {
                Ok(PathBuf::from(format!("custom/{}.{}.archive", entry.id, attempt)))
            }
        }

        fn install(&self, _: &ModelEntry, _: &Path, _: &mut dyn CancelCheck) -> Result<(), DownloadError> {
            unreachable!("metadata observation must not install")
        }

        fn clear_staging(&self, _: &ModelEntry) {
            unreachable!("metadata observation must not clear staging")
        }

        fn discard_inflight(&self, _: &ModelEntry) {
            unreachable!("metadata observation must not discard staging")
        }
    }

    #[test]
    fn paths_and_preparation_errors_are_isolated_and_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.sqlite");
        let bus = EventBus::new();
        let mut db = Database::open(&path, bus.sender()).unwrap();
        let model = &CATALOG[0];
        assert_eq!(get(&db, model.id, 1).unwrap(), None);
        observe(&mut db, &PreparationStore, model, 1).unwrap();
        let error = get(&db, model.id, 1).unwrap().unwrap().unwrap_err();
        assert!(error.contains("cache directory is read-only"));
        observe(&mut db, &PreparationStore, model, 2).unwrap();
        let expected = PathBuf::from(format!("custom/{}.2.archive", model.id));
        assert_eq!(get(&db, model.id, 2).unwrap(), Some(Ok(expected.clone())));
        assert_eq!(get(&db, model.id, 1).unwrap(), Some(Err(error.clone())));
        assert_eq!(get(&db, model.id, 3).unwrap(), None);
        observe(&mut db, &PreparationStore, model, 2).unwrap();
        assert_eq!(db.conn_ref().changes(), 0);
        observe(&mut db, &PreparationStore, model, 1).unwrap();
        assert_eq!(db.conn_ref().changes(), 0);
        drop(db);
        let db = Database::open(&path, bus.sender()).unwrap();
        assert_eq!(get(&db, model.id, 1).unwrap(), Some(Err(error)));
        assert_eq!(get(&db, model.id, 2).unwrap(), Some(Ok(expected)));
    }

    #[test]
    fn a_new_observation_can_replace_an_error_for_only_its_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&dir.path().join("models.sqlite"), bus.sender()).unwrap();
        let model = &CATALOG[0];
        observe(&mut db, &PreparationStore, model, 1).unwrap();
        observe(&mut db, &PreparationStore, model, 2).unwrap();
        let second = get(&db, model.id, 2).unwrap();
        let store = FixtureStore::new(dir.path().to_path_buf(), &[]);
        observe(&mut db, &store, model, 1).unwrap();
        assert_eq!(get(&db, model.id, 1).unwrap(), Some(Ok(dir.path().join(format!("{}.1.part", model.id)))));
        assert_eq!(get(&db, model.id, 2).unwrap(), second);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_survives_observation_and_database_reopen() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let path = dir.path().join("models.sqlite");
        let mut db = Database::open(&path, bus.sender()).unwrap();
        let root = dir.path().join(OsString::from_vec(b"cache-\xff".to_vec()));
        let store = FixtureStore::new(root, &[]);
        let model = &CATALOG[0];
        observe(&mut db, &store, model, 1).unwrap();
        let expected = store.staging_path(model, 1).unwrap();
        assert_eq!(get(&db, model.id, 1).unwrap(), Some(Ok(expected.clone())));
        drop(db);
        let db = Database::open(&path, bus.sender()).unwrap();
        assert_eq!(get(&db, model.id, 1).unwrap(), Some(Ok(expected)));
    }
}
