# Voice Bird

Terminal-based voice transcription. Runs fully locally — your audio never leaves your machine.

## Install

```bash
cargo install voice-bird
```

Voice Bird downloads Whisper models on first run (defaults to `distil-small.en`, ~250 MB) and caches them under your OS cache directory.

### macOS bonus: ANE-accelerated inference

On Apple Silicon, a bundled WhisperKit sidecar can run Whisper on the Neural Engine. Building it requires a working Swift toolchain (full Xcode or a repaired Command Line Tools install):

```bash
cd whisperkit-helper
swift build -c release
# then copy the binary next to the voice-bird binary:
cp .build/release/voice-bird-whisperkit "$(dirname "$(which voice-bird)")/"
```

Without the sidecar, Voice Bird falls back to `whisper-rs` (whisper.cpp bindings with Metal acceleration) — still fully local, just without ANE.

## Usage

```bash
voice-bird                          # start the TUI
voice-bird --recover <session-dir>  # rebuild transcript.{json,txt} after a crash
```

On first launch you pick a model. Subsequent launches remember your choice (stored in `~/.config/voice-bird/config.toml` on Linux/macOS, `%APPDATA%\voice-bird\config.toml` on Windows).

Recordings live under `~/voice-bird/sessions/<timestamp>-<source>/`:

| File | Content |
|------|---------|
| `audio.wav` | 16 kHz mono |
| `transcript.jsonl` | Append-only log, crash-safe |
| `transcript.json` | Finalized segments + metadata |
| `transcript.txt` | Plain text, one line per segment |
| `meta.json` | Device, model, engine, duration |

## Keys

| Key | Action |
|-----|--------|
| `r` | start recording |
| `s` | stop |
| `m` | change model (first-run picker) |
| `q` | quit |
| `?` | help |

## Scope caveats (current release)

- Capture is microphone-only. System-audio loopback (via ScreenCaptureKit on macOS / WASAPI on Windows) is deferred to a follow-up release.
- WhisperKit sidecar must be built by the user (see install notes).

## License

Proprietary.
