//! SQLite-backed local store.
//!
//! One file per machine, opened at the OS data directory (the same
//! resolution [`crate::event_log`] uses). The module is kept small
//! on purpose: every table is an implementation of [`Table`], and
//! [`Database::open`] / [`migrate`] / [`db_path`] are the only entry
//! points. Adding a new table is one impl of the trait, one line in
//! `Database::open`'s `migrate` call, and one module of free
//! functions that take `&mut Database`.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::Connection;

use crate::bus::EventSender;

/// A table in the local SQLite file. Implementing this trait is the
/// only obligation: name + definition. New tables plug into
/// [`migrate`] and `Database::open` without growing this module.
pub trait Table {
    /// SQL identifier for the table. Referenced by other tables'
    /// definitions and by indexers.
    const NAME: &'static str;
    /// `CREATE TABLE IF NOT EXISTS …` statement, run verbatim on
    /// every connection that opens the file. Keep it idempotent so
    /// re-opening an existing file is a no-op.
    const DEFINITION: &'static str;
}

/// Open (or create) the SQLite file at `path`. WAL mode lets a
/// worker-thread read connection observe writes from the main
/// thread without blocking; `synchronous=NORMAL` is the standard
/// WAL durability trade-off (a power loss loses the last transaction
/// but never corrupts the file); `busy_timeout` makes workers wait
/// briefly when they hit a write lock instead of failing
/// immediately. Returns an error if the file can't be created — the
/// caller in `main` exits with code 2 on that path, mirroring how
/// `CacheDirStore` failures are handled.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                "create {parent:?}: {e}"
            ))))
        })?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Run every table's `CREATE TABLE IF NOT EXISTS` definition on
/// `conn`. Each [`Table::DEFINITION`] is idempotent so this is safe
/// to call on an existing database — the call is the entire migration
/// surface today.
pub fn migrate<T: Table>(conn: &Connection, tables: &[T]) -> rusqlite::Result<()> {
    for t in tables {
        conn.execute_batch(t.definition())?;
    }
    Ok(())
}

/// Path the database file lives at. Mirrors
/// [`crate::event_log::EventLog`]'s `log_dir`: the platform data
/// directory via `directories`, falling back to the system temp
/// directory only when the OS can't provide one. Returns `None` so
/// `main` can exit with a clear error instead of panicking.
pub fn db_path() -> Option<PathBuf> {
    if let Some(proj) = ProjectDirs::from("com", "RekunDzmitry", "voice-bird-next") {
        return Some(proj.data_dir().join("downloads.sqlite"));
    }
    Some(std::env::temp_dir().join("voice-bird-next").join("downloads.sqlite"))
}

/// Extension trait so each `Table` impl can hand its `DEFINITION` /
/// `NAME` to `migrate` without re-typing the strings. Defined here
/// rather than on `Table` because `Table` itself only describes
/// what's constant.
pub trait TableExt: Table {
    fn definition(&self) -> &'static str {
        Self::DEFINITION
    }
}
impl<T: Table> TableExt for T {}

/// The local SQLite database.
///
/// Owns one writer connection, the path it was opened from, and the
/// cloneable [`EventSender`] used to publish status changes. Every
/// per-table operation is a free function in its module that takes
/// `&Database` / `&mut Database` so callers don't have to grow their
/// signatures as new tables land: they pass the whole database and
/// pull out the table they need (today [`downloads`]).
///
/// The connection is wrapped here rather than at each table call so
/// `Database::open` can run migration once, recover leftover
/// non-terminal rows once, and hand callers a handle that's ready to
/// read or write. Tables never construct their own connection.
pub struct Database {
    conn: Connection,
    path: PathBuf,
    tx: EventSender,
}

impl Database {
    /// Open (or create) the SQLite file at `path`, run the migration
    /// for every known [`Table`], and recover any rows left in a
    /// non-terminal state by a previous crashed session. The
    /// recovery is logged per row — a Cancelling row at startup
    /// means the previous Quit didn't reach `apply`, so the audit
    /// log records the upgrade.
    ///
    /// Adding a new table is one line in the [`migrate`] call below.
    pub fn open(path: &Path, tx: EventSender) -> rusqlite::Result<Self> {
        let conn = open(path)?;
        migrate(&conn, &[downloads::DownloadsTable])?;
        let mut db = Self {
            conn,
            path: path.to_path_buf(),
            tx,
        };
        downloads::recover_interrupted(&mut db)?;
        Ok(db)
    }

    /// Borrow the writer connection. Exposed only to per-table
    /// modules; callers outside `db/` use the table free functions.
    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Borrow the writer connection read-only. Exposed only to
    /// per-table modules.
    pub(crate) fn conn_ref(&self) -> &Connection {
        &self.conn
    }

    /// Reference to the event sender. Per-table transitions publish
    /// status changes through this.
    pub(crate) fn tx(&self) -> &EventSender {
        &self.tx
    }

    /// Open a worker-side read-only connection bound to the same
    /// file. Probes poll cancellation between fetch chunks. The
    /// fallback to an in-memory connection is intentional: a probe
    /// failure must NOT crash the worker, and the attempt gate on
    /// the main thread still discards stale publishes if the probe
    /// returns `false` on every call.
    pub(crate) fn open_probe_connection(&self) -> Connection {
        let conn = Connection::open(self.path.as_path()).unwrap_or_else(|e| {
            eprintln!("downloads: probe open failed: {e}");
            Connection::open_in_memory().expect("in-memory sqlite")
        });
        let _ = conn.pragma_update(None, "busy_timeout", 5_000);
        conn
    }
}

pub mod downloads;
