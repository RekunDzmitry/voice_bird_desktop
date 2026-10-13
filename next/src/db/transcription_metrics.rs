//! Per-transcript measurements and job identity; transcript text remains on disk.

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
