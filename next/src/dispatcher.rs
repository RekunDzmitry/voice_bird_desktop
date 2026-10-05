//! Bus command dispatcher for the long-lived collaborators.
//!
//! The `Downloader`, `ModelStore`, and `SourceCatalog` collaborators are wired
//! once in `main.rs` to a single [`Dispatcher`] that lives on the
//! event-loop thread. The resolver and the Quit-time cleanup no
//! longer hold them; they speak to the dispatcher through bus
//! command variants, and the dispatcher translates the command into
//! a direct collaborator call.
//!
//! ## Bus commands consumed
//!
//! - [`AppEvent::RequestBlock`](crate::bus::AppEvent::RequestBlock) —
//!   enumerate the current sources and open a source picker, or fall back to
//!   the language picker when enumeration is unavailable or has no devices.
//! - [`AppEvent::BeginLanguage`](crate::bus::AppEvent::BeginLanguage) —
//!   the resolver saw `Confirm` or `Retry`. The dispatcher calls
//!   [`download::begin_language`](crate::download::begin_language).
//! - [`AppEvent::ModelMissing`](crate::bus::AppEvent::ModelMissing) —
//!   an active block lost an installed model. The dispatcher calls
//!   [`download::ensure_model`](crate::download::ensure_model) to re-download it.
//! - [`AppEvent::DiscardInflight`] — Quit-time cleanup asks the
//!   dispatcher to drop the staged archive and unpack scratch
//!   directory for one model. No reply (best-effort).
//!
//! ## Why a loop-thread dispatcher, not a side-thread one?
//!
//! `download::begin_language` checks the model store synchronously. Keeping
//! the dispatcher on the loop thread avoids a bus-reply deadlock while the
//! loop is already inside `dispatch`.
//!
//! [`AppEvent::DiscardInflight`]: crate::bus::AppEvent::DiscardInflight

use std::sync::Arc;

use crate::audio_source::SourceCatalog;
use crate::bus::AppEvent;
use crate::db::Database;
use crate::download::{begin_language, ensure_model};
use crate::picker::{ModelEntry, CATALOG};
use crate::transcription_models::ModelStore;

/// Owns the download, model-store, and source-catalog collaborators and answers
/// the bus commands that ask them to do work. Constructed once in
/// `main::run`; never cloned.
pub struct Dispatcher {
    downloader: Arc<dyn crate::download::Downloader>,
    model_store: Arc<dyn ModelStore>,
    sources: Arc<dyn SourceCatalog>,
}

impl Dispatcher {
    /// Build a dispatcher that owns clones of the given collaborators.
    pub fn new(
        downloader: Arc<dyn crate::download::Downloader>,
        model_store: Arc<dyn ModelStore>,
        sources: Arc<dyn SourceCatalog>,
    ) -> Self {
        Self {
            downloader,
            model_store,
            sources,
        }
    }

    /// Process every bus command in `events`. Called once per tick
    /// from the main loop, after the UI reducer has folded the
    /// events. UI events are silently ignored — only the command
    /// variants do anything here.
    ///
    /// `db` is borrowed mutably for each `download::begin_language` call,
    /// so the next tick gets a fresh `&mut`.
    pub fn dispatch(&self, events: &[AppEvent], db: &mut Database, tx: &crate::bus::EventSender) {
        for ev in events {
            match ev {
                AppEvent::RequestBlock => {
                    let event = match self.sources.snapshot() {
                        Some(snapshot) if !snapshot.devices.is_empty() => {
                            AppEvent::AddSourceBlock { snapshot }
                        }
                        _ => AppEvent::AddBlock,
                    };
                    tx.publish(event);
                }
                AppEvent::BeginLanguage {
                    block, language, ..
                } => {
                    begin_language(
                        *block,
                        language,
                        self.model_store.clone(),
                        db,
                        self.downloader.clone(),
                        tx,
                    );
                }
                AppEvent::ModelMissing(entry) => {
                    ensure_model(
                        entry,
                        self.model_store.clone(),
                        db,
                        self.downloader.clone(),
                        tx,
                    );
                }
                AppEvent::DiscardInflight { model } => {
                    // `model` is `Arc<str>`; `catalog_lookup` wants
                    // `&str` so `Arc::as_ref` gives us that.
                    if let Some(entry) = catalog_lookup(model.as_ref()) {
                        self.model_store.discard_inflight(entry);
                    }
                }
                _ => {
                    // Non-command variants are silently ignored.
                    let _ = tx;
                }
            }
        }
    }
}

/// Resolve a static model id back to its `&'static ModelEntry` in
/// the catalog. Returns `None` if the id isn't in the catalog —
/// `discard_inflight` for an unknown model is a no-op rather than
/// a panic.
fn catalog_lookup(model: &str) -> Option<&'static ModelEntry> {
    CATALOG.iter().find(|e| e.id == model)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::audio_source::AudioSourceSnapshot;
    use crate::bus::EventBus;
    use crate::testing::{
        sample_source_snapshot, FixtureDownloader, FixtureSources, FixtureStore, Outcome,
    };

    fn request_block(snapshot: Option<AudioSourceSnapshot>) -> Vec<AppEvent> {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut db = Database::open(&tmp.path().join("dispatcher.db"), tx.clone()).unwrap();
        let dispatcher = Dispatcher::new(
            Arc::new(FixtureDownloader::new(
                Vec::new(),
                Outcome::Ok,
                Arc::new(AtomicUsize::new(0)),
            )),
            Arc::new(FixtureStore::new(tmp.path().into(), &[])),
            Arc::new(FixtureSources(snapshot)),
        );
        dispatcher.dispatch(&[AppEvent::RequestBlock], &mut db, &tx);
        bus.drain().collect()
    }

    #[test]
    fn request_block_starts_source_funnel_when_devices_exist() {
        let snapshot = sample_source_snapshot();
        assert_eq!(
            request_block(Some(snapshot.clone())),
            vec![AppEvent::AddSourceBlock { snapshot }]
        );
    }

    #[test]
    fn request_block_falls_back_when_sources_are_unavailable() {
        assert_eq!(request_block(None), vec![AppEvent::AddBlock]);
    }

    #[test]
    fn request_block_falls_back_when_only_apps_exist() {
        let mut snapshot = sample_source_snapshot();
        snapshot.devices.clear();
        assert_eq!(request_block(Some(snapshot)), vec![AppEvent::AddBlock]);
    }

    #[test]
    fn request_block_keeps_source_funnel_when_no_apps_are_running() {
        let mut snapshot = sample_source_snapshot();
        snapshot.apps.clear();
        assert_eq!(
            request_block(Some(snapshot.clone())),
            vec![AppEvent::AddSourceBlock { snapshot }]
        );
    }
}
