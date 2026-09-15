//! Download repository.
//!
//! Source of truth for "what is downloading right now". Keyed by model id
//! — *not* by block — which is what lets two blocks on the same model
//! share a single download.
//!
//! Shaped as a trait so swapping in sqlite later is one a impl and no
//! call-site change. `InMemoryDownloadRepository` is `Mutex<BTreeMap<..>>`
//! behind `&self`, so the trait signatures already match what a real
//! database connection pool will want.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::bus::AppEvent;

/// Phase of one download. Fetched bytes are hash-verified before the
/// format handler unpacks — for slow formats (e.g. the Nemotron tarball)
/// unpacking is a second phase; for fast ones (renaming a GGUF) it
/// collapses into the success event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Fetching,
    Installing,
}

/// One row in the repository. The shape intentionally mirrors the
/// render-side `crate::state::DownloadState` one-for-one — both are
/// folded from the same drained events, so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRecord {
    pub model: &'static str,
    pub phase: DownloadPhase,
    pub bytes: u64,
    /// `None` when the server sent no `Content-Length`.
    pub total: Option<u64>,
}

pub trait DownloadRepository: Send + Sync {
    fn get(&self, model: &str) -> Option<DownloadRecord>;
    fn all(&self) -> Vec<DownloadRecord>;
    fn upsert(&self, record: DownloadRecord);
    fn remove(&self, model: &str);
    fn is_active(&self, model: &str) -> bool {
        self.get(model).is_some()
    }
}

pub struct InMemoryDownloadRepository {
    rows: Mutex<BTreeMap<&'static str, DownloadRecord>>,
}

impl Default for InMemoryDownloadRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryDownloadRepository {
    pub fn new() -> Self {
        Self { rows: Mutex::new(BTreeMap::new()) }
    }
}

impl DownloadRepository for InMemoryDownloadRepository {
    fn get(&self, model: &str) -> Option<DownloadRecord> {
        self.rows.lock().expect("repo poisoned").get(model).cloned()
    }
    fn all(&self) -> Vec<DownloadRecord> {
        self.rows.lock().expect("repo poisoned").values().cloned().collect()
    }
    fn upsert(&self, record: DownloadRecord) {
        self.rows.lock().expect("repo poisoned").insert(record.model, record);
    }
    fn remove(&self, model: &str) {
        self.rows.lock().expect("repo poisoned").remove(model);
    }
}

/// Fold one `AppEvent` into the repository.
pub fn apply<R: DownloadRepository + ?Sized>(repo: &R, event: &AppEvent) {
    match event {
        AppEvent::DownloadRequested(entry) => {
            if repo.get(entry.id).is_none() {
                repo.upsert(DownloadRecord { model: entry.id, phase: DownloadPhase::Fetching, bytes: 0, total: None });
            }
        }
        AppEvent::DownloadProgress { model, bytes, total, .. } => {
            if let Some(mut row) = repo.get(model) {
                row.bytes = *bytes;
                row.total = *total;
                repo.upsert(row);
            }
        }
        AppEvent::DownloadInstalling { model } => {
            if let Some(mut row) = repo.get(model) {
                row.phase = DownloadPhase::Installing;
                repo.upsert(row);
            }
        }
        AppEvent::DownloadSucceeded { model }
        | AppEvent::DownloadFailed { model, .. }
        | AppEvent::DownloadCancelled { model } => {
            repo.remove(model);
        }
        _ => {}
    }
}
