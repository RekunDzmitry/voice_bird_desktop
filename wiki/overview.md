---
type: meta
title: "Overview"
created: 2026-04-21
updated: 2026-04-21
tags:
  - overview
---

# voice-bird: Project Overview

Executive summary of the voice-bird desktop app and its wiki.

## What voice-bird is

`voice-bird` is a Rust TUI (ratatui + crossterm) for local, real-time voice transcription on macOS. It runs Whisper models locally via [[dependencies/whisper-rs|whisper-rs]] with Metal acceleration and writes structured session artifacts (JSONL segments → finalized transcript.txt / transcript.json / meta.json).

## High-level architecture

- **Audio capture** → [[components/pcm-producer|PCM producer]] → fan-out channel.
- Two transcription engines consume the same PCM stream in parallel:
  - [[components/whisper-rs-engine|Streaming engine]] — small/fast model (`tiny.en`) with a sliding window; emits `Committed` events as it flushes hops.
  - [[components/refinement-engine|Refinement engine]] — larger model (`large-v3-turbo`) on non-overlapping ~20s windows with beam search; emits higher-quality commits at higher latency.
- **Session writer** appends both engines' commits to separate JSONLs and finalizes on stop.
- **TUI** renders refined text as the canonical transcript and dims live streaming tail below it.

## Wiki structure

- [[modules/_index]] — Rust crates and top-level source modules.
- [[components/_index]] — engines, writers, UI widgets.
- [[decisions/_index]] — ADRs (dual-engine, streaming timestamp policy, TUI scrolling UX).
- [[dependencies/_index]] — crate inventory and version notes.
- [[flows/_index]] — PCM → commits → JSONL → finalized transcript path.
- [[sessions/_index]] — session summaries filed by `/save`.

## Conventions

- All notes use YAML frontmatter with at minimum: `type`, `status`, `created`, `updated`, `tags`.
- Wikilinks use `[[Note Name]]` — filenames are unique; no paths needed.
- `wiki/log.md` is append-only; new entries go at the TOP.
- `wiki/hot.md` is overwritten every time; keep under ~500 words.
