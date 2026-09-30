//! Bus command dispatcher for the long-lived collaborators.
//!
//! The `Downloader` and `ModelStore` collaborators are wired once
//! in `main.rs` to a single [`Dispatcher`] that lives on the
//! event-loop thread. The resolver and the Quit-time cleanup no
//! longer hold them; they speak to the dispatcher through bus
//! command variants, and the dispatcher translates the command into
//! a direct collaborator call.
//!
//! ## Bus commands consumed
//!
//! - [`AppEvent::BeginLanguage`](crate::bus::AppEvent::BeginLanguage) —
//!   the resolver saw `Confirm` or `Retry`. The dispatcher calls
//!   [`download::begin_language`](crate::download::begin_language).
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

use crate::bus::AppEvent;
use crate::db::Database;
use crate::download::begin_language;
use crate::picker::{ModelEntry, CATALOG};
use crate::transcription_models::ModelStore;

/// Owns the `Downloader` and `ModelStore` collaborators and answers
/// the bus commands that ask them to do work. Constructed once in
/// `main::run`; never cloned.
pub struct Dispatcher {
    downloader: Arc<dyn crate::download::Downloader>,
    model_store: Arc<dyn ModelStore>,
}

impl Dispatcher {
    /// Build a dispatcher that owns clones of the given collaborators.
    pub fn new(
        downloader: Arc<dyn crate::download::Downloader>,
        model_store: Arc<dyn ModelStore>,
    ) -> Self {
        Self {
            downloader,
            model_store,
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
                AppEvent::BeginLanguage { block, language } => {
                    begin_language(
                        *block,
                        language,
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
