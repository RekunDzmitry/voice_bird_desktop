//! Session-local compare-and-set gate for the audio-source picker.
//!
//! A missing block is logically at `Device`, revision zero. Every
//! accepted transition increments the revision, including back edges,
//! so returning to the same step never revives an old event. SQL and
//! the rejection snapshot share an immediate transaction: separate
//! writer connections cannot race the initial insert or the CAS.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{Database, Table};
use crate::audio_source::{DeviceKind, FunnelStep};
use crate::bus::AppEvent;

pub struct BlockStepsTable;

impl Table for BlockStepsTable {
    const NAME: &'static str = "block_steps";
    const DEFINITION: &'static str = "CREATE TABLE IF NOT EXISTS block_steps (
        block        INTEGER PRIMARY KEY,
        step         TEXT NOT NULL,
        rev          INTEGER NOT NULL,
        device_name  TEXT,
        device_kind  TEXT,
        app_id       TEXT,
        app_name     TEXT,
        updated_at   TEXT NOT NULL
    )";
}

fn parse_step(value: &str) -> rusqlite::Result<FunnelStep> {
    FunnelStep::parse(value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!(
                "unknown FunnelStep: {value:?}"
            ))),
        )
    })
}

fn actual_on(conn: &Connection, block: u8) -> rusqlite::Result<Option<(FunnelStep, u32)>> {
    conn.query_row(
        "SELECT step, rev FROM block_steps WHERE block = ?1",
        params![block],
        |row| {
            let step: String = row.get(0)?;
            Ok((parse_step(&step)?, row.get(1)?))
        },
    )
    .optional()
}

/// Clear session-local state at startup; blocks do not survive a restart.
pub(super) fn clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM block_steps", [])?;
    Ok(())
}

/// Release a closed block's id so its next incarnation starts at Device/0.
pub fn forget(db: &mut Database, block: u8) -> rusqlite::Result<()> {
    db.conn_mut()
        .execute("DELETE FROM block_steps WHERE block = ?1", params![block])?;
    Ok(())
}

/// Accept source transitions only at the expected step and revision.
/// Ungated language commands (legacy blocks and retries) pass through.
/// A gated language command commits the existing selection unchanged.
pub fn apply(db: &mut Database, ev: &AppEvent) -> rusqlite::Result<bool> {
    let (block, from, to, rev) = match ev {
        AppEvent::SourceStepChanged {
            block,
            from,
            to,
            rev,
            ..
        } => (*block, *from, *to, *rev),
        AppEvent::BeginLanguage {
            block,
            source_rev: Some(rev),
            ..
        } => (*block, FunnelStep::Language, FunnelStep::Committed, *rev),
        _ => return Ok(true),
    };

    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let transaction = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = if let AppEvent::SourceStepChanged { device, app, .. } = ev {
        let device_name = device.as_ref().map(|device| device.name.as_str());
        let device_kind = device.as_ref().map(|device| match device.kind {
            DeviceKind::Input => "Input",
            DeviceKind::Output => "Output",
        });
        let app_id = app.as_ref().map(|app| app.id.as_str());
        let app_name = app.as_ref().map(|app| app.name.as_str());
        let changed = transaction.execute(
            "UPDATE block_steps SET step = ?1, rev = rev + 1, \
             device_name = ?2, device_kind = ?3, app_id = ?4, app_name = ?5, updated_at = ?6 \
             WHERE block = ?7 AND step = ?8 AND rev = ?9",
            params![
                to.as_str(),
                device_name,
                device_kind,
                app_id,
                app_name,
                now,
                block,
                from.as_str(),
                rev,
            ],
        )?;
        if changed == 0 && from == FunnelStep::Device && rev == 0 {
            // Only the virtual Device/0 row may be created. A conflicting
            // existing row must not be overwritten, even at the same step.
            transaction.execute(
                "INSERT INTO block_steps \
                 (block, step, rev, device_name, device_kind, app_id, app_name, updated_at) \
                 VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(block) DO NOTHING",
                params![
                    block,
                    to.as_str(),
                    device_name,
                    device_kind,
                    app_id,
                    app_name,
                    now
                ],
            )?
        } else {
            changed
        }
    } else {
        transaction.execute(
            "UPDATE block_steps SET step = ?1, rev = rev + 1, updated_at = ?2 \
             WHERE block = ?3 AND step = ?4 AND rev = ?5",
            params![to.as_str(), now, block, from.as_str(), rev],
        )?
    };
    let rejected_actual = if changed == 0 {
        actual_on(&transaction, block)?
    } else {
        None
    };
    transaction.commit()?;
    if changed == 0 {
        db.tx().publish(AppEvent::SourceStepRejected {
            block,
            from,
            to,
            rev,
            actual: rejected_actual,
        });
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::audio_source::{AppTarget, AudioDevice};
    use crate::bus::EventBus;
    use crate::language::LANGUAGES;

    #[derive(Debug, PartialEq, Eq)]
    struct BlockStepRow {
        block: u8,
        step: FunnelStep,
        rev: u32,
        device_name: Option<String>,
        device_kind: Option<String>,
        app_id: Option<String>,
        app_name: Option<String>,
        updated_at: String,
    }

    fn get(db: &Database, block: u8) -> rusqlite::Result<Option<BlockStepRow>> {
        db.conn_ref()
            .query_row(
                "SELECT block, step, rev, device_name, device_kind, app_id, app_name, updated_at \
                 FROM block_steps WHERE block = ?1",
                params![block],
                |row| {
                    let step: String = row.get("step")?;
                    Ok(BlockStepRow {
                        block: row.get("block")?,
                        step: parse_step(&step)?,
                        rev: row.get("rev")?,
                        device_name: row.get("device_name")?,
                        device_kind: row.get("device_kind")?,
                        app_id: row.get("app_id")?,
                        app_name: row.get("app_name")?,
                        updated_at: row.get("updated_at")?,
                    })
                },
            )
            .optional()
    }

    fn actual(db: &Database, block: u8) -> rusqlite::Result<Option<(FunnelStep, u32)>> {
        actual_on(db.conn_ref(), block)
    }

    fn database() -> (tempfile::TempDir, Database, EventBus) {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let db = Database::open(&dir.path().join("steps.sqlite"), bus.sender()).unwrap();
        (dir, db, bus)
    }

    fn device() -> AudioDevice {
        AudioDevice {
            name: "Speakers".into(),
            kind: DeviceKind::Output,
        }
    }

    fn app() -> AppTarget {
        AppTarget {
            id: "com.spotify.client".into(),
            name: "Spotify".into(),
            pid: 42,
        }
    }

    fn change(from: FunnelStep, to: FunnelStep, rev: u32) -> AppEvent {
        AppEvent::SourceStepChanged {
            block: 1,
            from,
            to,
            rev,
            device: (to != FunnelStep::Device).then(device),
            app: (to == FunnelStep::Language).then(app),
        }
    }

    fn begin(rev: Option<u32>) -> AppEvent {
        AppEvent::BeginLanguage {
            block: 1,
            language: &LANGUAGES[0],
            source_rev: rev,
        }
    }

    fn to_language(db: &mut Database) {
        assert!(apply(db, &change(FunnelStep::Device, FunnelStep::App, 0)).unwrap());
        assert!(apply(db, &change(FunnelStep::App, FunnelStep::Language, 1)).unwrap());
    }

    #[test]
    fn forward_and_back_update_selection_and_revision() {
        let (_dir, mut db, _bus) = database();
        assert_eq!(actual(&db, 1).unwrap(), None);
        to_language(&mut db);
        let row = get(&db, 1).unwrap().unwrap();
        assert_eq!((row.step, row.rev), (FunnelStep::Language, 2));
        assert_eq!(row.device_name.as_deref(), Some("Speakers"));
        assert_eq!(row.device_kind.as_deref(), Some("Output"));
        assert_eq!(row.app_id.as_deref(), Some("com.spotify.client"));
        assert_eq!(row.app_name.as_deref(), Some("Spotify"));

        assert!(apply(&mut db, &change(FunnelStep::Language, FunnelStep::App, 2)).unwrap());
        let row = get(&db, 1).unwrap().unwrap();
        assert_eq!((row.step, row.rev), (FunnelStep::App, 3));
        assert_eq!(row.device_name.as_deref(), Some("Speakers"));
        assert_eq!(row.app_id, None);
        assert_eq!(row.app_name, None);
        assert!(apply(&mut db, &change(FunnelStep::App, FunnelStep::Device, 3)).unwrap());
        let row = get(&db, 1).unwrap().unwrap();
        assert_eq!((row.step, row.rev), (FunnelStep::Device, 4));
        assert_eq!(row.device_name, None);
        assert_eq!(row.device_kind, None);
    }

    #[test]
    fn duplicate_and_aba_events_publish_actual_without_changing_selection() {
        let (_dir, mut db, mut bus) = database();
        let first = change(FunnelStep::Device, FunnelStep::App, 0);
        assert!(apply(&mut db, &first).unwrap());
        let before = get(&db, 1).unwrap();
        assert!(!apply(&mut db, &first).unwrap());
        assert_eq!(get(&db, 1).unwrap(), before);
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Device,
                to: FunnelStep::App,
                rev: 0,
                actual: Some((FunnelStep::App, 1)),
            }]
        );
        assert!(apply(&mut db, &change(FunnelStep::App, FunnelStep::Device, 1)).unwrap());
        assert!(!apply(&mut db, &first).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::Device, 2)));
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Device,
                to: FunnelStep::App,
                rev: 0,
                actual: Some((FunnelStep::Device, 2)),
            }]
        );
    }

    #[test]
    fn only_device_revision_zero_can_create_a_missing_row() {
        let (_dir, mut db, mut bus) = database();
        for (from, to, rev) in [
            (FunnelStep::App, FunnelStep::Language, 0),
            (FunnelStep::Device, FunnelStep::App, 1),
        ] {
            assert!(!apply(&mut db, &change(from, to, rev)).unwrap());
            assert_eq!(actual(&db, 1).unwrap(), None);
            assert_eq!(
                bus.drain().collect::<Vec<_>>(),
                vec![AppEvent::SourceStepRejected {
                    block: 1,
                    from,
                    to,
                    rev,
                    actual: None,
                }]
            );
        }
        assert!(!apply(&mut db, &begin(Some(0))).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), None);
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Language,
                to: FunnelStep::Committed,
                rev: 0,
                actual: None,
            }]
        );
    }

    #[test]
    fn forget_resets_only_the_closed_block() {
        let (_dir, mut db, _bus) = database();
        to_language(&mut db);
        let other = AppEvent::SourceStepChanged {
            block: 2,
            from: FunnelStep::Device,
            to: FunnelStep::Language,
            rev: 0,
            device: Some(AudioDevice {
                name: "Mic".into(),
                kind: DeviceKind::Input,
            }),
            app: None,
        };
        assert!(apply(&mut db, &other).unwrap());
        forget(&mut db, 1).unwrap();
        forget(&mut db, 1).unwrap();
        assert_eq!(actual(&db, 1).unwrap(), None);
        assert_eq!(actual(&db, 2).unwrap(), Some((FunnelStep::Language, 1)));
        assert!(apply(&mut db, &change(FunnelStep::Device, FunnelStep::App, 0)).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::App, 1)));
    }

    #[test]
    fn commitment_preserves_selection_and_rejects_back_and_duplicate_begin() {
        let (_dir, mut db, mut bus) = database();
        to_language(&mut db);
        let before = get(&db, 1).unwrap().unwrap();
        assert!(super::super::apply(&mut db, &begin(Some(2))).unwrap());
        let committed = get(&db, 1).unwrap().unwrap();
        assert_eq!((committed.step, committed.rev), (FunnelStep::Committed, 3));
        assert_eq!(committed.device_name, before.device_name);
        assert_eq!(committed.device_kind, before.device_kind);
        assert_eq!(committed.app_id, before.app_id);
        assert_eq!(committed.app_name, before.app_name);
        assert!(
            !super::super::apply(&mut db, &change(FunnelStep::Language, FunnelStep::App, 2))
                .unwrap()
        );
        assert!(!super::super::apply(&mut db, &begin(Some(2))).unwrap());
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![
                AppEvent::SourceStepRejected {
                    block: 1,
                    from: FunnelStep::Language,
                    to: FunnelStep::App,
                    rev: 2,
                    actual: Some((FunnelStep::Committed, 3)),
                },
                AppEvent::SourceStepRejected {
                    block: 1,
                    from: FunnelStep::Language,
                    to: FunnelStep::Committed,
                    rev: 2,
                    actual: Some((FunnelStep::Committed, 3)),
                },
            ]
        );
    }

    #[test]
    fn back_wins_before_language_command_and_retry_stays_ungated() {
        let (_dir, mut db, mut bus) = database();
        to_language(&mut db);
        assert!(
            super::super::apply(&mut db, &change(FunnelStep::Language, FunnelStep::App, 2))
                .unwrap()
        );
        assert!(!super::super::apply(&mut db, &begin(Some(2))).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::App, 3)));
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Language,
                to: FunnelStep::Committed,
                rev: 2,
                actual: Some((FunnelStep::App, 3)),
            }]
        );
        assert!(super::super::apply(&mut db, &begin(None)).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::App, 3)));
    }

    #[test]
    fn independent_writers_cannot_accept_both_initial_transitions() {
        let (dir, db, mut bus) = database();
        let other = Database::open(&dir.path().join("steps.sqlite"), bus.sender()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = [db, other]
            .into_iter()
            .map(|mut db| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let accepted =
                        apply(&mut db, &change(FunnelStep::Device, FunnelStep::App, 0)).unwrap();
                    (db, accepted)
                })
            })
            .collect();
        let mut results = workers.into_iter().map(|worker| worker.join().unwrap());
        let (db, first) = results.next().unwrap();
        let (_, second) = results.next().unwrap();
        assert_ne!(first, second);
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::App, 1)));
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Device,
                to: FunnelStep::App,
                rev: 0,
                actual: Some((FunnelStep::App, 1)),
            }]
        );
    }

    #[test]
    fn independent_writers_cannot_commit_language_and_step_back() {
        let (dir, mut db, mut bus) = database();
        let other = Database::open(&dir.path().join("steps.sqlite"), bus.sender()).unwrap();
        to_language(&mut db);
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = [
            (db, begin(Some(2))),
            (other, change(FunnelStep::Language, FunnelStep::App, 2)),
        ]
        .into_iter()
        .map(|(mut db, event)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let accepted = super::super::apply(&mut db, &event).unwrap();
                (db, accepted)
            })
        })
        .collect();
        let mut results = workers.into_iter().map(|worker| worker.join().unwrap());
        let (db, committed) = results.next().unwrap();
        let (_, backed) = results.next().unwrap();
        assert_ne!(committed, backed);
        let expected = if committed {
            FunnelStep::Committed
        } else {
            FunnelStep::App
        };
        assert_eq!(actual(&db, 1).unwrap(), Some((expected, 3)));
        let rejected_to = if committed {
            FunnelStep::App
        } else {
            FunnelStep::Committed
        };
        assert_eq!(
            bus.drain().collect::<Vec<_>>(),
            vec![AppEvent::SourceStepRejected {
                block: 1,
                from: FunnelStep::Language,
                to: rejected_to,
                rev: 2,
                actual: Some((expected, 3)),
            }]
        );
    }

    #[test]
    fn open_clears_previous_session_rows() {
        let (dir, mut db, bus) = database();
        to_language(&mut db);
        drop(db);
        let mut db = Database::open(&dir.path().join("steps.sqlite"), bus.sender()).unwrap();
        assert_eq!(get(&db, 1).unwrap(), None);
        assert!(apply(&mut db, &change(FunnelStep::Device, FunnelStep::App, 0)).unwrap());
        assert_eq!(actual(&db, 1).unwrap(), Some((FunnelStep::App, 1)));
    }
}
