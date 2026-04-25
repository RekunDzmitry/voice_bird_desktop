---
type: meta
title: "Modules Index"
created: 2026-04-21
updated: 2026-04-21
tags:
  - index
  - modules
---

# Modules

Top-level Rust crates and source modules. One page per major module.

## Crates in this workspace
_(To be filled as modules are ingested.)_

- `voice-bird` (binary) — main TUI app at `src/`.
- `voice-bird-cli` / `voice-bird-cli-crate` — CLI helpers.
- `whisperkit-helper` — helper binary.
- `xtask` — dev tooling.

## Notable source modules (src/)
- `app` — TUI App state, recording runtime.
- `ui` — ratatui renderers.
- `main` — entry point, event loop.
- `config` — serde TOML config.
- `transcription/whisper_rs_engine` — streaming engine.
- `transcription/refinement_engine` — background refinement engine.
- `session/writer` — JSONL segment writer.
- `session/finalize` — JSONL → transcript.txt/json/meta.json.
