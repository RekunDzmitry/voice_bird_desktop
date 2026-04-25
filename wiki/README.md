# voice-bird: LLM Wiki

Mode: B (GitHub / Repository)
Purpose: Map the voice-bird codebase — modules, components, decisions, dependencies, flows — and keep session summaries from `/save`.
Created: 2026-04-21

## Structure

```
wiki/
├── index.md            # master catalog
├── log.md              # append-only operations log (new entries at top)
├── hot.md              # ~500-word recent-context cache
├── overview.md         # executive summary
├── modules/            # Rust crates and top-level source modules
├── components/         # engines, writers, UI widgets
├── decisions/          # ADRs
├── dependencies/       # external crate inventory
├── flows/              # end-to-end data paths
├── sessions/           # session summaries filed by /save
└── _templates/         # per-type skeletons (module, component, decision, session, flow)
```

## Conventions

- All notes use YAML frontmatter: `type`, `status`, `created`, `updated`, `tags` (minimum).
- Wikilinks use `[[Note Name]]` — filenames are unique; no paths needed.
- `wiki/log.md` is append-only; new entries go at the TOP.
- `wiki/hot.md` is overwritten every update; keep under 500 words.
- The project's own `CLAUDE.md` at the repo root stays authoritative for Claude Code behavior. This wiki is additive, not a replacement.

## Operations

- Save session: `/save` or `/save session [name]` → files into `wiki/sessions/`.
- Query: `/wiki-query` or ask a question naturally — Claude reads `hot.md` first.
- Lint: `/wiki-lint` runs a health check.
- Ingest: drop a source in `.raw/` (create if needed), then `/wiki-ingest [filename]`.

## Visual setup (one-time)

1. Open Obsidian → Settings → Appearance → CSS Snippets.
2. Click "Open snippets folder" (this opens `.obsidian/snippets/`).
3. Refresh the list in settings → toggle `vault-colors` on.
4. (Optional) Install the "Minimal" theme under Appearance → Manage.
