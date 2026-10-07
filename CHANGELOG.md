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
- Move model-presence checks into `ModelWatcher` and a separate SQLite
  availability table. Refresh the full catalog at startup and on ticks;
  `LanguageConsumer` is stateless and queries availability and download attempts.
  Accepted installation successes update availability; stale successes do not.
- Decouple `ModelWatcher` from the UI projection. Publish persisted availability
  changes through the bus; consumers select affected sessions and publish
  `ModelMissing` for the next pass. Reject stale observations and duplicate work,
  including after Quit. Use `ui_view` for projection bindings across input,
  rendering, and tests instead of misleading whole-view `state` names.
- Remove the model store from `DownloadsConsumer`. Persist attempt-scoped
  staging paths via the watcher; downloads only claim and fetch. Route verified
  fetches through a gated model-store consumer for installation, and send
  `DiscardInflight` there for staging sweeps. Reject duplicate/stale handoffs and
  old-attempt status notifications so immediate cancellation retries stay independent.
- Remove unused model-store cleanup APIs, fixture tracking, and throttle state
  from `voice-bird-next`; remove dead-code warning suppressions.
- Unify download progress and completion reporting in `Throttle::call`; preserve
  unconditional known-total completion updates, including zero-byte downloads.
- Move source enumeration, fetch, and installation workers into producer services,
  including their task spawning and progress/result events. Consumers retain
  lifecycle and dispatch decisions and hand accepted work to those services.
  Move shared source catalogs/types and download transport/errors to neutral modules.
- Track the attempt in each UI download projection. Ignore stale worker events
  and terminal status replays accepted before a same-batch retry is claimed,
  preserving the new attempt's progress gauge and waiters. Keep same-attempt
  joins, late-join reconciliation, and pre-claim failures unchanged.
- Commit installation success and model availability in one SQLite transaction.
  Roll back both on write/commit failure and publish lifecycle success only
  after commit, preventing partially persisted success and false ready notifications.

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
