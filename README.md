<div align="center">

# 🐦 Voice Bird CLI

**Live voice transcription in your terminal — local-first, multi-source, crash-safe.**

[![crates.io](https://img.shields.io/crates/v/voice-bird-cli?logo=rust&label=crates.io)](https://crates.io/crates/voice-bird-cli)
[![npm](https://img.shields.io/npm/v/voice-bird-cli?logo=npm&label=npm)](https://www.npmjs.com/package/voice-bird-cli)
[![PyPI](https://img.shields.io/pypi/v/voice-bird-cli?logo=pypi&label=PyPI)](https://pypi.org/project/voice-bird-cli/)
[![license: personal use](https://img.shields.io/badge/license-personal%20use-blue)](LICENSE)

![Voice Bird CLI basic flow](docs/assets/basic-flow.svg)

</div>

Voice Bird CLI is a terminal app (TUI) that transcribes audio as you speak — from a microphone, your system output, or a single application — and streams the text live into your terminal. On macOS and Linux it runs **locally by default** with Whisper models, so recordings never leave your machine. Cloud mode (via [VoiceBird Web](https://voicebird.app)) is opt-in per source, and is the only mode on Windows.

## Highlights

- **Local-first** — Whisper inference on your machine; audio and transcripts stay on disk under your control.
- **Multiple sources, side by side** — transcribe a mic and an app (e.g. a browser meeting tab) at the same time, each in its own transcript slot.
- **Live streaming text** — tentative words appear as you speak and are committed as the engine settles, like a live captioner.
- **Hardware-aware** — on first launch it inspects your machine and picks a sensible default model (`distil-small.en`); swap models any time with `m`, including NVIDIA's `nemotron-3.5-asr-streaming-0.6b`.
- **Apple Silicon acceleration** — Metal GPU by default, plus an optional WhisperKit sidecar for ANE-accelerated inference.
- **Crash-safe sessions** — every segment is appended to a JSONL log the moment it's committed; `--recover` rebuilds the final transcript after a crash.
- **Cloud mode when you want it** — stream to your VoiceBird Web account for cloud language support or when local hardware can't keep up.

## Platform support

| Platform | Local transcription | Cloud mode | Notes |
| --- | :---: | :---: | --- |
| macOS | ✅ | ✅ | Metal + CoreML; optional WhisperKit sidecar; system & app audio via ScreenCaptureKit |
| Linux | ✅ | ✅ | whisper-rs (whisper.cpp) |
| Windows | — | ✅ | **Cloud-only since 0.4.0** — requires a Voice Bird API key; see [Windows install guide](docs/windows-install.md) |

## Getting started

Install (any one of these — they all deliver the same native binary):

```bash
cargo install voice-bird-cli      # native Rust install
npm install -g voice-bird-cli     # wrapper: installs the Cargo binary
pipx install voice-bird-cli       # wrapper: installs the Cargo binary
```

Then run:

```bash
voice-bird-cli
```

On first launch:

- **macOS / Linux** — Voice Bird picks a local Whisper model for your hardware and downloads it into your OS cache directory. No account or API key needed.
- **Windows** — Voice Bird prompts for your Voice Bird API key (cloud-only; there are no local models).

From there, the basic flow is:

1. Pick a **microphone**, **system-output loopback device**, or **app** audio source with `↑`/`↓` and `←`/`→`.
2. Optionally press `c` to use cloud mode for that source.
3. Press `Enter` to start a transcription slot.
4. Watch committed and tentative text stream into the TUI.
5. In local mode, find the session files under `~/voice-bird/sessions/<timestamp>-<source>/`.

> **macOS:** capturing system or app audio uses ScreenCaptureKit, which may ask you to grant **Screen Recording** permission the first time.

## Install

### Cargo (recommended)

Installs the native Rust binary directly:

```bash
cargo install voice-bird-cli
```

### npm / PyPI

Both packages are thin wrappers that install and run the Cargo binary, so **Rust (`cargo`) must be on the machine**:

```bash
npm install -g voice-bird-cli
# or
pipx install voice-bird-cli
# or
pip install voice-bird-cli
```

### From source

```bash
git clone https://github.com/RekunDzmitry/voice_bird_desktop.git
cd voice_bird_desktop
cargo install --path .
```

### Windows

Windows is **cloud-only since 0.4.0**: local Whisper inference is not built there, so installing needs only the standard Rust MSVC toolchain — **no CMake, no LLVM, no whisper.cpp compile**. In short:

1. Install [rustup](https://rustup.rs) (default MSVC toolchain) and the Visual Studio Build Tools (for `link.exe`).
2. Run `cargo install voice-bird-cli` (or the npm/pipx wrapper) — ideally from an *x64 Native Tools Command Prompt* so the correct `link.exe` is found.
3. On first launch, paste your Voice Bird API key when prompted.

Full walkthrough and troubleshooting: [docs/windows-install.md](docs/windows-install.md).

## The TUI

The interface is split into source panes (microphones, system output, running apps) and transcript slots. Each started source occupies a slot showing its engine, status, and live transcript. Several sources can record at once — for example your mic in one slot and a meeting app's audio in another.

### Keys

| Key | Action |
| --- | --- |
| `↑` / `↓` | Select device or app |
| `←` / `→` | Move between panes |
| `Enter` | Start the selected source |
| `Space` | Clear selected app pairing |
| `Tab` | Move between transcript slots |
| `r` | Refresh devices and apps |
| `c` | Toggle cloud mode for the focused source |
| `l` | Change language for cloud mode |
| `m` | Change model |
| `e` | Export the latest local transcript |
| `p` | Change local session path |
| `x` | Clear stopped transcript slot |
| `q` | Quit |
| `?` | Help |

On Windows, `c` opens the API-key dialog (cloud is always on), and the local-only keys `m`, `e`, and `p` are not available.

## Local and cloud modes

**Local mode** (default on macOS/Linux) runs Whisper inference on your machine — `whisper-rs`/whisper.cpp everywhere, or WhisperKit on Apple Silicon when the sidecar is built:

```bash
cargo run -p xtask -- build-sidecar
```

Local mode never sends audio to a server and writes session artifacts to disk. The bundled local models are English-focused.

**Cloud mode** streams audio to VoiceBird Web (`wss://voicebird.app/api/audio/stream`). It requires a Voice Bird API key and is useful when local hardware can't keep up or when you want cloud language support. Cloud recordings live in your VoiceBird Web account instead of the local sessions folder. On macOS/Linux it's opt-in per source; on Windows it's the only mode.

## Session files

Each local recording produces a session directory:

| File | Content |
| --- | --- |
| `audio.wav` | 16 kHz mono recording |
| `transcript.jsonl` | Append-only transcript log, written as segments commit (crash-safe) |
| `transcript.json` | Finalized transcript segments and metadata |
| `transcript.txt` | Plain-text transcript |
| `meta.json` | Device, source, model, engine, and duration |

If the app (or your machine) dies mid-recording, the JSONL log survives. Rebuild the final transcript with:

```bash
voice-bird-cli --recover <session-dir>
```

## Configuration

Settings live in a single TOML file:

| Platform | Path |
| --- | --- |
| macOS / Linux | `~/.config/voice-bird/config.toml` |
| Windows | `%APPDATA%\voice-bird\config.toml` |

The Voice Bird API key is stored in plaintext in `config.toml`; on Unix the app sets the file to `0600` best-effort. Don't commit or share this file.

## Agent targets (Kafka)

Committed transcript segments can be fanned out to a Kafka topic so a downstream agent can consume them live. Add a target from the Targets pane (`a` to add, `e` to edit, `d` to delete) — a step-by-step form collects the broker endpoint, topic, `acks` level, and security settings, then verifies the connection with a produce/consume round trip before saving.

Targets are stored in `config.toml` as `[[agent_targets]]` rows:

```toml
[[agent_targets]]
id = "b9c1…"                      # minted by the TUI
name = "prod-events"
kind = "kafka"
endpoint = "broker-1:9093,broker-2:9093"
topic = "voice-bird-events"
acks = "all"                      # all | one | zero
security_protocol = "sasl_ssl"    # plaintext | ssl | sasl_plaintext | sasl_ssl
sasl_mechanism = "scram-sha-256"  # plain | scram-sha-256 | scram-sha-512
sasl_username = "svc-voicebird"
sasl_password_env = "VOICE_BIRD_KAFKA_PASSWORD"
```

Voice transcripts are sensitive — use `sasl_ssl` (or at least `ssl`) for any broker that isn't localhost. The SASL password is **never stored in the config file**: `sasl_password_env` names an environment variable, and the password is read from it when the connection is opened. Export it before launching:

```bash
export VOICE_BIRD_KAFKA_PASSWORD=…
voice-bird-cli
```

`security_protocol` defaults to `plaintext` when omitted, which keeps configs from older versions working unchanged. GSSAPI/Kerberos is not supported (it would require a system libsasl2 and break the self-contained static binary); librdkafka is linked statically with vendored OpenSSL, so TLS and SASL work without any Homebrew/system packages.

## Development

```bash
cargo build            # debug build
cargo test             # unit + integration tests (mock engines, no downloads)
cargo test --features engine-smoke   # real-engine smoke tests (downloads tiny.en)
cargo run -p xtask -- build-sidecar  # build the macOS WhisperKit sidecar
```

Issues and questions are welcome on the [issue tracker](https://github.com/RekunDzmitry/voice_bird_desktop/issues).

## License

Voice Bird CLI is **free for personal use** — use it, modify it, enjoy it on your own machines without limitations.

Any other use — commercial use, use within an organization, redistribution, repackaging, or offering it as a service — **requires prior written permission** from the copyright holder. Open an issue on this repository to ask; reasonable requests are welcome.

See [LICENSE](LICENSE) for the full terms.
