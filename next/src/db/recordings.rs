//! Durable session index; transcript text stays exclusively on disk.
//!
//! A connection-local watermark prevents delayed workers from reopening a
//! reused block. Historical terminal updates target their own id, so closing
//! a block or starting its successor cannot prevent the old worker finalizing.

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, TransactionBehavior};

use super::{Database, Table};
use crate::audio::CaptureSource;
use crate::bus::{AppEvent, TranscriptPass, TranscriptionMetrics};

pub struct RecordingsTable;

impl Table for RecordingsTable {
    const NAME: &'static str = "recordings";
    const DEFINITION: &'static str = "
        CREATE TABLE IF NOT EXISTS recordings (
            id           INTEGER PRIMARY KEY,
            block        INTEGER NOT NULL,
            session_dir  TEXT NOT NULL UNIQUE,
            source_kind  TEXT NOT NULL CHECK (source_kind IN ('mic', 'app')),
            device_name  TEXT NOT NULL,
            app_id       TEXT,
            language     TEXT NOT NULL,
            started_at   TEXT NOT NULL,
            ended_at     TEXT,
            status       TEXT NOT NULL CHECK (status IN ('recording', 'stopped', 'failed'))
        );
        CREATE TEMP TABLE IF NOT EXISTS recording_watermarks (
            block        INTEGER PRIMARY KEY,
            recording    INTEGER NOT NULL
        )";
}

/// The previous process cannot still finalize these sessions. Recovery changes
/// only the index: it never invents final transcript text or rewrites artifacts.
pub fn recover_interrupted(db: &mut Database) -> rusqlite::Result<()> {
    let ended_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    db.conn_mut().execute(
        "UPDATE recordings SET status = 'failed', ended_at = ?1 WHERE status = 'recording'",
        params![ended_at],
    )?;
    Ok(())
}

/// Gate worker events before projection. IDs are process-wide monotonic epoch
/// microseconds, supplied by the producer, and must fit SQLite's signed integer.
pub fn apply(db: &mut Database, event: &AppEvent) -> rusqlite::Result<bool> {
    match event {
        AppEvent::RecordingStarted {
            block,
            recording,
            session_dir,
            source,
            language,
            started_at,
        } => {
            let (kind, device_name, app_id) = match source {
                CaptureSource::Mic { device_name } => ("mic", device_name.as_str(), None),
                CaptureSource::App { device_name, app } => {
                    ("app", device_name.as_str(), Some(app.id.as_str()))
                }
            };
            let transaction = db
                .conn_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = transaction.execute(
                "INSERT INTO recordings \
                 (id, block, session_dir, source_kind, device_name, app_id, language, started_at, status) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'recording' \
                 WHERE NOT EXISTS (SELECT 1 FROM recording_watermarks WHERE block = ?2 AND recording >= ?1) \
                 ON CONFLICT DO NOTHING",
                params![recording, block, session_dir, kind, device_name, app_id, language, started_at],
            )?;
            if changed != 0 {
                transaction.execute(
                    "INSERT INTO recording_watermarks (block, recording) VALUES (?1, ?2) \
                     ON CONFLICT(block) DO UPDATE SET recording = excluded.recording",
                    params![block, recording],
                )?;
            }
            transaction.commit()?;
            Ok(changed != 0)
        }
        AppEvent::TranscriptTentative {
            block, recording, job, worker, transcription, ..
        } => insert_metrics(db, (*block, *recording), (*job, *worker), "tentative", "live", None, transcription),
        AppEvent::TranscriptCommitted {
            block, recording, job, worker, pass, t_start_ms, t_end_ms, transcription, ..
        } => insert_metrics(
            db, (*block, *recording), (*job, *worker), "committed",
            match pass {
                TranscriptPass::Live => "live",
                TranscriptPass::Refined => "refined",
            },
            Some((*t_start_ms, *t_end_ms)), transcription,
        ),
        AppEvent::RecordingStopped { block, recording } => {
            terminal(db, *block, *recording, "stopped")
        }
        AppEvent::RecordingFailed {
            block, recording, ..
        } => terminal(db, *block, *recording, "failed"),
        _ => Ok(true),
    }
}

/// One statement both gates the event and records its metrics, so a rejected
/// event cannot leave a measurement behind, even with concurrent writers.
fn insert_metrics(
    db: &mut Database,
    recording_identity: (u8, u64),
    job_worker: (u64, u8),
    kind: &str,
    pass: &str,
    audio_range: Option<(u64, u64)>,
    metrics: &TranscriptionMetrics,
) -> rusqlite::Result<bool> {
    let (block, recording) = recording_identity;
    let (job, worker) = job_worker;
    Ok(db.conn_mut().execute(
        "INSERT INTO transcription_metrics \
         (recording_id, job_id, worker, kind, pass, t_start_ms, t_end_ms, start_ms, end_ms, duration_ms, queue_size_start, queue_size_end) \
         SELECT ?2, ?12, ?13, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11 \
         WHERE EXISTS(SELECT 1 FROM recordings r JOIN recording_watermarks w \
         ON r.block = w.block AND r.id = w.recording \
         WHERE r.block = ?1 AND r.id = ?2 AND r.status = 'recording')",
        params![block, recording, kind, pass,
            audio_range.map(|(start, _)| start), audio_range.map(|(_, end)| end),
            metrics.start_ms, metrics.end_ms, metrics.duration_ms,
            metrics.queue_size_start, metrics.queue_size_end, job, worker],
    )? != 0)
}

fn terminal(db: &mut Database, block: u8, recording: u64, status: &str) -> rusqlite::Result<bool> {
    let ended_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let transaction = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = transaction.execute(
        "UPDATE recordings SET status = ?1, ended_at = ?2 \
         WHERE id = ?3 AND block = ?4 AND status = 'recording'",
        params![status, ended_at, recording, block],
    )?;
    let accepted = if changed != 0 {
        true
    } else if status == "failed" {
        // Session creation may fail before there is a directory to index.
        // Tombstone that id so repeats/late starts cannot reopen it. Existing
        // terminal ids (even belonging to another block) are always rejected.
        transaction.execute(
            "INSERT INTO recording_watermarks (block, recording) \
             SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM recordings WHERE id = ?2) \
             ON CONFLICT(block) DO UPDATE SET recording = excluded.recording \
             WHERE recording_watermarks.recording < excluded.recording",
            params![block, recording],
        )? != 0
    } else {
        false
    };
    transaction.commit()?;
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sources::AppTarget;
    use crate::bus::{EventBus, TranscriptPass};
    use rusqlite::OptionalExtension;

    #[derive(Debug, PartialEq, Eq)]
    struct RecordingRow {
        id: u64,
        block: u8,
        session_dir: String,
        source_kind: String,
        device_name: String,
        app_id: Option<String>,
        language: String,
        started_at: String,
        ended_at: Option<String>,
        status: String,
    }

    fn row(db: &Database, id: u64) -> Option<RecordingRow> {
        db.conn_ref().query_row(
            "SELECT id, block, session_dir, source_kind, device_name, app_id, language, started_at, ended_at, status \
             FROM recordings WHERE id = ?1",
            params![id],
            |r| Ok(RecordingRow {
                id: r.get(0)?, block: r.get(1)?, session_dir: r.get(2)?,
                source_kind: r.get(3)?, device_name: r.get(4)?, app_id: r.get(5)?,
                language: r.get(6)?, started_at: r.get(7)?, ended_at: r.get(8)?, status: r.get(9)?,
            }),
        ).optional().unwrap()
    }

    fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(
            &dir.path().join("recordings.sqlite"),
            EventBus::new().sender(),
        )
        .unwrap();
        (dir, db)
    }

    fn start(block: u8, recording: u64) -> AppEvent {
        AppEvent::RecordingStarted {
            block,
            recording,
            session_dir: format!("/sessions/b{block}-r{recording}"),
            source: CaptureSource::Mic {
                device_name: "Microphone".into(),
            },
            language: "en".into(),
            started_at: "2026-10-08T10:00:00Z".into(),
        }
    }

    fn committed(block: u8, recording: u64) -> AppEvent {
        AppEvent::TranscriptCommitted {
            block,
            recording,
            job: 7,
            worker: 1,
            pass: TranscriptPass::Live,
            t_start_ms: 0,
            t_end_ms: 1000,
            text: "disk only".into(),
            transcription: TranscriptionMetrics::default(),
        }
    }

    type MetricRow = (u64, u64, u8, String, String, Option<u64>, Option<u64>, TranscriptionMetrics);

    fn metric_rows(db: &Database) -> Vec<MetricRow> {
        let mut statement = db.conn_ref().prepare(
            "SELECT recording_id, job_id, worker, kind, pass, t_start_ms, t_end_ms, start_ms, end_ms, duration_ms, queue_size_start, queue_size_end \
             FROM transcription_metrics ORDER BY id"
        ).unwrap();
        statement.query_map([], |r| Ok((
            r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
            TranscriptionMetrics {
                start_ms: r.get(7)?, end_ms: r.get(8)?, duration_ms: r.get(9)?,
                queue_size_start: r.get(10)?, queue_size_end: r.get(11)?,
            },
        ))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn accepted_transcripts_persist_exact_metrics_and_passes_without_text() {
        let (dir, mut db) = database();
        assert!(apply(&mut db, &start(1, 100)).unwrap());
        let metrics = TranscriptionMetrics {
            start_ms: 101, end_ms: 149, duration_ms: 48,
            queue_size_start: 32_000, queue_size_end: 48_000,
        };
        let tentative = AppEvent::TranscriptTentative {
            block: 1, recording: 100, text: "private tentative".into(),
            job: 8, worker: 1,
            transcription: metrics,
        };
        assert!(apply(&mut db, &tentative).unwrap());
        for pass in [TranscriptPass::Live, TranscriptPass::Refined] {
            let event = AppEvent::TranscriptCommitted {
                block: 1, recording: 100, pass, t_start_ms: 500, t_end_ms: 1500,
                job: 9, worker: if pass == TranscriptPass::Live { 1 } else { 2 },
                text: "private committed".into(), transcription: metrics,
            };
            assert!(apply(&mut db, &event).unwrap());
        }
        let expected = vec![
            (100, 8, 1, "tentative".into(), "live".into(), None, None, metrics),
            (100, 9, 1, "committed".into(), "live".into(), Some(500), Some(1500), metrics),
            (100, 9, 2, "committed".into(), "refined".into(), Some(500), Some(1500), metrics),
        ];
        assert_eq!(metric_rows(&db), expected);
        drop(db);
        let reopened = Database::open(&dir.path().join("recordings.sqlite"), EventBus::new().sender()).unwrap();
        assert_eq!(metric_rows(&reopened), expected);
    }

    #[test]
    fn rejected_stale_and_terminal_events_do_not_insert_metrics() {
        let (_dir, mut db) = database();
        assert!(apply(&mut db, &start(1, 100)).unwrap());
        assert!(apply(&mut db, &start(1, 101)).unwrap());
        for recording in [100, 101] {
            if recording == 101 {
                assert!(apply(&mut db, &AppEvent::RecordingStopped { block: 1, recording }).unwrap());
            }
            assert!(!apply(&mut db, &committed(1, recording)).unwrap());
            assert!(!apply(&mut db, &AppEvent::TranscriptTentative {
                block: 1, recording, text: "rejected".into(),
                job: 7, worker: 1,
                transcription: TranscriptionMetrics::default(),
            }).unwrap());
        }
        assert_eq!(metric_rows(&db), vec![]);
    }

    #[test]
    fn opening_old_recordings_database_adds_metrics_without_losing_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recordings.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(RecordingsTable::DEFINITION).unwrap();
        conn.execute(
            "INSERT INTO recordings (id, block, session_dir, source_kind, device_name, language, started_at, ended_at, status) \
             VALUES (100, 1, '/old/session', 'mic', 'Old Mic', 'en', 'start', 'end', 'stopped')",
            [],
        ).unwrap();
        drop(conn);
        let db = Database::open(&path, EventBus::new().sender()).unwrap();
        let old = row(&db, 100).unwrap();
        assert_eq!(old.session_dir, "/old/session");
        assert_eq!(old.device_name, "Old Mic");
        assert_eq!(old.status, "stopped");
        assert_eq!(metric_rows(&db), vec![]);
        drop(db);
        let mut reopened = Database::open(&path, EventBus::new().sender()).unwrap();
        assert_eq!(row(&reopened, 100).unwrap(), old);
        assert!(apply(&mut reopened, &start(1, 101)).unwrap());
        assert!(apply(&mut reopened, &committed(1, 101)).unwrap());
        assert_eq!(metric_rows(&reopened), vec![
            (101, 7, 1, "committed".into(), "live".into(), Some(0), Some(1000), TranscriptionMetrics::default()),
        ]);
    }

    #[test]
    fn persists_session_fields_but_never_transcript_text() {
        let (dir, mut db) = database();
        let mut event = start(1, 101);
        if let AppEvent::RecordingStarted { source, .. } = &mut event {
            *source = CaptureSource::App {
                device_name: "Speakers".into(),
                app: AppTarget {
                    id: "com.test.app".into(),
                    name: "Test App".into(),
                    pid: 42,
                },
            };
        }
        assert!(super::super::apply(&mut db, &event).unwrap());
        let initial = row(&db, 101).unwrap();
        assert_eq!(
            initial,
            RecordingRow {
                id: 101,
                block: 1,
                session_dir: "/sessions/b1-r101".into(),
                source_kind: "app".into(),
                device_name: "Speakers".into(),
                app_id: Some("com.test.app".into()),
                language: "en".into(),
                started_at: "2026-10-08T10:00:00Z".into(),
                ended_at: None,
                status: "recording".into(),
            }
        );
        assert!(super::super::apply(&mut db, &committed(1, 101)).unwrap());
        assert_eq!(row(&db, 101).unwrap(), initial);
        assert!(super::super::apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 1,
                recording: 101
            }
        )
        .unwrap());
        let stopped = row(&db, 101).unwrap();
        assert_eq!(stopped.status, "stopped");
        chrono::DateTime::parse_from_rfc3339(stopped.ended_at.as_ref().unwrap()).unwrap();
        drop(db);
        let reopened = Database::open(
            &dir.path().join("recordings.sqlite"),
            EventBus::new().sender(),
        )
        .unwrap();
        assert_eq!(row(&reopened, 101).unwrap(), stopped);
    }

    #[test]
    fn rejects_duplicate_stale_and_terminal_worker_events() {
        let (_dir, mut db) = database();
        assert!(!apply(&mut db, &committed(1, 1)).unwrap());
        assert!(apply(&mut db, &start(1, 100)).unwrap());
        assert!(!apply(&mut db, &start(1, 100)).unwrap());
        assert!(!apply(&mut db, &start(1, 99)).unwrap());
        assert!(!apply(&mut db, &start(2, 100)).unwrap());
        assert!(!apply(&mut db, &committed(2, 100)).unwrap());
        assert!(!apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 2,
                recording: 100
            }
        )
        .unwrap());
        assert!(apply(&mut db, &committed(1, 100)).unwrap());
        assert!(apply(
            &mut db,
            &AppEvent::RecordingFailed {
                block: 1,
                recording: 100,
                error: "capture stopped".into()
            }
        )
        .unwrap());
        assert!(!apply(&mut db, &committed(1, 100)).unwrap());
        assert!(!apply(
            &mut db,
            &AppEvent::TranscriptTentative {
                block: 1,
                recording: 100,
                job: 7,
                worker: 1,
                text: "late".into(),
                transcription: TranscriptionMetrics::default(),
            }
        )
        .unwrap());
        assert!(!apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 1,
                recording: 100
            }
        )
        .unwrap());
        assert!(!apply(
            &mut db,
            &AppEvent::RecordingFailed {
                block: 1,
                recording: 100,
                error: "replay".into()
            }
        )
        .unwrap());
        assert!(!apply(&mut db, &start(1, 100)).unwrap());
        assert_eq!(row(&db, 100).unwrap().status, "failed");
    }

    #[test]
    fn closed_and_reused_blocks_still_finalize_their_exact_session() {
        let (_dir, mut db) = database();
        assert!(super::super::apply(&mut db, &start(1, 100)).unwrap());
        assert!(super::super::apply(&mut db, &AppEvent::BlockClosed { block: 1 }).unwrap());
        assert!(super::super::apply(&mut db, &start(1, 101)).unwrap());
        assert!(!super::super::apply(&mut db, &committed(1, 100)).unwrap());
        assert!(super::super::apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 1,
                recording: 100
            }
        )
        .unwrap());
        assert_eq!(row(&db, 100).unwrap().status, "stopped");
        assert_eq!(row(&db, 101).unwrap().status, "recording");
        assert!(super::super::apply(&mut db, &committed(1, 101)).unwrap());
        assert!(!super::super::apply(&mut db, &start(1, 99)).unwrap());
        assert!(super::super::apply(&mut db, &AppEvent::BlockClosed { block: 1 }).unwrap());
        assert!(super::super::apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 1,
                recording: 101
            }
        )
        .unwrap());
        assert_eq!(row(&db, 101).unwrap().status, "stopped");
    }

    #[test]
    fn pre_start_failure_is_tombstoned_without_fake_index_row() {
        let (_dir, mut db) = database();
        let failed = AppEvent::RecordingFailed {
            block: 1,
            recording: 100,
            error: "cannot create session".into(),
        };
        assert!(apply(&mut db, &failed).unwrap());
        assert!(!apply(&mut db, &failed).unwrap());
        assert!(row(&db, 100).is_none());
        assert!(!apply(&mut db, &start(1, 100)).unwrap());
        assert!(apply(&mut db, &start(1, 101)).unwrap());
        assert!(!apply(&mut db, &failed).unwrap());
        assert!(!apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 1,
                recording: 102
            }
        )
        .unwrap());
        assert_eq!(row(&db, 101).unwrap().status, "recording");
    }

    #[test]
    fn opening_database_recovers_only_interrupted_recordings() {
        let (dir, mut db) = database();
        assert!(apply(&mut db, &start(1, 100)).unwrap());
        assert!(apply(&mut db, &start(2, 101)).unwrap());
        assert!(apply(
            &mut db,
            &AppEvent::RecordingStopped {
                block: 2,
                recording: 101
            }
        )
        .unwrap());
        let stopped = row(&db, 101).unwrap();
        drop(db);
        let mut reopened = Database::open(
            &dir.path().join("recordings.sqlite"),
            EventBus::new().sender(),
        )
        .unwrap();
        let interrupted = row(&reopened, 100).unwrap();
        assert_eq!(interrupted.status, "failed");
        chrono::DateTime::parse_from_rfc3339(interrupted.ended_at.as_ref().unwrap()).unwrap();
        assert_eq!(row(&reopened, 101).unwrap(), stopped);
        assert!(!apply(&mut reopened, &start(1, 100)).unwrap());
        assert!(!apply(&mut reopened, &committed(1, 100)).unwrap());
        assert!(apply(&mut reopened, &start(1, 102)).unwrap());
    }

    #[test]
    fn sql_failures_propagate_instead_of_admitting_events() {
        let (_dir, mut db) = database();
        db.conn_mut()
            .execute_batch("DROP TABLE recordings")
            .unwrap();
        assert!(apply(&mut db, &start(1, 1)).is_err());
        assert!(apply(&mut db, &committed(1, 1)).is_err());
        assert!(recover_interrupted(&mut db).is_err());
    }
}
