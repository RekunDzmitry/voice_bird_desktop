//! Pool-local inference identities and scheduling order. PCM and decoder handles
//! remain in the scheduler's indexed Rust slots; nothing is replayed on restart.

use rusqlite::{params, Connection, OptionalExtension};

use super::{migrate, Table};
use crate::bus::{TranscriptPass, TranscriptionJob};

struct InferenceQueueTable;

impl Table for InferenceQueueTable {
    const NAME: &'static str = "inference_queue";
    const DEFINITION: &'static str = "
        CREATE TABLE IF NOT EXISTS inference_queue (
            recording     INTEGER NOT NULL,
            pass          INTEGER NOT NULL CHECK (pass IN (0, 1)),
            status        INTEGER NOT NULL CHECK (status IN (0, 1)),
            job           INTEGER NOT NULL,
            block         INTEGER NOT NULL,
            t_start_ms    INTEGER NOT NULL,
            t_end_ms      INTEGER NOT NULL,
            sample_count  INTEGER NOT NULL,
            queued_at_ms  INTEGER NOT NULL,
            finishing     INTEGER NOT NULL CHECK (finishing IN (0, 1)),
            slot          INTEGER NOT NULL CHECK (slot >= 0),
            dispatched    INTEGER NOT NULL CHECK (dispatched IN (0, 1)),
            worker        INTEGER,
            PRIMARY KEY (recording, pass, status),
            CHECK ((status = 0 AND worker IS NULL) OR
                   (status = 1 AND worker IS NOT NULL AND dispatched = 1))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS inference_queue_pending_slot
            ON inference_queue (slot) WHERE status = 0;
        CREATE UNIQUE INDEX IF NOT EXISTS inference_queue_active_worker
            ON inference_queue (worker) WHERE status = 1;
        CREATE INDEX IF NOT EXISTS inference_queue_pending_pass
            ON inference_queue (pass, recording) WHERE status = 0;
        CREATE INDEX IF NOT EXISTS inference_queue_ready
            ON inference_queue (pass, recording) WHERE status = 0 AND dispatched = 1;
        CREATE TABLE IF NOT EXISTS inference_queue_cancelled (
            recording INTEGER PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS inference_queue_cursors (
            pass INTEGER PRIMARY KEY CHECK (pass IN (0, 1)),
            last_served INTEGER
        );
    ";
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub request: TranscriptionJob,
    pub dispatched: bool,
}

/// Access is serialized by the scheduler mutex, independently of the main DB.
/// The private in-memory connection's lifetime is exactly the pool's lifetime.
pub(crate) struct Queue {
    conn: Connection,
}

fn encode_pass(pass: TranscriptPass) -> i64 {
    match pass {
        TranscriptPass::Live => 0,
        TranscriptPass::Refined => 1,
    }
}

impl Queue {
    pub(crate) fn new() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        migrate(&conn, &[InferenceQueueTable])?;
        Ok(Self { conn })
    }

    pub(crate) fn pending(
        &self,
        recording: u64,
        pass: TranscriptPass,
    ) -> rusqlite::Result<Option<Pending>> {
        self.conn
            .prepare_cached(
                "SELECT block, recording, job, t_start_ms, t_end_ms, \
                 sample_count, queued_at_ms, finishing, dispatched \
                 FROM inference_queue WHERE recording = ?1 AND pass = ?2 AND status = 0",
            )?
            .query_row(params![recording, encode_pass(pass)], |row| {
                Ok(Pending {
                    request: TranscriptionJob {
                        block: row.get(0)?,
                        recording: row.get(1)?,
                        job: row.get(2)?,
                        pass,
                        t_start_ms: row.get(3)?,
                        t_end_ms: row.get(4)?,
                        sample_count: row.get(5)?,
                        queued_at_ms: row.get(6)?,
                        finishing: row.get(7)?,
                    },
                    dispatched: row.get(8)?,
                })
            })
            .optional()
    }

    pub(crate) fn active_matches(&self, request: &TranscriptionJob) -> rusqlite::Result<bool> {
        self.conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM inference_queue \
                 WHERE recording = ?1 AND pass = ?2 AND job = ?3 AND block = ?4 \
                 AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 \
                 AND queued_at_ms = ?8 AND finishing = ?9 AND status = 1)",
            )?
            .query_row(
                params![
                    request.recording,
                    encode_pass(request.pass),
                    request.job,
                    request.block,
                    request.t_start_ms,
                    request.t_end_ms,
                    request.sample_count,
                    request.queued_at_ms,
                    request.finishing
                ],
                |row| row.get(0),
            )
    }

    pub(crate) fn cancelled(&self, recording: u64) -> rusqlite::Result<bool> {
        self.conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM inference_queue_cancelled WHERE recording = ?1)",
            )?
            .query_row(params![recording], |row| row.get(0))
    }

    pub(crate) fn insert(&self, request: &TranscriptionJob, slot: usize) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO inference_queue \
             (recording, pass, job, block, t_start_ms, t_end_ms, sample_count, \
              queued_at_ms, finishing, slot, status, dispatched) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0)",
            )?
            .execute(params![
                request.recording,
                encode_pass(request.pass),
                request.job,
                request.block,
                request.t_start_ms,
                request.t_end_ms,
                request.sample_count,
                request.queued_at_ms,
                request.finishing,
                slot
            ])?;
        Ok(())
    }

    pub(crate) fn dispatch(&self, request: &TranscriptionJob) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "UPDATE inference_queue SET dispatched = 1 \
             WHERE recording = ?1 AND pass = ?2 AND job = ?3 AND block = ?4 \
             AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 \
             AND queued_at_ms = ?8 AND finishing = ?9 AND status = 0 AND dispatched = 0 \
             AND NOT EXISTS (SELECT 1 FROM inference_queue_cancelled WHERE recording = ?1)",
            )?
            .execute(params![
                request.recording,
                encode_pass(request.pass),
                request.job,
                request.block,
                request.t_start_ms,
                request.t_end_ms,
                request.sample_count,
                request.queued_at_ms,
                request.finishing
            ])?
            != 0)
    }

    /// Two indexed range scans implement the strict successor and wraparound.
    /// A cancellation fences dispatch, not work already admitted for execution.
    pub(crate) fn next_ready(
        &self,
        pass: TranscriptPass,
    ) -> rusqlite::Result<Option<(u64, usize)>> {
        let pass = encode_pass(pass);
        let next = self.conn.prepare_cached(
            "SELECT pending.recording, pending.slot FROM inference_queue AS pending \
             WHERE pending.pass = ?1 AND pending.status = 0 AND pending.dispatched = 1 \
             AND pending.recording > COALESCE(\
                 (SELECT last_served FROM inference_queue_cursors WHERE pass = ?1), -1) \
             AND NOT EXISTS (SELECT 1 FROM inference_queue AS active \
                 WHERE active.recording = pending.recording AND active.pass = ?1 AND active.status = 1) \
             ORDER BY pending.recording LIMIT 1",
        )?.query_row(params![pass], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        if next.is_some() {
            return Ok(next);
        }
        self.conn.prepare_cached(
            "SELECT pending.recording, pending.slot FROM inference_queue AS pending \
             WHERE pending.pass = ?1 AND pending.status = 0 AND pending.dispatched = 1 \
             AND pending.recording <= (SELECT last_served FROM inference_queue_cursors WHERE pass = ?1) \
             AND NOT EXISTS (SELECT 1 FROM inference_queue AS active \
                 WHERE active.recording = pending.recording AND active.pass = ?1 AND active.status = 1) \
             ORDER BY pending.recording LIMIT 1",
        )?.query_row(params![pass], |row| Ok((row.get(0)?, row.get(1)?))).optional()
    }

    pub(crate) fn count(&self, pass: TranscriptPass) -> rusqlite::Result<u32> {
        self.conn
            .prepare_cached("SELECT count(*) FROM inference_queue WHERE pass = ?1 AND status = 0")?
            .query_row(params![encode_pass(pass)], |row| row.get(0))
    }

    pub(crate) fn active_count(&self) -> rusqlite::Result<usize> {
        self.conn
            .prepare_cached("SELECT count(*) FROM inference_queue WHERE status = 1")?
            .query_row([], |row| row.get(0))
    }

    pub(crate) fn worker_busy(&self, worker: u8) -> rusqlite::Result<bool> {
        self.conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM inference_queue WHERE status = 1 AND worker = ?1)",
            )?
            .query_row(params![worker], |row| row.get(0))
    }

    /// A failed claim leaves both the pending row and round-robin cursor intact.
    pub(crate) fn claim(
        &mut self,
        recording: u64,
        pass: TranscriptPass,
        worker: u8,
    ) -> rusqlite::Result<usize> {
        let pass = encode_pass(pass);
        let tx = self.conn.transaction()?;
        let slot = tx
            .prepare_cached(
                "UPDATE inference_queue SET status = 1, worker = ?3 \
             WHERE recording = ?1 AND pass = ?2 AND status = 0 AND dispatched = 1 \
             AND NOT EXISTS (SELECT 1 FROM inference_queue AS active \
                 WHERE active.recording = ?1 AND active.pass = ?2 AND active.status = 1) \
             RETURNING slot",
            )?
            .query_row(params![recording, pass, worker], |row| row.get(0))?;
        tx.prepare_cached(
            "INSERT INTO inference_queue_cursors (pass, last_served) VALUES (?1, ?2) \
             ON CONFLICT(pass) DO UPDATE SET last_served = excluded.last_served",
        )?
        .execute(params![pass, recording])?;
        tx.commit()?;
        Ok(slot)
    }

    pub(crate) fn complete(&self, request: &TranscriptionJob, worker: u8) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "DELETE FROM inference_queue \
             WHERE recording = ?1 AND pass = ?2 AND job = ?3 AND block = ?4 \
             AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 \
             AND queued_at_ms = ?8 AND finishing = ?9 AND status = 1 AND worker = ?10",
            )?
            .execute(params![
                request.recording,
                encode_pass(request.pass),
                request.job,
                request.block,
                request.t_start_ms,
                request.t_end_ms,
                request.sample_count,
                request.queued_at_ms,
                request.finishing,
                worker
            ])?;
        Ok(())
    }

    pub(crate) fn remove(
        &self,
        recording: u64,
        pass: TranscriptPass,
    ) -> rusqlite::Result<Option<usize>> {
        self.conn
            .prepare_cached(
                "DELETE FROM inference_queue WHERE recording = ?1 AND pass = ?2 AND status = 0 RETURNING slot",
            )?
            .query_row(params![recording, encode_pass(pass)], |row| row.get(0))
            .optional()
    }

    /// Drain actual rows, not a fixed list of passes. Admitted jobs stay queued.
    pub(crate) fn remove_unaccepted(&self, recording: u64) -> rusqlite::Result<Option<usize>> {
        self.conn
            .prepare_cached(
                "DELETE FROM inference_queue WHERE rowid = (\
                    SELECT rowid FROM inference_queue \
                    WHERE recording = ?1 AND status = 0 AND dispatched = 0 \
                    ORDER BY pass LIMIT 1\
                 ) RETURNING slot",
            )?
            .query_row(params![recording], |row| row.get(0))
            .optional()
    }

    pub(crate) fn cancel(&self, recording: u64) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO inference_queue_cancelled (recording) VALUES (?1) ON CONFLICT DO NOTHING",
            )?
            .execute(params![recording])?;
        Ok(())
    }

    pub(crate) fn clear_pending(&self) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM inference_queue WHERE status = 0")?
            .execute([])?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn abort_inserts_for_test(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TEMP TRIGGER inference_queue_abort_inserts
             BEFORE INSERT ON inference_queue
             BEGIN
                 SELECT RAISE(ABORT, 'injected inference queue insert failure');
             END;",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(recording: u64, pass: TranscriptPass) -> TranscriptionJob {
        TranscriptionJob {
            block: 1,
            recording,
            job: recording + 100,
            pass,
            t_start_ms: 10,
            t_end_ms: 20,
            sample_count: 160,
            queued_at_ms: 30,
            finishing: false,
        }
    }

    fn mismatches(request: TranscriptionJob) -> [TranscriptionJob; 9] {
        [
            TranscriptionJob {
                block: request.block + 1,
                ..request
            },
            TranscriptionJob {
                recording: request.recording + 1,
                ..request
            },
            TranscriptionJob {
                job: request.job + 1,
                ..request
            },
            TranscriptionJob {
                pass: TranscriptPass::Refined,
                ..request
            },
            TranscriptionJob {
                t_start_ms: request.t_start_ms + 1,
                ..request
            },
            TranscriptionJob {
                t_end_ms: request.t_end_ms + 1,
                ..request
            },
            TranscriptionJob {
                sample_count: request.sample_count + 1,
                ..request
            },
            TranscriptionJob {
                queued_at_ms: request.queued_at_ms + 1,
                ..request
            },
            TranscriptionJob {
                finishing: !request.finishing,
                ..request
            },
        ]
    }

    #[test]
    fn dispatch_and_completion_require_every_metadata_field() {
        let mut queue = Queue::new().unwrap();
        let original = request(10, TranscriptPass::Live);
        queue.insert(&original, 4).unwrap();
        assert_eq!(
            queue.pending(10, TranscriptPass::Live).unwrap(),
            Some(Pending {
                request: original,
                dispatched: false,
            })
        );
        for stale in mismatches(original) {
            assert!(!queue.dispatch(&stale).unwrap());
        }
        assert!(queue.dispatch(&original).unwrap());
        assert!(!queue.dispatch(&original).unwrap());
        assert_eq!(queue.claim(10, TranscriptPass::Live, 1).unwrap(), 4);
        for stale in mismatches(original) {
            assert!(!queue.active_matches(&stale).unwrap());
            queue.complete(&stale, 1).unwrap();
            assert!(queue.active_matches(&original).unwrap());
        }
        queue.complete(&original, 2).unwrap();
        assert!(queue.worker_busy(1).unwrap());
        queue.complete(&original, 1).unwrap();
        assert!(!queue.active_matches(&original).unwrap());
        assert!(!queue.worker_busy(1).unwrap());
    }

    #[test]
    fn claims_are_exclusive_and_failed_claims_roll_back_pending_and_cursor() {
        let mut queue = Queue::new().unwrap();
        let first = request(10, TranscriptPass::Live);
        queue.insert(&first, 0).unwrap();
        queue.dispatch(&first).unwrap();
        assert_eq!(queue.claim(10, TranscriptPass::Live, 1).unwrap(), 0);
        let following = TranscriptionJob {
            job: first.job + 1,
            ..first
        };
        queue.insert(&following, 0).unwrap();
        queue.dispatch(&following).unwrap();
        assert!(queue.claim(10, TranscriptPass::Live, 2).is_err());
        assert_eq!(
            queue
                .pending(10, TranscriptPass::Live)
                .unwrap()
                .unwrap()
                .request,
            following
        );
        for (recording, slot) in [(20, 1), (30, 2)] {
            let next = request(recording, TranscriptPass::Live);
            queue.insert(&next, slot).unwrap();
            queue.dispatch(&next).unwrap();
        }
        assert!(queue.claim(30, TranscriptPass::Live, 1).is_err());
        assert_eq!(
            queue.next_ready(TranscriptPass::Live).unwrap(),
            Some((20, 1))
        );
        assert_eq!(
            queue
                .pending(30, TranscriptPass::Live)
                .unwrap()
                .unwrap()
                .request,
            request(30, TranscriptPass::Live)
        );
        assert_eq!(queue.active_count().unwrap(), 1);
        assert_eq!(queue.count(TranscriptPass::Live).unwrap(), 3);
        assert!(!queue.worker_busy(2).unwrap());
        queue.complete(&first, 1).unwrap();
        assert_eq!(queue.claim(10, TranscriptPass::Live, 2).unwrap(), 0);
        let undispatched = request(40, TranscriptPass::Refined);
        queue.insert(&undispatched, 0).unwrap();
        assert!(queue.claim(40, TranscriptPass::Refined, 3).is_err());
        assert_eq!(
            queue
                .pending(40, TranscriptPass::Refined)
                .unwrap()
                .unwrap()
                .request,
            undispatched
        );
        assert_eq!(queue.next_ready(TranscriptPass::Refined).unwrap(), None);
    }

    #[test]
    fn round_robin_wraps_and_keeps_pass_cursors_independent() {
        let mut queue = Queue::new().unwrap();
        for (recording, slot) in [(10, 0), (20, 1), (30, 2)] {
            let next = request(recording, TranscriptPass::Live);
            queue.insert(&next, slot).unwrap();
            queue.dispatch(&next).unwrap();
        }
        let refined = request(5, TranscriptPass::Refined);
        queue.insert(&refined, 3).unwrap();
        queue.dispatch(&refined).unwrap();
        for (recording, slot) in [(10, 0), (20, 1), (30, 2), (10, 0)] {
            assert_eq!(
                queue.next_ready(TranscriptPass::Live).unwrap(),
                Some((recording, slot))
            );
            assert_eq!(
                queue.next_ready(TranscriptPass::Refined).unwrap(),
                Some((5, 3))
            );
            assert_eq!(
                queue.claim(recording, TranscriptPass::Live, 1).unwrap(),
                slot
            );
            let served = request(recording, TranscriptPass::Live);
            queue.complete(&served, 1).unwrap();
            queue.insert(&served, slot).unwrap();
            queue.dispatch(&served).unwrap();
        }
    }

    #[test]
    fn cancellation_fences_only_that_recording_and_preserves_dispatched_work() {
        let mut queue = Queue::new().unwrap();
        let admitted = request(10, TranscriptPass::Live);
        let blocked = request(10, TranscriptPass::Refined);
        let replacement = request(11, TranscriptPass::Live);
        queue.insert(&admitted, 0).unwrap();
        queue.insert(&blocked, 1).unwrap();
        queue.insert(&replacement, 2).unwrap();
        assert!(queue.dispatch(&admitted).unwrap());
        queue.cancel(10).unwrap();
        queue.cancel(10).unwrap();
        assert!(queue.cancelled(10).unwrap());
        assert!(!queue.cancelled(11).unwrap());
        assert!(!queue.dispatch(&blocked).unwrap());
        assert_eq!(queue.remove_unaccepted(10).unwrap(), Some(1));
        assert_eq!(queue.remove_unaccepted(10).unwrap(), None);
        assert_eq!(
            queue
                .pending(11, TranscriptPass::Live)
                .unwrap()
                .unwrap()
                .request,
            replacement
        );
        assert!(queue.dispatch(&replacement).unwrap());
        assert_eq!(
            queue.next_ready(TranscriptPass::Live).unwrap(),
            Some((10, 0))
        );
        assert_eq!(queue.claim(10, TranscriptPass::Live, 1).unwrap(), 0);
        queue.clear_pending().unwrap();
        assert_eq!(queue.count(TranscriptPass::Live).unwrap(), 0);
        assert_eq!(queue.count(TranscriptPass::Refined).unwrap(), 0);
        assert!(queue.active_matches(&admitted).unwrap());
        assert!(queue.worker_busy(1).unwrap());
        assert!(queue.cancelled(10).unwrap());
    }

    #[test]
    fn new_queue_has_no_rows_or_cursors_from_another_pool() {
        let mut first = Queue::new().unwrap();
        let previous = request(20, TranscriptPass::Live);
        first.insert(&previous, 0).unwrap();
        first.dispatch(&previous).unwrap();
        first.claim(20, TranscriptPass::Live, 1).unwrap();
        first.cancel(30).unwrap();
        let mut second = Queue::new().unwrap();
        assert_eq!(second.active_count().unwrap(), 0);
        assert_eq!(second.count(TranscriptPass::Live).unwrap(), 0);
        assert_eq!(second.count(TranscriptPass::Refined).unwrap(), 0);
        assert_eq!(second.pending(20, TranscriptPass::Live).unwrap(), None);
        assert!(!second.cancelled(30).unwrap());
        assert!(!second.worker_busy(1).unwrap());
        for (recording, slot) in [(10, 0), (30, 1)] {
            let next = request(recording, TranscriptPass::Live);
            second.insert(&next, slot).unwrap();
            second.dispatch(&next).unwrap();
        }
        assert_eq!(
            second.next_ready(TranscriptPass::Live).unwrap(),
            Some((10, 0))
        );
        assert_eq!(second.claim(10, TranscriptPass::Live, 1).unwrap(), 0);
        drop(first);
        let third = Queue::new().unwrap();
        assert_eq!(third.active_count().unwrap(), 0);
        assert_eq!(third.next_ready(TranscriptPass::Live).unwrap(), None);
        assert!(!third.cancelled(30).unwrap());
    }
}
