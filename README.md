# Voice Bird Desktop

A Rust application for audio device enumeration and streaming with recording capabilities. Supports both input device recording (microphones) and output device loopback recording (system audio) on Windows.

## Features

- **Audio Device Enumeration**: List all available input and output audio devices
- **Input Device Recording**: Record from microphones and other input devices
- **Output Device Loopback** (Windows only): Record system audio (what you hear)
- **Real-time Audio Visualization**: See audio levels with colored bars
- **ESC to Save**: Press ESC to stop streaming and automatically save to WAV file
- **Timestamped Recordings**: Files saved with format `recording_YYYY-MM-DD_HH-MM-SS.wav`

## Prerequisites

### Install Rust

If you don't have Rust installed, install it using rustup:

**Linux/macOS:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**Windows:**
Download and run [rustup-init.exe](https://rustup.rs/)

After installation, restart your terminal and verify:
```bash
rustc --version
cargo --version
```

## Getting Started

### Build the Project

```bash
cargo build
```

This compiles the project in debug mode. The executable will be located at `target/debug/voice_bird_desktop`.

### Run the Project

```bash
cargo run
```

This will launch an interactive audio device selector:

1. **Select Device Type**: Choose between Input Device (Microphone) or Output Device (Speaker)
2. **Select Specific Device**: Pick from available devices (default device marked)
3. **Stream and Record**:
   - Real-time audio level visualization appears
   - Press **ESC** to stop streaming
   - Recording automatically saves to WAV file with timestamp

Example output:
```
=== Audio Device Selector ===
Using audio host: Wasapi

? Select device type ›
  Input Device (Microphone)
❯ Output Device (Speaker)

? Select audio device ›
❯ Speakers (Realtek High Definition Audio) [DEFAULT]
  HDMI Audio (NVIDIA)

=== AUDIO STREAMING (Loopback) ===
Device: Speakers (Realtek High Definition Audio)

Press ESC to stop streaming...

Stream config: 48000 Hz, 2 channels

Level: ██████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  38.50%
```

After pressing ESC:
```
Streaming stopped.
Saving audio file...
✓ Audio saved to: recording_2025-10-16_14-32-15.wav
  Duration: 12.45 seconds
  Samples: 1195200
```

### Build for Release

```bash
cargo build --release
```

This creates an optimized binary at `target/release/voice_bird_desktop`.

## Project Structure

```
voice_bird_desktop/
├── Cargo.toml      # Package manifest (dependencies, metadata)
├── src/
│   └── main.rs     # Main source file
└── README.md       # This file
```

## Windows Loopback Recording

Output device streaming uses **Windows WASAPI (Windows Audio Session API)** loopback mode to capture system audio (what you hear through your speakers).

### How It Works

- **Loopback Capture**: Records audio being played through the selected output device
- **System Audio**: Captures game audio, music, browser audio, etc.
- **No Virtual Cables**: Uses native Windows API, no third-party software needed
- **Real-time Processing**: Live audio visualization while recording

### Platform Support

| Feature | Windows | Linux | macOS |
|---------|---------|-------|-------|
| Input Device Recording | ✅ | ✅ | ✅ |
| Output Device Loopback | ✅ | ❌ | ❌ |

**Note**: Output device loopback is currently Windows-only. Linux and macOS users can still record from input devices.

### Troubleshooting

**No audio being captured from output device?**
- Make sure audio is actually playing through the selected device
- Check Windows sound settings to confirm the device is active
- Try playing audio (music, video) while recording

**Silent recordings?**
- Loopback only captures audio being played through the device
- If nothing is playing, the recording will be silent
- Test with music or a YouTube video

## Common Cargo Commands

- `cargo new <name>` - Create a new project
- `cargo build` - Compile the project
- `cargo run` - Compile and run the project
- `cargo test` - Run tests
- `cargo check` - Check code without building
- `cargo clean` - Remove build artifacts
- `cargo doc --open` - Build and open documentation

## Learn More

- [The Rust Programming Language Book](https://doc.rust-lang.org/book/)
- [Rust by Example](https://doc.rust-lang.org/rust-by-example/)
- [Cargo Documentation](https://doc.rust-lang.org/cargo/)
