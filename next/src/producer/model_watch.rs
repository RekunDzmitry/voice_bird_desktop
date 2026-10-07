//! Refresh model availability from disk at startup and on each loop tick.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::db::{downloads, model_staging, models, Database};
use crate::picker::CATALOG;
use crate::transcription_models::ModelStore;

pub struct ModelWatcher {
    store: Arc<dyn ModelStore>,
}

impl ModelWatcher {
    pub fn new(store: Arc<dyn ModelStore>) -> Self {
        Self { store }
    }

    /// Refresh the entire catalog, including models no block currently uses.
    /// Publish inserted or changed observations without making session decisions.
    pub fn check(&self, db: &mut Database, tx: &EventSender) -> rusqlite::Result<()> {
        for model in CATALOG {
            let available = self.store.is_available(model);
            let row = downloads::get(db, model.id)?;
            let attempt = match downloads::decide(row.as_ref()) {
                downloads::Claim::Start { attempt } | downloads::Claim::Restart { attempt } => attempt,
                downloads::Claim::Join => row.as_ref().expect("Join requires a download row").attempt,
            };
            model_staging::observe(db, self.store.as_ref(), model, attempt)?;
            // Prepare metadata first: a staging SQL failure must not consume
            // an availability transition that the next scan needs to publish.
            if models::set_available(db, model.id, available)? {
                tx.publish(AppEvent::ModelAvailabilityChanged { model, available });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::testing::FixtureStore;
    use crate::bus::DownloadStatus;
    use crate::transcription_models::CacheDirStore;

    #[test]
    fn startup_and_ticks_discover_cached_models_without_active_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("cache")).unwrap());
        let cached = &CATALOG[0];
        let installed_later = &CATALOG[1];
        std::fs::write(store.root().join(format!("{}.gguf", cached.id)), b"installed").unwrap();
        let watcher = ModelWatcher::new(store.clone());
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();

        watcher.check(&mut db, &bus.sender()).unwrap();
        assert!(models::is_available(&db, cached.id).unwrap());
        assert!(!models::is_available(&db, installed_later.id).unwrap());
        assert!(downloads::get(&db, cached.id).unwrap().is_none());
        assert_eq!(bus.drain().collect::<Vec<_>>(), CATALOG.iter().map(|model| {
            AppEvent::ModelAvailabilityChanged { model, available: model.id == cached.id }
        }).collect::<Vec<_>>());
        for model in CATALOG {
            assert_eq!(models::is_available(&db, model.id).unwrap(), model.id == cached.id);
            assert!(downloads::get(&db, model.id).unwrap().is_none());
        }
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);

        std::fs::write(store.root().join(format!("{}.gguf", installed_later.id)), b"installed").unwrap();
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert!(models::is_available(&db, installed_later.id).unwrap());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![
            AppEvent::ModelAvailabilityChanged { model: installed_later, available: true },
        ]);
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);

        std::fs::remove_file(store.root().join(format!("{}.gguf", cached.id))).unwrap();
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![
            AppEvent::ModelAvailabilityChanged { model: cached, available: false },
        ]);
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn restart_reconciles_stale_availability_independently_of_download_history() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models.sqlite");
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("cache")).unwrap());
        let removed = &CATALOG[0];
        let installed = &CATALOG[1];
        let watcher = ModelWatcher::new(store.clone());
        let mut bus = EventBus::new();
        let mut db = Database::open(&path, bus.sender()).unwrap();
        let attempt = downloads::start(&mut db, removed.id).unwrap();
        crate::db::apply(&mut db, &AppEvent::DownloadSucceeded {
            attempt,
            model: removed.id,
        }).unwrap();
        models::set_available(&mut db, installed.id, false).unwrap();
        drop(db);
        std::fs::write(store.root().join(format!("{}.gguf", installed.id)), b"installed").unwrap();
        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert!(models::is_available(&db, removed.id).unwrap());
        assert!(!models::is_available(&db, installed.id).unwrap());
        bus.drain().for_each(drop);

        watcher.check(&mut db, &bus.sender()).unwrap();
        assert!(!models::is_available(&db, removed.id).unwrap());
        assert!(models::is_available(&db, installed.id).unwrap());
        assert_eq!(downloads::get(&db, removed.id).unwrap().unwrap().status, crate::bus::DownloadStatus::Succeeded);
        assert_eq!(bus.drain().collect::<Vec<_>>(), CATALOG.iter().map(|model| {
            AppEvent::ModelAvailabilityChanged { model, available: model.id == installed.id }
        }).collect::<Vec<_>>());
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn startup_prepares_first_active_and_interrupted_retry_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models.sqlite");
        let bus = EventBus::new();
        let mut db = Database::open(&path, bus.sender()).unwrap();
        let model = &CATALOG[0];
        let store = Arc::new(FixtureStore::new(tmp.path().join("custom-cache"), &[]));
        let watcher = ModelWatcher::new(store.clone());

        watcher.check(&mut db, &bus.sender()).unwrap();
        let first_path = store.root.join(format!("{}.1.part", model.id));
        assert_eq!(model_staging::get(&db, model.id, 1).unwrap(), Some(Ok(first_path.clone())));
        assert!(downloads::get(&db, model.id).unwrap().is_none());

        let attempt = downloads::start(&mut db, model.id).unwrap();
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(model_staging::get(&db, model.id, attempt).unwrap(), Some(Ok(first_path.clone())));
        assert_eq!(model_staging::get(&db, model.id, attempt + 1).unwrap(), None);
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Downloading);
        drop(db);

        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Interrupted);
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(model_staging::get(&db, model.id, attempt).unwrap(), Some(Ok(first_path)));
        assert_eq!(model_staging::get(&db, model.id, attempt + 1).unwrap(),
            Some(Ok(store.root.join(format!("{}.{}.part", model.id, attempt + 1)))));
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().attempt, attempt);
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Interrupted);
    }

    #[test]
    fn cancellation_and_terminal_ticks_prepare_next_attempt_without_overwriting_history() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        let model = &CATALOG[0];
        let old_store = Arc::new(FixtureStore::new(tmp.path().join("old-cache"), &[]));
        let old_watcher = ModelWatcher::new(old_store.clone());
        let attempt = downloads::start(&mut db, model.id).unwrap();
        old_watcher.check(&mut db, &bus.sender()).unwrap();
        let first = model_staging::get(&db, model.id, attempt).unwrap();
        downloads::cancel(&mut db, model.id).unwrap();
        let new_store = Arc::new(FixtureStore::new(tmp.path().join("new-cache"), &[]));
        let watcher = ModelWatcher::new(new_store.clone());

        watcher.check(&mut db, &bus.sender()).unwrap();
        let second_path = new_store.root.join(format!("{}.{}.part", model.id, attempt + 1));
        assert_eq!(model_staging::get(&db, model.id, attempt + 1).unwrap(), Some(Ok(second_path.clone())));
        assert_eq!(model_staging::get(&db, model.id, attempt).unwrap(), first);
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Cancelling);
        crate::db::apply(&mut db, &AppEvent::DownloadCancelled { attempt, model: model.id }).unwrap();

        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(model_staging::get(&db, model.id, attempt + 1).unwrap(), Some(Ok(second_path)));
        assert_eq!(model_staging::get(&db, model.id, attempt).unwrap(), first);
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().status, DownloadStatus::Cancelled);
        assert_eq!(downloads::get(&db, model.id).unwrap().unwrap().attempt, attempt);
    }

    #[test]
    fn staging_sql_errors_propagate_without_starting_a_download() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        db.conn_mut().execute("DROP TABLE model_staging", []).unwrap();
        let watcher = ModelWatcher::new(Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[])));
        let error = watcher.check(&mut db, &bus.sender()).unwrap_err();
        assert!(error.to_string().contains("model_staging"));
        assert!(downloads::get(&db, CATALOG[0].id).unwrap().is_none());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
        let rows: usize = db.conn_ref().query_row("SELECT count(*) FROM models", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn staging_sql_failure_does_not_consume_a_missing_transition_on_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        let model = &CATALOG[0];
        models::set_available(&mut db, model.id, true).unwrap();
        db.conn_mut().execute("DROP TABLE model_staging", []).unwrap();
        let watcher = ModelWatcher::new(Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[])));

        assert!(watcher.check(&mut db, &bus.sender()).is_err());
        assert!(models::is_available(&db, model.id).unwrap());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
        db.conn_mut().execute_batch(
            <model_staging::ModelStagingTable as crate::db::Table>::DEFINITION,
        ).unwrap();

        watcher.check(&mut db, &bus.sender()).unwrap();
        assert!(!models::is_available(&db, model.id).unwrap());
        assert_eq!(bus.drain().collect::<Vec<_>>(), CATALOG.iter().map(|model| {
            AppEvent::ModelAvailabilityChanged { model, available: false }
        }).collect::<Vec<_>>());
        assert!(downloads::get(&db, model.id).unwrap().is_none());
        watcher.check(&mut db, &bus.sender()).unwrap();
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn availability_sql_errors_propagate_without_publishing_or_claiming() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        db.conn_mut().execute("DROP TABLE models", []).unwrap();
        let watcher = ModelWatcher::new(Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[])));

        let error = watcher.check(&mut db, &bus.sender()).unwrap_err();
        assert!(error.to_string().contains("models"));
        assert!(downloads::get(&db, CATALOG[0].id).unwrap().is_none());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
    }
}
