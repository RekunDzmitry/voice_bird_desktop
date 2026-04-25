---
type: meta
title: "Flows Index"
created: 2026-04-21
updated: 2026-04-21
tags:
  - index
  - flows
---

# Flows

End-to-end data paths through the system.

## Flows to document
- **Capture → Transcript**: mic/audio → PCM producer → tee → streaming engine + refinement engine → `Committed` events → `SegmentWriter` JSONL → `finalize` → `transcript.txt` / `.json` / `meta.json`.
- **Shutdown**: stop pressed → abort producer → PCM channel close → engines run tail flush → broadcast close → consumers drain and exit.
- **UI render**: App state (refined + committed vectors) → `render_committed` → refined normal, live streaming dim, scroll follow.
