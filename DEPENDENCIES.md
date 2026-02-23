# Voice Bird Desktop - Dependency & Architecture Documentation

> Comprehensive cross-dependency mapping with mermaid diagrams and object descriptions

## Table of Contents

1. [Overview](#overview)
2. [Module Dependency Graph](#module-dependency-graph)
3. [Data Flow Architecture](#data-flow-architecture)
4. [Type & Struct Relationships](#type--struct-relationships)
5. [External Dependencies](#external-dependencies)
6. [Audio Processing Pipeline](#audio-processing-pipeline)
7. [WebSocket Communication Flow](#websocket-communication-flow)
8. [Thread Architecture](#thread-architecture)
9. [Session Lifecycle](#session-lifecycle)
10. [Object Reference](#object-reference)

---

## Overview

**Voice Bird Desktop** is a Rust-based real-time audio recording application for Windows and macOS. It captures audio from multiple sources simultaneously (microphones and system audio via platform-specific APIs), encodes it with Opus compression, and streams it to a remote server via WebSocket.

### Key Technologies
- **Language**: Rust (async/sync hybrid)
- **Audio**: cpal (input), WASAPI (Windows output), ScreenCaptureKit (macOS output), Opus codec
- **Networking**: WebSocket (tokio-tungstenite)
- **UI**: Tauri (desktop application)
- **Platform**: Windows and macOS (full support), Linux (input only)

---

## Module Dependency Graph

```mermaid
graph TB
    main[main.rs<br/>Entry Point & Tauri App]

    main --> commands[commands.rs<br/>Tauri IPC Commands]
    main --> state[state.rs<br/>AppState Management]
    main --> config[config.rs<br/>Persistent Config]
    main --> session[session.rs<br/>SessionManager, RecordingSession]
    main --> wasapi[wasapi_sessions.rs<br/>WASAPI Enumeration]
    main --> audio[audio.rs<br/>Audio Capture]
    main --> events[events.rs<br/>Tauri Events]
    main --> audio_buffer[audio_buffer.rs<br/>AudioPreBuffer]

    commands --> state
    commands --> session
    state --> session
    audio --> audio_buffer
    audio_buffer --> server_streaming[server_streaming.rs<br/>WebSocket Streaming]
    server_streaming --> opus[opus_encoder.rs<br/>OpusAudioEncoder]

    subgraph UI["Tauri Web Frontend (ui/)"]
        html[index.html]
        js[main.js]
        css[styles.css]
    end

    commands -.->|IPC| UI

    style main fill:#e1f5ff
    style commands fill:#e2d3f8
    style state fill:#e2d3f8
    style audio fill:#fff3cd
    style server_streaming fill:#d4edda
    style opus fill:#d4edda
    style session fill:#f8d7da
    style UI fill:#f0f0f0
```

### Module Descriptions

| Module | Purpose | Key Exports |
|--------|---------|-------------|
| **main.rs** | Application entry point, Tauri app initialization and command registration | `main()`, Tauri builder |
| **commands.rs** | Tauri IPC command handlers for frontend communication | `enumerate_sessions`, `start_recording`, `stop_session`, `save_api_key` |
| **state.rs** | Application state management with thread-safe access | `AppState` |
| **config.rs** | Persistent configuration storage (JSON file in user config dir) | `AppConfig` |
| **events.rs** | Tauri event definitions for frontend notifications | `AudioLevelEvent`, `SessionStatusEvent` |
| **session.rs** | Recording session data structures and state management | `SessionManager`, `RecordingSession`, `AudioSessionInfo`, `SessionStatus` |
| **wasapi_sessions.rs** | Platform-specific audio session enumeration (WASAPI on Windows, ScreenCaptureKit on macOS) | `enumerate_audio_sessions()` |
| **audio.rs** | Audio device capture (cpal for input, WASAPI/ScreenCaptureKit for output loopback) | `start_input_recording()`, `start_output_recording()` |
| **audio_buffer.rs** | Thread-safe pre-buffer decoupling audio capture from WebSocket readiness | `AudioPreBuffer`, `AudioProducer`, `AudioConsumer` |
| **server_streaming.rs** | WebSocket streaming client with Opus encoding | `ServerStreamingService::stream_to_server()` |
| **opus_encoder.rs** | Opus audio codec encoder with buffering | `OpusAudioEncoder` |
| **ui/** | Tauri web frontend (HTML/JS/CSS) | Session browser, recording controls, settings UI |

---

## Data Flow Architecture

```mermaid
flowchart LR
    subgraph Input["Audio Input Sources"]
        mic[Microphone<br/>cpal]
        sys[System Audio<br/>WASAPI/ScreenCaptureKit]
    end

    subgraph Capture["Audio Capture"]
        callback[Audio Callback<br/>f32 samples]
    end

    subgraph Processing["Audio Processing"]
        buffer[Sample Buffer]
        opus_enc[Opus Encoder<br/>960 samples/frame]
        packets[Opus Packets<br/>~60 bytes]
    end

    subgraph Network["Network Streaming"]
        ws[WebSocket Client<br/>tokio-tungstenite]
        json[Init Message<br/>JSON metadata]
    end

    subgraph Server["Remote Server"]
        endpoint["/api/audio/stream"<br/>WebSocket]
    end

    mic --> callback
    sys --> callback
    callback --> buffer
    buffer --> opus_enc
    opus_enc --> packets
    packets --> ws
    json --> ws
    ws --> endpoint

    style Input fill:#e3f2fd
    style Capture fill:#fff3cd
    style Processing fill:#d4edda
    style Network fill:#f3e5f5
    style Server fill:#fce4ec
```

### Data Flow Steps

1. **Audio Capture**: cpal (input devices) or platform-specific loopback (WASAPI on Windows, ScreenCaptureKit on macOS) captures f32 audio samples at device sample rate
2. **Pre-Buffering**: Audio callback thread pushes samples to `AudioPreBuffer` via `AudioProducer`. Samples are captured immediately, even before the WebSocket connection is established, preventing audio loss during setup
3. **Opus Encoding**: `OpusAudioEncoder` buffers samples until complete frame (960 samples @ 48kHz = 20ms), then encodes to Opus packet
4. **WebSocket Streaming**: Encoded Opus packets sent as binary WebSocket frames to server
5. **Server Processing**: Server receives Opus audio for transcription or other processing

---

## Type & Struct Relationships

```mermaid
classDiagram
    class RecordingSession {
        +Uuid id
        +String device_name
        +String app_name
        +Arc~Mutex~SessionStatus~~ status
        +Arc~Mutex~f32~~ audio_level
        +Arc~Mutex~Vec~f32~~~ audio_buffer
        +u32 sample_rate
        +u16 channels
        +start_recording()
        +stop_recording()
        +get_duration() f32
    }

    class SessionManager {
        +HashMap~Uuid, RecordingSession~ active_sessions
        +add_session(RecordingSession) Uuid
        +stop_all()
        +get_all_sessions() Vec
    }

    class OpusAudioEncoder {
        -Encoder encoder
        -OpusEncoderConfig config
        -Vec~f32~ frame_buffer
        +new(config) Result
        +buffer_and_encode(samples) Result~Option~Vec~u8~~~
        +encode(samples) Result~Vec~u8~~
    }

    class ServerStreamingService {
        +stream_to_server(url, key, session_id, device, rx, rate, channels) Result
    }

    SessionManager "1" *-- "*" RecordingSession
    ServerStreamingService ..> OpusAudioEncoder : uses
```

---

## External Dependencies

### Core Dependencies (Cargo.toml)

```toml
[dependencies]
# Desktop Application Framework
tauri = { version = "2", features = [] }

# Audio I/O
cpal = "0.15"              # Cross-platform audio capture
audiopus = "0.3.0-rc.0"    # Opus codec encoder

# Networking
tokio = { version = "1.0", features = ["full"] }
tokio-tungstenite = { version = "0.23", features = ["connect", "native-tls"], default-features = false }
futures-util = "0.3"
http = "1.0"

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
base64 = "0.22"

# Configuration
dirs = "5.0"               # Platform-specific config directories

# Utilities
anyhow = "1.0"
chrono = "0.4"
uuid = { version = "1.0", features = ["v4"] }
dotenvy = "0.15"
log = "0.4"
fern = "0.7"               # File-based logging
colored = "2.0"            # Colored log output
hound = "3.5"              # WAV file utilities

# Windows-specific
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Media_Audio",
    "Win32_System_Com",
    # ... other features
] }
windows-core = "0.58"

# macOS-specific
[target.'cfg(target_os = "macos")'.dependencies]
screencapturekit = "0.3"   # System audio capture via ScreenCaptureKit (macOS 12.3+)

[build-dependencies]
tauri-build = { version = "2", features = [] }
```

---

## Audio Processing Pipeline

### 1. Audio Capture (cpal - Input Devices)

**File**: `src/audio.rs` (Lines 107-245)

```rust
// Callback-based audio capture
device.build_input_stream(
    &config.into(),
    move |data: &[f32], _: &_| {
        // Send to encoder thread
        server_tx.send(data.to_vec());
    },
    |err| log::error!("Stream error: {}", err),
    None,
)?
```

**Key Points**:
- Supports F32, I16, U16 sample formats (converted to f32)
- Real-time audio callback on separate thread
- Non-blocking channel send to avoid audio glitches
- RMS calculation for audio level visualization

### 2. Audio Capture (WASAPI - Output Loopback)

**File**: `src/audio.rs` (Lines 249-437)

```rust
// Windows WASAPI loopback capture
let capture_client: IAudioCaptureClient = audio_client.GetService()?;
audio_client.Start()?;

loop {
    let packet_size = capture_client.GetNextPacketSize()?;
    if packet_size == 0 { break; }

    // Get buffer of f32 samples
    capture_client.GetBuffer(&mut buffer_ptr, ...);
    server_tx.send(samples.to_vec());
    capture_client.ReleaseBuffer(num_frames_available)?;
}
```

**Key Points**:
- Windows-only feature using COM APIs
- Captures system audio (games, music, browser)
- AUDCLNT_STREAMFLAGS_LOOPBACK flag for output capture
- Packet-based buffer processing

### 2b. Audio Capture (ScreenCaptureKit - macOS Output Loopback)

**File**: `src/audio.rs` (Lines 827-1013)

```rust
// macOS ScreenCaptureKit capture
let content = SCShareableContent::get()?;
let display = content.displays().first()?.clone();
let applications = content.applications();

// Application-based filters avoid kCGErrorInvalidContext (1003)
let filter = if device_name.contains("All Applications") {
    let app_refs: Vec<&_> = applications.iter().collect();
    SCContentFilter::new()
        .with_display_including_application_excepting_windows(&display, &app_refs, &[])
} else {
    // Exclude all OTHER apps (avoids invalid context when target has no visible windows)
    let excluded: Vec<&_> = applications.iter()
        .filter(|a| a.process_id() != target.process_id()).collect();
    SCContentFilter::new()
        .with_display_excluding_applications_excepting_windows(&display, &excluded, &[])
};

let config = SCStreamConfiguration::new()
    .set_captures_audio(true)?
    .set_sample_rate(48000)?
    .set_channel_count(2)?
    .set_excludes_current_process_audio(true)?
    .set_width(2)?.set_height(2)?;

let mut stream = SCStream::new(&filter, &config);
stream.add_output_handler(handler, SCStreamOutputType::Audio);
stream.start_capture()?;
```

**Key Points**:
- macOS 12.3+ feature using Apple's ScreenCaptureKit framework
- Requires Screen Recording permission
- Uses **application-based** SCContentFilter (not window-based) to avoid `kCGErrorInvalidContext` (1003)
- For all-system audio: includes all apps explicitly via `with_display_including_application_excepting_windows`
- For per-app audio: excludes all other apps via `with_display_excluding_applications_excepting_windows`
- Minimum capture dimensions set to 2x2 (avoids degenerate context rejection on some macOS versions)
- Audio capture starts **before** WebSocket connection via `AudioPreBuffer`, preventing sample loss

### 3. Opus Encoding

**File**: `src/opus_encoder.rs` (Lines 61-222)

```rust
let mut encoder = OpusAudioEncoder::new(OpusEncoderConfig {
    sample_rate: 48000,    // 48 kHz
    channels: 1,           // Mono
    bitrate: 24000,        // 24 kbps
    frame_duration_ms: 20, // 20ms frames
})?;

// Buffer and encode
if let Some(opus_packet) = encoder.buffer_and_encode(&samples)? {
    // Send opus_packet (Vec<u8>) to server
}
```

**Opus Configuration**:
- **Sample Rate**: 48000 Hz (recommended for Opus)
- **Channels**: 1 (mono) or 2 (stereo)
- **Bitrate**: 24000 bps (24 kbps) - optimal for speech
- **Frame Duration**: 20ms (960 samples @ 48kHz)
- **Application Mode**: VOIP (optimized for voice)
- **Complexity**: 5 (medium CPU usage)
- **FEC**: Enabled for packet loss recovery

**Compression Ratio**: ~10-15x (raw f32 audio → Opus)

### 4. WebSocket Streaming

**File**: `src/server_streaming.rs` (Lines 57-403)

```rust
// Connect to WebSocket
let ws_stream = connect_async_with_config(
    "wss://server.com/api/audio/stream",
    ws_config,
    false
).await?;

// Send init message (JSON)
let init_msg = InitMessage {
    message_type: "init",
    api_key,
    session_id,
    device_name,
    sample_rate,
    channels,
    codec: "opus",
};
write.send(Message::Text(init_json)).await?;

// Stream Opus packets (binary)
write.send(Message::Binary(opus_packet)).await?;
```

**WebSocket Protocol**:
1. **Connection**: wss:// endpoint with Authorization header
2. **Init Message**: JSON with session metadata and codec="opus"
3. **Audio Streaming**: Binary frames containing Opus packets
4. **Keepalive**: Ping/Pong for connection maintenance
5. **Termination**: JSON terminate message when done

---

## WebSocket Communication Flow

```mermaid
sequenceDiagram
    participant App as Voice Bird Desktop
    participant WS as WebSocket Server
    participant Server as Audio Processing Server

    App->>WS: Connect (wss://server/api/audio/stream)
    WS-->>App: WebSocket Upgrade (HTTP 101)

    App->>WS: Init Message (JSON)<br/>{type: "init", codec: "opus", ...}
    WS-->>App: Connected Message<br/>{type: "connected"}

    loop Audio Streaming
        App->>WS: Binary Frame (Opus Packet)
        WS->>Server: Process Opus Audio
    end

    WS-->>App: Ping
    App->>WS: Pong

    App->>WS: Terminate Message (JSON)<br/>{type: "terminate"}
    WS-->>App: Close Frame
```

### Message Types

**Client → Server**:
- `init` (JSON): Session metadata (sample_rate, channels, codec, session_id, device_name)
- Binary frames: Opus-encoded audio packets
- `terminate` (JSON): End of stream notification
- `pong`: Response to server pings

**Server → Client**:
- `connected` (JSON): Acknowledgment of init
- `error` (JSON): Error notifications
- `ping`: Keepalive checks

---

## Thread Architecture

```mermaid
graph TB
    subgraph Main["Main Thread (Tauri)"]
        tauri_app[Tauri Application<br/>Window Management]
        ipc[IPC Command Handlers<br/>commands.rs]
    end

    subgraph WebView["WebView Thread"]
        ui[Web UI<br/>HTML/JS/CSS]
    end

    subgraph Audio["Audio Callback Threads"]
        cpal_thread[cpal Audio Thread<br/>f32 samples]
        wasapi_thread[WASAPI/SCK Thread<br/>f32 samples]
    end

    subgraph PreBuffer["AudioPreBuffer"]
        producer[AudioProducer<br/>push samples]
        consumer[AudioConsumer<br/>drain on ready]
    end

    subgraph Encoder["Streaming Thread"]
        tokio_rt[Tokio Runtime]
        opus[Opus Encoder]
        ws[WebSocket Client]
    end

    ui <-->|Tauri IPC| ipc
    ipc --> Audio
    cpal_thread -->|AudioProducer| producer
    wasapi_thread -->|AudioProducer| producer
    consumer --> opus
    opus --> ws
    ws --> tokio_rt
    ipc -.->|Events| ui

    style Main fill:#e3f2fd
    style WebView fill:#e2d3f8
    style Audio fill:#fff3cd
    style Encoder fill:#d4edda
```

### Thread Descriptions

1. **Main Thread (Tauri)**:
   - Tauri application lifecycle management
   - IPC command handlers (`commands.rs`)
   - State management (`AppState`)
   - Window and webview coordination

2. **WebView Thread**:
   - Renders HTML/JS/CSS frontend (`ui/`)
   - Handles user interactions
   - Communicates with backend via Tauri IPC
   - Receives events for audio level updates

3. **Audio Callback Threads** (per device):
   - cpal: Separate thread per audio stream
   - WASAPI (Windows): Custom thread for COM operations
   - ScreenCaptureKit (macOS): SCStreamOutput delegate callbacks
   - Real-time priority for low latency
   - Push samples to `AudioPreBuffer` via `AudioProducer`

4. **Streaming Thread** (per session):
   - Drains pre-buffered samples via `AudioConsumer` once WebSocket is ready
   - Buffers samples to complete Opus frames
   - Encodes to Opus packets
   - Streams via WebSocket to server
   - Runs on Tokio async runtime

### Synchronization Primitives

- `Arc<Mutex<T>>`: Shared state (audio_level, audio_buffer, status, stop_signal)
- `AudioPreBuffer` (`Arc<(Mutex<SharedState>, Condvar)>`): Lock-free-ish pre-buffer decoupling audio capture from WebSocket readiness; replaces `mpsc::channel`
- `tokio_mpsc::unbounded_channel`: Pong messages from read task → write loop
- `stop_signal`: Graceful shutdown coordination

---

## Session Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Idle: Create Session
    Idle --> Recording: start_recording()
    Recording --> Stopped: stop_recording()
    Stopped --> [*]: Cleanup

    state Recording {
        [*] --> AudioCapture
        AudioCapture --> OpusEncoding
        OpusEncoding --> WebSocketStreaming
        WebSocketStreaming --> AudioCapture
    }
```

### Session States

- **Idle**: Session created but not recording
- **Recording**: Active audio capture and streaming
- **Stopped**: Recording ended, cleanup in progress

### Session Operations

1. **Creation** (`RecordingSession::new`)
   - Generate UUID
   - Initialize Arc<Mutex> shared state
   - Set sample rate and channels

2. **Start Recording** (`start_recording`)
   - Set status to Recording
   - Start timestamp
   - Begin audio capture stream
   - Spawn encoder thread with WebSocket streaming

3. **Stop Recording** (`stop_recording`)
   - Set status to Stopped
   - Set stop_signal flag
   - Audio threads check flag and exit
   - WebSocket sends terminate message

---

## Object Reference

### RecordingSession

**Location**: `src/session.rs` (Lines 23-94)

**Purpose**: Represents a single audio recording session from one device

**Fields**:
- `id: Uuid` - Unique session identifier
- `device_name: String` - Audio device name
- `app_name: String` - Application associated with audio session
- `status: Arc<Mutex<SessionStatus>>` - Current recording state
- `audio_level: Arc<Mutex<f32>>` - Real-time RMS audio level (0.0-1.0)
- `audio_buffer: Arc<Mutex<Vec<f32>>>` - Accumulated audio samples
- `sample_rate: u32` - Audio sample rate (e.g., 48000 Hz)
- `channels: u16` - Number of audio channels (1=mono, 2=stereo)
- `start_time: Option<Instant>` - Recording start timestamp
- `stop_signal: Arc<Mutex<bool>>` - Flag to stop audio capture

### OpusAudioEncoder

**Location**: `src/opus_encoder.rs` (Lines 61-222)

**Purpose**: Opus codec encoder with automatic frame buffering

**Key Methods**:
- `new(config: OpusEncoderConfig) -> Result<Self>`
- `buffer_and_encode(&mut self, samples: &[f32]) -> Result<Option<Vec<u8>>>`
  - Buffers samples until complete frame
  - Returns Some(opus_packet) when frame ready
- `encode(&mut self, samples: &[f32]) -> Result<Vec<u8>>`
  - Direct encoding (requires exact frame size)

### AudioPreBuffer

**Location**: `src/audio_buffer.rs`

**Purpose**: Thread-safe pre-buffer that decouples audio capture from WebSocket readiness. Audio callbacks push samples via `AudioProducer` immediately when capture starts; the streaming thread drains buffered chunks via `AudioConsumer` once the WebSocket connection is established.

**Key Types**:
- `AudioPreBuffer` - Owns the shared buffer state; creates `AudioProducer` and `AudioConsumer` handles
- `AudioProducer` - Cloneable handle for pushing `Vec<f32>` sample chunks (used by audio callback threads)
- `AudioConsumer` - Handle for draining buffered chunks with blocking `recv()` (used by streaming thread)

**Capacity**: ~500 chunks (~5 seconds at ~100 chunks/sec)

### ServerStreamingService

**Location**: `src/server_streaming.rs`

**Purpose**: WebSocket client for streaming Opus audio to server

**Key Method**:
```rust
async fn stream_to_server(
    server_url: String,
    api_key: String,
    session_id: String,
    device_name: String,
    audio_consumer: AudioConsumer,
    sample_rate: u32,
    channels: u16,
) -> Result<()>
```

**Features**:
- Automatic WebSocket connection with retry
- Opus encoding integration
- Ping/Pong keepalive
- Binary frame streaming
- Compression disabled (matching server config)

---

## Performance Characteristics

### Latency Profile

- **Audio Capture**: <10ms (hardware buffer + callback)
- **Opus Encoding**: 20ms (frame duration)
- **WebSocket Send**: ~10ms (network + serialization)
- **End-to-End**: ~40-50ms (capture to server receive)

### Bandwidth Usage

**Raw Audio** (48kHz mono f32):
- 48000 samples/sec × 4 bytes/sample = 192 KB/s

**Opus Compressed** (24 kbps):
- 24000 bits/sec = 3 KB/s
- **Compression**: ~64x reduction

**Network Traffic**:
- ~3 KB/s per audio session
- WebSocket overhead: <100 bytes/packet
- Keepalive pings: ~50 bytes every 30s

### CPU Usage

- **Audio Capture**: <1% (hardware DMA)
- **Opus Encoding**: ~2-5% per session (complexity=5)
- **WebSocket**: <1% (async I/O)
- **UI Rendering**: ~1-2% (Tauri WebView)

**Total**: ~5-10% CPU for single session on modern processor

---

## Environment Configuration

**File**: `.env`

```bash
# WebSocket server endpoint (required)
VOICE_BIRD_SERVER_URL=https://voice-bird-app.example.com

# API key for authentication (required)
VOICE_BIRD_API_KEY=your-api-key-here
```

---

## Build & Deployment

### Build Artifacts

- **Binary Size**: ~4-6 MB (release build with optimizations)
- **Dependencies**: Static linking (no runtime dependencies besides OS libraries)
- **Platform**: Windows x64 (full support), macOS (full support, requires macOS 12.3+), Linux x64 (input only)

### Tauri Desktop Application

The application uses **Tauri 2** for cross-platform desktop packaging:

**Configuration**: `tauri.conf.json`
- Product: Voice Bird Desktop v0.1.0
- Identifier: `com.voicebird.desktop`
- Bundle targets: all (msi, dmg, app)
- macOS minimum: 10.13

### Release Build

```bash
# Direct Rust build
cargo build --release
# Binary: target/release/voice_bird_desktop.exe

# Tauri build (recommended for distribution)
cargo tauri build
# Outputs: target/release/bundle/{msi,dmg,macos}/
```

### macOS Build

See `BUILD_MACOS.md` for detailed instructions. Quick start:

```bash
# On macOS machine
./scripts/build-macos.sh      # Build .app and .dmg
./scripts/upload-to-wasabi.sh # Upload to Wasabi S3
```

### Distribution (Wasabi S3)

Build artifacts are uploaded to Wasabi S3-compatible storage:

- **Bucket**: `voice-bird-europe` (eu-central-2)
- **Path**: `releases/{platform}/v{version}/`
- **Latest**: `releases/{platform}/latest/`

Configuration in `.env`:
```bash
WASABI_ACCESS_KEY_ID=...
WASABI_SECRET_ACCESS_KEY=...
WASABI_REGION=eu-central-2
WASABI_BUCKET_NAME=voice-bird-europe
WASABI_ENDPOINT=https://s3.eu-central-2.wasabisys.com
```

### Distribution (cargo-binstall) - CLI Tool

Developers can install the Voice Bird CLI via Rust's cargo-binstall:

```bash
cargo binstall voice-bird-cli
```

**Architecture**:
- **CLI implementation** (`voice-bird-cli/`): Full ratatui TUI application (source kept private)
- **Stub crate** (`voice-bird-cli-crate/`): Published to crates.io with binstall metadata only (no source code)
- **Binaries**: Hosted on GitHub Releases at `RekunDzmitry/voice-bird-releases`
- **Naming convention**: `voice-bird-cli-{target}.zip` (e.g., `voice-bird-cli-x86_64-pc-windows-msvc.zip`)

**CLI Features**:
- Interactive TUI with ratatui
- Audio device/process enumeration
- Per-application audio capture (Windows/macOS)
- Server streaming with API key authentication
- Real-time audio level visualization

**Release workflow**:
1. Build CLI binaries on target platforms (`voice-bird-cli/`)
2. Package into ZIPs with correct naming
3. Create GitHub release with binaries at voice-bird-releases repo
4. Publish stub crate to crates.io (`voice-bird-cli-crate/`)

### Optimization Flags (Cargo.toml)

```toml
[profile.release]
opt-level = 3          # Maximum optimization
lto = true             # Link-time optimization
codegen-units = 1      # Better optimization
strip = true           # Remove debug symbols
```

---

## Future Enhancements

- [ ] HTTP/2 streaming as alternative to WebSocket
- [ ] Adaptive bitrate based on network conditions
- [ ] Multiple codec support (Opus, AAC, MP3)
- [x] macOS output loopback (ScreenCaptureKit) - **Implemented**
- [x] Tauri desktop UI (replaced terminal UI) - **Implemented**
- [ ] Linux output loopback (PulseAudio/PipeWire)
- [ ] Persistent session resumption
- [ ] Audio filters (noise reduction, AGC)
- [ ] Multi-track recording UI
