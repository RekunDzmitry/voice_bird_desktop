//! Connection-local recording worker identities; Rust handles stay in the producer.
use rusqlite::{params, OptionalExtension};

use super::{Database, Table};

pub struct RecordingWorkersTable;

impl Table for RecordingWorkersTable {
    const NAME: &'static str = "recording_workers";
    const DEFINITION: &'static str = "
        CREATE TEMP TABLE IF NOT EXISTS recording_workers (
            block        INTEGER PRIMARY KEY,
            recording    INTEGER NOT NULL UNIQUE,
            slot         INTEGER NOT NULL UNIQUE CHECK (slot >= 0)
        )";
}

/// Claim a block before spawning capture. A duplicate cannot replace a live worker.
pub(crate) fn claim(
    db: &mut Database,
    block: u8,
    recording: u64,
    slot: usize,
) -> rusqlite::Result<bool> {
    Ok(db.conn_mut().execute(
        "INSERT INTO recording_workers (block, recording, slot) VALUES (?1, ?2, ?3) \
         ON CONFLICT(block) DO NOTHING",
        params![block, recording, slot],
    )? != 0)
}

/// Only the current generation may reach its runtime handle.
pub(crate) fn slot(db: &Database, block: u8, recording: u64) -> rusqlite::Result<Option<usize>> {
    db.conn_ref()
        .query_row(
            "SELECT slot FROM recording_workers WHERE block = ?1 AND recording = ?2",
            params![block, recording],
            |row| row.get(0),
        )
        .optional()
}

/// Retire exactly this generation before making its handle slot reusable.
pub(crate) fn release(
    db: &mut Database,
    block: u8,
    recording: u64,
) -> rusqlite::Result<Option<usize>> {
    db.conn_mut()
        .query_row(
            "DELETE FROM recording_workers WHERE block = ?1 AND recording = ?2 RETURNING slot",
            params![block, recording],
            |row| row.get(0),
        )
        .optional()
}

/// Shutdown drains identities alongside their indexed runtime handles.
pub(crate) fn release_slot(db: &mut Database, slot: usize) -> rusqlite::Result<Option<(u8, u64)>> {
    db.conn_mut()
        .query_row(
            "DELETE FROM recording_workers WHERE slot = ?1 RETURNING block, recording",
            params![slot],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;

    #[test]
    fn occupied_blocks_and_stale_events_cannot_replace_or_release_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("index.sqlite"), bus.sender()).unwrap();
        assert!(claim(&mut db, 1, 100, 0).unwrap());
        assert!(!claim(&mut db, 1, 100, 1).unwrap());
        assert!(!claim(&mut db, 1, 101, 1).unwrap());
        assert!(claim(&mut db, 2, 200, 1).unwrap());
        assert_eq!(slot(&db, 1, 101).unwrap(), None);
        assert_eq!(release(&mut db, 1, 101).unwrap(), None);
        assert_eq!(release(&mut db, 1, 100).unwrap(), Some(0));
        assert!(claim(&mut db, 1, 101, 0).unwrap());
        assert_eq!(slot(&db, 1, 100).unwrap(), None);
        assert_eq!(release(&mut db, 1, 100).unwrap(), None);
        assert_eq!(slot(&db, 1, 101).unwrap(), Some(0));
        assert_eq!(slot(&db, 2, 200).unwrap(), Some(1));
        assert_eq!(release_slot(&mut db, 0).unwrap(), Some((1, 101)));
        assert_eq!(release_slot(&mut db, 0).unwrap(), None);
        assert_eq!(release_slot(&mut db, 1).unwrap(), Some((2, 200)));
    }

    #[test]
    fn worker_identities_do_not_survive_connection_close() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let path = tmp.path().join("index.sqlite");
        {
            let mut db = Database::open(&path, bus.sender()).unwrap();
            assert!(claim(&mut db, 1, 100, 0).unwrap());
        }
        let mut db = Database::open(&path, bus.sender()).unwrap();
        assert_eq!(slot(&db, 1, 100).unwrap(), None);
        assert!(claim(&mut db, 1, 101, 0).unwrap());
    }
}
