# voice-bird-next

The incremental rewrite of the Voice Bird desktop TUI. It lives beside
`voice-bird-cli` (the shipping app in `../src`) and grows by porting one
piece at a time; the old binary is untouched until this one can replace it.

A block is the unit of interaction. `+` opens a block in the language
picker; arrows move the highlight inside the focused block; Enter resolves
the selected language to its live and refine models. Recording starts only
when both models are installed, or immediately when both already exist in
the persistent cache. Multiple blocks can be at different stages at once;
blocks waiting on the same language share each model download.

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

## Features

- `net` (default) — `reqwest` + `rustls` for the live `HttpDownloader`.
- `--no-default-features` — the full flow exercises the
  `FixtureDownloader` in `testing.rs`; the binary builds but exits with
  a message if launched (a real downloader requires the feature).

## Key table

| key | intent |
|---|---|
| `+` | add a Picking block |
| `←` / `→` | move focus between blocks |
| `↑` / `↓` | move picker highlight in the focused Picking block |
| `Enter` | resolve the selected language and prepare both models |
| `r` / `R` | retry the focused Failed block, reusing any model that finished |
| `Esc` | close the focused block (last waiter on each pending model → cancel signal) |
| `q`, Ctrl-C | quit (always honoured, including mid-download) |

## Layout

| path | role |
|---|---|
| `src/language.rs` | language registry mapping each code to live + refine models |
| `src/picker.rs` | `LanguagePicker` plus the internal model download catalog |
| `src/bus.rs` | `AppEvent` commands/UI events + `EventBus` / `EventSender` |
| `src/state.rs` | `UiState` + `BlockState` + pure reducer |
| `src/ui.rs` | `render(f, &UiState)` — language rows, per-role gauges, borders |
| `src/input.rs` | `map_key(KeyEvent) -> Option<Intent>` |
| `src/db/downloads.rs` | persistent in-flight download claims, progress, and cancellation |
| `src/transcription_models.rs` | format handlers, persistent `CacheDirStore`, staging sweep |
| `src/download.rs` | `Downloader`, `HttpDownloader`, per-model workers, language orchestration |
| `src/producer.rs` | intent-to-command resolution and last-waiter cancellation |
| `src/dispatcher.rs` | command-side collaborator owner and `BeginLanguage` dispatch |
| `src/event_log.rs` | append-only JSONL of every event |
| `src/testing.rs` | render/download/store fixtures used by integration tests |
| `src/main.rs` | terminal guard and event loop (the only file touching a real terminal) |
| `tests/` | render goldens/properties and end-to-end language download flows |

## Growth rules

1. **State is plain data.** No `Instant`, runtime handles, channels or
   `JoinHandle`s in `UiState`. The loop (or later, adapters) writes into it.
2. **Every render fn gets a `render_to_string` test** next to it.
3. **Side effects live behind traits** (audio, engines, cloud), never in
   `UiState`; tests use fixture implementations.
4. **Input flows through the bus.** `input::map_key` returns
   `Option<Intent>`; the resolver in `main.rs` translates to bus events
   and `UiState::apply` folds them in. There is no direct input→state
   mutation.
5. **The producer resolves user intent; the dispatcher owns side effects.**
   `BeginLanguage` carries the target block id and registry profile. The
   dispatcher checks both models, publishes `LanguageSelected` before worker
   events, and deduplicates each per-model claim through SQLite. Closing the
   last waiter cancels only that model's in-flight work.
6. **SQLite owns download lifecycle; `UiState::downloads` is its render
   projection.** Decisions read SQLite. Accepted worker events update the table
   before the UI, and the table publishes `DownloadStatusChanged` after every
   persisted transition. Terminal status events reconcile outcomes that raced
   ahead of `LanguageSelected`, without a second ready/failed cache in
   `UiState`.
7. **No test may touch the network.** `HttpDownloader` is constructed
   only in `main.rs`; everything else uses `FixtureDownloader`.
8. **Installed models outlive sessions.** `CacheDirStore` reuses completed
   live and refine artifacts from `<cache_dir>/voice-bird/models/`.
   SQLite tracks in-flight work; quit cleanup removes staging artifacts only.

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