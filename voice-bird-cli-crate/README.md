# Voice Bird CLI

Terminal-based audio streaming for voice transcription.

## Installation

### Using cargo-binstall (Recommended)

```bash
# Install cargo-binstall if you don't have it
cargo install cargo-binstall

# Install Voice Bird CLI
cargo binstall voice-bird-cli
```

### Manual Download

Download pre-built binaries from [GitHub Releases](https://github.com/RekunDzmitry/voice-bird-releases/releases).

## Features

- Enumerate audio devices and running applications
- Capture per-application audio (Windows/macOS)
- Stream audio to Voice Bird server for transcription
- Real-time audio level visualization
- Interactive TUI interface

## Usage

```bash
# Run the CLI
voice-bird-cli
```

### Controls

- `↑/↓` - Navigate device list
- `Space` - Select/deselect audio source
- `Enter` - Start/stop streaming
- `c` - Configure API key
- `r` - Refresh device list
- `q` - Quit
- `?` - Help

## Requirements

- Windows 10 Build 20348+ or macOS 12+
- Voice Bird API key (get one at https://voicebird.app)

## License

Proprietary - See LICENSE file for details.
