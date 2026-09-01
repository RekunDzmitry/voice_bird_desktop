# voice-bird-next

The incremental rewrite of the Voice Bird desktop TUI. It lives beside
`voice-bird-cli` (the shipping app in `../src`) and grows by porting one
piece at a time; the old binary is untouched until this one can replace it.

Depends only on `ratatui` + `crossterm`, so it builds and tests
in seconds — no whisper-rs / cpal / rdkafka.

```bash
cargo run  -p voice-bird-next          # empty bordered window, `q` / Esc / Ctrl-C to quit
cargo test -p voice-bird-next          # unit + integration tests
cargo clippy -p voice-bird-next --all-targets -- -D warnings
```

## Layout

| path | role |
|---|---|
| `src/state.rs`   | `UiState` — plain data the UI draws from |
| `src/ui.rs`      | `render(f, &UiState)` + unit tests per render fn |
| `src/input.rs`   | `handle_key(&mut UiState, KeyEvent)` + tests |
| `src/testing.rs` | `render_to_string(state, w, h)` via `TestBackend`, shared by all tests |
| `src/main.rs`    | terminal guard + event loop — the only file touching a real terminal |
| `tests/`         | integration tests: golden snapshot, proptest "never panics at any size" |

## Growth rules

1. **State is plain data.** No `Instant`, runtime handles, channels or
   `JoinHandle`s in `UiState`. The loop (or later, adapters) writes into it.
2. **Every render fn gets a `render_to_string` test** next to it, ported
   from the old `src/ui.rs` `mod tests` when the fn is ported.
3. **Side effects live behind traits** (audio, engines, cloud), never in
   `UiState`; tests use fixture implementations.

## Refreshing the golden snapshot

```bash
UPDATE_SNAPSHOTS=1 cargo test -p voice-bird-next --test render_smoke
git diff next/tests/snapshots/   # review before committing
```
