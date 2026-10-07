//! Persisted model availability, refreshed from disk by the model watcher.
//!
//! Download lifecycle alone is not evidence that installed files still exist.
//! Accepted installation successes make a model available immediately; startup
//! and tick observations reconcile the table with the installed files.

use rusqlite::{Connection, OptionalExtension};

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

/// Return whether an observation inserted or changed a row, without a read.
pub fn set_available(db: &mut Database, model: &str, available: bool) -> rusqlite::Result<bool> {
    set_available_on(db.conn_ref(), model, available)
}

pub(super) fn set_available_on(
    connection: &Connection,
    model: &str,
    available: bool,
) -> rusqlite::Result<bool> {
    let changed = connection.execute(
        "INSERT INTO models (model, available) VALUES (?1, ?2)
         ON CONFLICT(model) DO UPDATE SET available = excluded.available
         WHERE models.available != excluded.available",
        rusqlite::params![model, available],
    )?;
    Ok(changed != 0)
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
        assert!(set_available(&mut db, "tiny.en", false).unwrap());
        assert!(!set_available(&mut db, "tiny.en", false).unwrap());
        assert!(set_available(&mut db, "tiny.en", true).unwrap());
        assert!(is_available(&db, "tiny.en").unwrap());
        assert!(!set_available(&mut db, "tiny.en", true).unwrap());
        drop(db);

        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert!(is_available(&db, "tiny.en").unwrap());
        assert!(set_available(&mut db, "tiny.en", false).unwrap());
        assert!(!is_available(&db, "tiny.en").unwrap());
        assert!(!set_available(&mut db, "tiny.en", false).unwrap());
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
        assert!(!db::apply(&mut db, &AppEvent::ModelAvailabilityChanged {
            model, available: false,
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

    #[test]
    fn availability_gate_propagates_sql_errors_instead_of_accepting_observations() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&dir.path().join("models.sqlite"), bus.sender()).unwrap();
        db.conn_mut().execute("DROP TABLE models", []).unwrap();
        let error = db::apply(&mut db, &AppEvent::ModelAvailabilityChanged {
            model: LANGUAGES[0].live, available: false,
        }).unwrap_err();
        assert!(error.to_string().contains("models"));
    }

    #[test]
    fn success_rolls_back_when_availability_write_fails() {
        use crate::bus::DownloadStatus;
        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut bus = EventBus::new();
            let mut db = Database::open(&dir.path().join("models.sqlite"), bus.sender()).unwrap();
            let model = LANGUAGES[0].live;
            if existing {
                set_available(&mut db, model.id, false).unwrap();
            }
            let attempt = downloads::start(&mut db, model.id).unwrap();
            assert!(db::apply(&mut db, &AppEvent::DownloadFetched { attempt, model: model.id }).unwrap());
            bus.drain().for_each(drop);
            let operation = if existing { "UPDATE" } else { "INSERT" };
            db.conn_mut().execute_batch(&format!(
                "CREATE TRIGGER deny_availability BEFORE {operation} ON models
                 BEGIN SELECT RAISE(ABORT, 'availability write denied'); END;"
            )).unwrap();
            let success = AppEvent::DownloadSucceeded { attempt, model: model.id };
            assert!(db::apply(&mut db, &success).is_err());
            assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Installing);
            assert!(!is_available(&db, model.id).unwrap());
            assert!(!bus.drain().any(|event| matches!(event,
                AppEvent::DownloadStatusChanged { to: DownloadStatus::Succeeded, .. }
            )));

            db.conn_mut().execute_batch("DROP TRIGGER deny_availability").unwrap();
            assert!(db::apply(&mut db, &success).unwrap());
            assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Succeeded);
            assert!(is_available(&db, model.id).unwrap());
            assert!(bus.drain().any(|event| matches!(event,
                AppEvent::DownloadStatusChanged { attempt: published, to: DownloadStatus::Succeeded, .. }
                    if published == attempt
            )));
        }
    }

    #[test]
    fn success_rolls_back_when_commit_fails() {
        use crate::bus::DownloadStatus;
        let dir = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&dir.path().join("models.sqlite"), bus.sender()).unwrap();
        let model = LANGUAGES[0].live;
        let attempt = downloads::start(&mut db, model.id).unwrap();
        assert!(db::apply(&mut db, &AppEvent::DownloadFetched { attempt, model: model.id }).unwrap());
        bus.drain().for_each(drop);
        db.conn_mut().execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE commit_targets (id INTEGER PRIMARY KEY);
             CREATE TABLE deferred_commit_check (
                 missing INTEGER REFERENCES commit_targets(id) DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER deny_success_commit AFTER INSERT ON models
             BEGIN INSERT INTO deferred_commit_check VALUES (1); END;"
        ).unwrap();
        let success = AppEvent::DownloadSucceeded { attempt, model: model.id };
        assert!(db::apply(&mut db, &success).is_err());
        assert!(db.conn_ref().is_autocommit());
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Installing);
        assert!(!is_available(&db, model.id).unwrap());
        assert!(!bus.drain().any(|event| matches!(event,
            AppEvent::DownloadStatusChanged { to: DownloadStatus::Succeeded, .. }
        )));

        db.conn_mut().execute_batch("DROP TRIGGER deny_success_commit").unwrap();
        assert!(db::apply(&mut db, &success).unwrap());
        assert!(is_available(&db, model.id).unwrap());
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Succeeded);
    }
}
