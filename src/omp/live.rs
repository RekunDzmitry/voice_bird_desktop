//! Cross-process segment bridge for the omp MCP integration.
//!
//! The TUI and the MCP server (spawned by omp as a separate process) do
//! not share memory. When the user picks `Target::Omp`, the TUI appends
//! every committed segment to `~/.voice-bird/live/<slot_id>.jsonl`;
//! the MCP server tails the same file when an agent calls
//! `voice_bird__pull_recent`.
//!
//! Why a file instead of a Unix socket or mmap:
//!
//! - **Append-only JSONL** is the same shape `SegmentWriter` already
//!   uses for session transcripts, so the existing fsync-per-line
//!   semantics carry over without a new I/O path.
//! - **No lifecycle to manage.** Both processes can come and go; the
//!   file is the source of truth. Truncating on session start gives us
//!   a clean slate; rotation happens implicitly when the user picks a
//!   new session path.
//! - **The agent already polls** (`pull_recent` with a `limit`), so we
//!   do not need push semantics — a stale-by-one-pull read is fine.
//!
//! The file lives at `~/.voice-bird/live/<slot_id>.jsonl` so the same
//! path resolves on both sides regardless of cwd. omp spawns the MCP
//! server with `cwd = getProjectDir()`, but a hard-coded absolute
//! path is more robust against the user launching omp from a different
//! directory between pulls.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::session::OmpSessionId;
use crate::transcription::Segment;

/// Subdirectory under `$HOME/.voice-bird/` that holds the live tails.
const LIVE_DIR: &str = "live";

/// Wire shape for one segment on the live tail. Mirrors the JSONL
/// fields the agent already sees via `voice_bird__pull_recent` so the
/// two paths return interchangeable data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LiveSegment {
    pub segment_index: u64,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub text: String,
    #[serde(default)]
    pub tokens: Vec<serde_json::Value>,
    /// Session id the segment belongs to. Optional so older tails
    /// written before this field existed still decode.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
}

impl LiveSegment {
    /// Build a live record from the engine's `Segment` plus the
    /// slot's monotonic counter and the active session id.
    pub fn from_engine(seg: &Segment, segment_index: u64, session: &OmpSessionId) -> Self {
        Self {
            segment_index,
            t_start_ms: seg.t_start.as_millis() as u64,
            t_end_ms: seg.t_end.as_millis() as u64,
            text: seg.text.clone(),
            tokens: seg
                .tokens
                .iter()
                .map(|t| {
                    serde_json::json!({"text": t.text, "t0": t.t_start_ms, "t1": t.t_end_ms})
                })
                .collect(),
            session_id: session.0.clone(),
        }
    }
}

/// Resolve `~/.voice-bird/live/<slot_id>.jsonl`. Both TUI and MCP
/// server call this so they always agree on the on-disk location.
pub fn live_path(slot_id: u8) -> PathBuf {
    home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".voice-bird")
        .join(LIVE_DIR)
        .join(format!("{slot_id}.jsonl"))
}

/// Truncate the live file for `slot_id`. Called by the TUI when a
/// session starts so a brand-new recording doesn't pick up segments
/// left behind by a previous session on the same slot.
pub fn truncate_slot(slot_id: u8) -> Result<()> {
    let p = live_path(slot_id);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    File::create(&p).with_context(|| format!("truncate {}", p.display()))?;
    Ok(())
}

/// Append one segment to the live file for `slot_id`. fsyncs per call
/// so a subsequent `pull_recent` from the MCP server process is
/// guaranteed to see the line.
pub fn append(slot_id: u8, seg: &LiveSegment) -> Result<()> {
    let p = live_path(slot_id);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .with_context(|| format!("open {}", p.display()))?;
    serde_json::to_writer(&mut file, seg).context("serialise segment")?;
    file.write_all(b"\n").context("write newline")?;
    file.flush().context("flush")?;
    file.sync_data().context("fsync")?;
    Ok(())
}

/// Read up to `limit` most recent segments from the live file for
/// `slot_id`, oldest first. Returns an empty Vec if the file is
/// missing or empty (the common case before the first segment lands).
///
/// We do not hold the file open across reads: each `pull_recent` call
/// re-opens it from the start, walks to the last `limit` lines, and
/// returns. This keeps the MCP server side stateless — no shared
/// file cursor to coordinate, no fs-of-the-future shenanigans.
pub fn pull_recent(slot_id: u8, limit: usize) -> Result<Vec<LiveSegment>> {
    let p = live_path(slot_id);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(&p).with_context(|| format!("open {}", p.display()))?;
    let reader = BufReader::new(file);

    let mut window: VecDeque<LiveSegment> = VecDeque::with_capacity(limit);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // skip — append-only, torn lines possible under concurrent writes
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<LiveSegment>(trimmed) {
            Ok(seg) => {
                if window.len() == limit {
                    window.pop_front();
                }
                window.push_back(seg);
            }
            Err(_) => continue, // skip unknown shapes — future versions may add fields
        }
    }
    Ok(window.into_iter().collect())
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Resolve the live dir without a slot — useful for ops / cleanup.
#[allow(dead_code)]
pub fn live_dir() -> Option<PathBuf> {
    home().map(|h| h.join(".voice-bird").join(LIVE_DIR))
}

/// Cheap: `live_path` exists and is readable.
#[allow(dead_code)]
pub fn exists(slot_id: u8) -> bool {
    Path::new(&live_path(slot_id)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn write_n(slot: u8, n: usize) -> Vec<LiveSegment> {
        let mut out = Vec::new();
        for i in 0..n {
            let seg = LiveSegment {
                segment_index: i as u64,
                t_start_ms: i as u64 * 1000,
                t_end_ms: i as u64 * 1000 + 500,
                text: format!("seg-{i}"),
                tokens: vec![],
                session_id: "test".into(),
            };
            append(slot, &seg).unwrap();
            out.push(seg);
        }
        out
    }

    #[test]
    #[serial]
    fn append_then_pull_returns_segments_in_order() {
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot = 1u8;
        truncate_slot(slot).unwrap();
        let written = write_n(slot, 5);
        let got = pull_recent(slot, 50).unwrap();
        restore("HOME", prev_home);
        assert_eq!(got, written);
    }

    #[test]
    #[serial]
    fn pull_recent_returns_only_last_limit() {
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot = 2u8;
        truncate_slot(slot).unwrap();
        let written = write_n(slot, 10);
        let got = pull_recent(slot, 3).unwrap();
        restore("HOME", prev_home);
        // Sliding window keeps the last 3 written: indices 7,8,9.
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].segment_index, 7);
        assert_eq!(got[2].segment_index, 9);
        assert_eq!(got, written[7..].to_vec());
    }

    #[test]
    #[serial]
    fn pull_recent_returns_empty_when_file_missing() {
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let got = pull_recent(99, 10).unwrap();
        restore("HOME", prev_home);
        assert!(got.is_empty());
    }

    #[test]
    #[serial]
    fn truncate_clears_previous_content() {
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot = 3u8;
        write_n(slot, 5);
        truncate_slot(slot).unwrap();
        let got = pull_recent(slot, 50).unwrap();
        restore("HOME", prev_home);
        assert!(got.is_empty());
    }

    #[test]
    #[serial]
    fn append_is_durable_across_reopens() {
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot = 4u8;
        truncate_slot(slot).unwrap();
        append(
            slot,
            &LiveSegment {
                segment_index: 0,
                t_start_ms: 0,
                t_end_ms: 500,
                text: "durable".into(),
                tokens: vec![],
                session_id: "".into(),
            },
        )
        .unwrap();
        let got = pull_recent(slot, 10).unwrap();
        restore("HOME", prev_home);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "durable");
    }

    #[test]
    #[serial]
    fn malformed_lines_are_skipped_not_fatal() {
        // Future versions of voice-bird might add a field that an older
        // MCP server doesn't recognise. We must not refuse the whole
        // pull on one bad line.
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot = 5u8;
        truncate_slot(slot).unwrap();
        let p = live_path(slot);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        f.write_all(b"not json\n").unwrap();
        f.write_all(
            br#"{"segment_index":42,"t_start_ms":0,"t_end_ms":500,"text":"good"}"#,
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
        f.sync_all().unwrap();
        let got = pull_recent(slot, 10).unwrap();
        restore("HOME", prev_home);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "good");
    }

    fn restore(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}