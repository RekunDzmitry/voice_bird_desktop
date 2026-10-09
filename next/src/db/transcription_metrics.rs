//! Per-transcript measurements and job identity; transcript text remains on disk.

use rusqlite::Connection;

use super::Table;

pub struct TranscriptionMetricsTable;

impl Table for TranscriptionMetricsTable {
    const NAME: &'static str = "transcription_metrics";
    const DEFINITION: &'static str = "
        CREATE TABLE IF NOT EXISTS transcription_metrics (
            id                INTEGER PRIMARY KEY,
            recording_id      INTEGER NOT NULL REFERENCES recordings(id),
            job_id            INTEGER,
            worker            INTEGER,
            kind              TEXT NOT NULL CHECK (kind IN ('tentative', 'committed')),
            pass              TEXT NOT NULL CHECK (pass IN ('live', 'refined')),
            t_start_ms        INTEGER,
            t_end_ms          INTEGER,
            start_ms          INTEGER NOT NULL,
            end_ms            INTEGER NOT NULL,
            duration_ms       INTEGER NOT NULL,
            queue_size_start  INTEGER NOT NULL,
            queue_size_end    INTEGER NOT NULL
        )";
}

/// Existing measurements predate inference jobs. Preserve those rows without
/// inventing a decoder identity; only newly accepted events supply these fields.
pub fn upgrade(conn: &Connection) -> rusqlite::Result<()> {
    for (column, migration) in [
        ("job_id", "ALTER TABLE transcription_metrics ADD COLUMN job_id INTEGER"),
        ("worker", "ALTER TABLE transcription_metrics ADD COLUMN worker INTEGER"),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('transcription_metrics') WHERE name = ?1)",
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute_batch(migration)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CaptureSource;
    use crate::bus::{AppEvent, EventBus, TranscriptionMetrics};
    use crate::db::{self, recordings::RecordingsTable, Database};
    use rusqlite::params;

    #[test]
    fn upgrades_legacy_measurements_without_fabricating_job_or_worker_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(RecordingsTable::DEFINITION).unwrap();
        conn.execute_batch(
            "CREATE TABLE transcription_metrics (
                id INTEGER PRIMARY KEY,
                recording_id INTEGER NOT NULL REFERENCES recordings(id),
                kind TEXT NOT NULL CHECK (kind IN ('tentative', 'committed')),
                pass TEXT NOT NULL CHECK (pass IN ('live', 'refined')),
                t_start_ms INTEGER,
                t_end_ms INTEGER,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                queue_size_start INTEGER NOT NULL,
                queue_size_end INTEGER NOT NULL
            );
            INSERT INTO recordings
                (id, block, session_dir, source_kind, device_name, language, started_at, ended_at, status)
                VALUES (100, 1, '/old/session', 'mic', 'Old Mic', 'en', 'start', 'end', 'stopped');
            INSERT INTO transcription_metrics
                (id, recording_id, kind, pass, t_start_ms, t_end_ms, start_ms, end_ms, duration_ms, queue_size_start, queue_size_end)
                VALUES (7, 100, 'committed', 'refined', 500, 1500, 101, 149, 48, 32000, 48000);"
        ).unwrap();
        drop(conn);
        for _ in 0..2 {
            let db = Database::open(&path, EventBus::new().sender()).unwrap();
            let legacy = db.conn_ref().query_row(
                "SELECT recording_id, job_id, worker, kind, pass, t_start_ms, t_end_ms,
                 start_ms, end_ms, duration_ms, queue_size_start, queue_size_end
                 FROM transcription_metrics WHERE id = 7",
                [],
                |r| Ok((
                    r.get::<_, u64>(0)?, r.get::<_, Option<u64>>(1)?,
                    r.get::<_, Option<u8>>(2)?, r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?, r.get::<_, u64>(5)?, r.get::<_, u64>(6)?,
                    TranscriptionMetrics {
                        start_ms: r.get(7)?, end_ms: r.get(8)?, duration_ms: r.get(9)?,
                        queue_size_start: r.get(10)?, queue_size_end: r.get(11)?,
                    },
                )),
            ).unwrap();
            assert_eq!(legacy, (
                100, None, None, "committed".into(), "refined".into(), 500, 1500,
                TranscriptionMetrics {
                    start_ms: 101, end_ms: 149, duration_ms: 48,
                    queue_size_start: 32_000, queue_size_end: 48_000,
                },
            ));
            let session: (String, String, String) = db.conn_ref().query_row(
                "SELECT session_dir, device_name, status FROM recordings WHERE id = 100",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).unwrap();
            assert_eq!(session, ("/old/session".into(), "Old Mic".into(), "stopped".into()));
        }
        let mut db = Database::open(&path, EventBus::new().sender()).unwrap();
        assert!(db::apply(&mut db, &AppEvent::RecordingStarted {
            block: 1, recording: 101, session_dir: "/new/session".into(),
            source: CaptureSource::Mic { device_name: "New Mic".into() },
            language: "en".into(), started_at: "2026-10-09T10:00:00Z".into(),
        }).unwrap());
        assert!(db::apply(&mut db, &AppEvent::TranscriptTentative {
            block: 1, recording: 101, job: 200, worker: 1,
            text: "not stored".into(), transcription: TranscriptionMetrics::default(),
        }).unwrap());
        let identity: (u64, u8) = db.conn_ref().query_row(
            "SELECT job_id, worker FROM transcription_metrics WHERE recording_id = ?1",
            params![101], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(identity, (200, 1));
        let legacy_identity: (Option<u64>, Option<u8>) = db.conn_ref().query_row(
            "SELECT job_id, worker FROM transcription_metrics WHERE id = 7",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(legacy_identity, (None, None));
    }
}
