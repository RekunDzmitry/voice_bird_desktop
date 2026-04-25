---
type: meta
title: "Decisions Index"
created: 2026-04-21
updated: 2026-04-21
tags:
  - index
  - decisions
---

# Decisions (ADRs)

Architecture and product decisions with rationale and date. File new decisions under this folder.

## Known decisions to capture
- Dual-engine transcription: fast streaming + slow refinement (Option C from the original brainstorm).
- Non-overlapping refinement windows with 1s overlap carry.
- Tail-flush propagation via channel-close (no explicit shutdown oneshots to consumers).
- Streaming timestamps stamped at consumer via wall-clock (workaround for engine's buffer-relative timestamps).
- TUI transcript scrolling: PgUp/PgDn + Home/End + ↑/↓/j/k + mouse wheel; auto-follow default.
