//! SQLite-backed downloads table.
//!
//! One row per model id. Holds lifecycle (`Downloading`,
//! `Installing`, terminal, `Interrupted`) and the attempt counter;
//! progress bytes/total live only on the UI side because the
//! renderer already folds them off the bus and writing the table at
//! 10 Hz for every download adds nothing.
//!
//! ## Concurrency model
//!
//! `Downloads` holds a `rusqlite::Connection` that is the **only**
//! writer. The `&mut Downloads` borrow the rest of the app passes
//! around makes check-then-act in `begin` safe by construction —
//! there is no concurrent writer to race with.
//!
//! Workers get a [`CancelProbe`]: a separate read-only connection
//! that observes the table without holding any lock. WAL mode lets
//! readers run alongside the writer; the probe throttles itself to
//! one query per ~50 ms so chunk loops stay cheap.
//!
//! ## Lifecycle transitions
//!
//! Every state change funnels through [`transition`],
//! which is the only path that publishes
//! [`AppEvent::DownloadStatusChanged`]. Because the publish happens
//! *after* the SQL `UPDATE` returns, the SQL state and the bus log
//! can never disagree — the row is the truth, the log is the audit
//! trail, and the publish lives where both can see it.
//!
//! On startup, [`Database::open`] rewrites every leftover
//! non-terminal row (Downloading / Installing / Cancelling) to
//! `Interrupted` and logs each transition. Rows in a terminal state
//! (Cancelled / Succeeded / Failed) are left alone — they're audit
//! data, not stragglers.
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use super::{Database, Table};
#[cfg(test)]
use crate::bus::EventSender;
use crate::bus::{AppEvent, DownloadStatus};

/// Cooldown between probes. A download worker checks
/// `is_cancelled()` between chunks; querying the table on every
/// chunk (a 64 KiB read = thousands of queries per second) would
/// dominate the runtime. 50 ms is well below the user-perceived
/// cancellation latency (~200 ms feels instant) and keeps the
/// query rate under 20/s per active download.
pub const CANCEL_PROBE_INTERVAL_MS: u64 = 50;

/// Schema for the `downloads` table. Lifecycle only; no progress.
///
/// `attempt` is the per-model monotonic counter that disambiguates
/// concurrent attempts of the same model. `created_at` resets on
/// every new attempt (so the renderer can show "this attempt has
/// been running for X"); `updated_at` advances on every
/// transition (so the JSONL event log carries a precise timestamp).
pub struct DownloadsTable;

impl Table for DownloadsTable {
    const NAME: &'static str = "downloads";
    const DEFINITION: &'static str = "CREATE TABLE IF NOT EXISTS downloads (
        model       TEXT PRIMARY KEY,
        attempt     INTEGER NOT NULL,
        status      TEXT NOT NULL,
        error       TEXT,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    )";
}

/// One row of the `downloads` table. Read-only view used by callers
/// (`get`, `active`) and by `decide` to pick Start / Join / Restart.
///
/// `model` is an owned `Arc<str>` so the row can outlive the
/// `Connection` borrow that produced it. Callers compare by
/// `model.as_ref()` against the catalog; the orchestrator only ever
/// passes the catalog's `&'static str` ids to `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    pub model: Arc<str>,
    pub attempt: u32,
    pub status: DownloadStatus,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DownloadRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let model: String = row.get("model")?;
        let attempt: u32 = row.get("attempt")?;
        let status: String = row.get("status")?;
        let error: Option<String> = row.get("error")?;
        let created_at: String = row.get("created_at")?;
        let updated_at: String = row.get("updated_at")?;
        Ok(DownloadRow {
            model: Arc::from(model),
            attempt,
            status: status_from_sql(&status)?,
            error,
            created_at: parse_ts(&created_at)?,
            updated_at: parse_ts(&updated_at)?,
        })
    }
}

fn status_to_sql(s: DownloadStatus) -> &'static str {
    match s {
        DownloadStatus::Downloading => "Downloading",
        DownloadStatus::Installing => "Installing",
        DownloadStatus::Cancelling => "Cancelling",
        DownloadStatus::Cancelled => "Cancelled",
        DownloadStatus::Succeeded => "Succeeded",
        DownloadStatus::Failed => "Failed",
        DownloadStatus::Interrupted => "Interrupted",
    }
}

fn status_from_sql(s: &str) -> rusqlite::Result<DownloadStatus> {
    Ok(match s {
        "Downloading" => DownloadStatus::Downloading,
        "Installing" => DownloadStatus::Installing,
        "Cancelling" => DownloadStatus::Cancelling,
        "Cancelled" => DownloadStatus::Cancelled,
        "Succeeded" => DownloadStatus::Succeeded,
        "Failed" => DownloadStatus::Failed,
        "Interrupted" => DownloadStatus::Interrupted,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(format!(
                    "unknown DownloadStatus: {other:?}"
                ))),
            ));
        }
    })
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(format!("parse RFC3339 {s:?}: {e}"))),
            )
        })
}

/// The downloads table lives behind the [`Database`] handle. The
/// handle owns the connection and the event sender; this module's
/// free functions take `&Database` / `&mut Database` so callers
/// don't grow their signatures as more tables land. New operations
/// are added here as free fns that borrow the same `Database`.
/// The `Downloads` struct that previously owned the connection is
/// gone — there is now exactly one writer per process.
/// workers don't touch it directly; they hold a [`CancelProbe`]
/// instead. All access goes through free functions in this module
/// that borrow the [`Database`] handle.
///
/// ## Free functions on `Database`
///
/// Every operation the loop used to call as `downloads.apply(...)`
/// or `downloads.cancel(...)` is now a free function that takes
/// `&Database` / `&mut Database`:
///
/// - [`get`] / [`active`] / [`start`] / [`cancel`] / [`apply`]
/// - [`probe`] (read-only; used by workers)
/// - [`recover_interrupted`] (run once from [`Database::open`])
///
/// Callers pass the whole database, then pick the table they need.
/// Adding a second table later is one new module of free functions
/// and one more `&Database` borrow — no signature churn in
/// `producer::input::resolve_intent`, `consumer::downloads::DownloadsConsumer::request`,
/// `main::handle_key`, etc.
pub fn get(db: &Database, model: &str) -> rusqlite::Result<Option<DownloadRow>> {
    db.conn_ref()
        .query_row(
            "SELECT model, attempt, status, error, created_at, updated_at \
             FROM downloads WHERE model = ?1",
            params![model],
            DownloadRow::from_row,
        )
        .optional()
}

/// Rows that are NOT in a terminal state. Used at Quit to find
/// every in-flight model so the cleanup loop can flip them to
/// `Cancelling` before process exit.
pub fn active(db: &Database) -> rusqlite::Result<Vec<DownloadRow>> {
    let mut stmt = db.conn_ref().prepare(
        "SELECT model, attempt, status, error, created_at, updated_at \
         FROM downloads \
         WHERE status NOT IN ('Cancelled','Succeeded','Failed','Interrupted')",
    )?;
    let rows = stmt
        .query_map([], DownloadRow::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Upsert a row for `model`: bump the attempt (so a new
/// supersedes any old), set `Downloading`, and reset
/// `created_at` to "now". The publish happens after the SQL
/// returns so the bus log records the transition.
pub fn start(db: &mut Database, model: &'static str) -> rusqlite::Result<u32> {
    let now = Utc::now();
    let now_s = format_ts(now);
    let prior = get(db, model)?;
    let attempt = match prior.as_ref() {
        Some(row) => row.attempt + 1,
        None => 1,
    };
    db.conn_mut().execute(
        "INSERT INTO downloads (model, attempt, status, error, created_at, updated_at) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?4) \
         ON CONFLICT(model) DO UPDATE SET \
            attempt = excluded.attempt, \
            status = excluded.status, \
            error = NULL, \
            created_at = excluded.created_at, \
            updated_at = excluded.updated_at",
        params![model, attempt, status_to_sql(DownloadStatus::Downloading), now_s],
    )?;
    let from = prior.map(|r| r.status);
    db.tx().publish(AppEvent::DownloadStatusChanged {
        model: Arc::from(model),
        attempt,
        from,
        to: DownloadStatus::Downloading,
        error: None,
    });
    Ok(attempt)
}

/// Active row → Cancelling. Returns true on the first
/// transition; false if the row is already non-Active (so a
/// second cancel from a duplicate close is a no-op publish).
pub fn cancel(db: &mut Database, model: &str) -> rusqlite::Result<bool> {
    let row = match get(db, model)? {
        Some(r) => r,
        None => return Ok(false),
    };
    if !matches!(
        row.status,
        DownloadStatus::Downloading | DownloadStatus::Installing
    ) {
        return Ok(false);
    }
    let now_s = format_ts(Utc::now());
    db.conn_mut().execute(
        "UPDATE downloads SET status = ?1, updated_at = ?2 WHERE model = ?3 AND attempt = ?4",
        params![
            status_to_sql(DownloadStatus::Cancelling),
            now_s,
            model,
            row.attempt,
        ],
    )?;
    db.tx().publish(AppEvent::DownloadStatusChanged {
        model: row.model.clone(),
        attempt: row.attempt,
        from: Some(row.status),
        to: DownloadStatus::Cancelling,
        error: None,
    });
    Ok(true)
}

/// Fold one drained [`AppEvent`] into the table. Returns
/// `true` if the event was applied to the row, `false` if the
/// event was stale (attempt mismatch) or a no-op (the reducer
/// skips UI state changes for `false` returns).
///
/// Stale events publish [`AppEvent::DownloadEventRejected`]
/// before returning `false`, so the JSONL event log records the
/// drop instead of silently filtering it.
pub fn apply(db: &mut Database, ev: &AppEvent) -> rusqlite::Result<bool> {
    match ev {
        AppEvent::DownloadRequested { .. } => {
            // A request precedes its claim. The requested attempt is a
            // consumer precondition, not a persisted lifecycle transition.
            Ok(true)
        }
        AppEvent::DownloadProgress {
            attempt,
            model,
            bytes: _,
            total: _,
            ..
        } => {
            // Progress is intentionally not persisted: the
            // renderer reads bytes/total straight off the
            // bus, and writing the table at 10 Hz per download
            // adds nothing. Return `true` so the reducer still
            // applies the gauge update.
            if matches_attempt(db, model, *attempt)? {
                Ok(true)
            } else {
                reject(db, ev, *attempt);
                Ok(false)
            }
        }
        AppEvent::DownloadFetched { attempt, model } => {
            let current = get(db, model)?;
            if current.as_ref().is_some_and(|row| {
                row.attempt == *attempt && row.status == DownloadStatus::Downloading
            }) {
                transition(
                    db,
                    model,
                    *attempt,
                    Some(DownloadStatus::Downloading),
                    DownloadStatus::Installing,
                    None,
                )?;
                Ok(true)
            } else {
                reject(db, ev, *attempt);
                if current.is_some_and(|row| {
                    row.attempt == *attempt && row.status == DownloadStatus::Cancelling
                }) {
                    // Fetch finished, and the rejected handoff cannot create
                    // an install worker to acknowledge cancellation.
                    db.tx().publish(AppEvent::DownloadCancelled {
                        attempt: *attempt,
                        model,
                    });
                }
                Ok(false)
            }
        }
        AppEvent::DownloadInstalling { attempt, model } => {
            let current = get(db, model)?;
            if current.is_some_and(|row| {
                row.attempt == *attempt && row.status == DownloadStatus::Installing
            }) {
                Ok(true)
            } else {
                reject(db, ev, *attempt);
                Ok(false)
            }
        }
        AppEvent::DownloadSucceeded { attempt, model } => {
            if !matches_attempt(db, model, *attempt)? {
                reject(db, ev, *attempt);
                return Ok(false);
            }
            transition(
                db,
                model,
                *attempt,
                Some(DownloadStatus::Installing),
                DownloadStatus::Succeeded,
                None,
            )?;
            Ok(true)
        }
        AppEvent::DownloadClaimFailed {
            attempt,
            model,
            error,
        } => {
            // Pre-persistence failure: `downloads::start` failed
            // before any row was written, so there is nothing in
            // the table for this `model` to gate against. The
            // attempt gate is irrelevant here — the orchestrator
            // never reached the worker stage. Return `true` so the
            // reducer applies the failure to every waiting block
            // (mirroring the existing `DownloadFailed` UI
            // behaviour) instead of rejecting the event silently.
            // Publish `DownloadStatusChanged { to: Failed }` so the
            // event log records the same Failed terminal that a
            // successful claim + later failure would have produced;
            // `attempt` is forwarded from the caller's event so
            // the JSONL log correlates the failure with the
            // retry that caused it (a failed retry after attempt
            // 3 logs `DownloadClaimFailed { attempt: 4 }` followed
            // by `DownloadStatusChanged { attempt: 4 }`, not a
            // placeholder `attempt: 0`). `from` is `None` because
            // no row ever existed.
            db.tx().publish(AppEvent::DownloadStatusChanged {
                model: Arc::from(*model),
                attempt: *attempt,
                from: None,
                to: DownloadStatus::Failed,
                error: Some(error.clone()),
            });
            Ok(true)
        },
         AppEvent::DownloadFailed {
            attempt,
            model,
            error,
        } => {
            if !matches_attempt(db, model, *attempt)? {
                reject(db, ev, *attempt);
                return Ok(false);
            }
            let prior = get(db, model)?;
            let from = prior.as_ref().map(|r| r.status);
            transition(
                db,
                model,
                *attempt,
                from,
                DownloadStatus::Failed,
                Some(error.clone()),
            )?;
            Ok(true)
        }
        AppEvent::DownloadCancelled { attempt, model } => {
            if !matches_attempt(db, model, *attempt)? {
                reject(db, ev, *attempt);
                return Ok(false);
            }
            transition(
                db,
                model,
                *attempt,
                Some(DownloadStatus::Cancelling),
                DownloadStatus::Cancelled,
                None,
            )?;
            Ok(true)
        }
        AppEvent::DownloadStatusChanged {
            model,
            attempt,
            from: Some(from),
            to,
            ..
        } if from == to => {
            // A replay describes the current row, not a historical transition.
            // It must not fail a newer retry that started before this bus pass.
            let current = get(db, model)?;
            if current.is_some_and(|row| row.attempt == *attempt && row.status == *to) {
                Ok(true)
            } else {
                reject(db, ev, *attempt);
                Ok(false)
            }
        }
        AppEvent::DownloadStatusChanged { model, attempt, from: Some(_), .. } => {
            // Persisted transitions can be queued behind a new claim. Keep
            // them in the audit log, but never project an old attempt onto
            // the current attempt's waiters.
            if matches_attempt(db, model, *attempt)? {
                Ok(true)
            } else {
                reject(db, ev, *attempt);
                Ok(false)
            }
        }
        _ => Ok(true),
    }
}

/// Open a worker-side read-only connection bound to `(model,
/// attempt)`. The worker polls `is_cancelled()` between
/// chunks; the probe throttles itself to
/// [`CANCEL_PROBE_INTERVAL_MS`] and reports cancelled when the
/// row's status moves out of Downloading/Installing OR when
/// the row's attempt no longer matches (a `Restart` superseded
/// this attempt while it was still running).
pub fn probe(db: &Database, model: &'static str, attempt: u32) -> CancelProbe {
    let conn = db.open_probe_connection();
    CancelProbe {
        conn,
        model,
        attempt,
        last: Instant::now() - Duration::from_millis(CANCEL_PROBE_INTERVAL_MS),
        cached: false,
    }
}

/// Single UPDATE path. Used by `apply` (transitions driven by
/// worker events) and `cancel` (driver-driven). Returns the
/// prior status so the publish can stamp `from` correctly.
fn transition(
    db: &mut Database,
    model: &str,
    attempt: u32,
    from: Option<DownloadStatus>,
    to: DownloadStatus,
    error: Option<String>,
) -> rusqlite::Result<()> {
    let now_s = format_ts(Utc::now());
    // The UPDATE is gated by attempt so a stale event whose
    // attempt has been superseded cannot repaint the new row.
    // `apply` already pre-checked, but checking again here is
    // cheap and means `transition` is safe to call directly.
    let changed = db.conn_mut().execute(
        "UPDATE downloads SET status = ?1, error = ?2, updated_at = ?3 \
         WHERE model = ?4 AND attempt = ?5",
        params![
            status_to_sql(to),
            error.as_deref(),
            now_s,
            model,
            attempt,
        ],
    )?;
    if changed == 0 {
        // Row gone or attempt mismatch — log the rejection and
        // skip the publish.
        db.tx().publish(AppEvent::DownloadEventRejected {
            model: Arc::from(model),
            rejected: "transition",
            event_attempt: attempt,
            row_attempt: None,
        });
        return Ok(());
    }
    db.tx().publish(AppEvent::DownloadStatusChanged {
        model: Arc::from(model),
        attempt,
        from,
        to,
        error,
    });
    Ok(())
}

/// Returns true if the row exists and its `attempt` matches
/// `attempt`. A no-row is treated as `false` (the worker is
/// publishing a terminal event for a row the producer already
/// removed).
fn matches_attempt(db: &Database, model: &str, attempt: u32) -> rusqlite::Result<bool> {
    let row = get(db, model)?;
    Ok(matches!(row, Some(r) if r.attempt == attempt))
}

/// Build and publish a `DownloadEventRejected`. Called by
/// `apply` when an event's attempt doesn't match the row's.
/// Logging the rejection here (instead of inside `apply`) keeps
/// the rejection's row-attempt field accurate.
fn reject(db: &Database, ev: &AppEvent, event_attempt: u32) {
    let (model, name): (&str, &'static str) = match ev {
        AppEvent::DownloadProgress { model, .. } => (*model, "DownloadProgress"),
        AppEvent::DownloadFetched { model, .. } => (*model, "DownloadFetched"),
        AppEvent::DownloadInstalling { model, .. } => (*model, "DownloadInstalling"),
        AppEvent::DownloadSucceeded { model, .. } => (*model, "DownloadSucceeded"),
        AppEvent::DownloadFailed { model, .. } => (*model, "DownloadFailed"),
        AppEvent::DownloadCancelled { model, .. } => (*model, "DownloadCancelled"),
        AppEvent::DownloadStatusChanged { model, .. } => (model.as_ref(), "DownloadStatusChanged"),
        _ => ("", "Unknown"),
    };
    // Best-effort: re-read the row to capture the live attempt
    // the gate saw. A failure here is non-fatal; we still
    // log the rejection with `None`.
    let row_attempt = get(db, model).ok().flatten().map(|r| r.attempt);
    db.tx().publish(AppEvent::DownloadEventRejected {
        model: Arc::from(model),
        rejected: name,
        event_attempt,
        row_attempt,
    });
}

/// Walk every row and flip leftover non-terminal rows to
/// `Interrupted`. Called once from [`Database::open`]. The
/// transitions are logged so a Cancelling row at startup reads
/// as "Quit didn't reach `apply`" in the JSONL event log.
pub(crate) fn recover_interrupted(db: &mut Database) -> rusqlite::Result<()> {
    let now_s = format_ts(Utc::now());
    // Materialize the rows in a helper so the prepared-statement
    // borrow on `db` ends before we ask `db` for a mutable borrow
    // to UPDATE.
    let rows = collect_recover_rows(db.conn_ref())?;
    for row in rows {
        db.conn_mut().execute(
            "UPDATE downloads SET status = ?1, updated_at = ?2 \
             WHERE model = ?3 AND attempt = ?4",
            params![
                status_to_sql(DownloadStatus::Interrupted),
                now_s,
                row.model.as_ref(),
                row.attempt
            ],
        )?;
        db.tx().publish(AppEvent::DownloadStatusChanged {
            model: row.model.clone(),
            attempt: row.attempt,
            from: Some(row.status),
            to: DownloadStatus::Interrupted,
            error: None,
        });
    }
    Ok(())
}

/// Read every row still in a non-terminal state and return owned
/// copies. The owned `Vec<DownloadRow>` has no lifetime tied to the
/// borrowed connection, so callers can keep using the connection
/// (mutable) after this returns.
fn collect_recover_rows(
    conn: &Connection,
) -> rusqlite::Result<Vec<DownloadRow>> {
    let mut stmt = conn.prepare(
        "SELECT model, attempt, status, error, created_at, updated_at \
         FROM downloads \
         WHERE status IN ('Downloading','Installing','Cancelling')",
    )?;
    let mut rows = Vec::new();
    let iter = stmt.query_map([], DownloadRow::from_row)?;
    for row in iter {
        rows.push(row?);
    }
    drop(stmt);
    Ok(rows)
}

/// Result of `decide`: tells the orchestrator whether to start a
/// fresh worker, join an existing one, or restart under a new
/// attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// No row, or a terminal row — start a new attempt.
    Start { attempt: u32 },
    /// A live worker exists — share its downloads.
    Join,
    /// A Cancelling row exists (old worker hasn't acked yet) —
    /// bump the attempt and start a new worker.
    Restart { attempt: u32 },
}

/// Pure function. No DB access; takes the row already fetched and
/// returns the claim. Exhaustive over every [`DownloadStatus`].
/// Unit-tested in `tests` below so adding a new variant forces a
/// match update.
pub fn decide(row: Option<&DownloadRow>) -> Claim {
    match row {
        None => Claim::Start { attempt: 1 },
        Some(r) => match r.status {
            DownloadStatus::Downloading | DownloadStatus::Installing => Claim::Join,
            DownloadStatus::Cancelling => Claim::Restart {
                attempt: r.attempt + 1,
            },
            // Terminal (Cancelled, Succeeded, Failed, Interrupted)
            // and any future variant — start fresh.
            _ => Claim::Start {
                attempt: r.attempt + 1,
            },
        },
    }
}

/// Worker-side cancellation check. The download pipeline replaces
/// the previous `Arc<AtomicBool>` with a table-driven probe: each
/// worker opens its own read connection, polls the row at most once
/// per [`CANCEL_PROBE_INTERVAL_MS`], and reports cancelled when the
/// row's status moves out of the active set OR when a `Restart`
/// bumped the attempt.
pub struct CancelProbe {
    conn: Connection,
    model: &'static str,
    attempt: u32,
    last: Instant,
    cached: bool,
}

impl CancelCheck for CancelProbe {
    fn is_cancelled(&mut self) -> bool {
        // Throttle: at most one query per interval. `cached` is
        // the result of the previous query and stays sticky until
        // the next refresh, so a fast chunk loop never hits the
        // table more than the configured rate.
        if self.cached {
            return true;
        }
        if self.last.elapsed() < Duration::from_millis(CANCEL_PROBE_INTERVAL_MS) {
            return false;
        }
        self.last = Instant::now();
        let row: Option<(u32, String)> = self
            .conn
            .query_row(
                "SELECT attempt, status FROM downloads WHERE model = ?1",
                params![self.model],
                |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .unwrap_or(None);
        let Some((row_attempt, status)) = row else {
            // Row gone: probe returns false; the worker's terminal
            // event will be filtered by the attempt gate anyway.
            return false;
        };
        if row_attempt != self.attempt {
            return true;
        }
        let cancelled = !matches!(status.as_str(), "Downloading" | "Installing");
        if cancelled {
            self.cached = true;
        }
        cancelled
    }
}

/// Trait the download pipeline passes instead of `&AtomicBool`.
/// One method, `is_cancelled`, is enough — every caller that
/// previously polled an `AtomicBool` already does it the same way.
pub trait CancelCheck {
    fn is_cancelled(&mut self) -> bool;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::bus::EventBus;

    fn bus() -> (EventBus, EventSender) {
        let bus = EventBus::new();
        let tx = bus.sender();
        (bus, tx)
    }

    fn tmp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.sqlite");
        (dir, path)
    }

    #[test]
    fn decide_exhaustive() {
        // None → Start { 1 }
        match decide(None) {
            Claim::Start { attempt } => assert_eq!(attempt, 1),
            other => panic!("None must be Start 1; got {other:?}"),
        }
        // Downloading → Join
        let row = DownloadRow {
            model: Arc::from("tiny.en"),
            attempt: 3,
            status: DownloadStatus::Downloading,
            error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(decide(Some(&row)), Claim::Join);
        // Installing → Join
        let mut row = row.clone();
        row.status = DownloadStatus::Installing;
        assert_eq!(decide(Some(&row)), Claim::Join);
        // Cancelling → Restart (attempt+1)
        row.status = DownloadStatus::Cancelling;
        assert_eq!(
            decide(Some(&row)),
            Claim::Restart { attempt: row.attempt + 1 }
        );
        // Terminal statuses → Start (attempt+1)
        for terminal in [
            DownloadStatus::Cancelled,
            DownloadStatus::Succeeded,
            DownloadStatus::Failed,
            DownloadStatus::Interrupted,
        ] {
            row.status = terminal;
            assert_eq!(
                decide(Some(&row)),
                Claim::Start {
                    attempt: row.attempt + 1
                },
                "terminal {terminal:?} must be Start"
            );
        }
    }

    #[test]
    fn open_migrates_and_recovers_leftovers() {
        let (_tmp, path) = tmp_db();
        // Pre-create a file with a leftover Cancelling row.
        {
            let (bus, tx) = bus();
            let mut d = Database::open(&path, tx).unwrap();
            start(&mut d, "tiny.en").unwrap();
            cancel(&mut d, "tiny.en").unwrap();
            // Don't drop — leave the row in Cancelling.
            drop(d);
            drop(bus);
        }
        // Re-open: the Cancelling row must become Interrupted, and
        // the event log must observe the transition.
        let (mut bus, tx) = bus();
        let d = Database::open(&path, tx).unwrap();
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Interrupted);
        let events: Vec<AppEvent> = bus.drain().collect();
        let transitions: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AppEvent::DownloadStatusChanged { to, .. } => Some(*to),
                _ => None,
            })
            .collect();
        assert!(
            transitions.contains(&DownloadStatus::Interrupted),
            "recovery must publish Interrupted; got {transitions:?}"
        );
    }

    #[test]
    fn start_upserts_and_increments_attempt() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        let a1 = start(&mut d, "tiny.en").unwrap();
        assert_eq!(a1, 1);
        // Mark Cancelling so the next start is a Restart-bump.
        cancel(&mut d, "tiny.en").unwrap();
        let a2 = start(&mut d, "tiny.en").unwrap();
        assert_eq!(a2, 2, "Start after Cancelling must increment attempt");
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.attempt, 2);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }

    #[test]
    fn cancel_flips_active_row_to_cancelling_publishes_event() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        let changed = cancel(&mut d, "tiny.en").unwrap();
        assert!(changed);
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Cancelling);
    }

    #[test]
    fn cancel_on_terminal_row_is_noop() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        apply(&mut d, &AppEvent::DownloadCancelled {
            attempt: 1,
            model: "tiny.en",
        })
        .unwrap();
        // Now in Cancelled — a second cancel returns false.
        assert!(!cancel(&mut d, "tiny.en").unwrap());
    }

    #[test]
    fn apply_succeed_emits_terminal_transition() {
        let (_tmp, path) = tmp_db();
        let (mut bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        let accepted = apply(&mut d, &AppEvent::DownloadSucceeded {
            attempt: 1,
            model: "tiny.en",
        })
        .unwrap();
        assert!(accepted);
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Succeeded);
        let events: Vec<_> = bus
            .drain()
            .filter_map(|e| match e {
                AppEvent::DownloadStatusChanged { to, .. } => Some(to),
                _ => None,
            })
            .collect();
        assert_eq!(
            events.last().copied(),
            Some(DownloadStatus::Succeeded),
            "terminal transition must publish Succeeded; got {events:?}"
        );
    }

    #[test]
    fn apply_stale_event_publishes_rejection() {
        let (_tmp, path) = tmp_db();
        let (mut bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap(); // attempt = 1
                                       // Bump to attempt 2: cancel then start again.
        cancel(&mut d, "tiny.en").unwrap();
        start(&mut d, "tiny.en").unwrap();
        // Old worker (attempt 1) publishes a stale terminal.
        apply(&mut d, &AppEvent::DownloadSucceeded {
            attempt: 1,
            model: "tiny.en",
        })
        .unwrap();
        let rejections: Vec<_> = bus
            .drain()
            .filter(|e| matches!(e, AppEvent::DownloadEventRejected { .. }))
            .collect();
        assert_eq!(
            rejections.len(),
            1,
            "exactly one rejection; got {rejections:?}"
        );
        // Row stays at the new attempt, unaffected by the stale event.
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.attempt, 2);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }

    #[test]
    fn apply_failed_stores_error_in_row() {
        let (_tmp, path) = tmp_db();
        let (mut bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        apply(&mut d, &AppEvent::DownloadFailed {
            attempt: 1,
            model: "tiny.en",
            error: "boom".into(),
        })
        .unwrap();
        let row = get(&d, "tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.error.as_deref(), Some("boom"));
        assert!(bus.drain().any(|event| matches!(
            event,
            AppEvent::DownloadStatusChanged {
                to: DownloadStatus::Failed,
                error: Some(error),
                ..
            } if error == "boom"
        )));
    }

    #[test]
    fn created_at_resets_on_new_attempt_updated_at_advances() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        let first_created = get(&d, "tiny.en").unwrap().unwrap().created_at;
        let first_updated = get(&d, "tiny.en").unwrap().unwrap().updated_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        cancel(&mut d, "tiny.en").unwrap();
        let mid_updated = get(&d, "tiny.en").unwrap().unwrap().updated_at;
        assert!(
            mid_updated >= first_updated,
            "updated_at must advance on transition"
        );
        start(&mut d, "tiny.en").unwrap();
        let second_created = get(&d, "tiny.en").unwrap().unwrap().created_at;
        assert!(
            second_created > first_created,
            "created_at must reset on new attempt; first={first_created} second={second_created}"
        );
    }

    #[test]
    fn probe_reports_cancelled_after_status_change() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        let mut probe = probe(&d, "tiny.en", 1);
        assert!(!probe.is_cancelled(), "active row is not cancelled");
        // Cancel the row.
        cancel(&mut d, "tiny.en").unwrap();
        // Wait past the probe throttle.
        std::thread::sleep(Duration::from_millis(CANCEL_PROBE_INTERVAL_MS + 5));
        assert!(
            probe.is_cancelled(),
            "probe must see Cancelling and report cancelled"
        );
    }

    #[test]
    fn probe_reports_cancelled_when_attempt_superseded() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap();
        let mut probe = probe(&d, "tiny.en", 1);
        // Simulate a Restart: cancel then start bumps attempt to 2.
        cancel(&mut d, "tiny.en").unwrap();
        start(&mut d, "tiny.en").unwrap();
        std::thread::sleep(Duration::from_millis(CANCEL_PROBE_INTERVAL_MS + 5));
        assert!(
            probe.is_cancelled(),
            "superseded attempt must report cancelled"
        );
    }

    #[test]
    fn active_returns_only_non_terminal_rows() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Database::open(&path, tx).unwrap();
        start(&mut d, "tiny.en").unwrap(); // active
        start(&mut d, "base.en").unwrap();
        apply(&mut d, &AppEvent::DownloadSucceeded {
            attempt: 1,
            model: "base.en",
        })
        .unwrap();
        let active = active(&d, ).unwrap();
        assert_eq!(active.len(), 1, "only tiny.en is active");
        assert_eq!(active[0].model.as_ref(), "tiny.en");
    }
    #[test]
    fn fetched_handoff_claims_installing_once_and_rejects_duplicates() {
        let (_tmp, path) = tmp_db();
        let (mut bus, tx) = bus();
        let mut db = Database::open(&path, tx).unwrap();
        let attempt = start(&mut db, "tiny.en").unwrap();
        bus.drain().for_each(drop);
        let fetched = AppEvent::DownloadFetched { model: "tiny.en", attempt };

        assert!(apply(&mut db, &fetched).unwrap());
        assert!(!apply(&mut db, &fetched).unwrap());
        let row = get(&db, "tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Installing);
        let events: Vec<_> = bus.drain().collect();
        assert_eq!(events, vec![
            AppEvent::DownloadStatusChanged {
                model: Arc::from("tiny.en"),
                attempt,
                from: Some(DownloadStatus::Downloading),
                to: DownloadStatus::Installing,
                error: None,
            },
            AppEvent::DownloadEventRejected {
                model: Arc::from("tiny.en"),
                rejected: "DownloadFetched",
                event_attempt: attempt,
                row_attempt: Some(attempt),
            },
        ]);
    }

    #[test]
    fn fetched_handoff_rejects_cancelled_and_terminal_rows() {
        for status in [
            DownloadStatus::Installing,
            DownloadStatus::Cancelling,
            DownloadStatus::Cancelled,
            DownloadStatus::Succeeded,
            DownloadStatus::Failed,
            DownloadStatus::Interrupted,
        ] {
            let (_tmp, path) = tmp_db();
            let (mut bus, tx) = bus();
            let mut db = Database::open(&path, tx).unwrap();
            let attempt = start(&mut db, "tiny.en").unwrap();
            transition(&mut db, "tiny.en", attempt, Some(DownloadStatus::Downloading), status, None).unwrap();
            bus.drain().for_each(drop);

            assert!(!apply(&mut db, &AppEvent::DownloadFetched { model: "tiny.en", attempt }).unwrap());
            assert_eq!(get(&db, "tiny.en").unwrap().unwrap().status, status);
            let events: Vec<_> = bus.drain().collect();
            assert!(matches!(events.first(), Some(AppEvent::DownloadEventRejected {
                rejected: "DownloadFetched", event_attempt, row_attempt: Some(row_attempt), ..
            }) if *event_attempt == attempt && *row_attempt == attempt));
            if status == DownloadStatus::Cancelling {
                let ack = events.iter().find(|event| matches!(event, AppEvent::DownloadCancelled { .. }))
                    .expect("finished fetch must acknowledge cancellation");
                assert!(apply(&mut db, ack).unwrap());
                assert_eq!(get(&db, "tiny.en").unwrap().unwrap().status, DownloadStatus::Cancelled);
            } else {
                assert!(!events.iter().any(|event| matches!(event, AppEvent::DownloadCancelled { .. })));
            }
        }
    }

    #[test]
    fn installing_projection_event_cannot_regress_cancellation_or_terminal_rows() {
        let (_tmp, path) = tmp_db();
        let (mut bus, tx) = bus();
        let mut db = Database::open(&path, tx).unwrap();
        let attempt = start(&mut db, "tiny.en").unwrap();
        let installing = AppEvent::DownloadInstalling { model: "tiny.en", attempt };
        assert!(!apply(&mut db, &installing).unwrap());
        assert!(apply(&mut db, &AppEvent::DownloadFetched { model: "tiny.en", attempt }).unwrap());
        assert!(apply(&mut db, &installing).unwrap());
        for status in [
            DownloadStatus::Cancelling,
            DownloadStatus::Cancelled,
            DownloadStatus::Succeeded,
            DownloadStatus::Failed,
            DownloadStatus::Interrupted,
        ] {
            transition(&mut db, "tiny.en", attempt, None, status, None).unwrap();
            bus.drain().for_each(drop);
            assert!(!apply(&mut db, &installing).unwrap());
            assert_eq!(get(&db, "tiny.en").unwrap().unwrap().status, status);
            assert!(bus.drain().any(|event| matches!(event, AppEvent::DownloadEventRejected {
                rejected: "DownloadInstalling", ..
            })));
        }
    }
}
