//! Persisted model availability, refreshed from disk by the model watcher.
//!
//! Download lifecycle alone is not evidence that installed files still exist.
//! Accepted installation successes make a model available immediately; startup
//! and tick observations reconcile the table with the installed files.

use rusqlite::OptionalExtension;

use super::{Database, Table};

pub struct ModelsTable;

impl Table for ModelsTable {
    const NAME: &'static str = "models";
    const DEFINITION: &'static str = "\
        CREATE TABLE IF NOT EXISTS models (
            model TEXT PRIMARY KEY,
            available INTEGER NOT NULL CHECK(available IN (0, 1))
        )";
}

/// Unknown models are unavailable until observed on disk or installed.
pub fn is_available(db: &Database, model: &str) -> rusqlite::Result<bool> {
    db.conn_ref()
        .query_row(
            "SELECT available FROM models WHERE model = ?1",
            [model],
            |row| row.get(0),
        )
        .optional()
        .map(|available| available.unwrap_or(false))
}

/// Repeated observations leave unchanged rows alone.
pub fn set_available(db: &mut Database, model: &str, available: bool) -> rusqlite::Result<()> {
    db.conn_mut().execute(
        "INSERT INTO models (model, available) VALUES (?1, ?2)
         ON CONFLICT(model) DO UPDATE SET available = excluded.available
         WHERE models.available != excluded.available",
        rusqlite::params![model, available],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{AppEvent, EventBus};
    use crate::db::{self, downloads};
    use crate::language::LANGUAGES;

    #[test]
    fn observations_persist_and_unchanged_values_do_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.sqlite");
        let bus = EventBus::new();
        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert!(!is_available(&db, "tiny.en").unwrap());
        set_available(&mut db, "tiny.en", true).unwrap();
        assert!(is_available(&db, "tiny.en").unwrap());
        set_available(&mut db, "tiny.en", true).unwrap();
        assert_eq!(db.conn_ref().changes(), 0);
        drop(db);

        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert!(is_available(&db, "tiny.en").unwrap());
        set_available(&mut db, "tiny.en", false).unwrap();
        assert!(!is_available(&db, "tiny.en").unwrap());
        set_available(&mut db, "tiny.en", false).unwrap();
        assert_eq!(db.conn_ref().changes(), 0);
    }

    #[test]
    fn only_accepted_success_marks_model_available() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&dir.path().join("models.sqlite"), bus.sender()).unwrap();
        let model = LANGUAGES[0].live;
        let stale_attempt = downloads::start(&mut db, model.id).unwrap();
        downloads::cancel(&mut db, model.id).unwrap();
        let current_attempt = downloads::start(&mut db, model.id).unwrap();
        assert!(!db::apply(&mut db, &AppEvent::DownloadSucceeded {
            attempt: stale_attempt,
            model: model.id,
        }).unwrap());
        assert!(!is_available(&db, model.id).unwrap());
        assert!(db::apply(&mut db, &AppEvent::DownloadSucceeded {
            attempt: current_attempt,
            model: model.id,
        }).unwrap());
        assert!(is_available(&db, model.id).unwrap());

        // A queued cached reply is an observation, not an installation.
        set_available(&mut db, model.id, false).unwrap();
        assert!(db::apply(&mut db, &AppEvent::ModelAlreadyCached(model)).unwrap());
        assert!(!is_available(&db, model.id).unwrap());
        assert!(!db::apply(&mut db, &AppEvent::DownloadSucceeded {
            attempt: stale_attempt,
            model: model.id,
        }).unwrap());
        assert!(!is_available(&db, model.id).unwrap());
    }
}
