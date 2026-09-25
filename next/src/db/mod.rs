//! SQLite-backed local store.
//!
//! One file per machine, opened at the OS data directory (the same
//! resolution [`crate::event_log`] uses). The module is kept small
//! on purpose: every table is an implementation of [`Table`], and
//! [`open`] / [`migrate`] / [`db_path`] are the only entry points.
//! Adding a new table is one impl of the trait and one line in
//! [`Downloads::open`].

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::Connection;

/// A table in the local SQLite file. Implementing this trait is the
/// only obligation: name + schema. New tables plug into
/// [`migrate`] and `Downloads::open` without growing this module.
pub trait Table {
    /// SQL identifier for the table. Referenced by other tables'
    /// schemas and by indexers.
    const NAME: &'static str;
    /// `CREATE TABLE IF NOT EXISTS …` statement, run verbatim on
    /// every connection that opens the file. Keep it idempotent so
    /// re-opening an existing file is a no-op.
    const SCHEMA: &'static str;
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

/// Run every table's `CREATE TABLE IF NOT EXISTS` schema on
/// `conn`. Each [`Table::SCHEMA`] is idempotent so this is safe to
/// call on an existing database — the call is the entire migration
/// surface today.
pub fn migrate<T: Table>(conn: &Connection, tables: &[T]) -> rusqlite::Result<()> {
    for t in tables {
        conn.execute_batch(t.schema())?;
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

/// Extension trait so each `Table` impl can hand its `SCHEMA` /
/// `NAME` to `migrate` without re-typing the strings. Defined here
/// rather than on `Table` because `Table` itself only describes
/// what's constant.
pub trait TableExt: Table {
    fn schema(&self) -> &'static str {
        Self::SCHEMA
    }
}
impl<T: Table> TableExt for T {}

pub mod downloads;
