# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Voice Bird Desktop is a Rust audio recording application supporting:
- Audio device enumeration (input/output)
- Input device recording (microphones)
- Output device loopback recording (Windows WASAPI - system audio capture)
- Real-time audio visualization
- WAV file export

## Build and Development Commands

### Build
```bash
cargo build              # Debug build (target/debug/)
cargo build --release    # Optimized release build (target/release/)
```

### Run
```bash
cargo run                # Run with debug build
cargo run --release      # Run with optimized build
```

### Testing and Validation
```bash
cargo test               # Run unit tests
cargo check              # Fast compilation check without building
cargo clippy             # Linting and suggestions
```

### Cleanup
```bash
cargo clean              # Remove build artifacts
```

## Architecture

### Single-File Structure
The entire application is in `src/main.rs` (~720 lines). This monolithic structure is intentional for a focused utility.

### Audio Pipeline Flow
1. **Device Enumeration** → `collect_input_devices()` / `collect_output_devices()`
2. **User Selection** → Interactive prompt via `dialoguer` crate
3. **Stream Setup** → Input via cpal API, Output via Windows WASAPI COM
4. **Audio Capture** → Callback-based sample collection into `Arc<Mutex<Vec<f32>>>`
5. **Visualization** → RMS calculation + colored terminal bars (crossterm)
6. **Persistence** → WAV export via `hound` crate with timestamped filenames

### Key Functions

#### Device Enumeration (lines 34-92)
- `collect_input_devices()` - Lists microphones, marks default
- `collect_output_devices()` - Lists speakers/headphones, marks default
- Returns `Vec<DeviceInfo>` with name and default flag

#### Audio Processing
- `calculate_rms()` (lines 95-102) - Root Mean Square level from samples
- `create_audio_bar()` (lines 105-119) - Terminal visualization with color thresholds (green/yellow/red)

#### Recording Functions
- `stream_audio()` (lines 182-379) - **Input device recording** using cpal
  - Handles 3 sample formats: F32, I16, U16
  - Real-time callback architecture
  - ESC key stops streaming
  - Automatic WAV save with diagnostics

- `stream_output_audio()` (lines 382-613) - **Windows WASAPI loopback** (Windows only)
  - Direct COM API usage (`IMMDeviceEnumerator`, `IAudioClient`, `IAudioCaptureClient`)
  - Loopback mode captures system audio (games, music, browser)
  - Packet-based buffer processing
  - Platform-gated with `#[cfg(windows)]`

#### WAV Export
- `save_audio_file()` (lines 122-179)
  - Timestamped filenames: `recording_YYYY-MM-DD_HH-MM-SS.wav`
  - 32-bit float format
  - Diagnostic output: capture rate, frame counts, duration validation

### Platform-Specific Code

#### Windows WASAPI (lines 382-613)
- Uses `windows` crate with Win32 Audio APIs
- COM initialization/cleanup with `CoInitializeEx`/`CoUninitialize`
- Loopback flag: `AUDCLNT_STREAMFLAGS_LOOPBACK`
- Format handling: IEEE_FLOAT (0x0003) and EXTENSIBLE (0xFFFE)

#### Non-Windows Fallback (lines 616-621)
- Gracefully informs user that output loopback is Windows-only
- Input device recording works cross-platform

### Dependencies (Cargo.toml)

**Core Audio**
- `cpal = "0.15"` - Cross-platform audio I/O
- `hound = "3.5"` - WAV file encoding/decoding

**UI/Terminal**
- `dialoguer = "0.11"` - Interactive prompts
- `console = "0.15"` - Styled terminal output
- `crossterm = "0.27"` - Terminal manipulation, keyboard input

**Utilities**
- `anyhow = "1.0"` - Error handling with context
- `chrono = "0.4"` - Timestamp generation

**Windows-Only**
- `windows = "0.58"` - Win32 API bindings (COM, WASAPI)
  - Features: `Win32_Media_Audio`, `Win32_System_Com`

### Error Handling Pattern
Uses `anyhow::Result` with `.context()` for error propagation:
```rust
device.build_input_stream(...)
    .context("Failed to build input stream")?
```

### Concurrency Model
- `Arc<Mutex<>>` for shared state between audio callback thread and main thread
- Mutex-protected: audio levels, sample buffers, statistics counters
- No explicit thread spawning (handled by cpal/WASAPI)

## Development Notes

### Adding Sample Format Support
New formats require:
1. New match arm in `stream_audio()` (line 222)
2. Sample conversion to f32 for RMS calculation
3. Clone of shared Arc state for callback

### Extending Audio Visualization
Modify `create_audio_bar()` for different thresholds/colors or add frequency analysis (requires FFT).

### WAV Format Customization
Change `WavSpec` in `save_audio_file()` (line 159). Current: 32-bit float, matches cpal's internal format.

### Platform Extensions
Linux/macOS loopback would require:
- **Linux**: PulseAudio/PipeWire monitor sources
- **macOS**: Core Audio loopback or virtual audio devices
- Pattern: Add `#[cfg(target_os = "...")]` functions similar to Windows implementation

### Safety Considerations
Windows COM code uses `unsafe` block (lines 391-612). When modifying:
- Ensure COM initialization before API calls
- Match `CoInitializeEx` with `CoUninitialize`
- Free COM memory with `CoTaskMemFree`
- Validate pointer lifetime (format_ptr)
