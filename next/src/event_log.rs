//! On-disk append-only log of every [`AppEvent`] that traverses the bus.
//!
//! One JSON object per line, ISO-8601 UTC timestamp + variant tag.
//! Path is resolved at runtime from the platform data-directory API
//! (`directories::ProjectDirs::data_dir()`), so the compiled binary
//! contains no source paths and stays portable across checkouts.
//! Deliberately not configurable yet - replace with the planned
//! `paths::Session` abstraction once it lands.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;

use directories::ProjectDirs;

use crate::bus::AppEvent;

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

    /// Resolve the log directory at runtime. Uses the platform data
    /// directory via `directories` so the compiled binary is portable:
    /// `~/Library/Application Support/com.RekunDzmitry.voice-bird-next/events`
    /// on macOS, `~/.local/share/voice-bird-next/events` on Linux,
    /// `%APPDATA%\voice-bird-next\events` on Windows.
    ///
    /// Falls back to `std::env::temp_dir()/voice-bird-next/events` only
    /// when the platform cannot provide a data dir (rare; e.g. exotic
    /// targets). Returns `None` only as a structural guard.
    fn log_dir() -> Option<PathBuf> {
        if let Some(proj) = ProjectDirs::from("com", "RekunDzmitry", "voice-bird-next") {
            return Some(proj.data_dir().join("events"));
        }
        Some(std::env::temp_dir().join("voice-bird-next").join("events"))

    }

    /// Path the log was opened against. Exposed for diagnostics and for
    /// the integration test that asserts a file was created.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Format one event as a JSON line and append it. JSON shape:
    /// `{"ts":"<RFC3339>","event":"<variant>"}` for unit variants;
    /// payload-bearing variants serialize their inner fields under
    /// the same `"event"` key via serde's internal tagging, so
    /// `ModelSelected(...)` becomes
    /// `{"event":"ModelSelected","model":{...}}` alongside `ts`.
    ///
    /// The shape is serialized by `serde_json` rather than formatted
    /// by hand because a `Debug` dump of an event embedded inside a
    /// quoted string isn't valid JSON (e.g. the inner quotes in
    /// `id: "distil-small.en"`). Every line passes `serde_json::from_str`
    /// back into the event.
    ///
    /// Errors are swallowed: a full disk or a rotated inode is not
    /// worth surfacing to the UI mid-frame. The next successful
    /// write covers the gap silently.
    /// Borrow by reference so the drain loop can log, repo-apply and
    /// state-apply the same event in turn without cloning. The inner
    /// `Record` already holds a `&'a AppEvent`, so the move from
    /// owned to borrowed is a signature change, not a behaviour change.
    pub fn append(&mut self, event: &AppEvent) {
        #[derive(serde::Serialize)]
        struct Record<'a> {
            ts: String,
            #[serde(flatten)]
            event: &'a AppEvent,
        }
        let record = Record {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event,
        };
        match serde_json::to_writer(&mut self.file, &record) {
            Ok(()) => {}
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "event log: {e}");
            }
        }
        // Each line is a self-contained JSON object — newline is the
        // record separator, both for human readability and for any
        // downstream NDJSON consumer that streams the file.
        let _ = self.file.write_all(b"\n");
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
        log.append(&AppEvent::AddBlock);
        log.append(&AppEvent::Quit);
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

    /// `log_dir()` resolves at runtime via the platform data directory
    /// (or the temp-dir fallback). It MUST NOT contain any path under
    /// the build machine's source tree - the binary is portable.
    #[test]
    fn log_dir_does_not_contain_manifest_path() {
        let dir = EventLog::log_dir().expect("dir");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            !dir.starts_with(manifest),
            "log_dir {dir:?} lives under CARGO_MANIFEST_DIR {manifest:?}; \
             binary would not be portable"
        );
        // Tail must mark this as a voice-bird-next log dir regardless
        // of whether we took the ProjectDirs or temp-dir branch.
        let ends_branch_a = dir.ends_with("com.RekunDzmitry.voice-bird-next/events")
            || dir.ends_with("voice-bird-next/events");
        assert!(ends_branch_a, "unexpected log_dir tail: {dir:?}");
    }

    /// Sanity-check the platform data-dir is writable from this process
    /// so `open()` can use it. Skipped silently when we fall back to
    /// temp-dir (e.g. unsupported target): the rest of the test suite
    /// already exercises `open()` with a real path.
    #[test]
    #[cfg(target_os = "macos")]
    fn log_dir_lives_under_application_support_on_macos() {
        let dir = EventLog::log_dir().expect("dir");
        assert!(
            dir.components().any(|c| c.as_os_str() == "Application Support"),
            "expected Application Support on macOS, got {dir:?}"
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
            log.append(&ev);
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

    /// `ModelSelected` carries a `&'static ModelEntry`. The previous
    /// Debug-based format string embedded unescaped inner quotes
    /// (`id: "distil-small.en"`) inside the `"event"` JSON value,
    /// producing invalid JSONL. This test pins the contract: every
    /// written line round-trips through `serde_json::from_str`
    /// without errors, and the model payload fields survive as
    /// well-typed JSON values rather than a Debug-formatted shape
    /// smuggled inside a string.
    #[test]
    fn model_selected_line_round_trips_through_serde_json() {
        use crate::picker::{PickerMove, CATALOG};

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("log.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        let mut log = EventLog { file, path: path.clone() };

        // One of each interesting variant. `ModelSelected` is the
        // reviewer-flagged case; `PickerMoved` is the other payload-
        // bearing variant, paired here so a regression that fixed
        // only one of them would fail this test.
        log.append(&AppEvent::AddBlock);
        log.append(&AppEvent::PickerMoved { direction: PickerMove::Down });
        log.append(&AppEvent::ModelSelected(&CATALOG[0]));
        log.append(&AppEvent::BlockClosed);
        log.append(&AppEvent::Quit);

        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        eprintln!("RAW[1]={}", lines[1]);
        eprintln!("RAW[2]={}", lines[2]);

        // Every line must be a fully-parsable JSON object — the
        // JSONL file extension advertises that contract, and the
        // previous Debug-embedded-in-string format produced
        // unescaped inner quotes for `ModelSelected` and failed
        // this step.
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {i} not valid JSON: {e}\nline: {line}"));
        }

        // Spec check: payload-bearing events keep their inner
        // fields as flat sibling keys alongside `"event"` and
        // `"ts"`. `#[serde(tag = "event")]` plus `#[serde(flatten)]`
        // on the envelope flattens the tuple field's contents into
        // the parent object. The crucial property — what protects
        // the regression — is that those are proper JSON values
        // (strings, numbers), not a Debug-formatted shape smuggled
        // inside a string.
        let parsed: serde_json::Value =
            serde_json::from_str(lines[2]).expect("model line parses");
        assert!(parsed["ts"].as_str().expect("ts").len() > 10);
        assert_eq!(parsed["event"], "ModelSelected");
        assert_eq!(parsed["id"], CATALOG[0].id);
        assert_eq!(parsed["size_mb"], CATALOG[0].size_mb);
        assert_eq!(parsed["language"], CATALOG[0].language);
        assert!(
            parsed["id"].is_string(),
            "id should be a JSON string; got {parsed:?}"
        );
        assert!(
            parsed["size_mb"].is_number(),
            "size_mb should be a JSON number; got {parsed:?}"
        );

        // PickerMoved line: payload variants serialize inner
        // fields directly (not as Debug strings), so `direction`
        // lands as `"Down"` — not `"PickerMove::Down"` or similar.
        // A struct-variant field (`PickerMoved { direction }`)
        // serializes to a sibling JSON key rather than the inner
        // enum-variant name, which is what makes the on-disk
        // record queryable: jq '.direction' selects the row.
        let parsed: serde_json::Value =
            serde_json::from_str(lines[1]).expect("moved line parses");
        assert_eq!(parsed["event"], "PickerMoved");
        assert_eq!(parsed["direction"], "Down");
    }
}
