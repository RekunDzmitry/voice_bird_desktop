# voice-bird-next

The incremental rewrite of the Voice Bird desktop TUI. It lives beside
`voice-bird-cli` (the shipping app in `../src`) and grows by porting one
piece at a time; the old binary is untouched until this one can replace it.

A block is the unit of interaction. On macOS, `+` first opens a real audio
device picker. Input microphones lead directly to language selection; output
speakers require a running app first (there is no all-apps option). Backspace
walks back one picker step, restoring the selected device or app row. An empty
app list shows `no running apps`; go back or close the block. An
`AudioSourceSnapshot` captures the enumerated devices and running apps on a
serialized Tokio task using `spawn_blocking` for native queries. Slow queries
do not block input or Quit; runtime shutdown never waits for stalled work.
Other platforms, or an unavailable/empty device snapshot, open the language
picker directly.
A Rust panic during enumeration disables that catalog for the session; the
current, queued, and future requests open the language picker instead. The
contained panic does not restore or print over the live terminal. This handles
unwinding panics, not process aborts or native crashes.

Arrows move the highlight inside the focused picker; Enter advances a source
step or resolves the selected language to its live and refine models. Recording
starts only when both models are installed, or immediately when both already
exist in the persistent cache. Multiple blocks can be at different stages at
once; blocks waiting on the same language share each model download. Source
titles show only selections before the current picker step; active blocks use
`id · device · app · language` (app omitted for inputs). Device and app selection
is real; recording is still mocked.

The registry currently maps `en` to `distil-small.en` for live
transcription and `large-v3-turbo` for refinement. Model identifiers remain
an implementation detail: the UI shows the language code and labels download
progress by `live` / `refine` role. Adding a language is one row in
`src/language.rs`.

```bash
cargo run  -p voice-bird-next          # empty bordered window, `q` / Ctrl-C to quit
cargo test -p voice-bird-next          # unit + integration tests, ~15 s
cargo test -p voice-bird-next --no-default-features  # offline suite, ~12 s
cargo clippy -p voice-bird-next --all-targets -- -D warnings
```

## Next-only linting

The commands above target only `voice-bird-next`, not the legacy app. The
optional `next/hooks/pre-commit` hook runs strict Clippy with default features
and without default features when staged changes touch `next/`, the workspace
manifest, or the lockfile. It checks working-tree source, not a staged snapshot.

To opt in from the repository root:

```bash
git config --show-origin --get core.hooksPath
git config --local core.hooksPath next/hooks
```

Setting `core.hooksPath` replaces an existing hooks path and applies to all
worktrees. If you already have hooks, call `sh next/hooks/pre-commit` from the
existing pre-commit hook instead. Hooks are local and bypassable; CI running
the same package-scoped Clippy commands provides pull-request enforcement.

## Features

- `net` (default) — `reqwest` + `rustls` for the live `HttpDownloader`.
- `--no-default-features` — the full flow exercises the
  `FixtureDownloader` in `testing.rs`; the binary builds but exits with
  a message if launched (a real downloader requires the feature).

## Key table

| key | intent |
|---|---|
| `+` | add a block at the device picker on macOS, otherwise language picker |
| `←` / `→` | move focus between blocks |
| `↑` / `↓` | move picker highlight in the focused block |
| `Enter` | select device/app, or resolve language and prepare both models |
| `Backspace` | go back one source picker step (language → app/device; app → device) |
| `r` / `R` | retry the focused Failed block, reusing any model that finished |
| `Esc` | close the focused block (last waiter on each pending model → cancel signal) |
| `q`, Ctrl-C | quit (always honoured, including mid-download) |

## Layout

| path | role |
|---|---|
| `src/language.rs` | language registry mapping each code to live + refine models |
| `src/producer/sources.rs` | audio source snapshots, `AudioSourcesCatalog`, and macOS enumeration |
| `src/picker.rs` | shared `ListPicker` selection state for device, app, and language lists; session menu and model download catalog |
| `src/bus.rs` | `AppEvent` commands/UI events + Tokio unbounded `EventBus` / synchronous `EventSender` |
| `src/consumer/ui_view.rs` | `UiView` + `BlockState` + pure reducer |
| `src/producer/model_watch.rs` | tick-driven presence checks for installed models used by active blocks |
| `src/ui.rs` | `render(f, &UiView)` — language rows, per-role gauges, borders |
| `src/input.rs` | `map_key(KeyEvent) -> Option<Intent>` |
| `src/db/downloads.rs` | persistent in-flight download claims, progress, and cancellation |
| `src/db/block_steps.rs` | session-local source step/revision compare-and-set gate |
| `src/transcription_models.rs` | format handlers, persistent `CacheDirStore`, staging sweep |
| `src/producer/download.rs` | async `Downloader`, HTTP streaming, and progress throttling |
| `src/producer/input.rs` | intent-to-command resolution and last-waiter cancellation |
| `src/producer/mod.rs` | external input and transport modules; no consumer-owned producer aggregate |
| `src/consumer/mod.rs` | `Consumer` routes accepted events to its independent `Consumers` |
| `src/consumer/audio_sources.rs` | serialized audio-source requests and session panic containment |
| `src/consumer/language.rs` | language availability checks and follow-up model requests |
| `src/consumer/downloads.rs` | attempt-aware SQLite claims, download workers, and staging cleanup |
| `src/event_log.rs` | append-only JSONL of every event |
| `src/testing.rs` | render/download/store fixtures used by integration tests |
| `src/main.rs` | terminal guard and Tokio `select!` over input, bus events, and 100 ms ticks |
| `tests/` | render goldens/properties and end-to-end language download flows |

## Growth rules

1. **The UI view is plain data.** No `Instant`, runtime handles, channels or
   `JoinHandle`s in `UiView`. The consumer projects accepted events into it.
2. **Every render fn gets a `render_to_string` test** next to it.
3. **Side effects live behind traits** (audio, engines, cloud), never in
   `UiView`; tests use fixture implementations.
4. **Input flows through the bus.** `input::map_key` returns
   `Option<Intent>`; `producer/input.rs` reads focus/cursors from the view and
   domain facts from SQLite, then publishes events. There is no direct
   input→view mutation.
5. **Consumers can produce the next stage of a flow.** `Consumer` owns
   `Consumers`, not producer handles. It routes events to the UI projection,
   audio-source, language, and download consumers. Each handler owns its
   collaborators; it never calls another consumer to advance the flow.
   While a block is `PickingLanguage`, input produces `BeginLanguage`.
   The language consumer publishes `LanguageSelected` and model requests.
   Those requests must cross the bus/log/SQLite gate before the download
   consumer claims work. Cached models lead straight to Recording; otherwise
   download success events move Waiting blocks to Recording.
   `DownloadRequested` carries the intended attempt: a terminal result that
   overtakes a join is reconciled, not silently retried. Superseded requests
   and stale terminal replays are rejected. An explicit retry requests the
   next attempt. A cancelled/interrupted late join enters Failed rather than
   waiting forever. Requests without waiters or after Quit launch no work.
   Duplicate valid requests share one SQLite claim. Closing the last waiter
   cancels only that model's in-flight work.
   No service calls `std::thread::spawn`: Tokio schedules asynchronous work;
   native enumeration and installation use `spawn_blocking`.
6. **SQLite is the source of truth; `UiView::downloads` is its render
   projection.** Every event enters the JSONL log before the database gate.
   Accepted worker events update the table before the view, and the table
   publishes `DownloadStatusChanged` after every persisted transition.
   Terminal status events reconcile outcomes that raced ahead of
   `LanguageSelected`, without a second ready/failed cache in `UiView`.
   Source transitions are CAS-gated the same way: `SourceStepChanged` names the
   block, from/to steps, expected revision, and resulting selections. SQLite
   increments the revision on every forward/back edge; duplicate and stale
   transitions publish `SourceStepRejected`. Source `BeginLanguage` atomically
   commits Language → Committed before consumption, so a racing Backspace cannot
   orphan a started download. Only accepted events reach the consumer.
   Block steps live in a connection-local SQLite TEMP table: another app
   instance cannot wipe or collide with them, and disconnect discards them.
   `BlockClosed` carries its target id and deletes the step row through the
   same event gate, after preceding transitions and before the reducer frees
   the id. Gate errors are logged and never applied to the UI. Retry remains
   ungated. Retained selections restore cursors on back steps; titles hide
   selections at or after the current step. Row inspection stays in unit tests.
7. **No test may touch the network.** `HttpDownloader` is constructed
   only in `main.rs`; everything else uses `FixtureDownloader`.
8. **Installed models outlive sessions.** `CacheDirStore` reuses completed
   live and refine artifacts from `<cache_dir>/voice-bird/models/`.
   SQLite tracks in-flight work; quit cleanup removes staging artifacts only.
   The loop's 100 ms Tokio interval calls the model-watch producer, including
   hidden sessions and ready models in Waiting blocks. A missing model sends
   every affected block back to Waiting and re-claims a shared download; success
   resumes Recording automatically. Failure enters Failed, where `r` retries.
   Presence checks do not detect corruption of files that still exist, and
   Recording remains mocked (no real audio device is stopped yet).
   Async downloads use a separate SQLite `CancelProbe` connection; it polls
   the current attempt's row every 50 ms between streamed chunks. Quit marks
   active rows Cancelling, consumes staging-cleanup commands through the same
   log/database gate, and calls `shutdown_background` without joining workers.

## Refreshing the golden snapshot

```bash
UPDATE_SNAPSHOTS=1 cargo test -p voice-bird-next --test render_smoke
git diff next/tests/snapshots/   # review before committing
```

## Honest dependencies

This crate now pulls `reqwest` + `rustls` (via the `net` feature, default-on)
for the live downloader. A cold `cargo build -p voice-bird-next` is no longer
"seconds" if you haven't built the workspace before — it goes from seconds
to minutes because `hyper`/`h2`/`tokio`/`ring` add ~90 crates. The mitigation
is the `net` feature: `cargo test --no-default-features` exercises the entire
flow through the fixture downloader without ever touching the network, and
finishes in seconds.

On macOS, source enumeration uses `cpal` for audio devices and typed
`objc2-app-kit` bindings for `NSWorkspace`/`NSRunningApplication`. Objective-C
objects use automatic retain/release with a scoped autorelease pool. App
enumeration does not require Screen Recording permission. Only the required
AppKit binding features are enabled; the old `objc` 0.2 dependency and its
`cargo-clippy` configuration allowance are not used by `next`.