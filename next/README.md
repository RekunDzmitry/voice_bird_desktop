# voice-bird-next

The incremental rewrite of the Voice Bird desktop TUI. It lives beside
`voice-bird-cli` (the shipping app in `../src`) and grows by porting one
piece at a time; the old binary is untouched until this one can replace it.

A block is the unit of interaction. `+` opens a block in model-picker
mode; arrows move the highlight inside the focused block; Enter runs
the resolver, which downloads the model on the first time it's needed
(or starts recording immediately when the weights are already on disk).
Multiple blocks can be at multiple stages at once — two blocks on the
same in-flight model share one download and one progress bar.

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
| `Enter` | run the resolver on the focused Picking block |
| `r` / `R` | retry the focused Failed block |
| `Esc` | close the focused block (last waiter on its model → cancel signal) |
| `q`, Ctrl-C | quit (always honoured, including mid-download) |

## Layout

| path | role |
|---|---|
| `src/bus.rs`     | `AppEvent` enum (incl. download events) + `EventBus` / `EventSender` |
| `src/state.rs`   | `UiState` + `BlockState` + pure reducer |
| `src/ui.rs`      | `render(f, &UiState)` — picks, gauges, borders, all in-block |
| `src/input.rs`   | `map_key(KeyEvent) -> Option<Intent>` |
| `src/store.rs`   | `DownloadRepository` trait + `InMemoryDownloadRepository` + `apply` |
| `src/model_store.rs` | format handlers (`GgufHandler`, `NemotronPackageHandler`), `CacheDirStore`, staging sweep |
| `src/download.rs` | `Downloader`, `HttpDownloader`, `Throttle`, `CancelRegistry`, `begin` + `spawn` |
| `src/event_log.rs` | append-only JSONL of every event |
| `src/testing.rs` | `render_to_string`, `FixtureStore`, `FixtureDownloader` (compiled unconditionally so `tests/` can use them) |
| `src/main.rs`    | terminal guard + event loop + resolver (the only file touching a real terminal) |
| `tests/`         | `render_smoke` (goldens + proptest never-panics) and `download_flow` (end-to-end, no network) |

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
5. **The resolver is the only place that decides what to download.** It
   is the dedup point (one thread per model), the presence check
   (record or instantiate), and the cancel signal (clear staging when
   the last waiter closes). Tests bypass it and drive events directly.
6. **Two representations of download state** (repository + `UiState::downloads`)
   are kept honest by a convention: decisions read the repository,
   renders read `UiState`. Both fold from the same drained events.
7. **No test may touch the network.** `HttpDownloader` is constructed
   only in `main.rs`; everything else uses `FixtureDownloader`.

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