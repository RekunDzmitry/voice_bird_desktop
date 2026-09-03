//! On-disk append-only log of every [`AppEvent`] that traverses the bus.
//!
//! One JSON object per line, ISO-8601 UTC timestamp + variant tag.
//! Hardcoded path under `target/voice-bird-next-events/`; deliberately
//! not configurable yet — replace with the planned `paths::Session`
//! abstraction once it lands.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;

use crate::bus::AppEvent;

/// Subdirectory under `next/target/` that holds per-session event logs.
const LOG_SUBDIR: &str = "voice-bird-next-events";

/// Owns the open append handle and the resolved path so `Drop` can flush.
pub struct EventLog {
    file: fs::File,
    path: PathBuf,
}

impl EventLog {
    /// Open a fresh per-session log file. Filename:
    /// `voice_bird_events_<UTC-timestamp>.jsonl`.
    ///
    /// Best-effort: if the directory or file cannot be created the
    /// caller logs a single warning and continues with `None`. Logging
    /// is observability, not a correctness requirement; an event loop
    /// that pauses to wait for disk I/O or dies because the log dir is
    /// read-only is worse than dropping events.
    pub fn open() -> Option<Self> {
        let dir = Self::log_dir()?;
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("event_log: cannot create {}: {e}", dir.display());
            return None;
        }
        let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ");
        let path = dir.join(format!("voice_bird_events_{stamp}.jsonl"));
        let file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("event_log: cannot open {}: {e}", path.display());
                return None;
            }
        };
        Some(Self { file, path })
    }

    /// Build the log directory path. Always rooted at the workspace's
    /// `next/target/` so the path is stable across machines and the
    /// existing root `.gitignore` `/target/` rule already covers it.
    fn log_dir() -> Option<PathBuf> {
        // CARGO_MANIFEST_DIR is the absolute path to the `next/` crate
        // directory at compile time. Tests in this crate live under
        // `next/tests/`; CARGO_MANIFEST_DIR resolves correctly for them
        // because `cargo test` runs in the package's directory.
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(LOG_SUBDIR),
        )
    }

    /// Path the log was opened against. Exposed for diagnostics and for
    /// the integration test that asserts a file was created.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Format one event as a JSON line and append it. JSON shape:
    /// `{"ts":"<RFC3339>","event":"<variant>"}`. Errors are swallowed:
    /// a full disk or a rotated inode is not worth surfacing to the UI
    /// mid-frame. The next successful write covers the gap silently.
    pub fn append(&mut self, event: AppEvent) {
        let line = format!(
            "{{\"ts\":\"{}\",\"event\":\"{:?}\"}}\n",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event,
        );
        let _ = self.file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use std::io::Read;

    /// Round-trip: open + append two events, read file back, parse out
    /// the JSON lines. Asserts both the variant tag and that each line
    /// is a single self-contained JSON object (no embedded newlines,
    /// brace balance = 1, ends with newline).
    #[test]
    fn append_writes_one_json_line_per_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Open via a manual helper that takes a path so we can target
        // a sandbox without touching the real target/ dir.
        let path = tmp.path().join("log.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        let mut log = EventLog { file, path: path.clone() };
        log.append(AppEvent::AddBlock);
        log.append(AppEvent::Quit);
        drop(log);

        let mut body = String::new();
        std::fs::File::open(&path)
            .expect("reopen")
            .read_to_string(&mut body)
            .expect("read");

        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected one line per event, got {body:?}");
        for line in &lines {
            assert!(line.starts_with("{\"ts\":\""), "missing ts prefix: {line}");
            assert!(line.ends_with("\"}"), "missing event close: {line}");
            // exactly one opening + one closing brace
            assert_eq!(line.matches('{').count(), 1);
            assert_eq!(line.matches('}').count(), 1);
            assert!(line.contains("\"event\":"), "missing event key: {line}");
        }
        assert!(lines[0].contains("\"event\":\"AddBlock\""));
        assert!(lines[1].contains("\"event\":\"Quit\""));
    }

    /// Confirm `log_dir()` resolves under the next-crate target/ dir.
    /// Pure path check — does not actually open a file.
    #[test]
    fn log_dir_is_inside_next_target() {
        let dir = EventLog::log_dir().expect("dir");
        assert!(dir.ends_with("voice-bird-next-events"), "{dir:?}");
        assert!(
            dir.components().any(|c| c.as_os_str() == "target"),
            "expected `target` in path, got {dir:?}"
        );
    }

    /// The drain path is what the loop calls per tick. Spin up a real
    /// EventBus, publish through two clones, drain, append each event
    /// to a temp `EventLog`, and assert both events hit disk in order.
    /// Locks the contract: every drained event reaches the log.
    #[test]
    fn drained_events_are_logged_in_publish_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("log.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        let mut log = EventLog { file, path: path.clone() };

        let mut bus = EventBus::new();
        let a = bus.sender();
        let b = bus.sender();
        a.publish(AppEvent::AddBlock);
        b.publish(AppEvent::Quit);
        for ev in bus.drain() {
            log.append(ev);
        }
        drop(log);

        let body = std::fs::read_to_string(&path).expect("read");
        let events: Vec<&str> = body
            .lines()
            .filter_map(|l| l.split("\"event\":\"").nth(1))
            .filter_map(|tail| tail.split('"').next())
            .collect();
        assert_eq!(events, vec!["AddBlock", "Quit"]);
    }
}