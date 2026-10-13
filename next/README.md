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
serialized `producer::audio_sources` task using `spawn_blocking` for native
queries. Slow source queries do not block input or Quit; recording shutdown
waits at most three seconds for finalization before restoring the terminal.
Other platforms, or an unavailable/empty device snapshot, open the language
picker directly.
A Rust panic during enumeration disables that catalog for the session; the
current, queued, and future requests open the language picker instead. The
contained panic does not restore or print over the live terminal. This handles
unwinding panics, not process aborts or native crashes.

Arrows move the highlight inside the focused picker; Enter advances a source
step or resolves the selected language to its required assets. Recording
starts only when every required asset is installed, or immediately when they
already exist in the persistent cache. Multiple blocks can be at different stages
at once; blocks waiting on the same language share each asset download. Source
titles show only selections before the current picker step; active blocks use
`id · device · app · language` (app omitted for inputs). Recording captures
real audio on macOS and shows committed and tentative text inside the block.

The registry currently maps `en` to `distil-small.en` for live
transcription and `large-v3-turbo` for refinement. macOS also requires their
compiled Core ML encoders; the distilled model uses its frozen small.en teacher
encoder. Downloads verify pinned checksums and install the bundles beside the
weights automatically. Missing or unloadable encoders fail recording rather than
silently switching backends. Model identifiers remain an implementation detail:
the UI labels gauges `live`, `refine`, `live encoder`, and `refine encoder`.
Required assets are a static slice in `src/language.rs`.

```bash
cargo run  -p voice-bird-next          # empty bordered window, `q` / Ctrl-C to quit
cargo test -p voice-bird-next          # unit + integration tests, ~15 s
cargo test -p voice-bird-next --no-default-features  # offline suite, ~12 s
cargo fmt -p voice-bird-next --check   # enforce package formatting and line wrapping
cargo clippy -p voice-bird-next --all-targets -- -D warnings
```

## Next-only linting

The commands above target only `voice-bird-next`, not the legacy app. The
optional `next/hooks/pre-commit` hook checks rustfmt before strict Clippy with
default features and without default features when staged changes touch `next/`,
the workspace manifest, or the lockfile. It checks working-tree source, not a
staged snapshot. `next/rustfmt.toml` sets a 100-column formatting target; rustfmt
wraps code expressions where possible, not arbitrary strings or comments.
Run `cargo fmt -p voice-bird-next` to apply the required formatting.

Fix dead-code warnings by removing unused code, not by adding
`#[allow(dead_code)]`. Startup cleanup uses `CacheDirStore::sweep_staging`;
quit cleanup uses `ModelStore::discard_inflight`.

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
- `local-engine` (default) — native `whisper-rs`; macOS enables Metal and
  requires verified Core ML encoder loading. Both passes retain the full encoder
  context, flash attention, and two CPU threads per call. Live uses greedy
  decoding with LocalAgreement; refinement uses five-second beam-search
  windows with one second of backward context.
- `--no-default-features` — native-free transcription fixtures and offline
  downloads exercise the full lifecycle without building whisper.cpp.
  The production factory reports `local engine disabled`; the binary still
  exits early without `net` because it cannot download models.

## Recording and local transcripts

On macOS, microphone capture uses `cpal` and app capture uses typed `objc2`
ScreenCaptureKit bindings (macOS 13+). Grant **Microphone** access to Voice Bird
or its hosting terminal. App capture also requires **Screen Recording** access
in System Settings → Privacy & Security; a denied preflight enters Failed with
an actionable message. Restart the hosting app after changing permissions, then
press `r` to retry. App audio is filtered to the selected application; the
hardware output-device label does not restrict its output route. Other
platforms currently report that native audio capture is unavailable.

Each Recording block owns a capture thread and a coordinator. The capture thread
opens, reads, downmixes/resamples to 16 kHz mono, finishes, and drops its native
stream; it keeps draining capture while model states initialize. A shared PCM
buffer supplies independent live/refinement cursors. Immutable chunks are shared
with inference tasks; PCM never enters the event bus. Callback queues remain
bounded to 30 seconds. The shared buffer starts its 30-second overrun limit only
after both live and refinement decoders are loaded. Audio captured during model
initialization is retained without that limit, including time spent waiting for
shared model loading or first-time Core ML compilation. This startup backlog is
exempt while it drains; at most 30 seconds of unread post-ready audio may accumulate.
Once both passes consume the startup backlog, the usual 30-second retention bound
applies. Startup memory therefore grows with decoder initialization time. Active
overruns still fail the block rather than silently dropping speech.

The engine factory caches model weights by canonical path. Each recording/pass
has an exclusive mutable decoder state. A persistent inference pool sizes itself
from available CPU parallelism and total RAM at startup. Every worker serves either
pass, choosing runnable live work before refinement, round-robin across recordings
within each pass. Multiple recordings can run live jobs concurrently; jobs for the
same recording/pass never overlap. Running refinement is not preempted.
Live tasks consume all available audio and decode only the newest hypothesis;
refinement processes ordered five-second windows without skipping audio.
At Quit the pool uses its separate shutdown budget; workers retain the same policy.
The coordinator keeps at most one pending/in-flight task per recording/pass.
Scheduler pending/active identities, dispatch readiness, cancellation fences, worker
occupancy, and per-pass round-robin cursors live in SQLite, not Rust maps or sets.
The pool owns a private in-memory connection protected by its scheduler mutex;
its tables disappear with the pool and never replay unfinished inference.
Rows index reusable Rust slots containing only the private PCM/decoder handoffs.
The separate application database still admits metadata-only transcription requests.
Queue initialization or SQL failures reject pending jobs with their decoder state
instead of starting untracked work.
Scheduler and queue APIs use `TranscriptPass`; numeric pass encoding is private to
the SQLite queue. Cancellation drains the recording's actual undispatched rows,
not a fixed pass range, and leaves admitted jobs to finish. Round-robin cursor rows
are created when a pass first claims work.

`src/producer/inference_limits.rs` derives conservative budgets once per pool:

- Each native decoder call uses two CPU threads, shared with the sizing policy.
- Recording reserves two CPU threads for capture/UI; its CPU call budget is
  `max(1, floor((available_threads - 2) / 2))`, with saturating subtraction.
- Shutdown can use `max(1, floor(available_threads / 2))` CPU calls.
- The RAM call budget is `max(1, floor((total_RAM - 4 GiB) / 2 GiB))`,
  with saturating subtraction. Unknown RAM limits the pool to one call.
- Both call budgets are capped by RAM and an eight-call safety ceiling.
  Persistent worker count equals shutdown capacity; pass assignment is independent
  of worker ID. `InferenceScheduler::limits()` exposes all three values.

For example, eight CPU threads and 16 GiB yield four workers, three recording calls,
and four shutdown calls. An eight-GiB host caps RAM-based concurrency at two.
These are startup resource heuristics, not allocation guarantees or measured
accelerator throughput. The pool does not resize in response to transient load,
free-memory changes, or thermal pressure.

The application runtime owns the persistent recording producer and inference pool.
The connection-local `recording_workers` table owns active block/recording identities
and indexes a producer-owned vector of Rust task handles and atomic stop flags.
SQLite reserves a slot before capture starts; only an exact identity may stop or
retire it. Terminal completion and shutdown release rows and make slots reusable.
Worker identities are not persisted across application restarts.
`RecordingConsumer` publishes `RecordingRequested` and identity-scoped
`RecordingStopRequested` commands instead of borrowing the producer.
Commands return through the log and SQLite gate; connection-local request claims
reject duplicate starts and stale stops, including replay after a session ends.
The same table owns pending start/stop intent, command admission, and terminal
state; `RecordingConsumer` queries SQLite instead of retaining a session map.
Rebuilding the consumer therefore preserves stop/resume coordination and
natural-end suppression. These TEMP rows live for the application connection;
block identities reset on restart, while the recording index remains durable.
The runtime routes accepted commands, transcription requests, and terminal results
to the producer independently of consumers. PCM remains private to workers.
Consumers wait for the old session's terminal acknowledgement before requesting a
replacement; a naturally ended source stays stopped until an explicit resume.
Queued starts received during shutdown fail without opening capture.

Tasks are prepared privately, published as metadata-only requests, and dispatched
only after SQLite admits the exact recording/job. Rejected requests return their
owned state without executing inference. Refined segments replace overlapping
live text in the UI; later live text and dim tentative words remain visible.
The UI retains only the newest 100 segments/lines.

Sessions are stored beneath the application data directory:

```text
~/Library/Application Support/com.RekunDzmitry.voice-bird-next/sessions/
  <YYYY-MM-DD_HH-MM-SS>-b<block>-<mic|app-slug>-r<recording-id>/
    live.jsonl        # fsynced committed live segments, integer millisecond times
    transcript.jsonl  # fsynced committed refined segments
    transcript.txt    # atomic final refined text, or live text if no refinement
    meta.json         # models, language, source, UTC times, duration, status
```

The recording-id suffix prevents collisions when a block resumes in the same
second. No WAV/audio is persisted. SQLite's `recordings` table indexes session
directories and their recording/stopped/failed status; transcript text remains
on disk. Startup marks interrupted jobs and recording rows failed without
inventing final transcript files. Close, model loss, and Quit stop capture and
flush both passes; model readiness resumes in a new directory. Failed capture
or inference can be retried with `r`. Session IDs reject stale UI results after
block reuse while already-admitted job lifecycles remain durable.
Quit pumps requests/results through the log/database gate for up to three seconds.
Timed-out workers are indexed as failed; already-fsynced JSONL remains readable
even without final files. Slow native model initialization or long decoding
on quiet input can exhaust this deadline; pooling does not guarantee a clean
four-stream drain in three seconds.
Scheduler teardown rejects remaining queued jobs and drops worker handles without
joining. Unfinished inference workers detach rather than extend Quit's drain
deadline, including decodes orphaned by a recording write error.
Each detached worker retains its shared queue, PCM, and exclusive decoder until
the current call returns, then releases them and exits without taking another
job. Native decoding is not forcibly interrupted; process exit can terminate
it before completion.

### Transcription telemetry

Every backend task emits `TranscriptionRequested`, `TranscriptionStarted`, and
exactly one `TranscriptionFinished` or `TranscriptionFailed` lifecycle result.
Requests carry the recording, block, globally unique job, pass, audio range,
sample count, enqueue time, and finalization flag. Started/results identify the
worker; job telemetry includes queue wait and pending jobs in its pass.
Cancellation before dispatch has no worker or fabricated inference timings.

`TranscriptTentative` and `TranscriptCommitted` identify their job and worker
and include a `transcription` object in the JSONL event log:

```json
{"start_ms":1088,"end_ms":1418,"duration_ms":330,"queue_size_start":0,"queue_size_end":5290}
```

- `start_ms` / `end_ms`: monotonic milliseconds since native capture opened,
  distinct from a committed segment's audio timestamps `t_start_ms` / `t_end_ms`.
- `duration_ms`: `end_ms - start_ms` around a task's `feed_chunks` and optional
  `finish`. Includes inference and text processing, not model loading, capture
  reads, event publication, or transcript disk writes. Tasks that reuse a
  hypothesis without decoding can take zero milliseconds.
- `queue_size_start` / `queue_size_end`: pending audio for this recording/pass,
  normalized to **16 kHz mono sample counts**, sampled before and after the task.
  Divide by 16 for backlog milliseconds. Excludes the current task, resampler
  lookahead, and retained model context; capture may grow the queue during inference.

All transcript events produced by one task share its metrics; do not sum their
durations as separate inference work. SQLite's `transcription_jobs` stores every
admitted task, including inference that produced no text, with queued/running/
finished/failed status and immutable request metadata. Duplicate, mismatched, or
out-of-order lifecycle events cannot reactivate a terminal job. Started worker
identity fences its completion. Recovery fails interrupted queued/running jobs
without inventing an end time or a worker for a queued task.

`transcription_metrics` stores one row per accepted transcript event: recording,
job, worker, kind (`tentative` / `committed`), pass (`live` / `refined`), optional
committed audio timestamps, and the five numeric fields above. Tentative rows
always use `live` and NULL audio timestamps. Transcript text remains on disk.
Stale and terminal recording transcript events cannot insert rows. Existing
databases migrate additively; historical metric rows retain NULL job/worker
instead of backfilled identities.

```sql
SELECT recording_id, job_id, pass, worker, status, duration_ms, queue_wait_ms,
       queue_jobs_start, queue_jobs_end, queue_size_start, queue_size_end, error
FROM transcription_jobs
ORDER BY rowid DESC
LIMIT 20;
```

Opt-in real-engine smoke, with both English weights and required macOS encoder
bundles already installed and the bundled short WAV (no downloads):

```bash
cargo test -p voice-bird-next --test engine_smoke -- --ignored --nocapture
```

## Key table

| key | intent |
|---|---|
| `+` | add a block at the device picker on macOS, otherwise language picker |
| `←` / `→` | move focus between blocks |
| `↑` / `↓` | move picker highlight in the focused block |
| `Enter` | select device/app, or resolve language and prepare all required assets |
| `Backspace` | go back one source picker step (language → app/device; app → device) |
| `r` / `R` | retry the focused Failed block, reusing any model that finished |
| `Esc` | close the focused block (last waiter on each pending model → cancel signal) |
| `q`, Ctrl-C | quit (always honoured, including mid-download) |

## Layout

| path | role |
|---|---|
| `src/language.rs` | language registry mapping codes to live/refine models and required assets |
| `src/audio_sources.rs` | shared source types, `AudioSourcesCatalog`, and native macOS catalog |
| `src/picker.rs` | shared `ListPicker` selection state for device, app, and language lists; session menu and model download catalog |
| `src/bus.rs` | `AppEvent` commands/UI events + Tokio unbounded `EventBus` / synchronous `EventSender` |
| `src/consumer/ui_view.rs` | `UiView` + `BlockState` + pure reducer |
| `src/producer/model_watch.rs` | startup/tick disk checks for the entire catalog, persisted in SQLite |
| `src/ui.rs` | `render(f, &UiView)` — language rows, per-role gauges, borders |
| `src/input.rs` | `map_key(KeyEvent) -> Option<Intent>` |
| `src/db/downloads.rs` | persistent in-flight download claims, progress, and cancellation |
| `src/db/models.rs` | observed model availability, independent of download lifecycle |
| `src/db/model_staging.rs` | exact attempt-scoped staging paths and preparation errors observed by the store |
| `src/db/block_steps.rs` | session-local source step/revision compare-and-set gate |
| `src/db/recordings.rs` | durable session index, recording-ID gate, and interrupted recording recovery |
| `src/db/transcription_jobs.rs` | durable inference admission, lifecycle fencing, and interrupted-job recovery |
| `src/db/inference_queue.rs` | pool-local pending/active identities, readiness, cancellation fences, and round-robin cursors |
| `src/db/transcription_metrics.rs` | per-transcript-event metrics linked to job/worker identities |
| `src/transcription_models.rs` | format handlers, persistent `CacheDirStore`, staging sweep |
| `src/audio/` | native capture, bounded callback bridge, shared PCM buffer/cursors, and 16 kHz resampling |
| `src/transcribe/` | shared Whisper weights, isolated decoder states, verified Core ML loading, windows, and LocalAgreement |
| `src/session_write.rs` | `SessionWriter`: fsynced committed JSONL and atomic final transcript/metadata |
| `src/download.rs` | shared download errors, async `Downloader`, and HTTP streaming transport |
| `src/producer/audio_sources.rs` | serialized native queries, session panic containment, and source-result events |
| `src/producer/downloads.rs` | fetch workers, progress throttling, and download-result events |
| `src/producer/model_store.rs` | blocking installation workers and install-result events |
| `src/producer/input.rs` | intent-to-command resolution and last-waiter cancellation |
| `src/producer/recording.rs` | capture threads, per-recording coordinators, stop flags, and bounded shutdown |
| `src/producer/inference.rs` | private tasks, shared live-priority workers, hardware-derived concurrency, and lifecycle events |
| `src/producer/inference_limits.rs` | CPU/RAM-derived startup worker, recording, and shutdown budgets |
| `src/producer/mod.rs` | external input and background worker services |
| `src/consumer/mod.rs` | `Consumer` routes accepted events to its independent `Consumers` |
| `src/consumer/audio_sources.rs` | accepted source-request handoff to the enumeration producer |
| `src/consumer/language.rs` | stateless SQLite availability/attempt queries and follow-up model requests |
| `src/consumer/downloads.rs` | attempt-aware SQLite claims, request reconciliation, and fetch handoff |
| `src/consumer/model_store.rs` | staging preparation, installation eligibility/handoff, and quit-time staging sweep |
| `src/consumer/recording.rs` | reconcile visible/hidden blocks against SQLite lifecycle state and publish identity-scoped start/stop commands |
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
5. **Consumers decide; producers execute background work.** `Consumer` owns
   `Consumers` and routes accepted events to the UI projection and event-specific
   handlers. Consumers retain lifecycle, attempt, and staging decisions, then hand
   work to `producer::audio_sources`, `producer::downloads`, or `producer::model_store`.
   Those producers own task spawning and every worker progress/result publication.
   Consumers may publish synchronous commands or decision failures; those events
   also cross the bus/log/SQLite gate. No handler calls another consumer to advance a flow.
   While a block is `PickingLanguage`, input produces `BeginLanguage`.
   The language consumer publishes `LanguageSelected` and model requests.
   Those requests must cross the bus/log/SQLite gate before the download
   consumer claims work. Cached models lead straight to Recording; otherwise
   download success events move Waiting blocks to Recording.
   `DownloadRequested` carries the intended attempt: a terminal result that
   overtakes a join is reconciled, not silently retried. Superseded requests
   and old-attempt terminal notifications/replays are rejected. An explicit retry requests the
   next attempt. A cancelled/interrupted late join enters Failed rather than
   waiting forever. Requests without waiters or after Quit launch no work.
   Duplicate valid requests share one SQLite claim. Closing the last waiter
   cancels only that model's in-flight work.
   Downloads read their exact attempt's staging path from SQLite. The watcher
   prepares paths at startup/ticks; the store consumer also prepares new requests
   before claiming, so immediate retries need no intervening tick. Preparation
   failures fail waiters without fetching. Verified fetches publish
   `DownloadFetched`; its gate atomically claims Installing and rejects duplicate,
   stale, cancelled, or terminal handoffs. Only then does the store consumer
   hand verified work to the installation producer, which runs on the blocking
   pool and publishes the outcome. Fast GGUF installation keeps the download
   gauge; slow package installation shows its own phase. Producers own all task
   creation: asynchronous work uses Tokio, native enumeration/installation use
   `spawn_blocking`, and capture plus the persistent inference pool use dedicated
   threads. Native capture objects are created and dropped on their owning thread.
6. **SQLite is the source of truth; `UiView::downloads` is its render
   projection.** Every event enters the JSONL log before the database gate.
   Accepted worker events update the table before the view, and the table
   publishes `DownloadStatusChanged` after every persisted transition.
   Installation success commits the Succeeded row and model availability in one
   immediate transaction. Availability-write or commit failures roll back both;
   the lifecycle notification is published only after commit, never for rolled-back success.
   Each download projection stores its attempt. Newer requests reset its gauge;
   same-attempt joins preserve progress. Since SQLite accepts a whole batch before
   consumers claim new work, the projection also rejects mismatched worker events
   and terminal status replays: an accepted old cancellation/failure cannot hide
   a newer gauge or fail its waiters. Attempt-zero pre-claim errors remain
   actionable; terminal replay without an active gauge still reconciles late joins.
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
   SQLite tracks availability separately from in-flight work; quit cleanup
   removes staging artifacts only. Before accepting input, the model watcher
   refreshes every catalog model from disk, correcting stale persisted rows.
   The loop repeats that scan every 100 ms, including unused models.
   The watcher has no `UiView` dependency: inserted or changed availability rows
   publish `ModelAvailabilityChanged`; unchanged ticks publish nothing.
   SQLite rejects observations that no longer match persisted availability.
   Consumers select interested sessions and publish `ModelMissing` through the
   bus before the view changes or a language consumer requests shared work.
   `LanguageConsumer` has no model-store dependency: it queries the availability
   table, then download history only for missing models. Accepted installation
   successes update availability before consumption; rejected stale successes
   cannot change it. A missing ready model sends every affected block, including
   hidden sessions and the ready subset of Waiting blocks, back to Waiting and
   re-claims a shared download; pending, picker, and Failed sessions launch no
   automatic work. Duplicate notifications and observations after Quit do not
   launch work. Success resumes Recording automatically; failure enters Failed,
   where `r` retries.
   Presence checks do not detect corruption of files that still exist.
   Missing assets stop real capture; readiness resumes with fresh decoder states
   and a distinct session directory after the previous coordinator finalizes.
   Async downloads use a separate SQLite `CancelProbe` connection; it polls
   the current attempt's row every 50 ms between streamed chunks. Quit stops
   recording workers and allows at most three seconds for both transcription
   passes and disk finalization. Terminal recording events still cross the
   log/database gate. Stalled workers are marked failed at the deadline.
   Quit also marks active downloads Cancelling and consumes `DiscardInflight`
   through the same gate. The model-store consumer sweeps `.part` files and
   `.tmp` directories, preserving installed models; remaining background work
   never holds terminal restoration indefinitely.

## Refreshing the golden snapshot

```bash
UPDATE_SNAPSHOTS=1 cargo test -p voice-bird-next --test render_smoke
git diff next/tests/snapshots/   # review before committing
```

## Honest dependencies

This crate pulls `reqwest` + `rustls` through default-on `net`, plus
whisper.cpp through default-on `local-engine`. Cold builds include native
CMake/C++ work; they are not seconds-long. `--no-default-features` excludes
both the network transport and native transcription, keeping lifecycle,
agreement, resampling, and storage tests offline and comparatively fast.

On macOS, source enumeration uses `cpal` for audio devices and typed
`objc2-app-kit` bindings for `NSWorkspace`/`NSRunningApplication`. Objective-C
objects use automatic retain/release with a scoped autorelease pool. App
enumeration does not require Screen Recording permission. Only the required
AppKit binding features are enabled; the old `objc` 0.2 dependency and its
`cargo-clippy` configuration allowance are not used by `next`.