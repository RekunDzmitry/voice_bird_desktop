# Cloud Transcription Engine (AssemblyAI) + In-App Settings — Design

Date: 2026-04-23
Status: approved, ready for implementation plan

## Motivation

Voice Bird currently runs fully locally (`whisper-rs` cross-platform, WhisperKit on macOS with ANE). Local inference requires enough CPU/GPU/ANE to keep up with real-time audio. Users on weaker machines (older laptops, low-end hardware, VMs) either hit backpressure or get degraded transcription quality from the smallest models.

This spec adds one opt-in cloud transcription engine (AssemblyAI Universal-Streaming v3) so those users get high-quality streaming transcription without local compute. It also introduces an in-app settings view so API keys (and existing config fields) can be edited without touching `config.toml` by hand.

The "fully local" promise is preserved as a default; cloud is opt-in and always visually indicated.

## Non-goals

- OS keychain / secret-service integration. Keys live in plaintext `config.toml` (v1).
- AssemblyAI batch/async endpoints. Streaming only.
- Voxtral or any other cloud provider. AssemblyAI is the only v1 provider.
- Refinement passes over cloud transcripts. Refinement stays whisper-only.
- Per-session cost estimates or usage dashboards.
- Auto-reconnect on network disconnect (would create silent gaps in the append-only transcript).

## Architecture

One new engine module and one new TUI view. The session writer, local-agreement merger, audio pipeline, and model catalog are untouched.

```
src/
  transcription/
    mod.rs                  # TranscriptionEngine trait, EngineConfig enum (changed)
    assemblyai_engine.rs    # new: AssemblyAiEngine
    whisper_rs_engine.rs    # adapted for EngineConfig::Local
    whisper_kit_engine.rs   # adapted for EngineConfig::Local
    refinement_engine.rs    # adapted for EngineConfig::Local
    mock.rs                 # adapted for EngineConfig::Local
  ui.rs                     # header badge
  ui/
    settings.rs             # new: full-screen settings view
  app.rs                    # AppMode::Settings, is_cloud_engine flag
  config.rs                 # assemblyai_api_key field, 0600 chmod on save
```

### `EngineConfig` as an enum

Today:

```rust
pub struct EngineConfig {
    pub model_path: std::path::PathBuf,
    pub language: Option<String>,
    pub sample_rate: u32,
    pub hop_ms: u32,
    pub min_window_ms: u32,
}
```

After:

```rust
pub enum EngineConfig {
    Local {
        model_path: std::path::PathBuf,
        language: Option<String>,
        sample_rate: u32,   // always 16_000
        hop_ms: u32,
        min_window_ms: u32,
    },
    Cloud {
        api_key: String,
        language: Option<String>,
        sample_rate: u32,   // always 16_000
    },
}
```

Every engine matches on the variant it expects and returns an error if given the wrong one. The `TranscriptionEngine::start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle>` signature is unchanged.

### `select_engine` behavior

| `engine_prefer` | Key present? | Result |
|---|---|---|
| `"auto"` (default) | — | macOS: WhisperKit if sidecar present, else whisper-rs. Non-macOS: whisper-rs. |
| `"whisperkit"` | — | WhisperKit if sidecar present, else whisper-rs. |
| `"whisper_rs"` | — | whisper-rs. |
| `"assemblyai"` | yes | `AssemblyAiEngine::new(api_key)` |
| `"assemblyai"` | no | Error surfaced via banner; recording blocked until user resolves. |

### Refinement and cloud

Refinement runs a second whisper engine in parallel. When the primary engine is cloud, refinement is disabled regardless of `refinement_model`. The settings view shows the refinement section as "whisper only" and disables editing when `engine_prefer == "assemblyai"`.

## AssemblyAI engine (`src/transcription/assemblyai_engine.rs`)

**Endpoint.** Universal-Streaming v3: `wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&format_turns=true`.

**Auth.** `Authorization: <api_key>` header on the WebSocket upgrade request.

**Outgoing frames.** Raw 16-bit little-endian PCM, 16 kHz mono, binary WebSocket messages. The existing audio pipeline delivers f32 samples at 16 kHz; conversion to i16 happens inside the engine's tokio task. Frame size targets ~50 ms (800 samples, 1600 bytes).

**Incoming messages.** JSON:
- `Begin` → `EngineEvent::ModelLoaded { name: "assemblyai-universal-v3" }`
- `Turn { end_of_turn: false, transcript, ... }` → `EngineEvent::Tentative(transcript)`
- `Turn { end_of_turn: true, ... }` or `FormattedTurn` → `EngineEvent::Committed(Segment)` with `t_start` / `t_end` derived from message timestamps and `tokens` populated from per-word timestamps when present; empty `Vec<Token>` otherwise.
- `Termination` → engine shutdown.
- `Error` → `EngineEvent::Error(message)` then shutdown.

**Runtime.** `start(cfg)` spawns one tokio task:

1. Open WebSocket, send opening config frame (`AudioEncoding: pcm_s16le`, `SampleRate: 16000`, language from `cfg`).
2. Loop `select!` between `pcm_rx.recv()` (convert f32 → i16 PCM, send binary frame) and `ws.next()` (parse JSON, emit `EngineEvent`).
3. On `shutdown` oneshot: send `Terminate` frame, drain remaining messages, close.

**New dependencies.** `tokio-tungstenite`, `rustls`, `webpki-roots`, `url`. `serde_json` already in-tree.

**LocalAgreement-2 is bypassed for cloud.** Whisper engines use `local_agreement::step()` to merge overlapping hypotheses into committed/tentative. AssemblyAI's protocol already distinguishes partial turns (`end_of_turn=false`) from finalized turns (`end_of_turn=true`); the engine emits `Tentative`/`Committed` directly from those flags without running LocalAgreement. `local_agreement.rs` is unchanged.

**Failure modes (user-visible).**

| Condition | UX |
|---|---|
| Missing/invalid API key | Banner: `AssemblyAI: auth failed (check settings)` |
| Network disconnect mid-session | Banner: `AssemblyAI: connection lost`, recording stops, `transcript.jsonl` preserved (recoverable via `--recover`) |
| Rate limit / quota | Banner with AssemblyAI's error text |

No auto-reconnect in v1 — silent gaps would corrupt the append-only transcript.

## Config changes (`src/config.rs`)

Add one field:

```rust
/// AssemblyAI API key. Stored in plaintext in config.toml — file
/// permissions are the only protection. Empty string = unset.
#[serde(default)]
pub assemblyai_api_key: String,
```

`engine_prefer` gains a new valid value: `"assemblyai"`. Existing values unchanged.

**File permissions.** On save, `config.toml` is chmod'd to `0600` on Unix (best-effort; no-op on Windows). On first save that includes a non-empty `assemblyai_api_key`, a header comment is written: `# Contains secrets. Do not share.`

**Migration.** Old configs deserialize fine via `#[serde(default)]`. No migration step needed.

**Validation on load.** If `engine_prefer == "assemblyai"` but `assemblyai_api_key` is empty, the app launches normally with a persistent banner: `Cloud engine selected but no API key — open settings (press ',').` Recording is blocked until the user either fills the key or switches engine.

## Settings view (`src/ui/settings.rs`)

**Activation.** `AppMode` gains a `Settings` variant. In `Normal` mode, pressing `,` (unused today) switches to `Settings`. Settings cannot be opened while `Recording` — the key is ignored and the footer hints `(stop recording first)`.

**Layout.** Full-screen ratatui view:

```
┌ Settings ────────────────────────────────────────────────────┐
│                                                              │
│  ▸ General                                                   │
│      Default model:         distil-small.en                  │
│      Language:              en                               │
│      Session directory:     ~/voice-bird/sessions            │
│                                                              │
│  ▸ Audio                                                     │
│      Default source:        microphone                       │
│      Input device:          (OS default)                     │
│                                                              │
│  ▸ Engine                                                    │
│      Engine preference:     auto                             │
│      Hop (ms):              750                              │
│      Min window (ms):       1000                             │
│                                                              │
│  ▸ Refinement (whisper only)                                 │
│      Refinement model:      (off)                            │
│      Window (ms):           20000                            │
│      Beam size:             5                                │
│                                                              │
│  ▸ Cloud                                                     │
│      AssemblyAI API key:    ••••••••••••••sk72  [plaintext]  │
│                                                              │
│  [ Save ]  [ Cancel ]                                        │
└──────────────────────────────────────────────────────────────┘
```

**Interaction.**
- `↑ / ↓` or `j / k`: move between editable fields; section headers are skipped.
- `Tab` / `Shift+Tab`: jump between sections.
- `Enter`: enter edit mode. Text input for strings/numbers; rotary cycle for enums (`engine_prefer`, `audio_default_source`).
- API key field: masked (last 4 chars visible) in display mode; typing is visible while editing; masking resumes on exit. No reveal/hide toggle.
- `Esc` in edit mode: cancel field change. `Esc` in navigation mode: close view (confirm if dirty).
- `s` in navigation mode: save.
- `q` in navigation mode with no pending edits: close.

**Validation on save.**
- Numeric fields parsed; invalid → cursor jumps to field, red error line at bottom.
- `engine_prefer == "assemblyai"` with empty key → refuse save, error line.
- Malformed `session_dir` path flagged; non-existent paths accepted (created on recording start).

**Persistence.** `AppConfig::save()` is called on successful save; view transitions to `Normal`. Field values snapshot on open so Cancel restores cleanly. Save failures (disk full / perms) surface a banner inside the settings view and keep it open.

**Footer hints.** `↑↓ move   Enter edit   s save   Esc cancel`.

## CLOUD badge + docs reframe

### Header badge (`src/ui.rs`)

`render_header` gets a conditional right-aligned badge when the active engine is cloud:

```
Voice Bird · distil-small.en · whisper-rs                   [CLOUD]
```

Style: `fg=Black, bg=Yellow, bold`. For the first 3 seconds of each recording with a cloud engine, a one-line reminder is rendered below the header: `Audio is being sent to AssemblyAI.` After 3 s it collapses back to just the badge.

`App` gains an `is_cloud_engine: bool` flag, set in `start_recording` from the `select_engine` result and reset in `stop_recording`. The badge renders only when the flag is true — between recordings the badge disappears even if `engine_prefer == "assemblyai"`.

### README reframe

Top of `README.md`:

> Voice Bird runs **locally by default** — your audio never leaves your machine. An optional cloud engine (AssemblyAI) is available for users without a GPU or ANE; it is off by default and requires both an API key and an explicit change to `engine_prefer`. When active, a `CLOUD` badge is shown in the header.

The "fully locally" language in the intro paragraph is softened to "locally by default." A new subsection `Cloud engines (optional)` lists how to get an AssemblyAI key and flip `engine_prefer`.

### CLAUDE.md reframe

Project description updated to: "Voice Bird is a Rust TUI for fully-local voice transcription, with an optional cloud engine (AssemblyAI) for users without local GPU/ANE." Architecture list gains `src/transcription/assemblyai_engine.rs` and `src/ui/settings.rs`. No other sections change.

## Testing

**Unit**
- `assemblyai_engine`: mock WebSocket server feeds canned JSON; asserts `EngineEvent` mapping (`Begin`, `Turn` partial → `Tentative`, `Turn` final → `Committed`, `Error` → `Error`), outgoing binary frames are i16 PCM of expected length, `Terminate` is sent on shutdown.
- `config`: roundtrip including `assemblyai_api_key`; old-config backward compat (missing field → empty string); `0600` chmod applied on Unix saves.
- `select_engine`: `"assemblyai"` with empty key → typed error; with non-empty key → cloud engine; non-cloud preferences unchanged.
- `EngineConfig` enum: each engine rejects the wrong variant with a typed error.
- Settings view: navigation skips section headers; masked key display; dirty-tracking; validation (engine=assemblyai + empty key refuses save).

**Integration**
- `engine-smoke-assemblyai` feature flag: real WebSocket call using `ASSEMBLYAI_API_KEY` env var, 5 s of sine-wave PCM, asserts at least one `Committed` event arrives. Skipped when env var is missing. Not run in CI.
- Render-path test confirming `is_cloud_engine` gates the badge.

## Error handling (authoritative)

| Condition | UX |
|---|---|
| `engine_prefer="assemblyai"`, empty key, on launch | Persistent banner; recording blocked until resolved |
| Auth failure | Banner: `AssemblyAI: auth failed (check settings)`; recording stops |
| Network disconnect mid-session | Banner: `AssemblyAI: connection lost`; recording stops; `transcript.jsonl` preserved and recoverable via `--recover` |
| Rate limit / quota | Banner with AssemblyAI's error text |
| Config save failure | Banner inside settings view; view stays open |

## Migration / backward compat

- Old `config.toml` without `assemblyai_api_key` keeps working (serde default = empty string).
- Old `engine_prefer` values (`auto`, `whisperkit`, `whisper_rs`) behave identically.
- Existing transcripts and session layout are unchanged.
- No data format changes to `transcript.jsonl`, `transcript.json`, `transcript.txt`, `meta.json`.

## Open risks

- `tokio-tungstenite` + `rustls` adds build time and binary size. Acceptable for an opt-in feature.
- AssemblyAI protocol changes (Universal-Streaming is v3 at time of writing). Engine module is the only place touching the wire format; schema changes localized there.
- Plaintext key in `config.toml` is a known v1 weakness. Documented in README and guarded by `0600` chmod + warning comment. Keychain migration is a future spec.
