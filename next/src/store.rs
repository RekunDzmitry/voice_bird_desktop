//! Download repository.
//!
//! Source of truth for "what is downloading right now". Keyed by model id
//! — *not* by block — which is what lets two blocks on the same model
//! share a single download.
//!
//! Owns two tables under one mutex: a per-model progress row (what the
//! renderer folds into `UiState.downloads`) and a per-model cancellation
//! token (what the in-flight thread polls between chunks). Both must
//! change in lock-step, so the mutex is outer; a claim is one critical
//! section that inserts both, and a cancel/terminal cleanup removes both
//! in the same critical section. Splitting them across two mutexes would
//! leave a window where a caller sees a row but no token (or vice versa).
//!
//! Shaped as a trait so swapping in sqlite later is one impl and no
//! call-site change. `InMemoryDownloadRepository` is `Mutex<DownloadTables>`
//! behind `&self`, with `request/cancel/apply_event` doing the only
//! cross-table mutations — the trait signatures already match what a real
//! database connection pool will want.
//!
//! Invariant: the store mutex is never held across `EventSender::publish`,
//! filesystem I/O, network I/O, or thread spawning. Operations that need
//! to do those return *before* invoking them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    pub total: Option<u64>,
}

/// Result of a [`DownloadRepository::request`] call.
///
/// - `Start { cancel }`: this caller owns the download. Insert the
///   progress row and a fresh `Arc<AtomicBool>` (false) under one lock;
///   the caller is expected to spawn the worker, passing the token.
///
/// - `Join`: another caller already owns the download for this model.
///   The caller should still publish `DownloadRequested` so the event
///   log records the join, but must NOT spawn a second worker.
pub enum DownloadClaim {
    Start { cancel: Arc<AtomicBool> },
    Join,
}

/// Storage facade. The trait hides the table mechanics so a future
/// SQLite backend can swap in without touching call sites. Methods that
/// mutate cross-table invariants take `&self` and an internal mutex;
/// callers never see the lock directly.
pub trait DownloadRepository: Send + Sync {
    /// Atomic claim for one model. Returns `Start` if no active claim
    /// existed (the caller should spawn), `Join` if one already does
    /// (the caller should publish `DownloadRequested` only).
    fn request(&self, model: &'static str) -> DownloadClaim;

    /// Atomic last-waiter cancellation. Removes both the progress row
    /// and the cancellation token, and sets the removed token so the
    /// in-flight thread observes the cancellation. Returns `true` if a
    /// claim existed (i.e. a worker may still be unwinding); `false`
    /// if there was nothing to cancel.
    fn cancel(&self, model: &str) -> bool;

    /// Read-only snapshot of one progress row, or `None` if no claim
    /// exists for the model.
    fn get(&self, model: &str) -> Option<DownloadRecord>;

    /// Read-only snapshot of every active progress row.
    fn all(&self) -> Vec<DownloadRecord>;

    /// Fold one drained [`AppEvent`] into the store. Idempotent — late
    /// `DownloadProgress`/`DownloadInstalling` for an absent or already
    /// terminal row are ignored.
    fn apply_event(&self, event: &AppEvent);
}

/// Combined storage tables. Held under one mutex so any operation that
/// touches both — `request`, `cancel`, terminal cleanup — runs as one
/// critical section.
struct DownloadTables {
    /// Per-model progress row. Inserted by `request`; updated by
    /// `DownloadProgress`/`DownloadInstalling`; removed by terminal
    /// events and by `cancel`.
    rows: BTreeMap<&'static str, DownloadRecord>,
    /// Per-model cancellation token. Inserted by `request`; removed
    /// by `cancel` and by terminal events.
    locks: BTreeMap<&'static str, Arc<AtomicBool>>,
}

impl DownloadTables {
    fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            locks: BTreeMap::new(),
        }
    }

    /// Take both the row and the lock out in one critical section. The
    /// token is set so the worker observes the cancellation regardless
    /// of which table it would have polled first.
    fn cancel(&mut self, model: &str) -> bool {
        let token = self.locks.remove(model);
        self.rows.remove(model);
        match token {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Remove both tables for a terminal event. Late `DownloadProgress`
    /// for an absent row is a no-op (the guard is in `apply_event`).
    fn terminal(&mut self, model: &str) {
        self.locks.remove(model);
        self.rows.remove(model);
    }

    /// Insert the initial `Fetching` row paired with a fresh token.
    /// Returns the token so the caller can spawn the worker.
    fn claim(&mut self, model: &'static str) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.rows.insert(
            model,
            DownloadRecord {
                model,
                phase: DownloadPhase::Fetching,
                bytes: 0,
                total: None,
            },
        );
        self.locks.insert(model, cancel.clone());
        cancel
    }
}

pub struct InMemoryDownloadRepository {
    tables: Mutex<DownloadTables>,
}

impl Default for InMemoryDownloadRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryDownloadRepository {
    pub fn new() -> Self {
        Self {
            tables: Mutex::new(DownloadTables::new()),
        }
    }
}

impl DownloadRepository for InMemoryDownloadRepository {
    fn request(&self, model: &'static str) -> DownloadClaim {
        let mut tables = self.tables.lock().expect("repo poisoned");
        if tables.locks.contains_key(model) {
            DownloadClaim::Join
        } else {
            let cancel = tables.claim(model);
            DownloadClaim::Start { cancel }
        }
    }

    fn cancel(&self, model: &str) -> bool {
        let mut tables = self.tables.lock().expect("repo poisoned");
        tables.cancel(model)
    }

    fn get(&self, model: &str) -> Option<DownloadRecord> {
        self.tables
            .lock()
            .expect("repo poisoned")
            .rows
            .get(model)
            .cloned()
    }

    fn all(&self) -> Vec<DownloadRecord> {
        self.tables
            .lock()
            .expect("repo poisoned")
            .rows
            .values()
            .cloned()
            .collect()
    }

    fn apply_event(&self, event: &AppEvent) {
        let mut tables = self.tables.lock().expect("repo poisoned");
        match event {
            AppEvent::DownloadRequested(entry) => {
                // request() already inserted the row; this branch is
                // a no-op for the in-memory backend but a real DB
                // would use it as the "INSERT IF NOT EXISTS" path.
                let _ = tables.rows.get(entry.id);
            }
            AppEvent::DownloadProgress {
                model,
                bytes,
                total,
                ..
            } => {
                if let Some(mut row) = tables.rows.remove(*model) {
                    row.bytes = *bytes;
                    row.total = *total;
                    tables.rows.insert(*model, row);
                }
            }
            AppEvent::DownloadInstalling { model } => {
                if let Some(mut row) = tables.rows.remove(*model) {
                    row.phase = DownloadPhase::Installing;
                    tables.rows.insert(*model, row);
                }
            }
            AppEvent::DownloadSucceeded { model }
            | AppEvent::DownloadFailed { model, .. }
            | AppEvent::DownloadCancelled { model } => {
                tables.terminal(model);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::ModelEntry;

    fn tiny() -> &'static ModelEntry {
        &crate::picker::CATALOG[5]
    }

    fn base() -> &'static ModelEntry {
        &crate::picker::CATALOG[4]
    }

    /// Two threads racing on the same model: one wins `Start`, the other
    /// gets `Join`. After the dust settles exactly one token/row pair
    /// exists.
    #[test]
    fn racing_same_model_claims_yield_one_start_one_join() {
        use std::sync::Barrier;

        let repo = Arc::new(InMemoryDownloadRepository::new());
        let barrier = Arc::new(Barrier::new(2));
        let a = Arc::clone(&repo);
        let b = Arc::clone(&repo);
        let bar_a = Arc::clone(&barrier);
        let bar_b = Arc::clone(&barrier);
        let h1 = std::thread::spawn(move || {
            bar_a.wait();
            a.request(tiny().id)
        });
        let h2 = std::thread::spawn(move || {
            bar_b.wait();
            b.request(tiny().id)
        });
        let (c1, c2) = (h1.join().unwrap(), h2.join().unwrap());
        let starts = [&c1, &c2]
            .iter()
            .filter(|c| matches!(c, DownloadClaim::Start { .. }))
            .count();
        let joins = [&c1, &c2]
            .iter()
            .filter(|c| matches!(c, DownloadClaim::Join))
            .count();
        assert_eq!(starts, 1, "exactly one Start; got {starts}");
        assert_eq!(joins, 1, "exactly one Join; got {joins}");
        assert!(repo.get(tiny().id).is_some(), "row must persist");
    }

    /// Two requests for different models: both `Start`, both rows exist.
    #[test]
    fn different_models_both_get_start_claims() {
        let repo = InMemoryDownloadRepository::new();
        match repo.request(tiny().id) {
            DownloadClaim::Start { .. } => {}
            DownloadClaim::Join => panic!("tiny must be Start"),
        }
        match repo.request(base().id) {
            DownloadClaim::Start { .. } => {}
            DownloadClaim::Join => panic!("base must be Start"),
        }
        assert!(repo.get(tiny().id).is_some());
        assert!(repo.get(base().id).is_some());
    }

    /// Cancelling an active claim flips the token, empties both tables,
    /// and reports `true`. A second cancel reports `false`.
    #[test]
    fn cancel_active_claim_empties_tables_and_returns_true() {
        let repo = InMemoryDownloadRepository::new();
        let token = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel } => cancel,
            DownloadClaim::Join => panic!("must be Start"),
        };
        assert!(!token.load(Ordering::Relaxed));

        assert!(repo.cancel(tiny().id), "active claim must report true");
        assert!(token.load(Ordering::Relaxed), "token must be flipped");

        assert!(repo.get(tiny().id).is_none(), "row must be gone");
        assert!(
            !repo.cancel(tiny().id),
            "second cancel returns false (nothing to cancel)"
        );
    }

    /// After a successful terminal event, a subsequent `request` returns
    /// `Start` with a *fresh* false token. This pins the contract that
    /// success/failure/cancellation all leave the store clean enough for
    /// a fresh attempt.
    #[test]
    fn terminal_event_then_fresh_request_yields_fresh_token() {
        let repo = InMemoryDownloadRepository::new();
        let token = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel } => cancel,
            DownloadClaim::Join => panic!("must be Start"),
        };
        repo.apply_event(&AppEvent::DownloadSucceeded { model: tiny().id });
        assert!(repo.get(tiny().id).is_none());

        let fresh = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel } => cancel,
            DownloadClaim::Join => panic!("must be Start"),
        };
        assert!(
            !fresh.load(Ordering::Relaxed),
            "fresh token must start false"
        );
        assert!(
            !Arc::ptr_eq(&token, &fresh),
            "fresh token must be a different allocation"
        );
    }

    /// Late `DownloadProgress` after a cancel must not recreate the row
    /// or the lock. (Same row-absent guard as success/failure.)
    #[test]
    fn late_progress_after_cancel_is_ignored() {
        let repo = InMemoryDownloadRepository::new();
        repo.request(tiny().id);
        repo.cancel(tiny().id);
        repo.apply_event(&AppEvent::DownloadProgress {
            model: tiny().id,
            bytes: 1024,
            total: None,
            bytes_per_sec: 0,
        });
        assert!(
            repo.get(tiny().id).is_none(),
            "late progress must not recreate the row"
        );
    }

    /// Late `DownloadCancelled` (e.g. the producer's publish reaches the
    /// bus after the worker already self-cancelled) is a no-op for the
    /// tables. Pinned because the reducer still folds the event into
    /// UiState, so the event matters even if the store is already empty.
    #[test]
    fn terminal_event_on_absent_row_is_noop() {
        let repo = InMemoryDownloadRepository::new();
        repo.apply_event(&AppEvent::DownloadCancelled { model: tiny().id });
        assert!(repo.get(tiny().id).is_none());
    }
}
