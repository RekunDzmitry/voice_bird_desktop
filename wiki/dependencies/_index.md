---
type: meta
title: "Dependencies Index"
created: 2026-04-21
updated: 2026-04-21
tags:
  - index
  - dependencies
---

# Dependencies

External crates and system tools this project depends on, plus version / risk notes.

## Notable runtime crates (Cargo.toml)
- `whisper-rs` 0.13 (features: Metal, whisper-cpp-log) — Whisper inference.
- `ratatui` 0.29 — TUI rendering.
- `crossterm` 0.28 — terminal backend + input.
- `tokio` 1 — async runtime; broadcast / mpsc / oneshot channels.
- `hound` — WAV fixture loading in tests.
- `tempfile` — integration-test temp dirs.
- `serde`, `serde_json`, `toml` — config + JSONL.

_(Ingest Cargo.toml/Cargo.lock into `.raw/` to populate the full dependency graph.)_
