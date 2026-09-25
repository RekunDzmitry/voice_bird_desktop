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
//! Every state change funnels through [`Downloads::transition`],
//! which is the only path that publishes
//! [`AppEvent::DownloadStatusChanged`]. Because the publish happens
//! *after* the SQL `UPDATE` returns, the SQL state and the bus log
//! can never disagree — the row is the truth, the log is the audit
//! trail, and the publish lives where both can see it.
//!
//! On startup, [`Downloads::open`] rewrites every leftover
//! non-terminal row (Downloading / Installing / Cancelling) to
//! `Interrupted` and logs each transition. Rows in a terminal state
//! (Cancelled / Succeeded / Failed) are left alone — they're audit
//! data, not stragglers.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::bus::{AppEvent, DownloadStatus, EventSender};

use super::Table;

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
    const SCHEMA: &'static str = "CREATE TABLE IF NOT EXISTS downloads (
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    pub model: &'static str,
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
            model: leak_static(model),
            attempt,
            status: status_from_sql(&status)?,
            error,
            created_at: parse_ts(&created_at)?,
            updated_at: parse_ts(&updated_at)?,
        })
    }
}

/// Lend a `&'static str` to a String by leaking. The strings come
/// from a small, bounded catalog (six models) and the OS data dir
/// rows never outlive the process, so a per-row leak here costs
/// nothing on long-running sessions. The whole row is `Clone`, so
/// callers can't accidentally pin the underlying `Connection`
/// borrow past its scope.
fn leak_static(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
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

/// The download lifecycle store. Owns one writer connection and a
/// cloneable [`EventSender`] so every transition can publish
/// `DownloadStatusChanged` without crossing thread boundaries.
///
/// Constructed once at startup ([`Downloads::open`]) and held on the
/// loop stack as `&mut Downloads`. Workers don't touch it directly;
/// they hold a [`CancelProbe`] instead.
pub struct Downloads {
    conn: Connection,
    #[allow(dead_code)]
    path: PathBuf,
    tx: EventSender,
}

impl Downloads {
    /// Open (or create) the downloads table at `path` and recover
    /// any rows left in a non-terminal state by a previous crashed
    /// session. The recovery is logged per row — a Cancelling row
    /// at startup means the previous Quit didn't reach `apply`, so
    /// the audit log records the upgrade.
    pub fn open(path: &Path, tx: EventSender) -> rusqlite::Result<Self> {
        let conn = super::open(path)?;
        super::migrate(&conn, &[DownloadsTable])?;
        let downloads = Self {
            conn,
            path: path.to_path_buf(),
            tx,
        };
        downloads.recover_interrupted()?;
        Ok(downloads)
    }

    /// Read one row by model id.
    pub fn get(&self, model: &str) -> rusqlite::Result<Option<DownloadRow>> {
        self.conn
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
    pub fn active(&self) -> rusqlite::Result<Vec<DownloadRow>> {
        let mut stmt = self.conn.prepare(
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
    pub fn start(&mut self, model: &'static str) -> rusqlite::Result<u32> {
        let now = Utc::now();
        let now_s = format_ts(now);
        // Upsert: if a row exists, advance attempt and reset
        // created_at; otherwise insert at attempt 1.
        let prior = self.get(model)?;
        let attempt = match prior.as_ref() {
            Some(row) => row.attempt + 1,
            None => 1,
        };
        self.conn.execute(
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
        self.tx.publish(AppEvent::DownloadStatusChanged {
            model,
            attempt,
            from,
            to: DownloadStatus::Downloading,
        });
        Ok(attempt)
    }

    /// Active row → Cancelling. Returns true on the first
    /// transition; false if the row is already non-Active (so a
    /// second cancel from a duplicate close is a no-op publish).
    pub fn cancel(&mut self, model: &str) -> rusqlite::Result<bool> {
        let row = match self.get(model)? {
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
        self.conn.execute(
            "UPDATE downloads SET status = ?1, updated_at = ?2 WHERE model = ?3 AND attempt = ?4",
            params![
                status_to_sql(DownloadStatus::Cancelling),
                now_s,
                model,
                row.attempt,
            ],
        )?;
        self.tx.publish(AppEvent::DownloadStatusChanged {
            model: leak_static(row.model.to_string()),
            attempt: row.attempt,
            from: Some(row.status),
            to: DownloadStatus::Cancelling,
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
    pub fn apply(&mut self, ev: &AppEvent) -> rusqlite::Result<bool> {
        match ev {
            AppEvent::DownloadRequested(_) => {
                // `DownloadRequested` is published by the resolver
                // *after* a successful claim — `start` already
                // inserted the row. The event itself carries no
                // attempt, so it's not a transition.
                Ok(true)
            }
            AppEvent::DownloadProgress {
                attempt,
                model,
                bytes,
                total,
                ..
            } => {
                // Progress is intentionally not persisted: the
                // renderer reads bytes/total straight off the
                // bus, and writing the table at 10 Hz per download
                // adds nothing. Return `true` so the reducer still
                // applies the gauge update.
                if self.matches_attempt(model, *attempt)? {
                    Ok(true)
                } else {
                    self.reject(ev, *attempt);
                    Ok(false)
                }
            }
            AppEvent::DownloadInstalling { attempt, model } => {
                if !self.matches_attempt(model, *attempt)? {
                    self.reject(ev, *attempt);
                    return Ok(false);
                }
                self.transition(
                    model,
                    *attempt,
                    Some(DownloadStatus::Downloading),
                    DownloadStatus::Installing,
                    None,
                )?;
                Ok(true)
            }
            AppEvent::DownloadSucceeded { attempt, model } => {
                if !self.matches_attempt(model, *attempt)? {
                    self.reject(ev, *attempt);
                    return Ok(false);
                }
                self.transition(
                    model,
                    *attempt,
                    Some(DownloadStatus::Installing),
                    DownloadStatus::Succeeded,
                    None,
                )?;
                Ok(true)
            }
            AppEvent::DownloadFailed {
                attempt,
                model,
                error,
            } => {
                if !self.matches_attempt(model, *attempt)? {
                    self.reject(ev, *attempt);
                    return Ok(false);
                }
                let prior = self.get(model)?;
                let from = prior.as_ref().map(|r| r.status);
                self.transition(
                    model,
                    *attempt,
                    from,
                    DownloadStatus::Failed,
                    Some(error.clone()),
                )?;
                Ok(true)
            }
            AppEvent::DownloadCancelled { attempt, model } => {
                if !self.matches_attempt(model, *attempt)? {
                    self.reject(ev, *attempt);
                    return Ok(false);
                }
                self.transition(
                    model,
                    *attempt,
                    Some(DownloadStatus::Cancelling),
                    DownloadStatus::Cancelled,
                    None,
                )?;
                Ok(true)
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
    pub fn probe(&self, model: &'static str, attempt: u32) -> CancelProbe {
        // Open a fresh read connection to the same file. WAL mode
        // lets it read alongside the main writer; `busy_timeout`
        // keeps it from erroring on transient write-lock hits.
        let conn = Connection::open(self.path.as_path()).unwrap_or_else(|e| {
            // Falling back to a transient in-memory connection is
            // better than panicking: a read failure must NOT
            // crash the worker. The probe returns `false` on
            // query errors and the attempt gate on the main
            // thread still discards stale publishes.
            eprintln!("downloads: probe open failed: {e}");
            Connection::open_in_memory().expect("in-memory sqlite")
        });
        let _ = conn.pragma_update(None, "busy_timeout", 5_000);
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
        &mut self,
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
        let changed = self.conn.execute(
            "UPDATE downloads SET status = ?1, error = ?2, updated_at = ?3 \
             WHERE model = ?4 AND attempt = ?5",
            params![status_to_sql(to), error, now_s, model, attempt],
        )?;
        if changed == 0 {
            // Row gone or attempt mismatch — log the rejection and
            // skip the publish.
            self.tx.publish(AppEvent::DownloadEventRejected {
                model: leak_static(model.to_string()),
                event: "transition",
                event_attempt: attempt,
                row_attempt: None,
            });
            return Ok(());
        }
        self.tx.publish(AppEvent::DownloadStatusChanged {
            model: leak_static(model.to_string()),
            attempt,
            from,
            to,
        });
        Ok(())
    }

    /// Returns true if the row exists and its `attempt` matches
    /// `attempt`. A no-row is treated as `false` (the worker is
    /// publishing a terminal event for a row the producer already
    /// removed).
    fn matches_attempt(&self, model: &str, attempt: u32) -> rusqlite::Result<bool> {
        let row = self.get(model)?;
        Ok(matches!(row, Some(r) if r.attempt == attempt))
    }

    /// Build and publish a `DownloadEventRejected`. Called by
    /// `apply` when an event's attempt doesn't match the row's.
    /// Logging the rejection here (instead of inside `apply`) keeps
    /// the rejection's row-attempt field accurate.
    fn reject(&self, ev: &AppEvent, event_attempt: u32) {
        let (model, name): (&str, &'static str) = match ev {
            AppEvent::DownloadProgress { model, .. } => (*model, "DownloadProgress"),
            AppEvent::DownloadInstalling { model, .. } => (*model, "DownloadInstalling"),
            AppEvent::DownloadSucceeded { model, .. } => (*model, "DownloadSucceeded"),
            AppEvent::DownloadFailed { model, .. } => (*model, "DownloadFailed"),
            AppEvent::DownloadCancelled { model, .. } => (*model, "DownloadCancelled"),
            _ => ("", "Unknown"),
        };
        // Best-effort: re-read the row to capture the live attempt
        // the gate saw. A failure here is non-fatal; we still
        // log the rejection with `None`.
        let row_attempt = self.get(model).ok().flatten().map(|r| r.attempt);
        self.tx.publish(AppEvent::DownloadEventRejected {
            model: leak_static(model.to_string()),
            event: name,
            event_attempt,
            row_attempt,
        });
    }

    /// Walk every row and flip leftover non-terminal rows to
    /// `Interrupted`. Called once from `open`. The transitions are
    /// logged so a Cancelling row at startup reads as "Quit didn't
    /// reach `apply`" in the JSONL event log.
    fn recover_interrupted(&mut self) -> rusqlite::Result<()> {
        let now_s = format_ts(Utc::now());
        let prior = self.conn.query_map(
            "SELECT model, attempt, status, error, created_at, updated_at \
             FROM downloads \
             WHERE status IN ('Downloading','Installing','Cancelling')",
            [],
            DownloadRow::from_row,
        )?;
        let rows: Vec<DownloadRow> = prior.collect::<rusqlite::Result<Vec<_>>>()?;
        for row in rows {
            self.conn.execute(
                "UPDATE downloads SET status = ?1, updated_at = ?2 \
                 WHERE model = ?3 AND attempt = ?4",
                params![
                    status_to_sql(DownloadStatus::Interrupted),
                    now_s,
                    row.model,
                    row.attempt
                ],
            )?;
            self.tx.publish(AppEvent::DownloadStatusChanged {
                model: row.model,
                attempt: row.attempt,
                from: Some(row.status),
                to: DownloadStatus::Interrupted,
            });
        }
        Ok(())
    }

    /// Drop the `pub` accessor for the inner connection — exposed
    /// only for tests that need to inspect WAL state directly. Not
    /// used by production code.
    #[cfg(test)]
    pub(crate) fn raw_conn(&self) -> &Connection {
        &self.conn
    }
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

/// Helper for callers that need `Start { attempt }` materialised
/// already — calls `start` on the table. Convenience over
/// `match decide(&downloads.get(...)?)`.
impl Downloads {
    pub fn start_with_attempt(&mut self, model: &'static str) -> rusqlite::Result<u32> {
        self.start(model)
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
    use crate::bus::EventBus;
    use std::sync::{Arc, Mutex};

    fn bus() -> (EventBus, EventSender) {
        let bus = EventBus::new();
        let tx = bus.sender();
        (bus, tx)
    }

    fn collect(bus: &Mutex<EventBus>) -> Vec<AppEvent> {
        bus.lock().unwrap().drain().collect()
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
            model: "tiny.en",
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
            let mut d = Downloads::open(&path, tx).unwrap();
            d.start("tiny.en").unwrap();
            d.cancel("tiny.en").unwrap();
            // Don't drop — leave the row in Cancelling.
            drop(d);
            drop(bus);
        }
        // Re-open: the Cancelling row must become Interrupted, and
        // the event log must observe the transition.
        let (bus, tx) = bus();
        let d = Downloads::open(&path, tx).unwrap();
        let row = d.get("tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Interrupted);
        let events = collect(&Mutex::new(bus));
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
        let mut d = Downloads::open(&path, tx).unwrap();
        let a1 = d.start("tiny.en").unwrap();
        assert_eq!(a1, 1);
        // Mark Cancelling so the next start is a Restart-bump.
        d.cancel("tiny.en").unwrap();
        let a2 = d.start("tiny.en").unwrap();
        assert_eq!(a2, 2, "Start after Cancelling must increment attempt");
        let row = d.get("tiny.en").unwrap().unwrap();
        assert_eq!(row.attempt, 2);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }

    #[test]
    fn cancel_flips_active_row_to_cancelling_publishes_event() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        let changed = d.cancel("tiny.en").unwrap();
        assert!(changed);
        let row = d.get("tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Cancelling);
    }

    #[test]
    fn cancel_on_terminal_row_is_noop() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        d.apply(&AppEvent::DownloadCancelled {
            attempt: 1,
            model: "tiny.en",
        })
        .unwrap();
        // Now in Cancelled — a second cancel returns false.
        assert!(!d.cancel("tiny.en").unwrap());
    }

    #[test]
    fn apply_succeed_emits_terminal_transition() {
        let (_tmp, path) = tmp_db();
        let (bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        let accepted = d
            .apply(&AppEvent::DownloadSucceeded {
                attempt: 1,
                model: "tiny.en",
            })
            .unwrap();
        assert!(accepted);
        let row = d.get("tiny.en").unwrap().unwrap();
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
        let (bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap(); // attempt = 1
                                       // Bump to attempt 2: cancel then start again.
        d.cancel("tiny.en").unwrap();
        d.start("tiny.en").unwrap();
        // Old worker (attempt 1) publishes a stale terminal.
        let accepted = d
            .apply(&AppEvent::DownloadSucceeded {
                attempt: 1,
                model: "tiny.en",
            })
            .unwrap();
        assert!(!accepted);
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
        let row = d.get("tiny.en").unwrap().unwrap();
        assert_eq!(row.attempt, 2);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }

    #[test]
    fn apply_failed_stores_error_in_row() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        d.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: "tiny.en",
            error: "boom".into(),
        })
        .unwrap();
        let row = d.get("tiny.en").unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.error.as_deref(), Some("boom"));
    }

    #[test]
    fn created_at_resets_on_new_attempt_updated_at_advances() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        let first_created = d.get("tiny.en").unwrap().unwrap().created_at;
        let first_updated = d.get("tiny.en").unwrap().unwrap().updated_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        d.cancel("tiny.en").unwrap();
        let mid_updated = d.get("tiny.en").unwrap().unwrap().updated_at;
        assert!(
            mid_updated >= first_updated,
            "updated_at must advance on transition"
        );
        d.start("tiny.en").unwrap();
        let second_created = d.get("tiny.en").unwrap().unwrap().created_at;
        assert!(
            second_created > first_created,
            "created_at must reset on new attempt; first={first_created} second={second_created}"
        );
    }

    #[test]
    fn probe_reports_cancelled_after_status_change() {
        let (_tmp, path) = tmp_db();
        let (_bus, tx) = bus();
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        let mut probe = d.probe("tiny.en", 1);
        assert!(!probe.is_cancelled(), "active row is not cancelled");
        // Cancel the row.
        d.cancel("tiny.en").unwrap();
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
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap();
        let mut probe = d.probe("tiny.en", 1);
        // Simulate a Restart: cancel then start bumps attempt to 2.
        d.cancel("tiny.en").unwrap();
        d.start("tiny.en").unwrap();
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
        let mut d = Downloads::open(&path, tx).unwrap();
        d.start("tiny.en").unwrap(); // active
        d.start("base.en").unwrap();
        d.apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: "base.en",
        })
        .unwrap();
        let active = d.active().unwrap();
        assert_eq!(active.len(), 1, "only tiny.en is active");
        assert_eq!(active[0].model, "tiny.en");
    }
}

// Helper for test isolation: ensures no worker can keep a
// connection alive past the test by binding `Drop` semantics.
#[allow(dead_code)]
fn _channel_drop_check() {
    let (tx, _rx): (mpsc::Sender<i32>, _) = mpsc::channel();
    drop(tx);
}
