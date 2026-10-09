//! Durable inference lifecycle, independent of transcript output and UI watermarks.
//!
//! Admission requires an active exact recording. Once admitted, a job may finish
//! after its block is closed/reused or its recording becomes terminal. Immutable
//! request metadata and the running worker fence stale lifecycle events.

use rusqlite::params;

use super::{Database, Table};
use crate::bus::{AppEvent, TranscriptPass};

pub struct TranscriptionJobsTable;

impl Table for TranscriptionJobsTable {
    const NAME: &'static str = "transcription_jobs";
    const DEFINITION: &'static str = "
        CREATE TABLE IF NOT EXISTS transcription_jobs (
            recording_id      INTEGER NOT NULL REFERENCES recordings(id),
            job_id            INTEGER NOT NULL,
            pass              TEXT NOT NULL CHECK (pass IN ('live', 'refined')),
            block             INTEGER NOT NULL,
            t_start_ms        INTEGER NOT NULL,
            t_end_ms          INTEGER NOT NULL,
            sample_count      INTEGER NOT NULL,
            queued_at_ms      INTEGER NOT NULL,
            finishing         INTEGER NOT NULL CHECK (finishing IN (0, 1)),
            status            TEXT NOT NULL CHECK (status IN ('queued', 'running', 'finished', 'failed')),
            worker            INTEGER,
            start_ms          INTEGER,
            end_ms            INTEGER,
            duration_ms       INTEGER,
            queue_wait_ms     INTEGER,
            queue_jobs_start  INTEGER,
            queue_jobs_end    INTEGER,
            queue_size_start  INTEGER,
            queue_size_end    INTEGER,
            error             TEXT,
            PRIMARY KEY (recording_id, pass, job_id)
        )";
}

/// The vanished process cannot complete these jobs. Keep observed timings and
/// backlog intact; an interruption has no observed completion time or worker for
/// a queued job. Reopening again leaves the same terminal rows unchanged.
pub fn recover_interrupted(db: &mut Database) -> rusqlite::Result<()> {
    db.conn_mut().execute(
        "UPDATE transcription_jobs SET status = 'failed', error = 'interrupted by process shutdown' \
         WHERE status IN ('queued', 'running')",
        [],
    )?;
    Ok(())
}

fn pass_name(pass: TranscriptPass) -> &'static str {
    match pass {
        TranscriptPass::Live => "live",
        TranscriptPass::Refined => "refined",
    }
}

/// Each transition is one conditional statement, so duplicate events cannot
/// reactivate a job or overwrite its terminal telemetry.
pub fn apply(db: &mut Database, event: &AppEvent) -> rusqlite::Result<bool> {
    let changed = match event {
        AppEvent::TranscriptionRequested { request } => db.conn_mut().execute(
            "INSERT INTO transcription_jobs \
             (job_id, recording_id, pass, block, t_start_ms, t_end_ms, sample_count, queued_at_ms, finishing, status) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued' \
             WHERE EXISTS (SELECT 1 FROM recordings WHERE id = ?2 AND block = ?4 AND status = 'recording') \
             ON CONFLICT DO NOTHING",
            params![request.job, request.recording, pass_name(request.pass), request.block,
                request.t_start_ms, request.t_end_ms, request.sample_count, request.queued_at_ms, request.finishing],
        )?,
        AppEvent::TranscriptionStarted { request, worker, start_ms, queue_jobs, queue_size_start } => db.conn_mut().execute(
            "UPDATE transcription_jobs SET status = 'running', worker = ?10, start_ms = ?11, \
             queue_wait_ms = ?12, queue_jobs_start = ?13, queue_size_start = ?14 \
             WHERE job_id = ?1 AND recording_id = ?2 AND pass = ?3 AND block = ?4 \
             AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 AND queued_at_ms = ?8 \
             AND finishing = ?9 AND status = 'queued'",
            params![request.job, request.recording, pass_name(request.pass), request.block,
                request.t_start_ms, request.t_end_ms, request.sample_count, request.queued_at_ms, request.finishing,
                worker, start_ms, start_ms.saturating_sub(request.queued_at_ms), queue_jobs, queue_size_start],
        )?,
        AppEvent::TranscriptionFinished { request, worker, transcription, queue_wait_ms, queue_jobs } => db.conn_mut().execute(
            "UPDATE transcription_jobs SET status = 'finished', start_ms = ?11, end_ms = ?12, duration_ms = ?13, \
             queue_wait_ms = ?14, queue_jobs_end = ?15, queue_size_start = ?16, queue_size_end = ?17 \
             WHERE job_id = ?1 AND recording_id = ?2 AND pass = ?3 AND block = ?4 \
             AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 AND queued_at_ms = ?8 \
             AND finishing = ?9 AND status = 'running' AND worker = ?10",
            params![request.job, request.recording, pass_name(request.pass), request.block,
                request.t_start_ms, request.t_end_ms, request.sample_count, request.queued_at_ms, request.finishing,
                worker, transcription.start_ms, transcription.end_ms, transcription.duration_ms,
                queue_wait_ms, queue_jobs, transcription.queue_size_start, transcription.queue_size_end],
        )?,
        AppEvent::TranscriptionFailed { request, worker, transcription, queue_wait_ms, queue_jobs, error } => db.conn_mut().execute(
            "UPDATE transcription_jobs SET status = 'failed', worker = ?10, \
             start_ms = COALESCE(?11, start_ms), end_ms = ?12, duration_ms = ?13, \
             queue_wait_ms = ?14, queue_jobs_end = ?15, \
             queue_size_start = COALESCE(?16, queue_size_start), queue_size_end = ?17, error = ?18 \
             WHERE job_id = ?1 AND recording_id = ?2 AND pass = ?3 AND block = ?4 \
             AND t_start_ms = ?5 AND t_end_ms = ?6 AND sample_count = ?7 AND queued_at_ms = ?8 \
             AND finishing = ?9 AND ((status = 'queued' AND ?10 IS NULL) \
             OR (status = 'running' AND worker = ?10))",
            params![request.job, request.recording, pass_name(request.pass), request.block,
                request.t_start_ms, request.t_end_ms, request.sample_count, request.queued_at_ms, request.finishing,
                worker, transcription.map(|m| m.start_ms), transcription.map(|m| m.end_ms),
                transcription.map(|m| m.duration_ms), queue_wait_ms, queue_jobs,
                transcription.map(|m| m.queue_size_start), transcription.map(|m| m.queue_size_end), error],
        )?,
        _ => return Ok(true),
    };
    Ok(changed != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CaptureSource;
    use crate::bus::{EventBus, TranscriptionJob, TranscriptionMetrics};

    #[derive(Debug, PartialEq, Eq)]
    struct JobRow {
        status: String,
        worker: Option<u8>,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
        duration_ms: Option<u64>,
        queue_wait_ms: Option<u64>,
        queue_jobs_start: Option<u32>,
        queue_jobs_end: Option<u32>,
        queue_size_start: Option<u64>,
        queue_size_end: Option<u64>,
        error: Option<String>,
    }

    fn row(db: &Database, request: TranscriptionJob) -> JobRow {
        db.conn_ref().query_row(
            "SELECT status, worker, start_ms, end_ms, duration_ms, queue_wait_ms, \
             queue_jobs_start, queue_jobs_end, queue_size_start, queue_size_end, error \
             FROM transcription_jobs WHERE recording_id = ?1 AND pass = ?2 AND job_id = ?3",
            params![request.recording, pass_name(request.pass), request.job],
            |r| Ok(JobRow {
                status: r.get(0)?, worker: r.get(1)?, start_ms: r.get(2)?,
                end_ms: r.get(3)?, duration_ms: r.get(4)?, queue_wait_ms: r.get(5)?,
                queue_jobs_start: r.get(6)?, queue_jobs_end: r.get(7)?,
                queue_size_start: r.get(8)?, queue_size_end: r.get(9)?, error: r.get(10)?,
            }),
        ).unwrap()
    }

    fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("jobs.sqlite"), EventBus::new().sender()).unwrap();
        (dir, db)
    }

    fn start(db: &mut Database, block: u8, recording: u64) {
        assert!(super::super::apply(db, &AppEvent::RecordingStarted {
            block, recording, session_dir: format!("/sessions/{recording}"),
            source: CaptureSource::Mic { device_name: "Mic".into() },
            language: "en".into(), started_at: "2026-10-08T10:00:00Z".into(),
        }).unwrap());
    }

    fn request(job: u64) -> TranscriptionJob {
        TranscriptionJob {
            block: 1, recording: 100, job, pass: TranscriptPass::Live,
            t_start_ms: 500, t_end_ms: 1500, sample_count: 16_000,
            queued_at_ms: 1000, finishing: false,
        }
    }

    fn queued(request: TranscriptionJob) -> AppEvent {
        AppEvent::TranscriptionRequested { request }
    }

    fn running(request: TranscriptionJob, worker: u8) -> AppEvent {
        AppEvent::TranscriptionStarted {
            request, worker, start_ms: 1020, queue_jobs: 3, queue_size_start: 32_000,
        }
    }

    fn metrics() -> TranscriptionMetrics {
        TranscriptionMetrics {
            start_ms: 1020, end_ms: 1090, duration_ms: 70,
            queue_size_start: 32_000, queue_size_end: 48_000,
        }
    }

    fn finished(request: TranscriptionJob, worker: u8) -> AppEvent {
        AppEvent::TranscriptionFinished {
            request, worker, transcription: metrics(), queue_wait_ms: 20, queue_jobs: 2,
        }
    }

    fn failed(request: TranscriptionJob, worker: Option<u8>) -> AppEvent {
        AppEvent::TranscriptionFailed {
            request, worker, transcription: worker.map(|_| metrics()),
            queue_wait_ms: 20, queue_jobs: 2, error: "decoder unavailable".into(),
        }
    }

    fn gate(db: &mut Database, event: AppEvent) -> bool {
        super::super::apply(db, &event).unwrap()
    }

    #[test]
    fn no_text_job_persists_every_timing_and_backlog_without_terminal_overwrites() {
        let (_dir, mut db) = database();
        start(&mut db, 1, 100);
        let request = request(1);
        assert!(gate(&mut db, queued(request)));
        assert!(!gate(&mut db, queued(request)));
        assert!(!gate(&mut db, finished(request, 1)));
        assert!(gate(&mut db, running(request, 1)));
        assert_eq!(row(&db, request).queue_wait_ms, Some(20));
        assert!(!gate(&mut db, running(request, 1)));
        assert!(gate(&mut db, finished(request, 1)));
        let expected = JobRow {
            status: "finished".into(), worker: Some(1), start_ms: Some(1020),
            end_ms: Some(1090), duration_ms: Some(70), queue_wait_ms: Some(20),
            queue_jobs_start: Some(3), queue_jobs_end: Some(2),
            queue_size_start: Some(32_000), queue_size_end: Some(48_000), error: None,
        };
        assert_eq!(row(&db, request), expected);
        assert!(!gate(&mut db, finished(request, 1)));
        assert!(!gate(&mut db, failed(request, Some(1))));
        assert!(!gate(&mut db, queued(request)));
        assert!(!gate(&mut db, running(request, 1)));
        assert_eq!(row(&db, request), expected);
        let transcript_count: u64 = db.conn_ref().query_row(
            "SELECT count(*) FROM transcription_metrics", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(transcript_count, 0);
    }

    #[test]
    fn admission_uses_exact_active_recording_not_the_reused_block_watermark() {
        let (_dir, mut db) = database();
        let request = request(1);
        assert!(!gate(&mut db, queued(request)));
        start(&mut db, 1, 100);
        assert!(!gate(&mut db, queued(TranscriptionJob { block: 2, ..request })));
        assert!(gate(&mut db, AppEvent::BlockClosed { block: 1 }));
        start(&mut db, 1, 101);
        assert!(gate(&mut db, queued(request)));
        assert!(gate(&mut db, AppEvent::RecordingStopped { block: 1, recording: 100 }));
        assert!(!gate(&mut db, queued(TranscriptionJob { job: 2, ..request })));
        assert!(gate(&mut db, running(request, 1)));
        assert!(gate(&mut db, finished(request, 1)));
        assert_eq!(row(&db, request).status, "finished");
        assert!(gate(&mut db, AppEvent::RecordingFailed {
            block: 1, recording: 101, error: "capture failed".into(),
        }));
        assert!(!gate(&mut db, queued(TranscriptionJob { recording: 101, job: 3, ..request })));
    }

    #[test]
    fn immutable_request_metadata_and_running_worker_fence_stale_updates() {
        let (_dir, mut db) = database();
        start(&mut db, 1, 100);
        let request = request(1);
        assert!(gate(&mut db, queued(request)));
        let stale_requests = [
            TranscriptionJob { block: 2, ..request },
            TranscriptionJob { recording: 101, ..request },
            TranscriptionJob { job: 2, ..request },
            TranscriptionJob { pass: TranscriptPass::Refined, ..request },
            TranscriptionJob { t_start_ms: 0, ..request },
            TranscriptionJob { t_end_ms: 2000, ..request },
            TranscriptionJob { sample_count: 32_000, ..request },
            TranscriptionJob { queued_at_ms: 999, ..request },
            TranscriptionJob { finishing: true, ..request },
        ];
        for stale in stale_requests {
            assert!(!gate(&mut db, running(stale, 1)));
            assert!(!gate(&mut db, failed(stale, None)));
        }
        assert!(!gate(&mut db, failed(request, Some(1))));
        assert!(gate(&mut db, running(request, 1)));
        for stale in stale_requests {
            assert!(!gate(&mut db, finished(stale, 1)));
            assert!(!gate(&mut db, failed(stale, Some(1))));
        }
        assert!(!gate(&mut db, finished(request, 2)));
        assert!(!gate(&mut db, failed(request, Some(2))));
        assert!(!gate(&mut db, failed(request, None)));
        assert_eq!(row(&db, request).status, "running");
        assert!(gate(&mut db, finished(request, 1)));
    }

    #[test]
    fn queued_and_running_failures_preserve_only_real_worker_measurements() {
        let (_dir, mut db) = database();
        start(&mut db, 1, 100);
        let queued_request = request(1);
        assert!(gate(&mut db, queued(queued_request)));
        assert!(gate(&mut db, failed(queued_request, None)));
        let cancelled = row(&db, queued_request);
        assert_eq!(cancelled.status, "failed");
        assert_eq!(cancelled.worker, None);
        assert_eq!(cancelled.start_ms, None);
        assert_eq!(cancelled.end_ms, None);
        assert_eq!(cancelled.duration_ms, None);
        assert_eq!(cancelled.queue_wait_ms, Some(20));
        assert_eq!(cancelled.queue_jobs_end, Some(2));
        assert_eq!(cancelled.error.as_deref(), Some("decoder unavailable"));
        assert!(!gate(&mut db, failed(queued_request, None)));
        assert!(!gate(&mut db, running(queued_request, 1)));
        assert_eq!(row(&db, queued_request), cancelled);
        let running_request = TranscriptionJob { job: 2, pass: TranscriptPass::Refined, ..queued_request };
        assert!(gate(&mut db, queued(running_request)));
        assert!(gate(&mut db, running(running_request, 3)));
        assert!(gate(&mut db, failed(running_request, Some(3))));
        let failed_row = row(&db, running_request);
        assert_eq!(failed_row.worker, Some(3));
        assert_eq!(failed_row.end_ms, Some(1090));
        assert_eq!(failed_row.duration_ms, Some(70));
        assert_eq!(failed_row.queue_size_start, Some(32_000));
        assert_eq!(failed_row.queue_size_end, Some(48_000));
        assert!(!gate(&mut db, finished(running_request, 3)));
        assert_eq!(row(&db, running_request), failed_row);
    }

    #[test]
    fn startup_recovery_is_deterministic_and_preserves_terminal_rows() {
        let (dir, mut db) = database();
        start(&mut db, 1, 100);
        let requests = [request(1), request(2), request(3), request(4)];
        for request in requests {
            assert!(gate(&mut db, queued(request)));
        }
        assert!(gate(&mut db, running(requests[1], 1)));
        assert!(gate(&mut db, running(requests[2], 1)));
        assert!(gate(&mut db, finished(requests[2], 1)));
        assert!(gate(&mut db, failed(requests[3], None)));
        let terminal = [row(&db, requests[2]), row(&db, requests[3])];
        drop(db);
        let mut db = Database::open(&dir.path().join("jobs.sqlite"), EventBus::new().sender()).unwrap();
        let queued_row = row(&db, requests[0]);
        let running_row = row(&db, requests[1]);
        assert_eq!(queued_row.status, "failed");
        assert_eq!(queued_row.worker, None);
        assert_eq!(queued_row.start_ms, None);
        assert_eq!(queued_row.end_ms, None);
        assert_eq!(running_row.status, "failed");
        assert_eq!(running_row.worker, Some(1));
        assert_eq!(running_row.start_ms, Some(1020));
        assert_eq!(running_row.queue_size_start, Some(32_000));
        assert_eq!(running_row.queue_wait_ms, Some(20));
        assert_eq!(running_row.end_ms, None);
        assert_eq!(running_row.duration_ms, None);
        for request in &requests[..2] {
            assert_eq!(row(&db, *request).error.as_deref(), Some("interrupted by process shutdown"));
            assert!(!gate(&mut db, finished(*request, 1)));
        }
        assert_eq!([row(&db, requests[2]), row(&db, requests[3])], terminal);
        drop(db);
        let db = Database::open(&dir.path().join("jobs.sqlite"), EventBus::new().sender()).unwrap();
        assert_eq!(row(&db, requests[0]), queued_row);
        assert_eq!(row(&db, requests[1]), running_row);
        assert_eq!([row(&db, requests[2]), row(&db, requests[3])], terminal);
    }
}
