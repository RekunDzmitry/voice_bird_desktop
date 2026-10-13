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
- Decouple recording decisions from the runtime producer. Route capture start/stop
  commands through the bus and SQLite identity gate; dispatch admitted inference
  jobs and retire completed workers independently of consumers. Preserve
  finalization-before-resume and natural-end behavior, reject duplicate/stale
  commands, and prevent queued starts from opening capture during shutdown.
- Rename the shared session/job ID generator to `next_monotonic_id`; retain its
  timestamp-seeded, process-wide monotonic behavior.
- Move recording-consumer lifecycle bookkeeping into SQLite request rows. Preserve
  queued commands, finalization-before-resume, and natural-end suppression across
  consumer reconstruction; remove the in-memory session map.
- Remove the legacy transcription-metrics schema upgrade. Fresh databases include
  job and worker identity directly; databases with the older schema are unsupported.
- Rename `RecordingProducer::consume` to `produce` and update runtime and test callers.
- Replace the recording producer's in-memory registry with the connection-local
  `recording_workers` SQLite table. Keep only Rust task/stop handles in indexed
  slots; preserve duplicate suppression, identity-scoped stops, and shutdown cleanup.
- Rename `session_store` to `session_write` and use `writer` for recording
  `SessionWriter` bindings and parameters, distinguishing writing from model stores.
- Replace inference scheduler maps and sets with pool-local SQLite queue rows,
  cancellation fences, and round-robin cursors. Keep PCM/decoders in indexed Rust
  slots; preserve duplicate fencing, exclusive decoder execution, and shutdown drain.
- Size inference workers and recording/shutdown call budgets from available CPU
  threads and RAM at startup. Let every worker serve either pass with live priority,
  removing the single-live-worker bottleneck. Share the decoder thread count with
  CPU sizing and bound unknown/undersized system profiles conservatively.
- Use `TranscriptPass` throughout inference scheduler and queue APIs, keeping numeric
  encoding inside SQLite. Cancel undispatched jobs by recording rows rather than a
  hardcoded pass range, and create round-robin cursors on first use.
- Clarify inference failure ownership: rename pool-wide handling to `fail_queue`
  and its startup/runtime failure state to `terminal_error`; behavior is unchanged.

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
