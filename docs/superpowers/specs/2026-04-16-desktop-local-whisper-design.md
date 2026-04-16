# Desktop Local Whisper — Design

**Date:** 2026-04-16
**Status:** Draft — pending user review
**Scope:** Project A of a three-part rework of `voice_bird` + `voice_bird_desktop`.

## Context

User feedback on the existing product is that transcription does not need to happen on the server. Today the Rust desktop client streams audio over a WebSocket to the Next.js server (`voice_bird`), which forwards it to AssemblyAI. This project moves transcription onto the user's machine, streams the results live into the TUI, and deletes the server-streaming path from the desktop codebase.

Projects B (Wasabi upload of finished sessions) and C (pivot of the web app to a Fireflies-style chat-with-data experience) are deferred; their dependencies on Project A are limited to the session-artifact shape defined here.

## Goals

1. Transcription runs fully locally on the user's machine — audio never leaves the device in Project A.
2. Live transcript streams into the TUI with a committed / tentative rendering model; target end-to-end latency ~3s on `distil-small.en`.
3. Every session produces a self-contained directory under `~/voice-bird/sessions/` containing the audio, an append-only JSONL log, a finalized JSON transcript, a plain-text transcript, and metadata.
4. macOS builds transcribe via a bundled WhisperKit Swift sidecar (ANE-accelerated). Windows and Linux use `whisper-rs` (whisper.cpp bindings). `whisper-rs` is also the fallback on macOS if the sidecar fails to start.
5. A model catalog is presented on first run; the user selects one (default `distil-small.en`) and it is remembered in config.
6. The Tauri desktop app at the repo root and the desktop-side server-streaming module are deleted. The repo becomes a Rust CLI product; `voice-bird-cli/` is promoted to the repo root.

## Non-goals

- Uploading anything to Wasabi or cloud storage (Project B).
- Any change to the `voice_bird` Next.js web app (Project C).
- Speaker diarization, language auto-detection, translation.
- Billing, licensing, or tier-logic changes.
- Cross-platform GUI. The product is a TUI.

## Architecture

### Top-level data flow for a single recording session

```
cpal / WASAPI / CoreAudio callback
     │  f32 PCM frames (device-native sample rate)
     ▼
┌───────────────────────┐
│ audio::Ring + resample │  lock-free ring, resampled to 16 kHz mono
└──────────┬────────────┘
           │
           ├──► hound WavWriter      → audio.wav    (tee, lossless on the 16 kHz stream)
           │
           ▼
┌───────────────────────────┐
│ TranscriptionEngine       │   trait, two implementations
│  ├─ WhisperRsEngine       │     runs LocalAgreement-2 inline
│  └─ WhisperKitEngine      │     spawns Swift sidecar, passes through its AlignAtt output
└──────────────┬────────────┘
               │ EngineEvent { Committed(Segment) | Tentative(String) | ModelLoaded | Error }
               ▼
         tokio::broadcast
            │       │
            │       └──► session::Writer  (append transcript.jsonl per Committed)
            ▼
     tui::Renderer (ratatui)
        • header: device, app, model, engine, level bar, timer
        • scrollback: committed segments with timestamps
        • footer: single-line tentative tail (dim italic)
```

Audio is resampled once upstream; engines receive 16 kHz mono f32 and never see raw device frames. The WAV file is tee'd from the same resampled stream as the engine so that WAV timestamps align 1:1 with transcript timestamps.

Events flow over a `tokio::broadcast` channel so the writer and the renderer consume independently. Neither can apply backpressure to the other; if a consumer is slow, it drops old events and logs a warning.

### Crate layout

```
voice_bird_desktop/            ← repo root, promoted from voice-bird-cli/
├── Cargo.toml                 ← [workspace] members = [".", "xtask"]
├── src/
│   ├── main.rs                ← arg parse, dispatch
│   ├── app.rs                 ← top-level event loop, wires audio/engine/writer/tui
│   ├── tui/
│   │   ├── mod.rs
│   │   ├── layout.rs
│   │   ├── render.rs
│   │   └── input.rs
│   ├── audio/
│   │   ├── mod.rs             ← cpal input, level meter
│   │   ├── loopback.rs        ← WASAPI (Windows), CoreAudio process tap (macOS)
│   │   └── resample.rs        ← rubato-based f32 mono 16 kHz
│   ├── transcription/
│   │   ├── mod.rs             ← TranscriptionEngine trait, EngineEvent, EngineConfig
│   │   ├── whisper_rs_engine.rs
│   │   ├── whisper_kit_engine.rs
│   │   ├── local_agreement.rs ← pure function, unit-tested
│   │   └── models.rs          ← catalog, download, SHA verification, cache layout
│   ├── session/
│   │   ├── mod.rs
│   │   ├── layout.rs          ← slug / path derivation
│   │   ├── writer.rs          ← append-only jsonl, level-0 fsync per segment
│   │   └── finalize.rs        ← jsonl + WAV duration → transcript.json / .txt
│   ├── config.rs              ← TOML at ~/.config/voice-bird/config.toml
│   ├── logger.rs
│   └── platform/
│       ├── mod.rs
│       ├── macos.rs
│       ├── windows.rs
│       └── linux.rs
├── whisperkit-helper/         ← Swift package, macOS build only
│   ├── Package.swift
│   └── Sources/VoiceBirdWhisperKit/
│       └── main.swift         ← stdin PCM → stdout JSONL protocol
├── xtask/                     ← build helpers: sign sidecar, bundle catalog
├── scripts/
└── docs/
```

### Files and directories deleted

From the current repo root (Tauri remnants):

- `src/main.rs` (old Tauri entry), `src/main_old.rs`, `src/commands.rs`, `src/opus_encoder.rs`, `src/server_streaming.rs`, `src/session.rs` (old), `src/state.rs`, `src/wasapi_sessions.rs` (functionality moves into `audio/loopback.rs`), `src/events.rs`, `src/audio_buffer.rs`, `src/audio_converter.rs`
- `ui/`, `gen/`, `icons/`, `dist/`, `npm/`
- `tauri.conf.json`, the Tauri parts of `build.rs`
- `python/`, `pyproject.toml`
- `voice-bird-cli-crate/`

`voice-bird-cli/` is moved to the repo root. Existing root Tauri files are deleted first to avoid conflicts. History is preserved via `git mv` where practical.

## Transcription engines

### Trait contract

```rust
pub trait TranscriptionEngine: Send {
    fn start(&mut self, cfg: EngineConfig) -> Result<EngineHandle>;
}

pub struct EngineConfig {
    pub model: ModelRef,           // catalog id or absolute path
    pub language: Option<String>,  // None = model default; "en" for .en models
    pub sample_rate: u32,          // always 16_000
    pub hop_ms: u32,               // re-run cadence (WhisperRsEngine only)
    pub min_window_ms: u32,        // minimum audio before first hypothesis
}

pub struct EngineHandle {
    pub pcm_tx: mpsc::Sender<Vec<f32>>,        // 16 kHz mono, ~500 ms chunks
    pub events_rx: broadcast::Receiver<EngineEvent>,
    pub shutdown: oneshot::Sender<()>,
}

pub enum EngineEvent {
    Committed(Segment),                        // final, never revised
    Tentative(String),                         // replaces current tentative line
    ModelLoaded { name: String },
    Error(EngineError),
}

pub struct Segment {
    pub t_start: Duration,
    pub t_end: Duration,
    pub text: String,
    pub tokens: Vec<Token>,
}
```

### WhisperRsEngine (Windows, Linux, macOS fallback)

- Owns a `whisper_rs::WhisperContext` built from a catalog-provided GGUF file.
- Spawns a dedicated OS thread (not tokio) — whisper.cpp inference is blocking and CPU-bound.
- Maintains a rolling buffer of up to 30 s of 16 kHz mono audio.
- Every `hop_ms` (default 750 ms), once at least `min_window_ms` (default 1000 ms) is buffered, runs inference on the whole buffer and produces a hypothesis (ordered tokens with timestamps).
- Feeds each hypothesis into `local_agreement::step` along with the previous hypothesis; commits tokens that agreed, keeps the rest as tentative.
- When the committed prefix advances, trims the buffer to `committed_upto − 200 ms` (overlap) to bound compute.
- Enables `whisper.cpp` Metal backend on macOS, CUDA on Linux/Windows when available, CPU otherwise — auto-detected at context init.

### WhisperKitEngine (macOS default)

- Spawns the `whisperkit-helper` sidecar from `Contents/Resources/` (when distributed as a `.app`) or from a location adjacent to the main binary (raw CLI install).
- Protocol on stdin: a JSON handshake line, then length-prefixed binary PCM frames.
- Protocol on stdout: line-delimited JSON events:
  ```json
  {"type":"ready","model":"distil-small.en"}
  {"type":"committed","t0":1.23,"t1":1.78,"text":"hello world","tokens":[...]}
  {"type":"tentative","text":"and then"}
  {"type":"error","message":"..."}
  ```
- The sidecar uses WhisperKit's built-in streaming (AlignAtt). We trust its commit policy and forward `committed` / `tentative` lines unchanged into `EngineEvent`.
- stderr is captured and logged at `warn` level.
- If the sidecar fails to start, the engine returns an error from `start()`; the app falls back to `WhisperRsEngine` transparently and shows a header banner.
- If the sidecar dies mid-session, the engine emits `Error`, the app finalizes the session with what was written so far, and the header banner shows "engine crashed, transcript may be truncated".

### Why two commit policies

WhisperKit ships a production-grade AlignAtt-based streamer. Reimplementing LocalAgreement on top of its output would regress quality. The trait contract shields the rest of the app from this asymmetry: both engines emit the same `Committed` / `Tentative` event stream.

### LocalAgreement-2

Based on the algorithm from [Turning Whisper into Real-Time Transcription System](https://arxiv.org/abs/2307.14743) (Macháček et al., 2023), implemented as a pure function:

```rust
pub fn step(
    prev: &[Token],
    curr: &[Token],
    committed_upto: Duration,
) -> AgreementOutput {
    // 1. Find the longest common prefix of `prev` and `curr` where
    //    a) normalized text matches (lowercase, punctuation stripped), and
    //    b) timestamps differ by ≤ 300 ms.
    // 2. Everything in that prefix whose end_time > committed_upto becomes newly
    //    Committed (grouped into sentence-level Segments on period / question
    //    mark / 500 ms gap).
    // 3. Everything in `curr` after the committed prefix is the Tentative tail.
}
```

Sentence grouping produces segments suitable for the committed scrollback; raw tokens remain on the `Segment` for later consumers (search indexing in Project C).

## Model management

### Catalog

| id                  | size         | language | default | notes                                   |
|---------------------|--------------|----------|---------|-----------------------------------------|
| `distil-small.en`   | ~250 MB      | English  | ✅       | fastest; v1 default                      |
| `distil-large-v3`   | ~1.5 GB      | multi    |         | higher accuracy, still streaming-friendly |
| `large-v3-turbo`    | ~1.6 GB / 0.6 GB on WhisperKit | multi | | best quality             |
| `base.en`           | ~150 MB      | English  |         | fallback if distil unavailable           |
| `tiny.en`           | ~75 MB       | English  |         | ultra-low-spec                           |

Each catalog entry carries two URLs: a GGUF for `whisper-rs` and a CoreML bundle for `WhisperKit`. The cross-platform builds fetch only the GGUF; macOS fetches both.

### Cache locations

- macOS: `~/Library/Caches/voice-bird/models/`
- Windows: `%LOCALAPPDATA%\voice-bird\models\`
- Linux: `~/.cache/voice-bird/models/`

Files are SHA-256 verified against the catalog. Partial downloads are resumable. If verification fails, the file is deleted and re-fetched.

### First-run experience

- No `config.toml` present triggers the model picker in the TUI.
- Picker shows each model's size, language, and a one-line accuracy/speed hint; `distil-small.en` is preselected.
- Choice is written to config and the download runs with a progress bar.
- Subsequent launches skip the picker; `voice-bird model` reopens it.

## Session persistence

### Directory layout

`~/voice-bird/sessions/<SLUG>/`:

```
2026-04-16_14-32-07-standup-zoom/
├── audio.wav              ← 16 kHz mono f32
├── transcript.jsonl       ← append-only during recording; one Committed segment per line
├── transcript.json        ← finalized on stop: {segments: [...], meta: {...}}
├── transcript.txt         ← finalized on stop: one line per segment
└── meta.json              ← device, app source, model, engine, start, end, duration, version
```

`<SLUG>` = `<timestamp>-<source-slug>` where source is `mic`, `system`, or a normalized app name (e.g. `zoom`, `chrome`).

### Write discipline

- `transcript.jsonl` is append-with-fsync per `Committed`. Surviving a crash, we can always reconstruct a valid transcript from it.
- `audio.wav` header is updated periodically via `hound` so a truncated file is still playable.
- `transcript.json` and `transcript.txt` are written via temp-file + atomic rename on stop.
- `meta.json` is written on stop; a partial version is written at start so orphaned session directories are identifiable.

### Recovery

`voice-bird recover <session-dir>` reads `transcript.jsonl` + WAV duration and regenerates `transcript.json` / `transcript.txt` / `meta.json`. Useful after crashes and used in integration tests.

## TUI rendering

### Layout

```
┌─ Voice Bird ────────────────── distil-small.en · whisperkit · 00:03:42 ─┐
│  Source: Zoom (system audio)     Level: ▓▓▓▓▓▓░░░░   -18 dB            │
├────────────────────────────────────────────────────────────────────────┤
│ 00:00:02  Good morning everyone, thanks for joining.                   │
│ 00:00:07  Today we're going through the quarterly review.              │
│ 00:00:15  Let's start with the engineering update.                     │
│ 00:00:22  ┃                                                            │   ← auto-scroll
├────────────────────────────────────────────────────────────────────────┤
│ … and then if we look at the backlog                                   │   ← tentative (dim italic)
└─ [r] record  [s] stop  [m] model  [↑↓] scroll  [q] quit ───────────────┘
```

### Widgets and behavior

- Header shows model, engine (`whisperkit` / `whisper-rs`), elapsed timer, source, and a color-threshold level meter.
- Committed zone: scrollable word-wrapped list of segments; auto-scroll unless the user hits `↑` / `↓`, which pauses auto-scroll until they return to the tail.
- Tentative line: a single-line paragraph in dim italic; replaces in place on each `Tentative` event; truncated with a leading `…` if it exceeds the width.
- Status banners: transient messages (engine fallback, sidecar crash, model download progress) appear above the footer and dismiss on timer or key.
- Render tick: ~30 fps. Event consumption runs on a separate task; slow render never blocks engine ingestion.

### Keys

| Key        | Action                                     |
|------------|--------------------------------------------|
| `r`        | start recording                            |
| `s`        | stop recording, finalize session           |
| `m`        | open model picker                          |
| `d`        | open device/source picker                  |
| `↑` / `↓`  | scroll committed zone; pauses auto-scroll  |
| `End`      | jump to tail, resume auto-scroll           |
| `q`        | quit (prompts if recording)                |
| `?`        | help overlay                               |

## Configuration

`~/.config/voice-bird/config.toml`:

```toml
default_model = "distil-small.en"
language = "en"                     # or "auto"
session_dir = "~/voice-bird/sessions"
hop_ms = 750
min_window_ms = 1000

[engine]
prefer = "auto"                     # "auto" | "whisper_rs" | "whisperkit"

[audio]
default_source = "microphone"       # "microphone" | "system" | "app:<name>"
```

Config is the single source of truth across runs. CLI flags override per-invocation but are not persisted. First launch writes the file after the model picker completes.

## Error handling

| Failure                                          | Behavior                                                                 |
|--------------------------------------------------|--------------------------------------------------------------------------|
| Model file missing / checksum fail                | Offer re-download in TUI; block recording until resolved                 |
| WhisperKit sidecar fails to start                | Log, fall back to `WhisperRsEngine`, show header banner                   |
| WhisperKit sidecar dies mid-session              | Emit `Error`, finalize with what exists, banner: "transcript may be truncated" |
| Audio device disconnects                         | Stop session cleanly, finalize outputs, show error banner                |
| Disk full / write error on `transcript.jsonl`    | Abort session, surface error; WAV closed best-effort                     |
| LocalAgreement buffer drift (>60 s since commit) | Force-commit tentative tail, trim buffer, warn in log                    |
| Resample underrun                                | Insert silence, log at warn                                              |

Overall policy: **transcripts are best-effort but must never corrupt.** Partial is acceptable; garbled is not. `.json` / `.txt` / `meta.json` writes go via temp-file + atomic rename. `.jsonl` writes fsync per segment.

## Testing

### Unit

- `local_agreement::step`: exhaustive table-driven cases plus `proptest` fuzzing over token sequences. Covers empty `prev`, no overlap, partial overlap, timestamp skew within and beyond tolerance, sentence-boundary grouping, force-commit path.
- `models`: catalog parsing, SHA-256 verification, resumable download against a local HTTP fixture.
- `session::writer` and `session::finalize`: golden-file tests using `tempfile`; assert that finalize reconstructs the exact shape from `.jsonl` plus WAV duration.
- `audio::resample`: sine-wave round-trip with `rubato`; assert RMS within tolerance.

### Integration (Rust)

- `MockEngine` feeding a canned event stream into the full app wiring; assert on `transcript.jsonl` contents and on a `ratatui::TestBackend` snapshot of the rendered frame.
- Crash-recovery test: write a partial `.jsonl`, run `voice-bird recover`, assert `.json` / `.txt` match a reference.

### Engine smoke tests (feature-gated, `cargo test --features engine-smoke`)

- A ~10 s fixture WAV in `tests/fixtures/` run through `WhisperRsEngine` with `tiny.en` (downloaded on demand).
- Assert the output text has WER ≤ 0.30 against a hand-written reference — loose enough to avoid flakes, tight enough to catch wiring regressions.

### Sidecar contract test (Swift)

- `whisperkit-helper/Tests/` replays the fixture WAV, captures stdout JSONL, and asserts the event shape (keys, types, monotonic timestamps). Does not assert WER.

### Manual verification checklist

- Microphone session (all three platforms).
- System-audio loopback session (macOS + Windows).
- Sidecar forced-kill mid-session on macOS; verify fallback and banner.
- Model switch between sessions; verify no leaked context.
- Crash during recording; verify `recover` produces a usable transcript.
- Long session (>30 min); verify buffer-drift path and steady memory.

### Deliberate non-tests

- Absolute WER numbers — model-dependent and flaky.
- End-to-end TUI keystroke tests — brittle; covered by the manual checklist and the `TestBackend` snapshot test.

## Migration and rollout

1. **Feature branch** off `main`. Project A lands as a single feature branch because it mixes deletion (Tauri) with new code (engines, session writer) that share the same `src/` surface.
2. **Stage 1:** promote `voice-bird-cli/` to repo root; delete Tauri root files; ensure `cargo build` still produces today's CLI at the new location. No behavior change.
3. **Stage 2:** land `TranscriptionEngine` trait + `MockEngine` + session writer + TUI rework against the mock. Full test suite runs.
4. **Stage 3:** land `WhisperRsEngine` + `local_agreement` + `models` catalog + first-run picker. Dogfood on all three platforms.
5. **Stage 4:** land `WhisperKitEngine` + Swift sidecar + macOS build pipeline (`xtask` signs the sidecar).
6. **Stage 5:** delete `server_streaming.rs`, all AssemblyAI mentions on the desktop side, update `README.md` + `CLAUDE.md`.
7. Each stage ends with a green `cargo test`, a manual smoke on the primary dev platform, and a merge to `main`.

Nothing in this project touches the `voice_bird` Next.js repo. The server-streaming ports (3001) keep running for now — Project A simply stops using them from the desktop side. They will be removed in Project B or C once the web app's dependencies are clear.

## Open questions (to resolve during planning)

- Exact Swift package version pin for WhisperKit (track latest stable at plan time).
- Whether to use `rubato` (pure Rust, well-tested) or `dasp_interpolate` for resampling — lean `rubato`, confirm benchmark before locking.
- Macros for the CoreAudio process-tap on macOS (ScreenCaptureKit introduced in 13.0 vs. legacy tap API) — confirm the target macOS baseline at plan time.
- Distribution: raw `cargo binstall` vs. `.app` bundle vs. both. The current `voice-bird-cli` README implies `cargo binstall` — the Swift sidecar makes a `.app` bundle attractive on macOS. Decide in the plan.

## References

- [Turning Whisper into Real-Time Transcription System (Macháček et al., 2023)](https://arxiv.org/abs/2307.14743)
- [WhisperKit: On-device Real-time ASR with Billion-Scale Transformers](https://arxiv.org/html/2507.10860v1)
- [ufal/whisper_streaming (LocalAgreement-2 reference implementation)](https://github.com/ufal/whisper_streaming)
- [argmaxinc/WhisperKit (macOS / Apple Silicon)](https://github.com/argmaxinc/WhisperKit)
- [ggerganov/whisper.cpp](https://github.com/ggerganov/whisper.cpp) (backend for `whisper-rs`)
