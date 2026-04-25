# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Voice Bird is a Rust TUI for voice transcription that is **local by default** (via `whisper-rs` / WhisperKit) with an **optional cloud engine** (AssemblyAI Universal-Streaming) for users without sufficient local compute. It records microphone audio via `cpal`, resamples to 16 kHz mono with `rubato`, and — depending on `engine_prefer` — runs Whisper locally or streams PCM to AssemblyAI over a WebSocket. Every session lives under `~/voice-bird/sessions/<timestamp>-<source>/` as an append-only `transcript.jsonl` plus finalized `transcript.{json,txt}`, `audio.wav`, and `meta.json`.

Cloud engines are opt-in, require an API key in `config.toml`, and are clearly indicated via a `CLOUD` badge in the TUI header plus a recording-start reminder.

## Architecture

- `src/main.rs` — entry point, crossterm event loop / key handlers, `--recover` CLI flag.
- `src/app.rs` — `App` state, start/stop recording, owns the tokio runtime handle and the cpal `Stream`, drives engine selection (WhisperKit → whisper-rs fallback).
- `src/ui.rs` — ratatui render pipeline: header / committed zone / tentative line / status banner / footer, plus the first-run model-picker overlay.
- `src/settings_view.rs` — full-screen in-app settings view (opens on `,` from Normal mode); edits `AppConfig` including the AssemblyAI API key.
- `src/config.rs` — TOML-backed `AppConfig` (selected model, engine preference).
- `src/audio/capture.rs` — cpal input stream, sample-format fan-in to `f32`.
- `src/audio/resample.rs` — `rubato` 16 kHz mono resampler.
- `src/session/layout.rs` — session-directory path conventions and slug rules.
- `src/session/writer.rs` — append-only JSONL writer, fsync per segment.
- `src/session/finalize.rs` — `transcript.jsonl` → `transcript.{json,txt}` + `meta.json`.
- `src/session/recover.rs` — reruns finalize over a partially-written session.
- `src/transcription/mod.rs` — `TranscriptionEngine` trait.
- `src/transcription/whisper_rs_engine.rs` — `WhisperRsEngine`, whisper.cpp via `whisper-rs` 0.13.
- `src/transcription/whisper_kit_engine.rs` — `WhisperKitEngine`, spawns the Swift sidecar and streams length-prefixed PCM over stdio, reads JSONL results.
- `src/transcription/assemblyai_engine.rs` — AssemblyAI Universal-Streaming v3 WebSocket client. Streams 16 kHz i16 PCM, maps Turn events onto `EngineEvent`.
- `src/transcription/mock.rs` — `MockEngine` for deterministic tests.
- `src/transcription/local_agreement.rs` — `step()` implements LocalAgreement-2 for committed/tentative merge.
- `src/transcription/models.rs` — model catalog (URLs, sizes, SHA-256) + downloader with resume + digest verify.
- `whisperkit-helper/` — Swift package (macOS only). Source is in-tree; the plan defers the actual `swift build` to the user.
- `xtask/` — workspace helper binary: `build-sidecar` shells out to `swift build` on macOS; `check-catalog` fails when `src/transcription/models.rs` still contains `<FILL>` placeholders.

## Essential Commands

```bash
cargo build                                    # debug build
cargo test                                     # unit + integration tests (no engine downloads)
cargo run                                      # launch the TUI
cargo test --all-targets --features engine-smoke
    # adds the whisper-rs smoke (downloads tiny.en, ~75 MB) and the
    # WhisperKit sidecar smoke (gracefully skipped if the sidecar binary
    # is not present next to the test harness)

cargo run -p xtask -- check-catalog            # CI gate: fails on <FILL> in models.rs
cargo run -p xtask -- build-sidecar            # macOS only: runs `swift build -c release`
voice-bird --recover <session-dir>             # rebuild transcript.{json,txt} from JSONL
```

## Key Conventions

- Binary is `voice-bird`, library is `voice_bird` (underscore). The workspace has two members: the root crate and `xtask/`.
- Rust 2021, strict build (no `-D warnings` in CI yet, but we don't land warnings). No clippy-fix — review diffs by hand.
- Session timestamps are UTC ISO-8601 without colons (filesystem-friendly). Source slugs are normalized to `[a-z0-9-]`.
- `transcript.jsonl` writes are flushed and fsync'd per segment — crash-mid-recording must still produce a recoverable file.
- Every entry in `src/transcription/models.rs` must have a real SHA-256 digest before shipping a release; `<FILL>` placeholders are intentional mid-development and `xtask check-catalog` is the CI gate that prevents accidentally shipping unverified downloads.
- Engine selection prefers WhisperKit on macOS when the sidecar binary is on `PATH` (or co-located with `voice-bird`), else falls back to `whisper-rs`.
- Cloud engines are opt-in. `engine_prefer = "assemblyai"` requires `assemblyai_api_key` to be set; otherwise start_recording refuses and a persistent banner prompts the user to open settings. The CLOUD badge is driven by `App::is_cloud_engine`, set at start_recording, cleared at stop_recording.

## Known Gotchas

- **WhisperKit sidecar build**: requires a working Swift PackageManager. Full Xcode works. A broken Command Line Tools install has been seen to ship `libPackageDescription.dylib` with ABI drift — if `swift build` fails with `Undefined symbols: PackageDescription.Package.__allocating_init`, reinstall CLT or switch to Xcode.
- **`whisper-rs` 0.13 state**: a single `WhisperState` retained across inference calls leaks KV-cache and drifts the output. `WhisperRsEngine` builds a fresh state per call.
- **Short-buffer rejection**: whisper.cpp errors on inputs shorter than 1 s. The engine pads buffers below that to 1.1 s of silence before calling `full`.
- **cpal `Stream` is `!Send`**: the stream is owned by `App` so it lives on the thread that created it. Samples flow into the async world via `tokio::sync::mpsc::Receiver<Vec<f32>>`.

## Design Sources

Architecture and rationale live in `docs/superpowers/specs/2026-04-16-desktop-local-whisper-design.md`; the executable plan (with scope amendments noting microphone-only capture and user-built sidecar) is in `docs/superpowers/plans/2026-04-16-desktop-local-whisper.md`. Consult those before large changes.
