# Desktop Local Whisper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the desktop CLI's server-streaming transcription with fully local Whisper inference (`whisper-rs` cross-platform, WhisperKit Swift sidecar on macOS), stream committed/tentative text into the TUI, and persist every session as a self-contained directory.

**Architecture:** A `TranscriptionEngine` trait decouples audio from text. Two implementations share a `Committed` / `Tentative` event contract: `WhisperRsEngine` runs LocalAgreement-2 on whisper.cpp hypotheses inline; `WhisperKitEngine` proxies a bundled Swift sidecar that uses WhisperKit's AlignAtt streaming. Audio is resampled once upstream and tee'd to both the engine and the WAV writer. Events flow over `tokio::broadcast` to an append-only JSONL writer and the ratatui renderer.

**Tech Stack:** Rust 2021 (existing `voice-bird-cli` crate promoted to repo root), ratatui 0.29, crossterm 0.28, cpal 0.15, rubato 0.14, tokio 1, hound 3.5, whisper-rs 0.13, `proptest` + `tempfile` for tests, Swift Package Manager + WhisperKit 0.9+ for the macOS sidecar.

**Spec:** See `docs/superpowers/specs/2026-04-16-desktop-local-whisper-design.md` for the architectural rationale and full requirements referenced below.

**Working branch:** All tasks run on a feature branch (e.g. `feat/local-whisper`) off `main`. Do NOT commit to `main` directly. Create the branch before Task 1.

**Scope amendment (added during execution):** System-audio / loopback capture is deferred for Project A on both Windows and macOS. Project A guarantees **microphone capture only via cpal** on every platform. Rationale:

- `screencapturekit 1.5.x` (used by the existing `voice-bird-cli/src/platform/macos.rs`) fails to build against current CLT `PackageDescription` ABI on our dev machine.
- Loopback is not on the critical path for shipping local transcription — microphone capture covers the common user story and unblocks all other Stage 2 / 3 / 4 work.
- Loopback returns in a follow-up project using a CLT-neutral approach (e.g. `objc2-screen-capture-kit` on macOS, keeping WASAPI on Windows).

Concrete effects on the task list below:

- In **Task 2** (file moves), additionally delete: `voice-bird-cli/src/platform/macos.rs`, `voice-bird-cli/src/platform/windows.rs` (contains WASAPI loopback), and the `pub mod macos; pub mod windows;` lines inside `voice-bird-cli/src/platform/mod.rs`. Keep only the trait/enumeration scaffolding that's purely about input devices, collapsing the module to expose only microphone enumeration via cpal.
- In **Task 2** (file moves), also strip any `platform::start_output_recording` and `AudioSession::is_input`-related branching from `src/main.rs` and `src/app.rs` — after Project A there is only one kind of session (microphone). `RecordingSource::Microphone` is the only variant used until loopback returns.
- In **Task 3** (Cargo.toml rewrite), the `[target.'cfg(target_os = "macos")'.dependencies]` block is empty (see code block in Task 3 — already amended).

Subagents: treat the amendment above as authoritative wherever it conflicts with earlier task-body text.

---

## Stage 1 — Promote the CLI to repo root, delete Tauri

### Task 1: Create working branch and snapshot current state

**Files:**
- No file changes in this task — git only.

- [ ] **Step 1: Verify clean working tree and branch off main**

```bash
cd /Users/dzmitryrekun/github/voice_bird_desktop
git status
git checkout main
git pull --ff-only
git checkout -b feat/local-whisper
```

Expected: `git status` clean (the pre-existing untracked `gen/schemas/` and `npm/.../_CodeSignature/` can stay untracked — do not touch them).

- [ ] **Step 2: Tag the pre-rewrite state**

```bash
git tag pre-local-whisper
```

Expected: `git tag -l pre-local-whisper` prints the tag. This is the rollback point if Stage 1 goes wrong.

### Task 2: Move `voice-bird-cli/` contents to repo root, delete Tauri files

**Files:**
- Delete: `src/main.rs` (the Tauri one), `src/main_old.rs`, `src/commands.rs`, `src/opus_encoder.rs`, `src/server_streaming.rs`, `src/session.rs`, `src/state.rs`, `src/wasapi_sessions.rs`, `src/events.rs`, `src/audio_buffer.rs`, `src/audio_converter.rs`, `src/audio.rs`, `src/config.rs`, `src/logger.rs` (all current Tauri-root sources)
- Delete: `ui/`, `gen/`, `icons/`, `dist/`, `npm/`
- Delete: `tauri.conf.json`, `build.rs`, `pyproject.toml`, `python/`, `voice-bird-cli-crate/`
- Modify: `Cargo.toml` (replace Tauri-root contents with what will become the new workspace + package definition)
- Move: `voice-bird-cli/src/*` → `src/*`, `voice-bird-cli/README.md` → `README.md` (keep the existing top-level `README.md` text in a merged form; overwrite with the CLI one since the product is now a CLI)
- Move: `voice-bird-cli/Cargo.toml` → merge into new root `Cargo.toml` (see Task 3)
- Delete: `voice-bird-cli/` (after its contents are moved)

- [ ] **Step 1: Move CLI sources to the root**

```bash
cd /Users/dzmitryrekun/github/voice_bird_desktop

# Stage the existing Tauri-root src/ for deletion (preserves git history)
git rm -r src/ ui/ gen/ icons/ dist/ npm/ python/ voice-bird-cli-crate/
git rm tauri.conf.json build.rs pyproject.toml

# Move the CLI contents up
git mv voice-bird-cli/src src
git mv voice-bird-cli/README.md README.md
git mv voice-bird-cli/Cargo.lock Cargo.lock
# Keep voice-bird-cli/Cargo.toml for Task 3's merge; we'll delete it then.
```

Expected: `git status` shows the renames and deletions staged. No build yet.

- [ ] **Step 2: Commit the structural move (no build expected to pass yet — root `Cargo.toml` still references Tauri)**

```bash
git commit -m "refactor: promote voice-bird-cli to repo root; delete Tauri app

Project A Stage 1: the product is now a Rust TUI, not a Tauri desktop
app. Source files move from voice-bird-cli/ to the repo root. All
Tauri-specific files are removed. Build is intentionally broken at
this commit — fixed in the next commit which rewrites Cargo.toml."
```

Expected: commit succeeds; `cargo build` fails (Cargo.toml still references Tauri). That is expected and fixed next task.

### Task 3: Rewrite root `Cargo.toml` as the CLI package + workspace

**Files:**
- Modify: `Cargo.toml` (full rewrite)
- Delete: `voice-bird-cli/Cargo.toml`, `voice-bird-cli/`

- [ ] **Step 1: Replace root `Cargo.toml` with the CLI's contents plus a workspace declaration**

Overwrite `/Users/dzmitryrekun/github/voice_bird_desktop/Cargo.toml` with:

```toml
[package]
name = "voice-bird"
version = "0.3.0"
edition = "2021"
description = "Voice Bird — local voice transcription TUI"

[[bin]]
name = "voice-bird"
path = "src/main.rs"

[workspace]
members = ["."]
# xtask is added in Stage 4 when sidecar packaging requires it.

[dependencies]
# TUI
ratatui = "0.29"
crossterm = "0.28"

# Audio
cpal = "0.15"
hound = "3.5"
rubato = "0.14"

# Async
tokio = { version = "1.0", features = ["full"] }

# Logging
log = "0.4"
fern = "0.7"

# Clipboard
arboard = "3"

# Utilities
anyhow = "1.0"
thiserror = "1.0"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"
dirs = "5.0"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1.0", features = ["v4"] }

# Transcription (added concretely in Stage 3, declared here to avoid re-edits)
whisper-rs = "0.13"

[dev-dependencies]
proptest = "1"
tempfile = "3"
pretty_assertions = "1"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Media_Audio",
    "Win32_System_Com",
    "Win32_System_Threading",
    "Win32_Foundation",
    "Win32_System_ProcessStatus",
    "Win32_UI_Shell_PropertiesSystem",
    "Win32_Devices_FunctionDiscovery",
    "Win32_System_SystemInformation",
    "implement",
] }
windows-core = "0.58"

# (No macOS-specific dependencies in Project A. macOS system-audio
# loopback is deferred to a follow-up: the `screencapturekit 1.5.x`
# crate fails to build against the current CLT PackageDescription
# ABI, and loopback is not on Project A's critical path. Microphone
# capture works cross-platform via cpal. When loopback returns,
# options include objc2-screen-capture-kit or a direct FFI shim.)
```

Note: the name is `voice-bird` (not `voice-bird-cli`) because the product is the only binary now. The binary name matches. The `tokio-tungstenite`, `futures-util`, `http`, `uuid v4`, and `screencapturekit` dependencies from the old CLI Cargo.toml are intentionally dropped — the first three were for server streaming (being removed), and `screencapturekit` is deferred until macOS loopback returns in a follow-up. `arboard` stays — still used by config paste. `thiserror`, `toml`, `proptest`, `tempfile`, `pretty_assertions` are added for new code in later tasks.

- [ ] **Step 2: Delete the now-empty `voice-bird-cli/` directory**

```bash
cd /Users/dzmitryrekun/github/voice_bird_desktop
git rm -r voice-bird-cli/
```

Expected: `voice-bird-cli/` gone from the index.

- [ ] **Step 3: Adjust `src/main.rs` to use the new binary name**

Find `fn main()` in `src/main.rs`. The existing code logs `log::info!("Voice Bird CLI starting")` — leave it. The binary name change is Cargo-level only; no source edits required unless a `env!("CARGO_BIN_NAME")` or `env!("CARGO_PKG_NAME")` is referenced. Grep to confirm:

```bash
grep -rn "voice-bird-cli\|voice_bird_cli" src/
```

Expected: no matches. If matches exist, change them to `voice-bird` / `voice_bird`.

- [ ] **Step 4: Build the CLI at the new root**

```bash
cargo build
```

Expected: PASS. The binary is produced at `target/debug/voice-bird`.

- [ ] **Step 5: Run existing unit tests**

```bash
cargo test
```

Expected: PASS (the CLI has few tests today; all that existed must still pass).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "refactor: rewrite root Cargo.toml for CLI package + workspace

Renames the package to 'voice-bird' (binary name matches).
Drops server-streaming dependencies (tokio-tungstenite, futures-util,
http) that will be removed with streaming.rs in Stage 5.
Adds thiserror, toml, proptest, tempfile, pretty_assertions, whisper-rs
as placeholders — wired up in Stage 2 and Stage 3."
```

Expected: clean commit; `cargo build` and `cargo test` both green.

---

## Stage 2 — Trait, session writer, mock engine, TUI rework

### Task 4: Scaffold the new module layout

**Files:**
- Create: `src/transcription/mod.rs`
- Create: `src/transcription/local_agreement.rs` (empty module file)
- Create: `src/transcription/mock.rs` (empty)
- Create: `src/transcription/whisper_rs_engine.rs` (empty, filled in Stage 3)
- Create: `src/transcription/whisper_kit_engine.rs` (empty, filled in Stage 4)
- Create: `src/transcription/models.rs` (empty, filled in Stage 3)
- Create: `src/session/mod.rs`
- Create: `src/session/layout.rs` (empty)
- Create: `src/session/writer.rs` (empty)
- Create: `src/session/finalize.rs` (empty)
- Create: `src/audio/mod.rs`, `src/audio/resample.rs` (empty; existing `src/audio.rs` stays until Task 14)
- Modify: `src/main.rs` — add `mod transcription;` and `mod session;` and `mod audio;` (nested). Note: existing `mod audio;` references the flat `src/audio.rs`. To avoid conflict, rename the existing file now.

- [ ] **Step 1: Rename the flat audio module to make room for a directory module**

```bash
cd /Users/dzmitryrekun/github/voice_bird_desktop
git mv src/audio.rs src/audio_legacy.rs
```

Update `src/main.rs`:

```rust
// old:
mod audio;
// new:
mod audio_legacy;
mod audio;       // becomes src/audio/mod.rs below
```

Update `src/app.rs` and any other imports of `crate::audio::...` to `crate::audio_legacy::...` for now:

```bash
grep -rln "crate::audio::" src/
```

For each hit, replace `crate::audio::` with `crate::audio_legacy::`. `audio_legacy` is the pre-rewrite audio code; it is fully deleted in Task 14 once the new module replaces it.

- [ ] **Step 2: Create empty module files**

```bash
mkdir -p src/transcription src/session src/audio
for f in \
  src/transcription/mod.rs \
  src/transcription/local_agreement.rs \
  src/transcription/mock.rs \
  src/transcription/whisper_rs_engine.rs \
  src/transcription/whisper_kit_engine.rs \
  src/transcription/models.rs \
  src/session/mod.rs \
  src/session/layout.rs \
  src/session/writer.rs \
  src/session/finalize.rs \
  src/audio/mod.rs \
  src/audio/resample.rs ; do
  : > "$f"
done
```

Then populate `src/transcription/mod.rs`:

```rust
pub mod local_agreement;
pub mod mock;
pub mod models;
pub mod whisper_kit_engine;
pub mod whisper_rs_engine;
```

Populate `src/session/mod.rs`:

```rust
pub mod finalize;
pub mod layout;
pub mod writer;
```

Populate `src/audio/mod.rs`:

```rust
pub mod resample;
```

- [ ] **Step 3: Wire the new modules into `src/main.rs`**

Add these lines near the top, alongside the existing `mod app;` etc.:

```rust
mod session;
mod transcription;
```

(`mod audio;` is already added from Step 1.)

- [ ] **Step 4: Build to verify empty modules compile**

```bash
cargo build
```

Expected: PASS. Unused warnings for the new empty files are fine.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: scaffold transcription/session/audio module layout"
```

### Task 5: Implement `session::layout` — slug + path derivation

**Files:**
- Modify: `src/session/layout.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// src/session/layout.rs
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn slug_uses_timestamp_and_source() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 4, 16, 14, 32, 7).unwrap();
        let s = session_slug(ts, &SessionSource::App("Zoom".into()));
        assert_eq!(s, "2026-04-16_14-32-07-zoom");
    }

    #[test]
    fn slug_normalizes_app_name() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let s = session_slug(ts, &SessionSource::App("Google Chrome Helper".into()));
        assert_eq!(s, "2026-01-02_03-04-05-google-chrome-helper");
    }

    #[test]
    fn slug_for_mic_and_system() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(session_slug(ts, &SessionSource::Microphone), "2026-01-02_03-04-05-mic");
        assert_eq!(session_slug(ts, &SessionSource::System),     "2026-01-02_03-04-05-system");
    }

    #[test]
    fn session_dir_joins_base_and_slug() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let root = std::path::PathBuf::from("/tmp/voice-bird/sessions");
        let dir = session_dir(&root, ts, &SessionSource::Microphone);
        assert_eq!(dir, std::path::PathBuf::from("/tmp/voice-bird/sessions/2026-01-02_03-04-05-mic"));
    }
}
```

- [ ] **Step 2: Run tests to see them fail**

```bash
cargo test --lib session::layout
```

Expected: FAIL — the types don't exist.

- [ ] **Step 3: Implement the minimal code to pass**

Prepend above the `#[cfg(test)]` block in `src/session/layout.rs`:

```rust
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum SessionSource {
    Microphone,
    System,
    App(String),
}

pub fn session_slug(ts: chrono::DateTime<chrono::Utc>, source: &SessionSource) -> String {
    let ts = ts.format("%Y-%m-%d_%H-%M-%S");
    let src = match source {
        SessionSource::Microphone => "mic".to_string(),
        SessionSource::System => "system".to_string(),
        SessionSource::App(name) => normalize_app_name(name),
    };
    format!("{}-{}", ts, src)
}

pub fn session_dir(
    base: &Path,
    ts: chrono::DateTime<chrono::Utc>,
    source: &SessionSource,
) -> PathBuf {
    base.join(session_slug(ts, source))
}

fn normalize_app_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --lib session::layout
```

Expected: PASS (4/4).

- [ ] **Step 5: Commit**

```bash
git add src/session/layout.rs
git commit -m "feat(session): slug + path derivation for session directories"
```

### Task 6: Implement `session::writer` — append-only JSONL

**Files:**
- Modify: `src/session/writer.rs`
- Reference types: uses `transcription::Segment` which is created in Task 9. To avoid a dependency cycle, Task 6 defines a **minimal writer-local type** that Task 9's `Segment` will be convertible into, and Task 9 will add the `From` impl. This keeps tasks independent.

- [ ] **Step 1: Write the failing tests**

```rust
// src/session/writer.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn seg(t0: f64, t1: f64, text: &str) -> WrittenSegment {
        WrittenSegment {
            t_start_ms: (t0 * 1000.0) as u64,
            t_end_ms:   (t1 * 1000.0) as u64,
            text: text.into(),
        }
    }

    #[test]
    fn appends_one_line_per_segment() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut w = SegmentWriter::open(&path).unwrap();
        w.append(&seg(0.0, 1.5, "hello")).unwrap();
        w.append(&seg(1.5, 3.0, "world")).unwrap();
        drop(w);

        let s = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"text\":\"hello\""));
        assert!(lines[1].contains("\"text\":\"world\""));
    }

    #[test]
    fn survives_reopen_and_appends() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        {
            let mut w = SegmentWriter::open(&path).unwrap();
            w.append(&seg(0.0, 1.0, "first")).unwrap();
        }
        {
            let mut w = SegmentWriter::open(&path).unwrap();
            w.append(&seg(1.0, 2.0, "second")).unwrap();
        }
        let s = std::fs::read_to_string(&path).unwrap();
        assert_eq!(s.lines().count(), 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --lib session::writer
```

Expected: FAIL — types undefined.

- [ ] **Step 3: Implement**

Prepend to `src/session/writer.rs`:

```rust
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrittenSegment {
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub text: String,
}

pub struct SegmentWriter {
    file: BufWriter<File>,
}

impl SegmentWriter {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file: BufWriter::new(file) })
    }

    pub fn append(&mut self, seg: &WrittenSegment) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.file, seg)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.get_ref().sync_data()?;  // fsync per segment
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib session::writer
```

Expected: PASS (2/2).

- [ ] **Step 5: Commit**

```bash
git add src/session/writer.rs
git commit -m "feat(session): append-only JSONL segment writer with per-line fsync"
```

### Task 7: Implement `session::finalize` — JSONL + WAV duration → `transcript.json` and `.txt`

**Files:**
- Modify: `src/session/finalize.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// src/session/finalize.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::writer::{SegmentWriter, WrittenSegment};
    use tempfile::TempDir;

    #[test]
    fn writes_json_and_txt_from_jsonl() {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");
        {
            let mut w = SegmentWriter::open(&jsonl).unwrap();
            w.append(&WrittenSegment { t_start_ms: 0,    t_end_ms: 1500, text: "hello".into() }).unwrap();
            w.append(&WrittenSegment { t_start_ms: 1500, t_end_ms: 3000, text: "world".into() }).unwrap();
        }

        let meta = SessionMeta {
            version: "0.3.0".into(),
            model: "distil-small.en".into(),
            engine: "whisper_rs".into(),
            source: "mic".into(),
            device: "MacBook Pro Microphone".into(),
            started_at: "2026-04-16T14:32:07Z".into(),
            ended_at: "2026-04-16T14:32:10Z".into(),
            duration_ms: 3000,
        };

        let out_json = dir.path().join("transcript.json");
        let out_txt  = dir.path().join("transcript.txt");
        let out_meta = dir.path().join("meta.json");
        finalize(&jsonl, &out_json, &out_txt, &out_meta, &meta).unwrap();

        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out_json).unwrap()).unwrap();
        assert_eq!(j["segments"].as_array().unwrap().len(), 2);
        assert_eq!(j["meta"]["model"], "distil-small.en");

        let t = std::fs::read_to_string(&out_txt).unwrap();
        assert_eq!(t, "hello\nworld\n");

        let m: SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(&out_meta).unwrap()).unwrap();
        assert_eq!(m.model, "distil-small.en");
    }

    #[test]
    fn empty_jsonl_produces_empty_transcript() {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");
        std::fs::write(&jsonl, "").unwrap();
        let meta = SessionMeta::default();

        finalize(
            &jsonl,
            &dir.path().join("transcript.json"),
            &dir.path().join("transcript.txt"),
            &dir.path().join("meta.json"),
            &meta,
        ).unwrap();

        assert_eq!(std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap(), "");
    }
}
```

- [ ] **Step 2: Run tests — expect failure**

```bash
cargo test --lib session::finalize
```

Expected: FAIL — `SessionMeta`, `finalize` don't exist.

- [ ] **Step 3: Implement**

Prepend to `src/session/finalize.rs`:

```rust
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::session::writer::WrittenSegment;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: String,
    pub model: String,
    pub engine: String,
    pub source: String,
    pub device: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct FinalTranscript<'a> {
    segments: &'a [WrittenSegment],
    meta: &'a SessionMeta,
}

pub fn finalize(
    jsonl: &Path,
    out_json: &Path,
    out_txt: &Path,
    out_meta: &Path,
    meta: &SessionMeta,
) -> anyhow::Result<()> {
    let segments = read_jsonl(jsonl)?;
    write_atomic(out_json, |f| {
        serde_json::to_writer_pretty(f, &FinalTranscript { segments: &segments, meta })?;
        Ok(())
    })?;
    write_atomic(out_txt, |f| {
        for s in &segments {
            writeln!(f, "{}", s.text)?;
        }
        Ok(())
    })?;
    write_atomic(out_meta, |f| {
        serde_json::to_writer_pretty(f, meta)?;
        Ok(())
    })?;
    Ok(())
}

fn read_jsonl(path: &Path) -> anyhow::Result<Vec<WrittenSegment>> {
    let s = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

fn write_atomic<F>(path: &Path, write: F) -> anyhow::Result<()>
where F: FnOnce(&mut std::fs::File) -> anyhow::Result<()>
{
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        write(&mut f)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib session::finalize
```

Expected: PASS (2/2).

- [ ] **Step 5: Commit**

```bash
git add src/session/finalize.rs
git commit -m "feat(session): finalize JSONL + meta into transcript.{json,txt} atomically"
```

### Task 8: Define `TranscriptionEngine` trait, events, types

**Files:**
- Modify: `src/transcription/mod.rs`

- [ ] **Step 1: Write the types and trait**

Replace `src/transcription/mod.rs` contents:

```rust
pub mod local_agreement;
pub mod mock;
pub mod models;
pub mod whisper_kit_engine;
pub mod whisper_rs_engine;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::session::writer::WrittenSegment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub text: String,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub t_start: Duration,
    pub t_end: Duration,
    pub text: String,
    pub tokens: Vec<Token>,
}

impl From<&Segment> for WrittenSegment {
    fn from(s: &Segment) -> Self {
        WrittenSegment {
            t_start_ms: s.t_start.as_millis() as u64,
            t_end_ms:   s.t_end.as_millis() as u64,
            text: s.text.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    ModelLoaded { name: String },
    Committed(Segment),
    Tentative(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model_path: std::path::PathBuf,
    pub language: Option<String>,
    pub sample_rate: u32,   // always 16_000
    pub hop_ms: u32,        // WhisperRsEngine only
    pub min_window_ms: u32, // WhisperRsEngine only
}

pub struct EngineHandle {
    pub pcm_tx: mpsc::Sender<Vec<f32>>,
    pub events_rx: broadcast::Receiver<EngineEvent>,
    pub shutdown: oneshot::Sender<()>,
}

pub trait TranscriptionEngine: Send {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle>;
}
```

- [ ] **Step 2: Build to verify**

```bash
cargo build
```

Expected: PASS (the empty child modules compile; nothing else references these types yet).

- [ ] **Step 3: Commit**

```bash
git add src/transcription/mod.rs
git commit -m "feat(transcription): define TranscriptionEngine trait + events + types"
```

### Task 9: Implement `MockEngine` + tests

**Files:**
- Modify: `src/transcription/mock.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// src/transcription/mock.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn emits_scripted_events_in_order() {
        let script = vec![
            MockEvent::ModelLoaded("mock".into()),
            MockEvent::Tentative("hel".into()),
            MockEvent::Tentative("hello".into()),
            MockEvent::Committed {
                t_start_ms: 0, t_end_ms: 1000, text: "hello".into(),
            },
        ];
        let mut engine = MockEngine::new(script);
        let handle = engine.start(test_cfg()).unwrap();
        let mut rx = handle.events_rx;

        // Drive the script by sending any PCM; MockEngine emits one script
        // step per received PCM chunk.
        for _ in 0..4 {
            handle.pcm_tx.send(vec![0.0; 16]).await.unwrap();
        }

        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, EngineEvent::ModelLoaded { .. }));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, EngineEvent::Tentative(s) if s == "hel"));
        let e3 = rx.recv().await.unwrap();
        assert!(matches!(e3, EngineEvent::Tentative(s) if s == "hello"));
        let e4 = rx.recv().await.unwrap();
        assert!(matches!(e4, EngineEvent::Committed(seg) if seg.text == "hello"));
    }

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            model_path: std::path::PathBuf::from("/dev/null"),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        }
    }
}
```

- [ ] **Step 2: Run tests — expect failure**

```bash
cargo test --lib transcription::mock
```

Expected: FAIL — `MockEngine`, `MockEvent` undefined.

- [ ] **Step 3: Implement**

Prepend to `src/transcription/mock.rs`:

```rust
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, Token, TranscriptionEngine};

#[derive(Debug, Clone)]
pub enum MockEvent {
    ModelLoaded(String),
    Tentative(String),
    Committed { t_start_ms: u64, t_end_ms: u64, text: String },
}

pub struct MockEngine {
    script: Vec<MockEvent>,
}

impl MockEngine {
    pub fn new(script: Vec<MockEvent>) -> Self {
        Self { script }
    }
}

impl TranscriptionEngine for MockEngine {
    fn start(&mut self, _cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(16);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(64);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let script = std::mem::take(&mut self.script);

        tokio::spawn(async move {
            let mut iter = script.into_iter();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    msg = pcm_rx.recv() => {
                        if msg.is_none() { break; }
                        let Some(evt) = iter.next() else { continue; };
                        let out = match evt {
                            MockEvent::ModelLoaded(name) => EngineEvent::ModelLoaded { name },
                            MockEvent::Tentative(text)   => EngineEvent::Tentative(text),
                            MockEvent::Committed { t_start_ms, t_end_ms, text } =>
                                EngineEvent::Committed(Segment {
                                    t_start: Duration::from_millis(t_start_ms),
                                    t_end:   Duration::from_millis(t_end_ms),
                                    text,
                                    tokens:  Vec::new(),
                                }),
                        };
                        let _ = events_tx.send(out);
                    }
                }
            }
        });

        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib transcription::mock
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/transcription/mock.rs
git commit -m "feat(transcription): MockEngine for integration tests"
```

### Task 10: Implement `local_agreement::step` — pure function

**Files:**
- Modify: `src/transcription/local_agreement.rs`

- [ ] **Step 1: Write failing unit tests**

```rust
// src/transcription/local_agreement.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::Token;
    use std::time::Duration;

    fn tok(text: &str, t0: u64, t1: u64) -> Token {
        Token { text: text.into(), t_start_ms: t0, t_end_ms: t1 }
    }

    #[test]
    fn no_prev_produces_all_tentative() {
        let curr = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let out = step(&[], &curr, Duration::from_millis(0));
        assert!(out.committed_segments.is_empty());
        assert_eq!(out.tentative_text, "hello world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(0));
    }

    #[test]
    fn exact_match_commits_all() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 1);
        assert_eq!(out.committed_segments[0].text, "hello world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(1000));
        assert_eq!(out.tentative_text, "");
    }

    #[test]
    fn partial_prefix_agreement_commits_prefix_only() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000), tok("again", 1000, 1500)];
        let curr = vec![tok("hello", 0, 500), tok("world", 500, 1000), tok("friend", 1000, 1500)];
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments[0].text, "hello world");
        assert_eq!(out.tentative_text, "friend");
    }

    #[test]
    fn normalizes_punctuation_and_case_when_matching() {
        let prev = vec![tok("Hello,", 0, 500), tok("World.", 500, 1000)];
        let curr = vec![tok("hello",  0, 500), tok("world",  500, 1000)];
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(!out.committed_segments.is_empty(), "should commit via normalized match");
    }

    #[test]
    fn timestamp_skew_within_tolerance_matches() {
        let prev = vec![tok("hello", 0, 500)];
        let curr = vec![tok("hello", 100, 600)];  // 100ms skew, within 300
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(!out.committed_segments.is_empty());
    }

    #[test]
    fn timestamp_skew_beyond_tolerance_does_not_match() {
        let prev = vec![tok("hello", 0, 500)];
        let curr = vec![tok("hello", 400, 900)]; // 400ms, over 300
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(out.committed_segments.is_empty());
    }

    #[test]
    fn committed_upto_filter_skips_already_committed() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(500));
        assert_eq!(out.committed_segments.len(), 1);
        assert_eq!(out.committed_segments[0].text, "world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(1000));
    }

    #[test]
    fn sentence_split_on_period() {
        let prev = vec![
            tok("one",    0,   300),
            tok("two.",  300,  700),
            tok("three", 700, 1100),
        ];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 2);
        assert_eq!(out.committed_segments[0].text, "one two.");
        assert_eq!(out.committed_segments[1].text, "three");
    }

    #[test]
    fn sentence_split_on_gap() {
        let prev = vec![
            tok("before",  0,    300),
            tok("pause", 300,   700),   // 900ms silence after
            tok("after", 1600, 2000),
        ];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 2);
        assert_eq!(out.committed_segments[0].text, "before pause");
        assert_eq!(out.committed_segments[1].text, "after");
    }
}
```

- [ ] **Step 2: Run tests — expect failure**

```bash
cargo test --lib transcription::local_agreement
```

Expected: FAIL — types undefined.

- [ ] **Step 3: Implement**

Prepend to `src/transcription/local_agreement.rs`:

```rust
use std::time::Duration;

use crate::transcription::{Segment, Token};

pub const TIMESTAMP_TOLERANCE_MS: i64 = 300;
pub const SENTENCE_GAP_MS: u64 = 500;

#[derive(Debug, Clone, Default)]
pub struct AgreementOutput {
    pub committed_segments: Vec<Segment>,
    pub tentative_text: String,
    pub new_committed_upto: Duration,
}

pub fn step(prev: &[Token], curr: &[Token], committed_upto: Duration) -> AgreementOutput {
    let prefix_len = longest_agreeing_prefix(prev, curr);
    let committed_tokens: Vec<Token> = curr[..prefix_len]
        .iter()
        .filter(|t| Duration::from_millis(t.t_end_ms) > committed_upto)
        .cloned()
        .collect();

    let new_upto = committed_tokens
        .last()
        .map(|t| Duration::from_millis(t.t_end_ms))
        .unwrap_or(committed_upto);

    let committed_segments = group_into_sentences(&committed_tokens);

    let tentative_tokens = &curr[prefix_len..];
    let tentative_text = tentative_tokens
        .iter()
        .map(|t| t.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    AgreementOutput {
        committed_segments,
        tentative_text,
        new_committed_upto: new_upto,
    }
}

fn longest_agreeing_prefix(prev: &[Token], curr: &[Token]) -> usize {
    let mut n = 0;
    for (p, c) in prev.iter().zip(curr.iter()) {
        if tokens_agree(p, c) { n += 1; } else { break; }
    }
    n
}

fn tokens_agree(a: &Token, b: &Token) -> bool {
    let skew = (a.t_start_ms as i64 - b.t_start_ms as i64).abs();
    skew <= TIMESTAMP_TOLERANCE_MS && normalize(&a.text) == normalize(&b.text)
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn group_into_sentences(tokens: &[Token]) -> Vec<Segment> {
    if tokens.is_empty() { return Vec::new(); }
    let mut out = Vec::new();
    let mut group: Vec<Token> = Vec::new();

    for t in tokens {
        if let Some(prev) = group.last() {
            let gap = t.t_start_ms.saturating_sub(prev.t_end_ms);
            if gap >= SENTENCE_GAP_MS {
                out.push(make_segment(&group));
                group.clear();
            }
        }
        let ends_sentence = t.text.trim_end().ends_with(|c: char| matches!(c, '.'|'?'|'!'));
        group.push(t.clone());
        if ends_sentence {
            out.push(make_segment(&group));
            group.clear();
        }
    }
    if !group.is_empty() {
        out.push(make_segment(&group));
    }
    out
}

fn make_segment(tokens: &[Token]) -> Segment {
    let text = tokens
        .iter()
        .map(|t| t.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Segment {
        t_start: Duration::from_millis(tokens.first().unwrap().t_start_ms),
        t_end:   Duration::from_millis(tokens.last().unwrap().t_end_ms),
        text,
        tokens: tokens.to_vec(),
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib transcription::local_agreement
```

Expected: PASS (9/9).

- [ ] **Step 5: Commit**

```bash
git add src/transcription/local_agreement.rs
git commit -m "feat(transcription): LocalAgreement-2 step + sentence grouping"
```

### Task 11: Add proptest fuzz for `local_agreement::step`

**Files:**
- Modify: `src/transcription/local_agreement.rs`

- [ ] **Step 1: Add the proptest**

Append inside `#[cfg(test)] mod tests { ... }`:

```rust
use proptest::prelude::*;

prop_compose! {
    fn arb_tokens(max_len: usize)(
        texts in prop::collection::vec("[a-z]{1,8}", 0..max_len)
    ) -> Vec<Token> {
        texts.into_iter().enumerate().map(|(i, text)| Token {
            text,
            t_start_ms: (i as u64) * 400,
            t_end_ms:   (i as u64) * 400 + 350,
        }).collect()
    }
}

proptest! {
    #[test]
    fn step_never_panics_and_monotonic_upto(
        prev in arb_tokens(20),
        curr in arb_tokens(20),
        upto_ms in 0u64..5_000u64,
    ) {
        let upto = Duration::from_millis(upto_ms);
        let out = step(&prev, &curr, upto);
        prop_assert!(out.new_committed_upto >= upto,
            "committed_upto must never regress");
        for seg in &out.committed_segments {
            prop_assert!(seg.t_end >= seg.t_start);
        }
    }
}
```

- [ ] **Step 2: Run**

```bash
cargo test --lib transcription::local_agreement
```

Expected: PASS (proptest runs 256 cases by default).

- [ ] **Step 3: Commit**

```bash
git add src/transcription/local_agreement.rs
git commit -m "test(transcription): proptest fuzz LocalAgreement step invariants"
```

### Task 12: Implement `audio::resample` (16 kHz mono f32)

**Files:**
- Modify: `src/audio/resample.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// src/audio/resample.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_already_16k_mono() {
        let input: Vec<f32> = (0..16_000).map(|i| (i as f32 / 16_000.0).sin()).collect();
        let mut r = Resampler::new(16_000, 1).unwrap();
        let out = r.process(&input).unwrap();
        assert!((out.len() as i64 - input.len() as i64).abs() < 32);
    }

    #[test]
    fn downsample_48k_to_16k_preserves_duration() {
        let sr_in = 48_000;
        let len   = 48_000;   // 1 second
        let input: Vec<f32> = (0..len).map(|i| (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin()).collect();
        let mut r = Resampler::new(sr_in, 1).unwrap();
        let out = r.process(&input).unwrap();
        // Expect ~16_000 samples (±5%)
        let expected = 16_000;
        let diff = (out.len() as i64 - expected).abs();
        assert!(diff < (expected as f32 * 0.05) as i64,
            "out.len = {}, expected ~{}", out.len(), expected);
    }

    #[test]
    fn stereo_downmix_to_mono() {
        // interleaved [L,R,L,R,...]
        let input: Vec<f32> = (0..16_000 * 2).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut r = Resampler::new(16_000, 2).unwrap();
        let out = r.process(&input).unwrap();
        // Downmix should produce ~0 for all samples (1 + -1)/2 = 0
        assert!(out.iter().all(|&s| s.abs() < 0.01));
    }
}
```

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --lib audio::resample
```

Expected: FAIL.

- [ ] **Step 3: Implement**

Prepend to `src/audio/resample.rs`:

```rust
use rubato::{FftFixedInOut, Resampler as RubatoResampler};

const TARGET_SR: u32 = 16_000;

pub struct Resampler {
    input_sr: u32,
    channels: u16,
    inner: Option<FftFixedInOut<f32>>,
    chunk_size_in: usize,
    leftover: Vec<f32>,
}

impl Resampler {
    pub fn new(input_sr: u32, channels: u16) -> anyhow::Result<Self> {
        let chunk_size_in = 1024.max((input_sr as usize) / 50);
        let inner = if input_sr == TARGET_SR {
            None
        } else {
            Some(FftFixedInOut::new(
                input_sr as usize,
                TARGET_SR as usize,
                chunk_size_in,
                1,  // output channels — we downmix to mono upstream
            )?)
        };
        Ok(Self {
            input_sr,
            channels,
            inner,
            chunk_size_in,
            leftover: Vec::new(),
        })
    }

    pub fn process(&mut self, interleaved: &[f32]) -> anyhow::Result<Vec<f32>> {
        let mono = downmix(interleaved, self.channels);

        if self.inner.is_none() {
            return Ok(mono);
        }

        let mut buf = std::mem::take(&mut self.leftover);
        buf.extend_from_slice(&mono);

        let mut out = Vec::new();
        while buf.len() >= self.chunk_size_in {
            let chunk = &buf[..self.chunk_size_in];
            let input_channels = vec![chunk.to_vec()];
            let resampled = self.inner.as_mut().unwrap().process(&input_channels, None)?;
            out.extend_from_slice(&resampled[0]);
            buf.drain(..self.chunk_size_in);
        }
        self.leftover = buf;
        Ok(out)
    }
}

fn downmix(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 { return interleaved.to_vec(); }
    let ch = channels as usize;
    let mut out = Vec::with_capacity(interleaved.len() / ch);
    for frame in interleaved.chunks_exact(ch) {
        let sum: f32 = frame.iter().sum();
        out.push(sum / ch as f32);
    }
    out
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib audio::resample
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/resample.rs
git commit -m "feat(audio): resample to 16 kHz mono via rubato FftFixedInOut"
```

### Task 13: Session recover command + integration test

**Files:**
- Create: `src/session/recover.rs`
- Modify: `src/session/mod.rs` (add `pub mod recover;`)
- Modify: `src/main.rs` (add a `--recover <dir>` CLI path in `fn main()` before the TUI comes up)

- [ ] **Step 1: Write the failing integration test**

`tests/session_recover.rs`:

```rust
use std::path::PathBuf;
use tempfile::TempDir;

use voice_bird::session::writer::{SegmentWriter, WrittenSegment};
use voice_bird::session::recover;

#[test]
fn recover_regenerates_json_and_txt_from_partial_jsonl() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("transcript.jsonl");
    {
        let mut w = SegmentWriter::open(&jsonl).unwrap();
        w.append(&WrittenSegment { t_start_ms: 0, t_end_ms: 1000, text: "recovered".into() }).unwrap();
    }
    // No audio.wav needed for recover — duration comes from last segment.

    recover::recover(dir.path()).unwrap();

    let json = std::fs::read_to_string(dir.path().join("transcript.json")).unwrap();
    let txt  = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
    assert!(json.contains("recovered"));
    assert_eq!(txt, "recovered\n");
}
```

This requires the crate to be usable as a library. Update `Cargo.toml`:

```toml
# add after [[bin]]
[lib]
name = "voice_bird"
path = "src/lib.rs"
```

Create `src/lib.rs`:

```rust
pub mod audio;
pub mod session;
pub mod transcription;
```

(Keep `src/main.rs` using `mod` declarations as before — the library and binary have parallel module trees sharing the same files via path attributes is unnecessary since we just re-export. Simpler: delete the `mod` declarations for `audio`, `session`, `transcription` from `main.rs` and import them from `voice_bird::...`. Other existing modules like `app`, `config`, `platform`, `ui`, `logger` stay as binary-local `mod`s until they are touched in later tasks. Do that swap here.)

Specifically, in `src/main.rs`:
- Remove `mod audio;`, `mod session;`, `mod transcription;`
- Add at the top: `use voice_bird as vb;` and change the (currently absent) usages to route through `vb::session::...` etc.

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --test session_recover
```

Expected: FAIL — `recover` module undefined.

- [ ] **Step 3: Implement recover**

`src/session/recover.rs`:

```rust
use std::path::Path;

use crate::session::finalize::{finalize, SessionMeta};

pub fn recover(session_dir: &Path) -> anyhow::Result<()> {
    let jsonl    = session_dir.join("transcript.jsonl");
    let out_json = session_dir.join("transcript.json");
    let out_txt  = session_dir.join("transcript.txt");
    let out_meta = session_dir.join("meta.json");

    let meta = if out_meta.exists() {
        serde_json::from_str::<SessionMeta>(&std::fs::read_to_string(&out_meta)?)?
    } else {
        SessionMeta::default()
    };

    finalize(&jsonl, &out_json, &out_txt, &out_meta, &meta)
}
```

Update `src/session/mod.rs`:

```rust
pub mod finalize;
pub mod layout;
pub mod recover;
pub mod writer;
```

- [ ] **Step 4: Wire `--recover` into `src/main.rs`**

Near the top of `fn main()`, before terminal setup, add:

```rust
let args: Vec<String> = std::env::args().collect();
if let Some(pos) = args.iter().position(|a| a == "--recover") {
    let dir = args.get(pos + 1).ok_or_else(|| anyhow::anyhow!("--recover requires a path"))?;
    voice_bird::session::recover::recover(std::path::Path::new(dir))?;
    println!("Recovered transcripts in {}", dir);
    return Ok(());
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test --test session_recover
cargo build
```

Expected: PASS, build succeeds.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/lib.rs src/session/ src/main.rs tests/session_recover.rs
git commit -m "feat(session): recover command regenerates finalized transcripts from JSONL"
```

### Task 14: Replace old audio/streaming path with new pipeline (still using `MockEngine`)

**Files:**
- Delete: `src/streaming.rs`, `src/audio_legacy.rs`
- Modify: `src/app.rs` — remove `streaming`, `RecordingStatus::Connecting`, `InitSuccess`, `UsageInfo`, `StreamError`. Replace with a simpler `RecordingStatus::{Idle, Recording, Error(String)}`.
- Modify: `src/app.rs` — add an engine-agnostic recording pipeline: when user hits record, spawn an audio capture task, spin up an engine (MockEngine for now), tee PCM to WAV writer and engine, consume `EngineEvent`s on a tokio task into `App` state (committed segments + tentative line).
- Modify: `src/ui.rs` — render the new two-zone layout.
- Modify: `src/main.rs` — remove `handle_config_mode` and `RecordingStatus::Streaming` references; simplify to `AppMode::{Normal, ModelPicker, Help}` (ModelPicker is wired in Stage 3 but the variant is introduced now).
- Modify: `src/config.rs` — drop `api_key`, `server_url`; add `default_model`, `language`, `session_dir`, `hop_ms`, `min_window_ms`, `engine.prefer`, `audio.default_source`.

**This is the biggest single task.** It coordinates the replacement of the entire network path with the local-engine path. Split it into three commits.

- [ ] **Step 1: Strip out streaming — commit 1**

Delete files and remove references:

```bash
git rm src/streaming.rs src/audio_legacy.rs
```

In `src/app.rs`, remove:
- the `use crate::streaming::...` line,
- `RecordingError::QuotaExceeded`, `InvalidApiKey`, `ConnectionFailed`, `InitTimeout`, `NoApiKey`, `NoSessionsStarted` variants,
- `init_result_rx`, `check_init_result`, the whole `RecordingStatus::Connecting` case,
- `RecordingStatus::Streaming { usage }`,
- `RecordingError::from(StreamError)` impl,
- API-key related fields: `api_key_input`, `api_key_visible`, `masked_stored_key`, `paste_from_clipboard`, `save_api_key`, `enter_config_mode`, `toggle_api_key_visibility`, `cancel_config`.

Replace `RecordingStatus` with:

```rust
#[derive(Debug, Clone)]
pub enum RecordingStatus {
    Idle,
    Recording,
    Error(String),
}
```

Replace `AppMode` with:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker,  // wired in Stage 3
    Help,
}
```

In `src/main.rs`:
- Delete `handle_config_mode` entirely.
- Delete `copy_error_to_clipboard` (or simplify to copy `status` Error string).
- In `handle_normal_mode`, delete the `'c'` key handler.
- In `toggle_recording`, change the match: `Idle | Error(_)` → `start_recording`, `Recording` → `stop_all_sessions`.
- In `start_recording`, delete everything related to api_key / server_url / init channels; leave only a stub that sets `status = Recording` — the real pipeline lands in Step 2.

Build:

```bash
cargo build
```

Expected: FAIL with compilation errors that remain after you've cleaned up all mentions. Keep editing until `cargo build` passes with only **warnings** (unused code). Do NOT add new behavior in this step — just strip.

Commit once green:

```bash
git add -A
git commit -m "refactor(app): strip server-streaming scaffolding from App/main

Removes api_key, server_url, ConfigInput mode, RecordingStatus::Streaming
and everything tied to the old WebSocket path. Leaves record/stop
keybindings wired to stub functions; real local pipeline lands next
commit."
```

- [ ] **Step 2: Add the engine pipeline — commit 2**

In `src/app.rs`, add new fields to `App`:

```rust
use std::sync::Arc;
use parking_lot::Mutex;  // add parking_lot = "0.12" to Cargo.toml

use crate::transcription::{EngineEvent, Segment};

pub struct CommittedLine {
    pub t_start_ms: u64,
    pub text: String,
}

pub struct RecordingRuntime {
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
}
```

Add to `App`:

```rust
pub committed: Arc<Mutex<Vec<CommittedLine>>>,
pub tentative: Arc<Mutex<String>>,
pub runtime: Option<RecordingRuntime>,
pub session_dir: Option<std::path::PathBuf>,
pub session_started_at: Option<chrono::DateTime<chrono::Utc>>,
```

Replace `start_recording` with a function that:
1. Creates the session directory via `session::layout::session_dir(&config.session_dir, now, &source)`.
2. Spawns a tokio runtime (use `tokio::runtime::Runtime::new()` in a dedicated thread — the current `fn main` is sync).
3. Starts a `cpal` input stream capturing f32 frames into an `mpsc::Sender<Vec<f32>>`.
4. Wires frames through `audio::resample::Resampler` → tees to:
   - `hound::WavWriter` for `audio.wav`
   - `engine.pcm_tx` for transcription
5. Starts a `MockEngine` for now (real one in Stage 3). The script for the mock can be hard-coded as a demo until the real engine lands.
6. Spawns a tokio task consuming `engine.events_rx` into `App.committed` / `App.tentative` via `Arc<Mutex<...>>`.
7. Writes each `Committed` segment to `session::writer::SegmentWriter`.
8. On shutdown (stop_recording), flushes writer, calls `session::finalize::finalize(...)`.

Because the Tauri-era `App` was fully sync and `main` used blocking event loops, introducing tokio requires creating a `tokio::runtime::Builder::new_multi_thread().enable_all().build()?` once at app startup and holding it on `App`.

Add to `App`:

```rust
pub rt: tokio::runtime::Runtime,
```

Initialize in `App::new()`:

```rust
rt: tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2)
    .enable_all()
    .build()
    .expect("tokio runtime"),
```

Full `start_recording`:

```rust
pub fn start_recording(&mut self, source: crate::session::layout::SessionSource) {
    let now = chrono::Utc::now();
    let session_dir = crate::session::layout::session_dir(
        std::path::Path::new(&self.config.session_dir_expanded()),
        now,
        &source,
    );
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        self.status = RecordingStatus::Error(format!("create session dir: {e}"));
        return;
    }

    self.session_dir = Some(session_dir.clone());
    self.session_started_at = Some(now);

    // Clear live state
    self.committed.lock().clear();
    *self.tentative.lock() = String::new();

    // Stub: script a mock event stream so we can see it rendered
    use crate::transcription::mock::{MockEngine, MockEvent};
    use crate::transcription::TranscriptionEngine;
    let mut engine = MockEngine::new(vec![
        MockEvent::ModelLoaded("mock".into()),
        MockEvent::Tentative("warming up".into()),
        MockEvent::Committed { t_start_ms: 0, t_end_ms: 1000, text: "Welcome to Voice Bird".into() },
    ]);

    let cfg = crate::transcription::EngineConfig {
        model_path: std::path::PathBuf::from("mock"),
        language: None,
        sample_rate: 16_000,
        hop_ms: 750,
        min_window_ms: 1000,
    };

    let handle = match engine.start(cfg) {
        Ok(h) => h,
        Err(e) => {
            self.status = RecordingStatus::Error(format!("engine start: {e}"));
            return;
        }
    };

    let committed = self.committed.clone();
    let tentative = self.tentative.clone();
    let writer_path = session_dir.join("transcript.jsonl");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let mut events_rx = handle.events_rx;
    let pcm_tx = handle.pcm_tx.clone();

    let join = self.rt.spawn(async move {
        let mut writer = match crate::session::writer::SegmentWriter::open(&writer_path) {
            Ok(w) => w,
            Err(e) => { log::error!("writer: {e}"); return; }
        };

        // Drive mock with dummy PCM ticks so the scripted events fire.
        let pcm_tx2 = pcm_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                ticker.tick().await;
                if pcm_tx2.send(vec![0.0; 16]).await.is_err() { break; }
            }
        });

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                evt = events_rx.recv() => match evt {
                    Ok(crate::transcription::EngineEvent::ModelLoaded{..}) => {}
                    Ok(crate::transcription::EngineEvent::Tentative(s)) => {
                        *tentative.lock() = s;
                    }
                    Ok(crate::transcription::EngineEvent::Committed(seg)) => {
                        let written = (&seg).into();
                        if let Err(e) = writer.append(&written) {
                            log::error!("writer append: {e}"); break;
                        }
                        committed.lock().push(CommittedLine {
                            t_start_ms: seg.t_start.as_millis() as u64,
                            text: seg.text,
                        });
                        tentative.lock().clear();
                    }
                    Ok(crate::transcription::EngineEvent::Error(e)) => {
                        log::error!("engine error: {e}"); break;
                    }
                    Err(_) => break,
                }
            }
        }
    });

    self.runtime = Some(RecordingRuntime { shutdown_tx, join });
    self.status = RecordingStatus::Recording;
    self.start_time = Some(std::time::Instant::now());
}
```

(Real audio capture via cpal is not wired in this task because we're still on `MockEngine` — the mock is driven by the tokio ticker above. Cpal is wired in Task 17 alongside the real engine. That keeps this task focused on the event plumbing.)

Also implement `stop_recording`:

```rust
pub fn stop_recording(&mut self) {
    if let Some(rt) = self.runtime.take() {
        let _ = rt.shutdown_tx.send(());
        // best-effort await
        let _ = self.rt.block_on(async move { let _ = rt.join.await; });
    }

    if let (Some(dir), Some(started)) = (self.session_dir.take(), self.session_started_at.take()) {
        let ended = chrono::Utc::now();
        let meta = crate::session::finalize::SessionMeta {
            version: env!("CARGO_PKG_VERSION").into(),
            model: self.config.default_model.clone(),
            engine: "mock".into(),
            source: "mic".into(),
            device: "mock".into(),
            started_at: started.to_rfc3339(),
            ended_at: ended.to_rfc3339(),
            duration_ms: (ended - started).num_milliseconds().max(0) as u64,
        };
        let _ = crate::session::finalize::finalize(
            &dir.join("transcript.jsonl"),
            &dir.join("transcript.json"),
            &dir.join("transcript.txt"),
            &dir.join("meta.json"),
            &meta,
        );
    }

    self.status = RecordingStatus::Idle;
    self.start_time = None;
    self.duration = 0.0;
}
```

Update `src/main.rs`:
- `toggle_recording` calls `app.start_recording(SessionSource::Microphone)` / `app.stop_recording()`.

Build:

```bash
cargo build
```

Expected: PASS. Runtime test at this stage should produce a session dir with a mock-scripted transcript.

Commit:

```bash
git add -A
git commit -m "feat(app): wire engine-agnostic recording pipeline with MockEngine

Records always-running tokio runtime on App. On start, creates a
session directory, starts a MockEngine driven by a tokio tick, tees
engine events into live App state and an append-only JSONL writer.
On stop, finalizes the transcript and meta files. Real audio
capture lands in Stage 3 alongside WhisperRsEngine."
```

- [ ] **Step 3: Rework the TUI — commit 3**

Replace `src/ui.rs` with the new layout:

```rust
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, RecordingStatus};

pub fn render(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),   // header
            Constraint::Min(4),      // committed zone
            Constraint::Length(3),   // tentative line
            Constraint::Length(1),   // footer/keys
        ])
        .split(f.size());

    render_header(f, root[0], app);
    render_committed(f, root[1], app);
    render_tentative(f, root[2], app);
    render_footer(f, root[3], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let timer = app.format_duration();
    let status = match &app.status {
        RecordingStatus::Idle     => "idle",
        RecordingStatus::Recording => "● REC",
        RecordingStatus::Error(_) => "ERROR",
    };
    let line = Line::from(vec![
        Span::styled("Voice Bird", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  │  "),
        Span::raw(app.config.default_model.clone()),
        Span::raw("  │  "),
        Span::raw(format!("engine: {}", app.config.engine_prefer.clone())),
        Span::raw("  │  "),
        Span::styled(status, status_style(&app.status)),
        Span::raw("  │  "),
        Span::raw(timer),
    ]);
    let p = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
}

fn status_style(status: &RecordingStatus) -> Style {
    match status {
        RecordingStatus::Recording => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        RecordingStatus::Error(_)  => Style::default().fg(Color::Red),
        _                          => Style::default().fg(Color::Gray),
    }
}

fn render_committed(f: &mut Frame, area: Rect, app: &App) {
    let committed = app.committed.lock();
    let lines: Vec<Line> = committed.iter().map(|c| {
        let ts = format!("{:02}:{:02}", c.t_start_ms / 60_000, (c.t_start_ms % 60_000) / 1000);
        Line::from(vec![
            Span::styled(ts, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::raw(c.text.clone()),
        ])
    }).collect();

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" Transcript "));
    f.render_widget(p, area);
}

fn render_tentative(f: &mut Frame, area: Rect, app: &App) {
    let text = app.tentative.lock().clone();
    let p = Paragraph::new(Line::from(Span::styled(
        format!("… {}", text),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    ))).block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = match &app.status {
        RecordingStatus::Idle     => "[r] record  [m] model  [q] quit  [?] help",
        RecordingStatus::Recording => "[s] stop  [q] quit",
        RecordingStatus::Error(_) => "[r] retry  [q] quit",
    };
    let p = Paragraph::new(keys).style(Style::default().fg(Color::Gray));
    f.render_widget(p, area);
}
```

Update `src/main.rs` `handle_normal_mode`:

```rust
fn handle_normal_mode(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('r') => {
            if matches!(app.status, RecordingStatus::Idle | RecordingStatus::Error(_)) {
                app.start_recording(crate::session::layout::SessionSource::Microphone);
            }
        }
        KeyCode::Char('s') => {
            if matches!(app.status, RecordingStatus::Recording) {
                app.stop_recording();
            }
        }
        KeyCode::Char('m') => app.mode = crate::app::AppMode::ModelPicker, // wired in Stage 3
        _ => {}
    }
}
```

Build and manual smoke:

```bash
cargo build
cargo run -- --help 2>/dev/null  # just to ensure start
```

Launch the TUI manually, press `r`, verify the mock transcript appears, press `s`, verify a session directory is created in `~/voice-bird/sessions/` with a `transcript.jsonl` containing the mock segment.

Commit:

```bash
git add -A
git commit -m "feat(ui): two-zone TUI layout with committed + tentative zones"
```

### Task 15: Rewrite `config.rs` for new fields

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write failing tests**

```rust
// src/config.rs
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_when_file_missing() {
        let c = AppConfig::default();
        assert_eq!(c.default_model, "distil-small.en");
        assert_eq!(c.hop_ms, 750);
        assert_eq!(c.engine_prefer, "auto");
    }

    #[test]
    fn roundtrip_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig {
            default_model: "large-v3-turbo".into(),
            language: "auto".into(),
            session_dir: "~/foo".into(),
            hop_ms: 600, min_window_ms: 800,
            engine_prefer: "whisperkit".into(),
            audio_default_source: "system".into(),
        };
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, c);
    }
}
```

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --lib config
```

- [ ] **Step 3: Implement**

Replace `src/config.rs`:

```rust
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_model: String,
    pub language: String,
    pub session_dir: String,
    pub hop_ms: u32,
    pub min_window_ms: u32,
    #[serde(rename = "engine_prefer")]
    pub engine_prefer: String,
    pub audio_default_source: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: "distil-small.en".into(),
            language: "en".into(),
            session_dir: "~/voice-bird/sessions".into(),
            hop_ms: 750,
            min_window_ms: 1000,
            engine_prefer: "auto".into(),
            audio_default_source: "microphone".into(),
        }
    }
}

impl AppConfig {
    pub fn config_path() -> anyhow::Result<PathBuf> {
        let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        Ok(base.join("voice-bird").join("config.toml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path()?;
        if path.exists() { Self::load_from(&path) } else { Ok(Self::default()) }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn session_dir_expanded(&self) -> String {
        if let Some(rest) = self.session_dir.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest).to_string_lossy().into_owned();
            }
        }
        self.session_dir.clone()
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib config
cargo build
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): new TOML schema for local transcription settings"
```

### Task 16: Stage 2 end-to-end integration test

**Files:**
- Create: `tests/e2e_mock_session.rs`

- [ ] **Step 1: Write the test**

```rust
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Builder;

use voice_bird::session::writer::SegmentWriter;
use voice_bird::session::finalize::{finalize, SessionMeta};
use voice_bird::transcription::{
    mock::{MockEngine, MockEvent},
    EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn mock_engine_events_land_in_jsonl_and_finalize() {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");

        let mut engine = MockEngine::new(vec![
            MockEvent::Tentative("he".into()),
            MockEvent::Committed { t_start_ms: 0, t_end_ms: 500,  text: "hello".into() },
            MockEvent::Committed { t_start_ms: 500, t_end_ms: 1100, text: "world".into() },
        ]);

        let handle = engine.start(EngineConfig {
            model_path: "mock".into(),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();

        // drive mock
        for _ in 0..3 { handle.pcm_tx.send(vec![0.0; 16]).await.unwrap(); }

        let mut writer = SegmentWriter::open(&jsonl).unwrap();
        let mut rx = handle.events_rx;
        let mut commits = 0;
        while commits < 2 {
            let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
            if let EngineEvent::Committed(seg) = ev {
                writer.append(&(&seg).into()).unwrap();
                commits += 1;
            }
        }
        drop(writer);

        finalize(
            &jsonl,
            &dir.path().join("transcript.json"),
            &dir.path().join("transcript.txt"),
            &dir.path().join("meta.json"),
            &SessionMeta::default(),
        ).unwrap();

        let txt = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
        assert_eq!(txt, "hello\nworld\n");
    });
}
```

- [ ] **Step 2: Run**

```bash
cargo test --test e2e_mock_session
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/e2e_mock_session.rs
git commit -m "test(e2e): mock engine + writer + finalize pipeline integration"
```

---

## Stage 3 — WhisperRsEngine, models, first-run picker, real audio

### Task 17: Implement model catalog + download with SHA-256 verify

**Files:**
- Modify: `src/transcription/models.rs`
- Modify: `Cargo.toml` — add `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "blocking", "stream"] }`, `sha2 = "0.10"`, `hex = "0.4"`, `futures = "0.3"` (for stream download).

- [ ] **Step 1: Write failing tests**

```rust
// src/transcription/models.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_default_and_required_ids() {
        let catalog = Catalog::builtin();
        let default = catalog.default_id();
        assert_eq!(default, "distil-small.en");
        for id in ["distil-small.en", "distil-large-v3", "large-v3-turbo", "base.en", "tiny.en"] {
            assert!(catalog.get(id).is_some(), "missing {id}");
        }
    }

    #[test]
    fn sha256_verify_detects_mismatch() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello").unwrap();
        let wrong = "0".repeat(64);
        assert!(verify_sha256(tmp.path(), &wrong).is_err());
    }
}
```

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --lib transcription::models
```

- [ ] **Step 3: Implement**

```rust
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: &'static str,
    pub size_mb: u32,
    pub language: &'static str,
    pub gguf_url: &'static str,
    pub gguf_sha256: &'static str,
    pub coreml_url: Option<&'static str>,      // WhisperKit bundle
    pub coreml_sha256: Option<&'static str>,
    pub is_default: bool,
}

pub struct Catalog(Vec<ModelEntry>);

impl Catalog {
    pub fn builtin() -> Self {
        Catalog(vec![
            ModelEntry {
                id: "distil-small.en", size_mb: 250, language: "en",
                gguf_url: "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin",
                gguf_sha256: "<FILL AT PLAN-EXECUTION TIME BY RUNNING sha256sum>",
                coreml_url: Some("https://huggingface.co/distil-whisper/distil-small.en/resolve/main/coreml-distil-small.en.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: true,
            },
            ModelEntry {
                id: "distil-large-v3", size_mb: 1_500, language: "multi",
                gguf_url: "https://huggingface.co/distil-whisper/distil-large-v3/resolve/main/ggml-distil-large-v3.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/distil-whisper/distil-large-v3/resolve/main/coreml-distil-large-v3.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: false,
            },
            ModelEntry {
                id: "large-v3-turbo", size_mb: 1_600, language: "multi",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/argmaxinc/whisperkit-coreml/resolve/main/openai_whisper-large-v3-turbo.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: false,
            },
            ModelEntry {
                id: "base.en", size_mb: 150, language: "en",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None, coreml_sha256: None,
                is_default: false,
            },
            ModelEntry {
                id: "tiny.en", size_mb: 75, language: "en",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None, coreml_sha256: None,
                is_default: false,
            },
        ])
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.0.iter().find(|m| m.id == id)
    }

    pub fn default_id(&self) -> &'static str {
        self.0.iter().find(|m| m.is_default).map(|m| m.id).unwrap_or("distil-small.en")
    }

    pub fn all(&self) -> &[ModelEntry] { &self.0 }
}

pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| anyhow!("no cache dir"))?;
    Ok(base.join("voice-bird").join("models"))
}

pub fn gguf_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(cache_dir()?.join(format!("{id}.gguf")))
}

pub fn verify_sha256(path: &Path, expected_hex: &str) -> anyhow::Result<()> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&data);
    let got = hex::encode(h.finalize());
    if got != expected_hex {
        return Err(anyhow!("sha256 mismatch for {}: got {} expected {}", path.display(), got, expected_hex));
    }
    Ok(())
}

pub fn download_with_verify(
    url: &str,
    dest: &Path,
    expected_sha: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
    let resp = reqwest::blocking::get(url)?.error_for_status()?;
    let total = resp.content_length();
    let mut downloaded = 0u64;
    let mut out = std::fs::File::create(dest)?;
    let mut src = resp;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut src, &mut buf)?;
        if n == 0 { break; }
        std::io::Write::write_all(&mut out, &buf[..n])?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    drop(out);
    if !expected_sha.starts_with("<FILL") {
        verify_sha256(dest, expected_sha)?;
    }
    Ok(())
}
```

Note the `<FILL>` placeholders in the catalog: **before merging Stage 3**, run `sha256sum` against each downloaded file and replace every `<FILL>` with the real hex digest. The `download_with_verify` function short-circuits the verification for `<FILL>`-prefixed values so tests and early development can run, but production builds must have real hashes. Add a CI check in Task 24 that fails if any `<FILL` remains in the catalog.

- [ ] **Step 4: Run tests**

```bash
cargo test --lib transcription::models
```

Expected: PASS (verification test uses a known-bad hash; catalog test just checks IDs).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/transcription/models.rs
git commit -m "feat(models): catalog + SHA-256 verified download with progress"
```

### Task 18: First-run model picker (TUI mode)

**Files:**
- Modify: `src/app.rs` — add `AppMode::ModelPicker` handling and a `PickerState` field.
- Modify: `src/ui.rs` — render the picker as an overlay when in that mode.
- Modify: `src/main.rs` — key handlers for the picker (↑/↓/Enter/Esc).

- [ ] **Step 1: Add picker state**

```rust
// in src/app.rs
pub struct PickerState {
    pub index: usize,
    pub downloading: Option<DownloadProgress>,
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub model_id: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}
```

Add to `App`: `pub picker: Option<PickerState>`. Initialize to `Some(PickerState { index: 0, downloading: None })` if config file does not exist at startup; else `None`.

In `App::new()`, check `AppConfig::config_path()?.exists()`; if not, set `mode = ModelPicker` and initialize `picker`. Otherwise stay in `Normal`.

- [ ] **Step 2: Render the picker**

Add to `src/ui.rs`:

```rust
pub fn render_model_picker(f: &mut Frame, area: Rect, app: &App) {
    let catalog = crate::transcription::models::Catalog::builtin();
    let items: Vec<Line> = catalog.all().iter().enumerate().map(|(i, m)| {
        let marker = if Some(i) == app.picker.as_ref().map(|p| p.index) { "▶ " } else { "  " };
        Line::from(vec![
            Span::raw(marker),
            Span::styled(m.id, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {} MB  {}  ", m.size_mb, m.language)),
            Span::raw(if m.is_default { "(default)" } else { "" }),
        ])
    }).collect();

    let p = Paragraph::new(items).block(
        Block::default().borders(Borders::ALL).title(" Pick a model ")
    );
    f.render_widget(p, area);

    if let Some(dp) = app.picker.as_ref().and_then(|p| p.downloading.as_ref()) {
        let pct = dp.total.map(|t| (dp.bytes * 100 / t.max(1))).unwrap_or(0);
        let msg = format!("Downloading {}: {pct}%", dp.model_id);
        let popup = Paragraph::new(msg).block(Block::default().borders(Borders::ALL));
        let area = centered(60, 3, area);
        f.render_widget(popup, area);
    }
}

fn centered(pct_w: u16, h: u16, parent: Rect) -> Rect {
    let w = parent.width * pct_w / 100;
    Rect {
        x: parent.x + (parent.width - w) / 2,
        y: parent.y + (parent.height - h) / 2,
        width: w,
        height: h,
    }
}
```

In `render`, dispatch:

```rust
match app.mode {
    AppMode::ModelPicker => render_model_picker(f, f.size(), app),
    _ => { /* existing render */ }
}
```

- [ ] **Step 3: Wire keys in `src/main.rs`**

```rust
fn handle_picker_mode(app: &mut App, key: KeyCode) {
    let Some(picker) = app.picker.as_mut() else { return; };
    let catalog = crate::transcription::models::Catalog::builtin();
    match key {
        KeyCode::Up   | KeyCode::Char('k') => { picker.index = picker.index.saturating_sub(1); }
        KeyCode::Down | KeyCode::Char('j') => {
            if picker.index + 1 < catalog.all().len() { picker.index += 1; }
        }
        KeyCode::Enter => {
            let entry = catalog.all()[picker.index].clone();
            app.begin_model_download(&entry);
        }
        KeyCode::Esc => {
            if app.config_was_loaded_from_disk { app.mode = AppMode::Normal; }
            // Otherwise stay in picker — first run requires a model.
        }
        _ => {}
    }
}
```

In `App`, implement `begin_model_download` that spawns a tokio blocking task invoking `download_with_verify`, threading progress into `picker.downloading`. On success, write config with the chosen model id and transition to `Normal`.

- [ ] **Step 4: Build + manual verify**

```bash
cargo build
cargo run
```

Delete `~/Library/Application\ Support/voice-bird/config.toml` first to force first-run. Verify picker appears, arrow keys move, pressing Enter on `tiny.en` downloads it (smallest, fastest for dev) and writes config.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(ui): first-run model picker with download progress"
```

### Task 19: Implement `WhisperRsEngine`

**Files:**
- Modify: `src/transcription/whisper_rs_engine.rs`

- [ ] **Step 1: Write an engine smoke test (feature-gated)**

Add to `Cargo.toml`:

```toml
[features]
engine-smoke = []
```

Create `tests/fixtures/hello_world_16k.wav` — a 2-3 second WAV at 16 kHz mono saying "hello world this is a test". Check this binary into git.

Create `tests/engine_smoke.rs`:

```rust
#![cfg(feature = "engine-smoke")]

use std::time::Duration;
use voice_bird::transcription::{
    whisper_rs_engine::WhisperRsEngine,
    EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn whisper_rs_produces_non_empty_transcript_for_fixture() {
    // Downloads tiny.en on demand. Path is cached; test is slow first time.
    let tiny = voice_bird::transcription::models::Catalog::builtin()
        .get("tiny.en").unwrap().clone();
    let cache = voice_bird::transcription::models::gguf_path("tiny.en").unwrap();
    if !cache.exists() {
        voice_bird::transcription::models::download_with_verify(
            tiny.gguf_url, &cache, tiny.gguf_sha256, &mut |_, _| {},
        ).unwrap();
    }

    let mut spec = hound::WavReader::open("tests/fixtures/hello_world_16k.wav").unwrap();
    let samples: Vec<f32> = spec.samples::<i16>().map(|s| s.unwrap() as f32 / i16::MAX as f32).collect();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut engine = WhisperRsEngine::default();
        let handle = engine.start(EngineConfig {
            model_path: cache,
            language: Some("en".into()),
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();

        // Feed in 500ms chunks
        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(handle.pcm_tx);

        let mut transcript = String::new();
        let mut rx = handle.events_rx;
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            if let EngineEvent::Committed(seg) = evt {
                transcript.push_str(&seg.text);
                transcript.push(' ');
            }
        }
        assert!(transcript.to_lowercase().contains("hello"), "transcript = {:?}", transcript);
    });
}
```

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --test engine_smoke --features engine-smoke
```

Expected: FAIL — `WhisperRsEngine` undefined.

- [ ] **Step 3: Implement**

```rust
// src/transcription/whisper_rs_engine.rs
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{
    local_agreement::{step, AgreementOutput},
    EngineConfig, EngineEvent, EngineHandle, Segment, Token, TranscriptionEngine,
};

#[derive(Default)]
pub struct WhisperRsEngine;

impl TranscriptionEngine for WhisperRsEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let model_path = cfg.model_path.clone();
        let language = cfg.language.clone();
        let hop_ms = cfg.hop_ms as u64;
        let min_window_ms = cfg.min_window_ms as u64;

        std::thread::spawn(move || {
            let ctx = match WhisperContext::new_with_params(
                model_path.to_string_lossy().as_ref(),
                WhisperContextParameters::default(),
            ) {
                Ok(c) => c,
                Err(e) => { let _ = events_tx.send(EngineEvent::Error(format!("load model: {e}"))); return; }
            };
            let _ = events_tx.send(EngineEvent::ModelLoaded { name: model_path.file_name().unwrap_or_default().to_string_lossy().into() });

            let mut state = ctx.create_state().expect("create state");
            let mut buffer: Vec<f32> = Vec::new();
            let mut prev_hypothesis: Vec<Token> = Vec::new();
            let mut committed_upto = Duration::from_millis(0);
            let mut last_run = std::time::Instant::now();

            loop {
                if shutdown_rx.try_recv().is_ok() { break; }

                match pcm_rx.blocking_recv() {
                    Some(chunk) => buffer.extend_from_slice(&chunk),
                    None => break,
                }

                // Cap buffer at 30 s
                let max = (16_000 * 30) as usize;
                if buffer.len() > max {
                    let cut = buffer.len() - max;
                    buffer.drain(..cut);
                    // Shift committed_upto by the cut amount (approximate)
                }

                let buf_ms = (buffer.len() as u64 * 1000) / 16_000;
                if buf_ms < min_window_ms { continue; }
                if last_run.elapsed() < std::time::Duration::from_millis(hop_ms) { continue; }
                last_run = std::time::Instant::now();

                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                params.set_no_context(false);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_special(false);
                params.set_print_timestamps(false);
                params.set_token_timestamps(true);
                if let Some(ref lang) = language { params.set_language(Some(lang.as_str())); }

                if let Err(e) = state.full(params, &buffer) {
                    let _ = events_tx.send(EngineEvent::Error(format!("whisper full: {e}")));
                    continue;
                }

                let n_segments = state.full_n_segments().unwrap_or(0);
                let mut hypothesis: Vec<Token> = Vec::new();
                for i in 0..n_segments {
                    let n_tokens = state.full_n_tokens(i).unwrap_or(0);
                    for t in 0..n_tokens {
                        let txt = state.full_get_token_text(i, t).unwrap_or_default();
                        if txt.starts_with("[_") { continue; }  // whisper special tokens
                        let data = match state.full_get_token_data(i, t) { Ok(d) => d, Err(_) => continue };
                        hypothesis.push(Token {
                            text: txt,
                            t_start_ms: (data.t0 as u64) * 10,  // whisper.cpp t in 10ms units
                            t_end_ms:   (data.t1 as u64) * 10,
                        });
                    }
                }

                let out: AgreementOutput = step(&prev_hypothesis, &hypothesis, committed_upto);
                committed_upto = out.new_committed_upto;
                prev_hypothesis = hypothesis;

                for seg in out.committed_segments {
                    let _ = events_tx.send(EngineEvent::Committed(seg));
                }
                let _ = events_tx.send(EngineEvent::Tentative(out.tentative_text));

                // Trim buffer up to committed_upto - 200ms
                let keep_from_ms = committed_upto.as_millis().saturating_sub(200) as u64;
                let keep_from_samples = ((keep_from_ms * 16_000) / 1000) as usize;
                if keep_from_samples < buffer.len() {
                    buffer.drain(..keep_from_samples);
                    committed_upto = committed_upto.saturating_sub(Duration::from_millis(keep_from_ms));
                    prev_hypothesis.clear();   // reset after trim
                }
            }
        });

        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
```

- [ ] **Step 4: Run smoke test**

```bash
cargo test --test engine_smoke --features engine-smoke -- --nocapture
```

Expected: PASS. This is slow (downloads + inference) — can take 1-2 minutes.

- [ ] **Step 5: Commit**

```bash
git add src/transcription/whisper_rs_engine.rs tests/engine_smoke.rs tests/fixtures/hello_world_16k.wav
git commit -m "feat(engine): WhisperRsEngine with LocalAgreement-2 and buffer trim"
```

### Task 20: Wire real cpal audio capture + resample + WAV tee into `app.rs`

**Files:**
- Modify: `src/app.rs` (`start_recording`)
- Modify: `src/audio/mod.rs` — add a `capture` module that owns cpal stream setup.

- [ ] **Step 1: Create `src/audio/capture.rs`**

```rust
use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

pub struct CaptureHandle {
    pub frames_rx: mpsc::Receiver<Vec<f32>>,
    pub sample_rate: u32,
    pub channels: u16,
    _stream: cpal::Stream,
}

pub fn capture_default_input() -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| anyhow!("no default input"))?;
    let config = device.default_input_config()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let format = config.sample_format();

    let (tx, rx) = mpsc::channel::<Vec<f32>>(64);

    let err_fn = |e| log::error!("cpal stream error: {e}");

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                let _ = tx.blocking_send(data.to_vec());
            },
            err_fn, None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _| {
                let v = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                let _ = tx.blocking_send(v);
            },
            err_fn, None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config.into(),
            move |data: &[u16], _| {
                let v = data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0).collect();
                let _ = tx.blocking_send(v);
            },
            err_fn, None,
        ),
        f => return Err(anyhow!("unsupported sample format: {f:?}")),
    }.context("build input stream")?;

    stream.play()?;

    Ok(CaptureHandle { frames_rx: rx, sample_rate, channels, _stream: stream })
}
```

Add `pub mod capture;` to `src/audio/mod.rs`.

- [ ] **Step 2: Rewire `App::start_recording`**

Replace the mock-driving block in `start_recording` with real audio:

```rust
// Start cpal capture
let mut capture = match crate::audio::capture::capture_default_input() {
    Ok(c) => c,
    Err(e) => { self.status = RecordingStatus::Error(format!("capture: {e}")); return; }
};

// Build resampler
let mut resampler = match crate::audio::resample::Resampler::new(capture.sample_rate, capture.channels) {
    Ok(r) => r,
    Err(e) => { self.status = RecordingStatus::Error(format!("resample: {e}")); return; }
};

// Build engine (WhisperRsEngine; WhisperKit comes in Stage 4)
use crate::transcription::{whisper_rs_engine::WhisperRsEngine, EngineConfig, TranscriptionEngine};
let model_path = crate::transcription::models::gguf_path(&self.config.default_model)
    .unwrap_or_else(|_| "".into());
let mut engine = WhisperRsEngine::default();
let handle = match engine.start(EngineConfig {
    model_path,
    language: Some(self.config.language.clone()).filter(|s| s != "auto"),
    sample_rate: 16_000,
    hop_ms: self.config.hop_ms,
    min_window_ms: self.config.min_window_ms,
}) {
    Ok(h) => h,
    Err(e) => { self.status = RecordingStatus::Error(format!("engine: {e}")); return; }
};

// WAV writer
let wav_path = session_dir.join("audio.wav");
let spec = hound::WavSpec {
    channels: 1, sample_rate: 16_000,
    bits_per_sample: 32, sample_format: hound::SampleFormat::Float,
};
let mut wav = hound::WavWriter::create(&wav_path, spec).expect("create wav");

let pcm_tx = handle.pcm_tx.clone();

// Producer task: cpal → resample → tee(WAV + engine)
let producer = self.rt.spawn(async move {
    while let Some(frames) = capture.frames_rx.recv().await {
        match resampler.process(&frames) {
            Ok(out) => {
                for s in &out { let _ = wav.write_sample(*s); }
                if pcm_tx.send(out).await.is_err() { break; }
            }
            Err(e) => { log::error!("resample: {e}"); break; }
        }
    }
    let _ = wav.finalize();
});

// Consumer task (same as before, but pulls from the real handle)
// ... (the event-loop tokio::spawn from Task 14/Step 2 remains, now receiving
// events from WhisperRsEngine instead of MockEngine)
```

Store `producer` on `RecordingRuntime` so stop can await it.

- [ ] **Step 3: Build + manual smoke**

```bash
cargo build
cargo run
```

In the TUI, press `r`, speak a few sentences, press `s`. Verify:
- `audio.wav` exists and plays back your recording.
- `transcript.jsonl` has one line per committed segment.
- `transcript.json` + `transcript.txt` + `meta.json` are written on stop.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(app): real cpal capture + resample + WAV tee wired to WhisperRsEngine"
```

### Task 21: Crash-recovery integration test

**Files:**
- Create: `tests/crash_recovery.rs`

- [ ] **Step 1: Write the test**

```rust
use tempfile::TempDir;
use voice_bird::session::recover;
use voice_bird::session::writer::{SegmentWriter, WrittenSegment};

#[test]
fn partial_jsonl_plus_recover_produces_valid_json_and_txt() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("transcript.jsonl");
    {
        let mut w = SegmentWriter::open(&jsonl).unwrap();
        w.append(&WrittenSegment { t_start_ms: 0,    t_end_ms: 500,  text: "partial".into() }).unwrap();
        w.append(&WrittenSegment { t_start_ms: 500, t_end_ms: 1000, text: "transcript".into() }).unwrap();
        // Writer dropped without finalize — simulates crash mid-session.
    }

    recover::recover(dir.path()).unwrap();

    let j: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("transcript.json")).unwrap()
    ).unwrap();
    assert_eq!(j["segments"].as_array().unwrap().len(), 2);
    let t = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
    assert_eq!(t, "partial\ntranscript\n");
}
```

- [ ] **Step 2: Run**

```bash
cargo test --test crash_recovery
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/crash_recovery.rs
git commit -m "test: crash-recovery regenerates finalized transcript from partial JSONL"
```

---

## Stage 4 — WhisperKit Swift sidecar

### Task 22: Create the Swift package skeleton + JSONL protocol

**Files:**
- Create: `whisperkit-helper/Package.swift`
- Create: `whisperkit-helper/Sources/VoiceBirdWhisperKit/main.swift`
- Create: `whisperkit-helper/Tests/VoiceBirdWhisperKitTests/ProtocolTests.swift`

- [ ] **Step 1: `Package.swift`**

```swift
// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "VoiceBirdWhisperKit",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "voice-bird-whisperkit", targets: ["VoiceBirdWhisperKit"]),
    ],
    dependencies: [
        .package(url: "https://github.com/argmaxinc/WhisperKit.git", from: "0.9.0"),
    ],
    targets: [
        .executableTarget(
            name: "VoiceBirdWhisperKit",
            dependencies: [.product(name: "WhisperKit", package: "WhisperKit")]
        ),
        .testTarget(
            name: "VoiceBirdWhisperKitTests",
            dependencies: ["VoiceBirdWhisperKit"]
        ),
    ]
)
```

- [ ] **Step 2: Minimal `main.swift` — protocol only, no WhisperKit call yet**

```swift
import Foundation

// Protocol:
// - stdin: 4-byte LE length + N float32 samples (16kHz mono)
// - stdout: line-delimited JSON events
//     {"type":"ready","model":"<name>"}
//     {"type":"committed","t0":<sec>,"t1":<sec>,"text":"...","tokens":[...]}
//     {"type":"tentative","text":"..."}
//     {"type":"error","message":"..."}

struct OutEvent: Encodable {
    let type: String
    let model: String?
    let t0: Double?
    let t1: Double?
    let text: String?
    let message: String?
}

func emit(_ e: OutEvent) {
    let data = try! JSONEncoder().encode(e)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}

// Stub: emit ready, echo each received buffer length as a tentative event,
// flush committed on EOF. Real WhisperKit wiring in Task 23.
emit(OutEvent(type: "ready", model: "stub", t0: nil, t1: nil, text: nil, message: nil))

let stdin = FileHandle.standardInput
while true {
    guard let header = try? stdin.read(upToCount: 4), header.count == 4 else { break }
    let n = header.withUnsafeBytes { $0.load(as: UInt32.self).littleEndian }
    guard let body = try? stdin.read(upToCount: Int(n * 4)), body.count == Int(n * 4) else { break }
    emit(OutEvent(type: "tentative", model: nil, t0: nil, t1: nil, text: "(received \(n) samples)", message: nil))
}
emit(OutEvent(type: "committed", model: nil, t0: 0, t1: 0, text: "", message: nil))
```

- [ ] **Step 3: Protocol-shape test (no WhisperKit required yet)**

```swift
// Tests/VoiceBirdWhisperKitTests/ProtocolTests.swift
import XCTest
@testable import VoiceBirdWhisperKit

final class ProtocolTests: XCTestCase {
    func testOutEventEncodes() throws {
        let e = OutEvent(type: "ready", model: "tiny.en", t0: nil, t1: nil, text: nil, message: nil)
        let data = try JSONEncoder().encode(e)
        let s = String(data: data, encoding: .utf8)!
        XCTAssertTrue(s.contains("\"type\":\"ready\""))
        XCTAssertTrue(s.contains("\"model\":\"tiny.en\""))
    }
}
```

- [ ] **Step 4: Build + test (macOS only)**

```bash
cd whisperkit-helper
swift build
swift test
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd ..
git add whisperkit-helper/
git commit -m "feat(whisperkit): Swift package skeleton + JSONL protocol stub"
```

### Task 23: Wire WhisperKit into the sidecar

**Files:**
- Modify: `whisperkit-helper/Sources/VoiceBirdWhisperKit/main.swift`

- [ ] **Step 1: Replace the stub body with real WhisperKit streaming**

```swift
import Foundation
import WhisperKit
import AVFoundation

struct OutEvent: Encodable {
    let type: String
    let model: String?
    let t0: Double?
    let t1: Double?
    let text: String?
    let message: String?
}

func emit(_ e: OutEvent) {
    let data = try! JSONEncoder().encode(e)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}

@main
struct Main {
    static func main() async {
        // Parse first line (handshake): JSON with model id.
        guard let line = readLine(strippingNewline: true),
              let data = line.data(using: .utf8),
              let handshake = try? JSONDecoder().decode([String: String].self, from: data),
              let model = handshake["model"] else {
            emit(OutEvent(type: "error", model: nil, t0: nil, t1: nil, text: nil, message: "missing handshake"))
            return
        }

        do {
            let whisperKit = try await WhisperKit(model: model)
            emit(OutEvent(type: "ready", model: model, t0: nil, t1: nil, text: nil, message: nil))

            // Accumulate samples, run decode on cadence
            var accum = [Float]()
            let stdin = FileHandle.standardInput
            var lastDecode = Date()
            let hopSeconds = 0.75

            while true {
                guard let header = try stdin.read(upToCount: 4), header.count == 4 else { break }
                let n = header.withUnsafeBytes { $0.load(as: UInt32.self).littleEndian }
                guard let body = try stdin.read(upToCount: Int(n) * 4), body.count == Int(n) * 4 else { break }
                let samples: [Float] = body.withUnsafeBytes { ptr in
                    Array(ptr.bindMemory(to: Float.self))
                }
                accum.append(contentsOf: samples)

                if Date().timeIntervalSince(lastDecode) >= hopSeconds, accum.count >= 16_000 {
                    lastDecode = Date()
                    let result = try await whisperKit.transcribe(audioArray: accum)
                    // WhisperKit returns TranscriptionResult with segments; WhisperKit's
                    // streaming mode already performs AlignAtt-like commitment — emit
                    // its committed segments and treat the tail as tentative.
                    for seg in result?.segments ?? [] {
                        emit(OutEvent(
                            type: "committed",
                            model: nil,
                            t0: Double(seg.start),
                            t1: Double(seg.end),
                            text: seg.text,
                            message: nil
                        ))
                    }
                    if let tail = result?.text {
                        emit(OutEvent(type: "tentative", model: nil, t0: nil, t1: nil, text: tail, message: nil))
                    }
                }
            }
        } catch {
            emit(OutEvent(type: "error", model: nil, t0: nil, t1: nil, text: nil, message: "\(error)"))
        }
    }
}
```

Note: the exact WhisperKit streaming API is version-dependent — verify against the locked version at implementation time. If the API has changed, adapt the call while preserving the stdout event shape.

- [ ] **Step 2: Build (macOS only)**

```bash
cd whisperkit-helper
swift build -c release
```

Expected: binary at `.build/release/voice-bird-whisperkit`. If the WhisperKit API diverges from what's above, fix here before moving on.

- [ ] **Step 3: Manual smoke**

```bash
# Feed a 2s 16kHz mono raw-float WAV as samples
cat <(printf '{"model":"tiny.en"}\n') <(some fixture generator) | ./.build/release/voice-bird-whisperkit
```

Expected: `{"type":"ready",...}` appears on stdout, then a `committed` line.

- [ ] **Step 4: Commit**

```bash
cd ..
git add whisperkit-helper/
git commit -m "feat(whisperkit): decode audio via WhisperKit, emit committed/tentative events"
```

### Task 24: Implement `WhisperKitEngine` (Rust client)

**Files:**
- Modify: `src/transcription/whisper_kit_engine.rs`

- [ ] **Step 1: Write failing integration test (macOS-gated)**

```rust
// tests/whisperkit_engine.rs
#![cfg(all(target_os = "macos", feature = "engine-smoke"))]

// Tests that the Rust side can spawn the Swift sidecar binary, send a
// handshake + one PCM frame, and receive a parseable "ready" event.

use std::time::Duration;
use voice_bird::transcription::{
    whisper_kit_engine::WhisperKitEngine,
    EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn sidecar_starts_and_emits_ready() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut engine = WhisperKitEngine::new("whisperkit-helper/.build/release/voice-bird-whisperkit".into());
        let handle = engine.start(EngineConfig {
            model_path: "tiny.en".into(), // WhisperKit takes a name not a path
            language: Some("en".into()),
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();

        let mut rx = handle.events_rx;
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(got, EngineEvent::ModelLoaded { .. }));
    });
}
```

- [ ] **Step 2: Implement the client**

```rust
// src/transcription/whisper_kit_engine.rs
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, TranscriptionEngine, Token};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum SidecarEvent {
    #[serde(rename = "ready")]    Ready { model: Option<String> },
    #[serde(rename = "committed")] Committed { t0: f64, t1: f64, text: String },
    #[serde(rename = "tentative")] Tentative { text: String },
    #[serde(rename = "error")]     Error { message: String },
}

pub struct WhisperKitEngine {
    sidecar_path: PathBuf,
}

impl WhisperKitEngine {
    pub fn new(sidecar_path: PathBuf) -> Self { Self { sidecar_path } }
}

impl TranscriptionEngine for WhisperKitEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let mut child = Command::new(&self.sidecar_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let handshake = serde_json::json!({
            "model": cfg.model_path.to_string_lossy(),
            "language": cfg.language,
        });
        tokio::spawn(async move {
            let _ = stdin.write_all(handshake.to_string().as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
        });

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // Producer: pcm → sidecar stdin
        tokio::spawn(async move {
            let mut stdin = child.stdin.take().expect("stdin2");
            while let Some(samples) = pcm_rx.recv().await {
                let n = samples.len() as u32;
                let _ = stdin.write_all(&n.to_le_bytes()).await;
                let bytes: &[u8] = bytemuck::cast_slice(&samples);
                let _ = stdin.write_all(bytes).await;
            }
        });

        // Consumer: sidecar stdout → EngineEvent
        let events_tx2 = events_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    line = reader.next_line() => match line {
                        Ok(Some(l)) => {
                            match serde_json::from_str::<SidecarEvent>(&l) {
                                Ok(SidecarEvent::Ready { model }) => {
                                    let _ = events_tx2.send(EngineEvent::ModelLoaded {
                                        name: model.unwrap_or_else(|| "whisperkit".into())
                                    });
                                }
                                Ok(SidecarEvent::Committed { t0, t1, text }) => {
                                    let seg = Segment {
                                        t_start: Duration::from_secs_f64(t0),
                                        t_end:   Duration::from_secs_f64(t1),
                                        text, tokens: Vec::new(),
                                    };
                                    let _ = events_tx2.send(EngineEvent::Committed(seg));
                                }
                                Ok(SidecarEvent::Tentative { text }) => {
                                    let _ = events_tx2.send(EngineEvent::Tentative(text));
                                }
                                Ok(SidecarEvent::Error { message }) => {
                                    let _ = events_tx2.send(EngineEvent::Error(message));
                                }
                                Err(e) => log::warn!("sidecar parse: {e}; line = {l}"),
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        });

        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
```

Add `bytemuck = "1"` to `Cargo.toml`.

- [ ] **Step 3: Run test (macOS only)**

```bash
cargo test --test whisperkit_engine --features engine-smoke -- --nocapture
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(engine): WhisperKitEngine — spawns Swift sidecar over stdio JSONL"
```

### Task 25: Engine selection + fallback banner

**Files:**
- Modify: `src/app.rs`
- Modify: `src/transcription/mod.rs` (add a factory function)

- [ ] **Step 1: Add `select_engine(...) -> Box<dyn TranscriptionEngine>`**

```rust
// src/transcription/mod.rs
pub fn select_engine(prefer: &str, sidecar_path: Option<&std::path::Path>)
    -> Box<dyn TranscriptionEngine>
{
    #[cfg(target_os = "macos")]
    {
        if prefer == "whisperkit" || prefer == "auto" {
            if let Some(path) = sidecar_path {
                if path.exists() {
                    return Box::new(whisper_kit_engine::WhisperKitEngine::new(path.to_path_buf()));
                }
            }
        }
    }
    Box::new(whisper_rs_engine::WhisperRsEngine::default())
}
```

Find the sidecar:

```rust
pub fn sidecar_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // .app bundle
    let bundle = dir.join("../Resources/voice-bird-whisperkit");
    if bundle.exists() { return Some(bundle); }
    // next to binary
    let sibling = dir.join("voice-bird-whisperkit");
    if sibling.exists() { return Some(sibling); }
    None
}
```

- [ ] **Step 2: Use `select_engine` in `App::start_recording`**

Replace the direct `WhisperRsEngine::default()` construction:

```rust
let sidecar = crate::transcription::sidecar_path();
let sidecar_ref = sidecar.as_deref();
let engine_kind_used = match (sidecar_ref, self.config.engine_prefer.as_str()) {
    (Some(_), "auto") | (Some(_), "whisperkit") if cfg!(target_os = "macos") => "whisperkit",
    _ => "whisper_rs",
};
let mut engine = crate::transcription::select_engine(&self.config.engine_prefer, sidecar_ref);
```

Store `engine_kind_used` on `App` so the header can render it and `meta.json` can record it.

- [ ] **Step 3: Fallback banner**

On `EngineEvent::Error` from the WhisperKit path, set a transient banner on `App`:

```rust
pub banner: Option<String>,  // Option<(String, Instant)> with TTL — kept simple
```

In the consumer task, on `EngineEvent::Error(e)`, push a banner "WhisperKit crashed — continuing with whisper-rs" and restart the engine with `WhisperRsEngine::default()`. This is best-effort recovery; if the second engine also fails, stop the session.

- [ ] **Step 4: Build + manual smoke on macOS**

```bash
cargo build
# with sidecar built earlier
cp whisperkit-helper/.build/release/voice-bird-whisperkit target/debug/
cargo run
```

Verify header shows `engine: whisperkit`. Kill the sidecar PID externally (`pkill voice-bird-whisperkit`) while recording; verify banner appears and recording continues.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(app): engine selection with WhisperKit→whisper-rs fallback"
```

### Task 26: xtask for sidecar build + packaging

**Files:**
- Create: `xtask/Cargo.toml`
- Create: `xtask/src/main.rs`
- Modify: root `Cargo.toml` workspace members to include `"xtask"`.

- [ ] **Step 1: Skeleton xtask**

```toml
# xtask/Cargo.toml
[package]
name = "xtask"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "xtask"
path = "src/main.rs"

[dependencies]
anyhow = "1"
```

```rust
// xtask/src/main.rs
fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "build-sidecar" => build_sidecar(),
        "check-catalog" => check_catalog(),
        _ => { eprintln!("usage: xtask {{build-sidecar|check-catalog}}"); std::process::exit(1); }
    }
}

fn build_sidecar() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("swift")
            .arg("build").arg("-c").arg("release")
            .current_dir("whisperkit-helper")
            .status()?;
        anyhow::ensure!(status.success(), "swift build failed");
        std::fs::copy(
            "whisperkit-helper/.build/release/voice-bird-whisperkit",
            "target/release/voice-bird-whisperkit",
        )?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("build-sidecar is macOS-only; skipping.");
    }
    Ok(())
}

fn check_catalog() -> anyhow::Result<()> {
    let src = std::fs::read_to_string("src/transcription/models.rs")?;
    if src.contains("<FILL") {
        anyhow::bail!("models catalog still has <FILL> placeholders; replace with real SHA-256 digests");
    }
    Ok(())
}
```

Update root `Cargo.toml`:

```toml
[workspace]
members = [".", "xtask"]
```

- [ ] **Step 2: Run**

```bash
cargo run -p xtask -- check-catalog
```

At this point this will fail (catalog has `<FILL>`). Download each model, compute `sha256sum`, update `src/transcription/models.rs`, re-run until it passes.

```bash
cargo run -p xtask -- build-sidecar  # macOS
```

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(xtask): build-sidecar + check-catalog helpers"
```

---

## Stage 5 — Docs, cleanup

### Task 27: Update `README.md` and `CLAUDE.md`

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Rewrite `README.md`**

Replace entirely:

```markdown
# Voice Bird

Terminal-based voice transcription. Runs fully locally — your audio never leaves your machine.

## Install

### macOS

```
cargo install voice-bird
# then build the WhisperKit sidecar:
cargo run -p xtask -- build-sidecar
```

### Windows / Linux

```
cargo install voice-bird
```

## Usage

```
voice-bird            # start the TUI
voice-bird --recover <session-dir>   # rebuild finalized transcripts after a crash
```

On first launch, pick a model. `distil-small.en` (~250 MB) is the default.

Recordings are stored under `~/voice-bird/sessions/`, one directory per recording:

- `audio.wav` — 16 kHz mono
- `transcript.jsonl` — append-only, crash-safe
- `transcript.json` + `transcript.txt` — finalized on stop
- `meta.json` — device, model, engine, duration

## Keys

| Key | Action |
|-----|--------|
| `r` | start recording |
| `s` | stop |
| `m` | change model |
| `q` | quit |
| `?` | help |

## License

Proprietary.
```

- [ ] **Step 2: Rewrite `CLAUDE.md`**

Update so the Architecture section describes the new CLI-only product (see spec for exact structure), dependencies list reflects new `Cargo.toml`, and the "Known Gotchas" list drops the Tauri WASAPI ones and adds:
- WhisperKit sidecar must be built separately on macOS; use `cargo run -p xtask -- build-sidecar`.
- Model catalog has SHA-256 digests; CI fails if any are still `<FILL>`.
- Transcript JSONL is fsync'd per segment; don't optimize that away.

- [ ] **Step 3: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs: rewrite README + CLAUDE.md for local-transcription CLI"
```

### Task 28: Final smoke + merge

**Files:** none (just verification)

- [ ] **Step 1: Run the full test suite**

```bash
cargo test
cargo test --features engine-smoke -- --nocapture
cargo run -p xtask -- check-catalog
```

Expected: all green. No `<FILL>` remaining.

- [ ] **Step 2: Platform matrix manual smoke**

Run the end-to-end scenario on each platform dev machine:

- [ ] macOS: mic + system loopback + sidecar kill + recover
- [ ] Windows: mic + WASAPI loopback + recover
- [ ] Linux: mic only + recover

For each: record a ~30s clip, stop, inspect `~/voice-bird/sessions/<SLUG>/`, play back `audio.wav`, verify `transcript.txt` is reasonable.

- [ ] **Step 3: Open PR back to `main`**

```bash
git push -u origin feat/local-whisper
gh pr create \
  --title "feat: Project A — local Whisper transcription in the CLI" \
  --body "See docs/superpowers/specs/2026-04-16-desktop-local-whisper-design.md and docs/superpowers/plans/2026-04-16-desktop-local-whisper.md."
```

- [ ] **Step 4: Verify CI green, request review, merge**

---

## Self-Review

**Spec coverage checked against `docs/superpowers/specs/2026-04-16-desktop-local-whisper-design.md`:**

| Spec Section                         | Plan Coverage                             |
|--------------------------------------|-------------------------------------------|
| Goals 1 (local transcription)        | Tasks 19, 20, 24                          |
| Goal 2 (TUI committed/tentative)     | Task 14 Step 3                            |
| Goal 3 (session directory)           | Tasks 5, 6, 7, 13                         |
| Goal 4 (engine selection)            | Tasks 19, 24, 25                          |
| Goal 5 (model catalog + picker)      | Tasks 17, 18                              |
| Goal 6 (Tauri deletion, repo pivot)  | Tasks 2, 3, 27                            |
| Non-goals                            | Nothing in this plan touches them.        |
| Architecture diagram                 | Tasks 14, 20                              |
| Crate layout                         | Tasks 4, 26                               |
| Deleted files list                   | Task 2                                    |
| TranscriptionEngine trait contract   | Task 8                                    |
| WhisperRsEngine                      | Task 19                                   |
| WhisperKitEngine                     | Tasks 22, 23, 24                          |
| LocalAgreement-2                     | Tasks 10, 11                              |
| Model catalog                        | Task 17                                   |
| First-run picker                     | Task 18                                   |
| Session layout                       | Task 5                                    |
| Incremental writes + recovery        | Tasks 6, 13, 21                           |
| TUI layout                           | Task 14 Step 3                            |
| Keys                                 | Task 14 Step 3, Task 18 Step 3            |
| Config TOML schema                   | Task 15                                   |
| Error handling matrix                | Tasks 14, 25 (engine fallback, banner)    |
| Testing plan (unit, integration, smoke, sidecar contract, manual) | Tasks 5–21, 22, 28 |
| Migration rollout stages             | Plan stages 1–5 mirror spec rollout 1–5; spec stage 6 is split into Tasks 27+28 |

**Placeholder scan:** The only deliberate placeholders are the `<FILL>` SHA-256 values in `src/transcription/models.rs`. The plan explicitly requires replacing them and adds `cargo run -p xtask -- check-catalog` as a CI gate (Task 26) and manual gate (Task 28). No TBDs, TODOs, or "similar to Task N" references elsewhere.

**Type consistency:**

- `EngineConfig`, `EngineEvent`, `EngineHandle`, `Segment`, `Token`, `TranscriptionEngine` declared in Task 8; used unchanged in Tasks 9, 10, 14, 19, 24, 25.
- `WrittenSegment` declared in Task 6; `Segment → WrittenSegment` conversion added in Task 8; used in Tasks 7, 13, 14, 16, 19, 21.
- `SessionMeta` declared in Task 7; used in Tasks 13, 14, 16, 21.
- `SessionSource` declared in Task 5; used in Tasks 14, 15.
- `AppConfig` rewritten in Task 15; its `default_model`, `hop_ms`, `min_window_ms`, `engine_prefer`, `session_dir_expanded` are referenced consistently in Tasks 14, 18, 20, 25.
- `AppMode::{Normal, ModelPicker, Help}` introduced in Task 14, extended/used in Task 18.

Names and signatures check out across tasks.
