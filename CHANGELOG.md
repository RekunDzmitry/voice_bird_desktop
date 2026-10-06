# Changelog

## Next (unreleased)

- Refactor `voice-bird-next` into producers, an event bus, the SQLite gate,
  and a consumer-owned `UiView`; remove the dispatcher and old state modules.
- Schedule input, source enumeration, downloads, and model-watch ticks with
  Tokio. Stream HTTP downloads asynchronously; run native enumeration and
  model installation on the blocking pool.
- Preserve serialized source requests, session panic fallback, download
  cancellation, render snapshots, and immediate Quit without joining stalled work.
- Decouple event consumers from the producer aggregate. Route audio-source,
  language, model-request, and UI stages through independent `Consumers`;
  follow-up events cross the bus and SQLite gate before the next stage runs.
  Preserve attempt lineage across delayed requests and terminal replays.
- Rename the audio catalog to `AudioSourcesCatalog`, its dependency to
  `audio_sources`, and the language-picker state to `PickingLanguage`.

## 0.5.0 (2026-08-05)

Breaking: the local agent / Kafka funnel path is retired. Agents are
now configured at voicebird.app. The desktop CLI no
longer runs an MCP server, talks to a local Kafka broker, or ships
with the omp/oh-my-pi detection code.

  * CLI flags removed: `--mcp-server`, `--register`.
  * New run path: pressing `g` against a focused slot fires a
    cloud Agent run; the result streams back over an SSE
    channel and lands in `voicebird.app` (see the web app for the
    full UX).
  * Agents picker is now a single `Stdout` row. The pane will be
    repainted for the cloud Agent picker in a follow-up.
  * `agent_targets` config key + `[agent_targets]` rows are no
    longer parsed. Existing config.toml files still load (the key
    is silently ignored), but the values cannot be edited back in
    — recreate the targets at voicebird.app instead.
  * `src/agent/` module removed (~1900 lines).
  * `src/agent_funnel.rs` removed (~590 lines).
  * `rdkafka` dependency removed. The build no longer pulls in
    cmake or vendored OpenSSL.

## 0.4.0

Previous stable release.
