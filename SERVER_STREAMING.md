# Server Streaming Implementation

This document explains the server streaming feature that allows Voice Bird Desktop to stream audio from multiple devices to a centralized server for transcription.

## Overview

The desktop client now supports streaming audio to a custom Voice Bird server in addition to (or instead of) the local AssemblyAI transcription. This enables:

- **Multi-device support**: Multiple computers/devices can stream to the same user account
- **Centralized transcription**: All audio is processed on the server
- **User identification**: API keys link streams to specific users
- **Parallel streaming**: Audio goes to both AssemblyAI (local) and your server simultaneously

## Architecture

```
Desktop Client (Audio Capture)
    ↓
    ├── Local Buffer (for WAV export)
    ├── AssemblyAI WebSocket (optional - local transcription)
    └── Voice Bird Server WebSocket (optional - server streaming)
```

## Configuration

### Environment Variables

Add to your `.env` file:

```env
# Voice Bird Server Configuration
VOICE_BIRD_SERVER_URL=https://voice-bird-app-ebrln.ondigitalocean.app
VOICE_BIRD_API_KEY=your-user-api-key-here

# Optional: Local transcription
ASSEMBLYAI_API_KEY=your-assemblyai-key
```

- `VOICE_BIRD_SERVER_URL`: Your server's base URL (converts to WebSocket internally)
- `VOICE_BIRD_API_KEY`: User-specific API key for authentication

## WebSocket Protocol

### Connection

The client connects to: `wss://your-server.com/api/audio/stream`

**Headers:**
- `Authorization: <VOICE_BIRD_API_KEY>`

### Message Flow

#### 1. Initialization (Client → Server)

```json
{
  "type": "init",
  "api_key": "user-api-key",
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "device_name": "Microphone (Realtek HD Audio)",
  "sample_rate": 48000,
  "channels": 2
}
```

#### 2. Server Acknowledgment (Server → Client)

```json
{
  "type": "connected",
  "message": "Audio streaming session started"
}
```

#### 3. Audio Data (Client → Server)

**Binary frames** containing raw f32 audio samples (little-endian):
- Each sample: 4 bytes (32-bit float)
- Format: `[sample1_bytes][sample2_bytes][sample3_bytes]...`
- Interleaved channels (e.g., stereo: L, R, L, R, ...)

#### 4. Server Responses (Server → Client)

**Transcription results:**
```json
{
  "type": "transcription",
  "message": "Hello, this is the transcribed text"
}
```

**Errors:**
```json
{
  "type": "error",
  "error": "Authentication failed: Invalid API key"
}
```

#### 5. Termination (Client → Server)

```json
{
  "type": "terminate",
  "session_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

## Implementation Details

### Client-Side Components

#### 1. **src/server_streaming.rs**
- `ServerStreamingService::stream_to_server()`: Main WebSocket streaming logic
- Handles connection, message serialization, and error reporting
- Progress logging every 100 chunks

#### 2. **src/audio.rs**
- Updated `start_input_recording()` to accept `server_config`
- Updated `start_output_recording()` to accept `server_config`
- Parallel channels: transcription (i16) + server streaming (f32)

#### 3. **src/main.rs**
- Loads `VOICE_BIRD_SERVER_URL` and `VOICE_BIRD_API_KEY` from environment
- Passes server config to audio recording functions

### Audio Format

**Sent to server:**
- Format: 32-bit float PCM
- Range: -1.0 to +1.0
- Channels: As captured (mono, stereo, etc.)
- Sample rate: Device native (typically 48kHz or 44.1kHz)

**Sent to AssemblyAI (if configured):**
- Format: 16-bit signed integer PCM
- Channels: Mono (downmixed if needed)
- Sample rate: Device native

## Server Requirements

The Next.js server must implement a WebSocket endpoint at `/api/audio/stream` that:

1. **Authenticates** requests using the `Authorization` header
2. **Parses** the init message to extract session metadata
3. **Receives** binary audio frames and processes them
4. **Forwards** audio to a transcription service (e.g., AssemblyAI)
5. **Stores** audio and transcripts in a database
6. **Sends** transcription results back to the client (optional)
7. **Handles** termination messages to clean up sessions

### Example Server Implementation (Pseudocode)

```typescript
// WebSocket handler
wss.on('connection', (ws) => {
  let session = null;

  ws.on('message', async (data) => {
    if (typeof data === 'string') {
      const msg = JSON.parse(data);

      if (msg.type === 'init') {
        // Authenticate
        const user = await validateApiKey(msg.api_key);
        if (!user) {
          ws.close(4001, 'Invalid API key');
          return;
        }

        // Create session
        session = {
          userId: user.id,
          sessionId: msg.session_id,
          deviceName: msg.device_name,
          sampleRate: msg.sample_rate,
          channels: msg.channels,
        };

        // Start transcription
        startTranscription(session);

        // Acknowledge
        ws.send(JSON.stringify({
          type: 'connected',
          message: 'Session started'
        }));
      }
      else if (msg.type === 'terminate') {
        // Clean up
        await finalizeSession(session);
      }
    }
    else {
      // Binary audio data
      if (session) {
        // Convert Buffer to Float32Array
        const float32Array = new Float32Array(
          data.buffer,
          data.byteOffset,
          data.byteLength / 4
        );

        // Process audio
        await processAudioChunk(session, float32Array);
      }
    }
  });
});
```

## Testing

### Without Server

If the server is not configured, the app works normally with just local features:

```
ℹ Voice Bird server not configured (optional)
```

### With Server

When configured, you'll see:

```
✓ Voice Bird server configuration loaded
🔗 Connecting to Voice Bird server...
   Server: https://voice-bird-app-ebrln.ondigitalocean.app
   API Key: your-api-k****
   Session: 550e8400-e29b-41d4-a716-446655440000
   WebSocket: wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream
✓ Connected to Voice Bird server!
📡 Streaming started for device: Microphone (Realtek HD Audio)
📊 Streamed 100 chunks (2.1s of audio)
📊 Streamed 200 chunks (4.2s of audio)
...
```

### Manual Testing Steps

1. **Setup server endpoint** (see SERVER_IMPLEMENTATION.md for Next.js guide)
2. **Configure .env** with server URL and API key
3. **Run the client**: `cargo run`
4. **Select an audio device** and start recording
5. **Verify streaming** in server logs
6. **Stop recording** and check for termination message

## Troubleshooting

### Connection Failures

```
✗ Failed to connect to server: Connection refused
```

**Solutions:**
- Verify `VOICE_BIRD_SERVER_URL` is correct
- Check if server is running
- Ensure firewall allows WebSocket connections

### Authentication Errors

```
✗ Server error: Authentication failed: Invalid API key
```

**Solutions:**
- Check `VOICE_BIRD_API_KEY` in .env
- Verify API key is valid on server
- Remove whitespace around `=` in .env

### No Audio Received on Server

**Client shows:**
```
📊 Streamed 100 chunks (2.1s of audio)
```

**But server receives nothing**

**Solutions:**
- Check server WebSocket handler is processing binary messages
- Verify audio format parsing (f32 little-endian)
- Check for buffer size limits on server

## Performance Considerations

### Bandwidth

- **Stereo @ 48kHz**: ~384 KB/s (48000 * 2 * 4 bytes)
- **Mono @ 16kHz**: ~64 KB/s (16000 * 1 * 4 bytes)

### Backpressure

The client adds a 10ms delay between chunks to prevent overwhelming the WebSocket:

```rust
sleep(TokioDuration::from_millis(10)).await;
```

Adjust this value in `src/server_streaming.rs` if needed.

### Memory

Each recording session creates:
- 1 transcription thread (if AssemblyAI configured)
- 1 server streaming thread (if server configured)
- Shared audio buffer (grows with recording duration)

## Multi-Device Support

### Same User, Multiple Devices

Each device creates a unique session ID:

```
Device 1: session_id = "550e8400-e29b-41d4-a716-446655440000"
Device 2: session_id = "7c9e6679-7425-40de-944b-e07fc1f90ae7"
Device 3: session_id = "9f3c5f8b-8e3d-4c9a-b2f1-5d8a7e6c9b4a"
```

All linked to the same user via `VOICE_BIRD_API_KEY`.

### Server-Side Aggregation

The server can query all sessions for a user:

```sql
SELECT * FROM streams WHERE userId = 'user123'
```

And combine transcripts across devices.

## Security Considerations

1. **API Key Protection**: Never commit .env files
2. **TLS/WSS**: Always use `wss://` in production
3. **API Key Rotation**: Implement key expiration on server
4. **Rate Limiting**: Prevent abuse with per-user limits
5. **Audio Storage**: Consider encryption for sensitive recordings

## Future Enhancements

- **Compression**: Use Opus codec to reduce bandwidth
- **Reconnection**: Auto-reconnect on network failures
- **Buffering**: Queue audio during disconnections
- **Real-time Feedback**: Display server transcription in UI
- **Multi-format**: Support MP3/OGG export formats
