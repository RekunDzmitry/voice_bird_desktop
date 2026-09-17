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
//! ## Attempt tracking
//!
//! Each row carries an `attempt: u32` and a `state: ClaimState`. While
//! the worker is alive the row is `Active`. When the user closes the
//! last waiter, `cancel` flips the row to `Cancelling` (sets the token,
//! but keeps the row) so an immediate retry sees the row still exists
//! and returns `Restart` under a fresh attempt + a fresh token +
//! attempt-scoped staging paths. The old worker is still alive; it
//! writes to `<id>.<old>.tar.gz.part` while the new worker writes to
//! `<id>.<new>.tar.gz.part`. Every event the store accepts is gated on
//! `event.attempt == row.attempt`; mismatches are stale and discarded.
//!
//! The previous design released the row immediately on cancel, which
//! let an immediate retry reuse the same paths and race the still-
//! running worker: a late `DownloadInstalling` from attempt A would
//! paint into attempt B's progress row, and the GGUF handler's
//! `fs::rename` would install attempt B's partially-written `.part`
//! into the live model path. Attempt-scoped staging paths + attempt-
//! matched event filtering closes both windows.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Fetching,
    Installing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    Active,
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRecord {
    pub model: &'static str,
    pub attempt: u32,
    pub state: ClaimState,
    pub phase: DownloadPhase,
    pub bytes: u64,
    pub total: Option<u64>,
}

#[derive(Debug)]
pub enum DownloadClaim {
    Start {
        cancel: Arc<AtomicBool>,
        attempt: u32,
    },
    Join,
    Restart {
        cancel: Arc<AtomicBool>,
        attempt: u32,
    },
}

pub trait DownloadRepository: Send + Sync {
    fn request(&self, model: &'static str) -> DownloadClaim;

    /// Set the cancellation token for an active claim and mark the
    /// row as `Cancelling`. Returns `true` only on the **first**
    /// cancel transition (Active → Cancelling); a second cancel
    /// on an already-Cancelling row returns `false` so the
    /// producer doesn't double-publish `DownloadCancelled`.
    fn cancel(&self, model: &str) -> bool;

    fn get(&self, model: &str) -> Option<DownloadRecord>;

    fn all(&self) -> Vec<DownloadRecord>;

    /// Fold one drained [`AppEvent`] into the store. Returns `true`
    /// when the event was **accepted** (non-stale: either a
    /// non-download event, or a download event whose attempt matches
    /// the current row). Returns `false` when the event was a stale
    /// download event from a superseded attempt — the caller uses
    /// this signal to skip `UiState::apply` for the same event, so
    /// the UI projection can't be repainted by a worker that's been
    /// cancelled but hasn't yet acknowledged.
    ///
    /// `DownloadRequested` is always accepted: it carries no attempt
    /// (the attempt id is sealed inside `repo.request`, which the
    /// resolver already called), and the reducer needs to see it
    /// unconditionally so a focused block transitions to `Waiting`
    /// and `downloads[id]` is materialised.
    fn apply_event(&self, event: &AppEvent) -> bool;
}

struct DownloadTables {
    rows: BTreeMap<&'static str, DownloadRecord>,
    locks: BTreeMap<&'static str, Arc<AtomicBool>>,
}

impl DownloadTables {
    fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            locks: BTreeMap::new(),
        }
    }

    fn cancel(&mut self, model: &str) -> bool {
        match self.rows.get_mut(model) {
            Some(row) if row.state == ClaimState::Active => {
                let flag = self
                    .locks
                    .get(model)
                    .cloned()
                    .expect("Active row must have a token");
                flag.store(true, Ordering::Relaxed);
                row.state = ClaimState::Cancelling;
                true
            }
            _ => false,
        }
    }

    /// Drop the row AND the lock for a terminal event whose attempt
    /// matches the row's current attempt. Mismatches are silently
    /// ignored — the event came from a superseded attempt.
    fn terminal(&mut self, model: &str, attempt: u32) {
        if let Some(row) = self.rows.get(model) {
            if row.attempt == attempt {
                self.locks.remove(model);
                self.rows.remove(model);
            }
        }
    }

    fn claim(&mut self, model: &'static str, attempt: u32) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.rows.insert(
            model,
            DownloadRecord {
                model,
                attempt,
                state: ClaimState::Active,
                phase: DownloadPhase::Fetching,
                bytes: 0,
                total: None,
            },
        );
        self.locks.insert(model, cancel.clone());
        cancel
    }

    fn restart(&mut self, model: &'static str, attempt: u32) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(row) = self.rows.get_mut(model) {
            row.attempt = attempt;
            row.state = ClaimState::Active;
            row.phase = DownloadPhase::Fetching;
            row.bytes = 0;
            row.total = None;
        }
        self.locks.insert(model, cancel.clone());
        cancel
    }
}

pub struct InMemoryDownloadRepository {
    tables: Mutex<DownloadTables>,
    next_attempt: Mutex<BTreeMap<&'static str, u32>>,
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
            next_attempt: Mutex::new(BTreeMap::new()),
        }
    }

    fn take_attempt(&self, model: &'static str) -> u32 {
        let mut counters = self.next_attempt.lock().expect("counters poisoned");
        let next = counters.get(model).copied().unwrap_or(1).saturating_add(1);
        counters.insert(model, next);
        next
    }
}

impl DownloadRepository for InMemoryDownloadRepository {
    fn request(&self, model: &'static str) -> DownloadClaim {
        let attempt = self.take_attempt(model);
        let mut tables = self.tables.lock().expect("repo poisoned");
        match tables.rows.get(model) {
            None => {
                let cancel = tables.claim(model, attempt);
                DownloadClaim::Start { cancel, attempt }
            }
            Some(row) if row.state == ClaimState::Active => DownloadClaim::Join,
            Some(_) => {
                let cancel = tables.restart(model, attempt);
                DownloadClaim::Restart { cancel, attempt }
            }
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

    fn apply_event(&self, event: &AppEvent) -> bool {
        let mut tables = self.tables.lock().expect("repo poisoned");
        match event {
            AppEvent::DownloadRequested(entry) => {
                // Always accepted. The row is already inserted by
                // `request`; this branch is a no-op for the
                // in-memory backend but a real DB would use it as
                // the "INSERT IF NOT EXISTS" path.
                let _ = tables.rows.get(entry.id);
                true
            }
            AppEvent::DownloadProgress {
                attempt,
                model,
                bytes,
                total,
                ..
            } => {
                if let Some(row) = tables.rows.get_mut(*model) {
                    if row.attempt != *attempt {
                        return false;
                    }
                    row.bytes = *bytes;
                    row.total = *total;
                    true
                } else {
                    // No row for this model — could be a late event
                    // from a worker that was cancelled and
                    // superseded before this fired. Drop.
                    false
                }
            }
            AppEvent::DownloadInstalling { attempt, model } => {
                if let Some(row) = tables.rows.get_mut(*model) {
                    if row.attempt != *attempt {
                        return false;
                    }
                    row.phase = DownloadPhase::Installing;
                    true
                } else {
                    false
                }
            }
            AppEvent::DownloadSucceeded { attempt, model }
            | AppEvent::DownloadFailed { attempt, model, .. }
            | AppEvent::DownloadCancelled { attempt, model } => {
                if let Some(row) = tables.rows.get(model) {
                    if row.attempt == *attempt {
                        tables.terminal(model, *attempt);
                        true
                    } else {
                        // Stale: the row now represents a newer
                        // attempt. Don't drop it; let the
                        // newer attempt's terminal event do that.
                        false
                    }
                } else {
                    // Terminal for an already-terminal or never-
                    // existed row. Drop silently.
                    false
                }
            }
            _ => true, // Non-download events are accepted as-is;
                       // they don't touch the store but the reducer still
                       // needs to see them.
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

    #[test]
    fn different_models_both_get_start_claims() {
        let repo = InMemoryDownloadRepository::new();
        match repo.request(tiny().id) {
            DownloadClaim::Start { .. } => {}
            DownloadClaim::Join => panic!("tiny must be Start"),
            DownloadClaim::Restart { .. } => panic!("tiny must be Start"),
        }
        match repo.request(base().id) {
            DownloadClaim::Start { .. } => {}
            DownloadClaim::Join => panic!("base must be Start"),
            DownloadClaim::Restart { .. } => panic!("base must be Start"),
        }
        assert!(repo.get(tiny().id).is_some());
        assert!(repo.get(base().id).is_some());
    }

    /// `@REVIEWER_BUG_FIX cancel-immediate-retry`: cancel the active
    /// claim and IMMEDIATELY request the same model. The second
    /// request (which sees the row in `Cancelling` state) must get
    /// `Restart` with a fresh attempt id, NOT a fresh `Start` (which
    /// would have re-used the same staging paths and let two workers
    /// race on the same `.part`/`.tmp/` files).
    #[test]
    fn cancel_then_immediate_retry_returns_restart_under_new_attempt() {
        let repo = InMemoryDownloadRepository::new();

        let (cancel_a, attempt_a) = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel, attempt } => (cancel, attempt),
            other => panic!("first request must be Start; got {other:?}"),
        };

        assert!(repo.cancel(tiny().id), "active claim must report true");
        assert!(cancel_a.load(Ordering::Relaxed), "token must be flipped");
        let row = repo
            .get(tiny().id)
            .expect("row must persist through cancel");
        assert_eq!(
            row.state,
            ClaimState::Cancelling,
            "row must stay in Cancelling until worker acks"
        );
        assert_eq!(row.attempt, attempt_a, "attempt must not change on cancel");

        let (cancel_b, attempt_b) = match repo.request(tiny().id) {
            DownloadClaim::Restart { cancel, attempt } => (cancel, attempt),
            other => panic!("retry after cancel must be Restart; got {other:?}"),
        };
        assert!(
            attempt_b > attempt_a,
            "new attempt must increment; got {attempt_b} after {attempt_a}"
        );
        assert!(
            !cancel_b.load(Ordering::Relaxed),
            "new token must start false (not pre-cancelled)"
        );
        assert!(
            !Arc::ptr_eq(&cancel_a, &cancel_b),
            "new token must be a different allocation"
        );

        let row = repo
            .get(tiny().id)
            .expect("row must persist across restart");
        assert_eq!(row.state, ClaimState::Active, "new attempt is Active");
        assert_eq!(row.attempt, attempt_b);
    }

    /// After Restart, a stale terminal event for the OLD attempt
    /// must NOT remove the row.
    #[test]
    fn stale_terminal_event_for_superseded_attempt_is_ignored() {
        let repo = InMemoryDownloadRepository::new();

        let attempt_a = match repo.request(tiny().id) {
            DownloadClaim::Start { attempt, .. } => attempt,
            _ => panic!(),
        };
        assert!(repo.cancel(tiny().id));
        let attempt_b = match repo.request(tiny().id) {
            DownloadClaim::Restart { attempt, .. } => attempt,
            _ => panic!(),
        };

        repo.apply_event(&AppEvent::DownloadCancelled {
            attempt: attempt_a,
            model: tiny().id,
        });
        assert!(
            repo.get(tiny().id).is_some(),
            "stale Cancelled must not drop the row"
        );

        repo.apply_event(&AppEvent::DownloadFailed {
            attempt: attempt_a,
            model: tiny().id,
            error: "late".into(),
        });
        assert!(
            repo.get(tiny().id).is_some(),
            "stale Failed must not drop the row"
        );

        repo.apply_event(&AppEvent::DownloadSucceeded {
            attempt: attempt_b,
            model: tiny().id,
        });
        assert!(
            repo.get(tiny().id).is_none(),
            "matching terminal event must drop the row"
        );
    }

    /// Late `DownloadProgress` for a stale attempt must NOT update
    /// the new attempt's row. THIS IS THE TEST THE REVIEWER FLAGGED:
    /// without the attempt gate, a late `DownloadProgress` from a
    /// cancelled-but-still-unwinding worker would repaint the new
    /// attempt's progress row, jumping the gauge backwards.
    #[test]
    fn stale_progress_for_superseded_attempt_is_ignored() {
        let repo = InMemoryDownloadRepository::new();

        let attempt_a = match repo.request(tiny().id) {
            DownloadClaim::Start { attempt, .. } => attempt,
            _ => panic!(),
        };
        assert!(repo.cancel(tiny().id));
        let attempt_b = match repo.request(tiny().id) {
            DownloadClaim::Restart { attempt, .. } => attempt,
            _ => panic!(),
        };

        repo.apply_event(&AppEvent::DownloadProgress {
            attempt: attempt_a,
            model: tiny().id,
            bytes: 999_999,
            total: Some(999_999),
            bytes_per_sec: 0,
        });
        let row = repo.get(tiny().id).unwrap();
        assert_eq!(
            row.bytes, 0,
            "stale progress must not leak into the new attempt"
        );
        assert_eq!(row.attempt, attempt_b, "row must still represent attempt B");

        repo.apply_event(&AppEvent::DownloadProgress {
            attempt: attempt_b,
            model: tiny().id,
            bytes: 42,
            total: Some(100),
            bytes_per_sec: 1,
        });
        let row = repo.get(tiny().id).unwrap();
        assert_eq!(row.bytes, 42);
    }

    /// Late `DownloadInstalling` for a stale attempt must NOT
    /// repaint the new attempt's phase. The reviewer specifically
    /// called out the Installing event as one that needs the gate.
    #[test]
    fn stale_installing_for_superseded_attempt_is_ignored() {
        let repo = InMemoryDownloadRepository::new();

        let attempt_a = match repo.request(tiny().id) {
            DownloadClaim::Start { attempt, .. } => attempt,
            _ => panic!(),
        };
        assert!(repo.cancel(tiny().id));
        let attempt_b = match repo.request(tiny().id) {
            DownloadClaim::Restart { attempt, .. } => attempt,
            _ => panic!(),
        };

        // Old worker publishes Installing after the restart has
        // bumped to attempt B. Without the gate, this would
        // overwrite attempt B's phase to Installing even though
        // attempt B is still in Fetching.
        repo.apply_event(&AppEvent::DownloadInstalling {
            attempt: attempt_a,
            model: tiny().id,
        });
        let row = repo.get(tiny().id).unwrap();
        assert_eq!(
            row.phase,
            DownloadPhase::Fetching,
            "stale Installing must not repaint new attempt's phase"
        );
        assert_eq!(row.attempt, attempt_b);

        // The matching-attempt Installing does apply.
        repo.apply_event(&AppEvent::DownloadInstalling {
            attempt: attempt_b,
            model: tiny().id,
        });
        let row = repo.get(tiny().id).unwrap();
        assert_eq!(row.phase, DownloadPhase::Installing);
    }

    #[test]
    fn cancel_active_claim_flips_token_marks_row_and_reports_true_once() {
        let repo = InMemoryDownloadRepository::new();
        let token = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel, .. } => cancel,
            _ => panic!("must be Start"),
        };
        assert!(!token.load(Ordering::Relaxed));

        assert!(repo.cancel(tiny().id), "active claim must report true");
        assert!(token.load(Ordering::Relaxed), "token must be flipped");
        assert_eq!(repo.get(tiny().id).unwrap().state, ClaimState::Cancelling);

        assert!(
            !repo.cancel(tiny().id),
            "second cancel returns false (already Cancelling; no transition to signal)"
        );
    }

    #[test]
    fn terminal_event_then_fresh_request_yields_fresh_attempt_and_token() {
        let repo = InMemoryDownloadRepository::new();
        let (token_a, attempt_a) = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel, attempt } => (cancel, attempt),
            _ => panic!("must be Start"),
        };
        repo.apply_event(&AppEvent::DownloadSucceeded {
            attempt: attempt_a,
            model: tiny().id,
        });
        assert!(repo.get(tiny().id).is_none());

        let (token_b, attempt_b) = match repo.request(tiny().id) {
            DownloadClaim::Start { cancel, attempt } => (cancel, attempt),
            _ => panic!("must be Start"),
        };
        assert!(!token_b.load(Ordering::Relaxed));
        assert!(attempt_b > attempt_a, "attempt must increment");
        assert!(
            !Arc::ptr_eq(&token_a, &token_b),
            "token must be a fresh allocation"
        );
    }

    #[test]
    fn terminal_event_on_absent_row_is_noop() {
        let repo = InMemoryDownloadRepository::new();
        repo.apply_event(&AppEvent::DownloadCancelled {
            attempt: 1,
            model: tiny().id,
        });
        assert!(repo.get(tiny().id).is_none());
    }
}
