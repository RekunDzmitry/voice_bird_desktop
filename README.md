# Voice Bird

Terminal-based voice transcription. Runs **locally by default** — your audio never leaves your machine. An optional cloud engine (AssemblyAI) is available for users without a GPU or ANE; it is off by default and requires both an API key and an explicit change to `engine_prefer`. When active, a `CLOUD` badge is shown in the header.

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

### Cloud engines (optional)

If your machine can't keep up with local Whisper, Voice Bird can stream audio to AssemblyAI's Universal-Streaming service instead. This is off by default; when on, a `CLOUD` badge is shown in the header and a reminder appears at the start of each recording.

1. Get an API key from https://www.assemblyai.com/.
2. Open Voice Bird and press `,` to open Settings.
3. Set `Engine preference` to `assemblyai`, paste your key into `AssemblyAI API key`, press `s` to save.

Your key lives in `~/.config/voice-bird/config.toml` in plaintext (chmod `0600` on Unix). Anyone with read access to that file can read your key.

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
| `,` | open settings |
| `q` | quit |
| `?` | help |

## Scope caveats (current release)

- Capture is microphone-only. System-audio loopback (via ScreenCaptureKit on macOS / WASAPI on Windows) is deferred to a follow-up release.
- WhisperKit sidecar must be built by the user (see install notes).

## License

Proprietary.
