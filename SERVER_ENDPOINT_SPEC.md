# Voice Bird Server Endpoint Specification

## Overview

The Voice Bird Desktop application streams audio to a web server via WebSocket. This document specifies the server endpoint requirements for receiving and processing audio streams from the desktop client.

## WebSocket Endpoint

**URL Pattern:** `/api/audio/stream`

**Protocol:** WebSocket (WSS for HTTPS servers, WS for HTTP)

**Example Full URLs:**
- Production: `wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream`
- Local Development: `ws://localhost:3000/api/audio/stream`

## Authentication

### Authorization Header

The client sends authentication via HTTP header during the WebSocket handshake:

```
Authorization: vb_live_LPSHLXC-FU2FeOhjAM1FcI0qJdRe-J90BUcvCBGXQJc
```

**Header Name:** `Authorization`
**Header Value:** The raw API key (no "Bearer" prefix)

### Validation Required

The server MUST:
1. Extract the `Authorization` header from the WebSocket upgrade request
2. Validate the API key against the database
3. Reject connections with invalid/missing keys (HTTP 401 or 403)
4. Associate the validated user with the session

## Message Protocol

### 1. Initialization Message (Client → Server)

**Sent:** Immediately after WebSocket connection established
**Format:** JSON text frame
**Direction:** Client → Server

```json
{
  "type": "init",
  "api_key": "vb_live_LPSHLXC-FU2FeOhjAM1FcI0qJdRe-J90BUcvCBGXQJc",
  "session_id": "db07f283-9eb9-4810-b8f5-8f1e706ead71",
  "device_name": "Speakers (Realtek HD Audio)",
  "sample_rate": 48000,
  "channels": 2
}
```

**Fields:**

| Field | Type | Description | Example |
|-------|------|-------------|---------|
| `type` | string | Message type identifier | `"init"` |
| `api_key` | string | User's API key (duplicate for convenience) | `"vb_live_..."` |
| `session_id` | string | UUID for this recording session | `"db07f283-..."` |
| `device_name` | string | Human-readable audio device name | `"Speakers (Realtek HD Audio)"` |
| `sample_rate` | number | Audio sample rate in Hz | `48000` |
| `channels` | number | Number of audio channels (1=mono, 2=stereo) | `2` |

**Server Response Expected:**

```json
{
  "type": "connected",
  "message": "Session initialized successfully"
}
```

### 2. Audio Data Frames (Client → Server)

**Sent:** Continuously during recording (every ~10ms)
**Format:** Binary WebSocket frame
**Direction:** Client → Server

**Binary Format:**
- **Data Type:** IEEE 754 32-bit floating-point (f32)
- **Byte Order:** Little-endian
- **Range:** -1.0 to +1.0 (standard PCM float representation)
- **Chunk Size:** Variable (typically 480-2048 samples per chunk)

**Frame Structure:**
```
[sample_1_byte_0][sample_1_byte_1][sample_1_byte_2][sample_1_byte_3]
[sample_2_byte_0][sample_2_byte_1][sample_2_byte_2][sample_2_byte_3]
...
```

**Interpreting Binary Data:**

Each audio sample is 4 bytes (32 bits). For stereo audio, samples alternate: L, R, L, R, ...

**Example Parsing (Node.js):**
```javascript
// Receive binary frame
ws.on('message', (data) => {
  if (data instanceof Buffer) {
    // Convert bytes to f32 array
    const floatArray = new Float32Array(
      data.buffer,
      data.byteOffset,
      data.length / 4
    );

    // floatArray now contains audio samples
    // For stereo (channels=2): [L1, R1, L2, R2, ...]
    // For mono (channels=1): [S1, S2, S3, ...]
  }
});
```

**Example Parsing (Python):**
```python
import struct

def parse_audio_frame(binary_data):
    # Unpack little-endian f32 values
    num_samples = len(binary_data) // 4
    samples = struct.unpack(f'<{num_samples}f', binary_data)
    return samples
```

**Audio Processing:**

The server should:
1. Buffer incoming audio frames
2. Optionally resample/convert if AssemblyAI requires different format
3. Stream to transcription service (e.g., AssemblyAI Real-Time API)
4. Send transcription results back to client

### 3. Termination Message (Client → Server)

**Sent:** When user stops recording (ESC key pressed)
**Format:** JSON text frame
**Direction:** Client → Server

```json
{
  "type": "terminate",
  "session_id": "db07f283-9eb9-4810-b8f5-8f1e706ead71"
}
```

**Fields:**

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Message type identifier (always `"terminate"`) |
| `session_id` | string | UUID of the session being closed |

**Server Actions:**
1. Stop buffering audio for this session
2. Finalize transcription processing
3. Save session metadata to database
4. Close connection gracefully
5. Clean up resources

### 4. Server Response Messages (Server → Client)

**Format:** JSON text frames
**Direction:** Server → Client

#### 4.1 Connection Acknowledgment

```json
{
  "type": "connected",
  "message": "Session db07f283-9eb9-4810-b8f5-8f1e706ead71 initialized"
}
```

#### 4.2 Error Messages

```json
{
  "type": "error",
  "error": "Invalid API key",
  "message": "Authentication failed"
}
```

**Common Error Scenarios:**
- Invalid/expired API key
- Database connection failure
- Transcription service unavailable
- Quota exceeded

#### 4.3 Transcription Results

```json
{
  "type": "transcription",
  "message": "Hello, this is a test recording."
}
```

**Sent:** When transcription service (AssemblyAI) returns results

#### 4.4 Progress Updates (Optional)

```json
{
  "type": "progress",
  "message": "Processed 10 seconds of audio"
}
```

## Expected Server Logs

When functioning correctly, the server should log:

```
[INFO] New WebSocket connection established { remoteAddress: "::ffff:127.0.0.1" }
[INFO] Received init message { sessionId: "db07f283-...", deviceName: "Speakers...", deviceType: "output" }
[INFO] API key validated successfully { userId: "user_abc123" }
[INFO] Initializing audio stream { sessionId: "db07f283-...", sampleRate: 48000, channels: 2 }
[INFO] Stream initialized for session { sessionId: "db07f283-..." }
[INFO] AssemblyAI transcriber connected for session: db07f283-...
[INFO] Received audio frame { sessionId: "db07f283-...", bytes: 1920, samples: 480 }
[INFO] Transcription result { sessionId: "db07f283-...", text: "Hello world" }
[INFO] Received terminate message { sessionId: "db07f283-..." }
[INFO] Session terminated { sessionId: "db07f283-...", duration: "12.5s" }
```

## Implementation Examples

### Next.js App Router (Recommended)

**File:** `app/api/audio/stream/route.ts`

```typescript
import { NextRequest } from 'next/server';

export const runtime = 'edge'; // Or 'nodejs'

export async function GET(req: NextRequest) {
  // Check for WebSocket upgrade
  const upgrade = req.headers.get('upgrade');
  if (upgrade !== 'websocket') {
    return new Response('Expected WebSocket', { status: 426 });
  }

  // Validate API key from Authorization header
  const apiKey = req.headers.get('authorization');
  if (!apiKey || !await validateApiKey(apiKey)) {
    return new Response('Unauthorized', { status: 401 });
  }

  // Upgrade to WebSocket
  const { socket, response } = Deno.upgradeWebSocket(req);

  socket.onopen = () => {
    console.log('[INFO] WebSocket connection established');
  };

  socket.onmessage = async (event) => {
    if (typeof event.data === 'string') {
      // JSON message (init or terminate)
      const msg = JSON.parse(event.data);

      if (msg.type === 'init') {
        console.log('[INFO] Received init message', {
          sessionId: msg.session_id,
          deviceName: msg.device_name,
          sampleRate: msg.sample_rate,
          channels: msg.channels
        });

        // Initialize session
        await initializeSession(msg);

        // Send acknowledgment
        socket.send(JSON.stringify({
          type: 'connected',
          message: `Session ${msg.session_id} initialized`
        }));
      } else if (msg.type === 'terminate') {
        console.log('[INFO] Received terminate message', {
          sessionId: msg.session_id
        });
        await finalizeSession(msg.session_id);
      }
    } else {
      // Binary audio data
      const audioData = new Float32Array(await event.data.arrayBuffer());
      await processAudioChunk(audioData);
    }
  };

  socket.onerror = (error) => {
    console.error('[ERROR] WebSocket error:', error);
  };

  socket.onclose = () => {
    console.log('[INFO] WebSocket connection closed');
  };

  return response;
}
```

### Node.js with Express + ws Library

```javascript
const express = require('express');
const { WebSocketServer } = require('ws');

const app = express();
const server = app.listen(3000);
const wss = new WebSocketServer({
  server,
  path: '/api/audio/stream'
});

wss.on('connection', (ws, req) => {
  console.log('[INFO] New WebSocket connection established', {
    remoteAddress: req.socket.remoteAddress
  });

  // Validate API key from headers
  const apiKey = req.headers['authorization'];
  if (!validateApiKey(apiKey)) {
    ws.close(1008, 'Unauthorized');
    return;
  }

  let session = null;

  ws.on('message', async (data, isBinary) => {
    if (!isBinary) {
      // Text message (JSON)
      const msg = JSON.parse(data.toString());

      if (msg.type === 'init') {
        console.log('[INFO] Received init message', {
          sessionId: msg.session_id,
          deviceName: msg.device_name
        });

        session = await initializeSession(msg);

        ws.send(JSON.stringify({
          type: 'connected',
          message: 'Session initialized successfully'
        }));
      } else if (msg.type === 'terminate') {
        console.log('[INFO] Received terminate message', {
          sessionId: msg.session_id
        });
        await finalizeSession(msg.session_id);
      }
    } else {
      // Binary audio data
      const floatArray = new Float32Array(
        data.buffer,
        data.byteOffset,
        data.length / 4
      );

      await processAudioFrame(session, floatArray);
    }
  });

  ws.on('close', () => {
    console.log('[INFO] WebSocket connection closed');
    if (session) {
      finalizeSession(session.id);
    }
  });

  ws.on('error', (error) => {
    console.error('[ERROR] WebSocket error:', error);
  });
});
```

## Testing the Endpoint

### 1. Check if Endpoint Exists

```bash
curl -i https://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream
```

**Expected Response (if WebSocket endpoint exists):**
```
HTTP/1.1 426 Upgrade Required
Upgrade: websocket
```

**Actual Response (if endpoint missing):**
```
HTTP/1.1 404 Not Found
```

### 2. Test WebSocket Connection with `websocat`

Install websocat: `cargo install websocat` or `brew install websocat`

```bash
websocat -v \
  -H="Authorization: vb_live_LPSHLXC-FU2FeOhjAM1FcI0qJdRe-J90BUcvCBGXQJc" \
  wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream
```

**Expected:** WebSocket connection establishes, then you can send test messages:

```json
{"type":"init","api_key":"vb_live_LPSHLXC-FU2FeOhjAM1FcI0qJdRe-J90BUcvCBGXQJc","session_id":"test-123","device_name":"Test Device","sample_rate":48000,"channels":2}
```

### 3. Desktop App Test

```bash
cd /mnt/c/Projects/voice_bird_desktop
cargo run
```

**Expected Console Output (success):**
```
✓ Voice Bird server configuration loaded
🔗 Connecting to Voice Bird server...
   Server: https://voice-bird-app-ebrln.ondigitalocean.app
   API Key: vb_live_LP****
   Session: db07f283-9eb9-4810-b8f5-8f1e706ead71
   WebSocket: wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream
⏳ Attempting WebSocket connection...
✓ Connected to Voice Bird server!
📡 Streaming started for device: Speakers (Realtek HD Audio)
```

**Expected Console Output (failure - endpoint missing):**
```
✓ Voice Bird server configuration loaded
🔗 Connecting to Voice Bird server...
   Server: https://voice-bird-app-ebrln.ondigitalocean.app
   API Key: vb_live_LP****
   Session: db07f283-9eb9-4810-b8f5-8f1e706ead71
   WebSocket: wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream
⏳ Attempting WebSocket connection...
✗ Connection timeout (10 seconds)

The server did not respond within 10 seconds.

Most likely cause:
  → The WebSocket endpoint '/api/audio/stream' is NOT implemented on the server

Action required:
  1. Check if your server has a WebSocket handler at '/api/audio/stream'
  2. See SERVER_ENDPOINT_SPEC.md for implementation requirements
  3. Verify server logs show NO incoming connection attempts
```

## Troubleshooting Checklist

### Server-Side Checklist

- [ ] WebSocket endpoint `/api/audio/stream` is implemented
- [ ] Server accepts WebSocket upgrade requests
- [ ] Authorization header is extracted and validated
- [ ] Init message is parsed and session is created
- [ ] Binary frames are correctly interpreted as f32 little-endian
- [ ] Server sends "connected" acknowledgment after init
- [ ] Server handles terminate message and cleans up
- [ ] Proper logging is configured for debugging

### Client-Side Checklist

- [ ] `.env` file exists with `VOICE_BIRD_SERVER_URL` and `VOICE_BIRD_API_KEY`
- [ ] Server URL does NOT include trailing slash
- [ ] API key is valid and active in database
- [ ] Desktop app shows "Attempting WebSocket connection..." message
- [ ] No firewall blocking outbound WSS connections

### Common Issues

| Symptom | Cause | Solution |
|---------|-------|----------|
| Connection timeout (10s) | Endpoint doesn't exist | Implement `/api/audio/stream` handler |
| Connection refused | Server not running | Start server, verify it's listening |
| HTTP 401/403 | Invalid API key | Check key in database, verify header parsing |
| HTTP 426 Upgrade Required shown, but no WebSocket | Wrong HTTP method or missing Upgrade header | Ensure WebSocket upgrade handling |
| Connection drops immediately | Server closes after init | Check server logs for errors, verify binary frame handling |

## Audio Format Details

### PCM Float32 Specifications

- **Format:** IEEE 754 32-bit floating-point
- **Range:** -1.0 to +1.0
- **Zero Point:** 0.0 (silence)
- **Clipping:** Values outside [-1.0, +1.0] indicate clipping/distortion

### Channel Layout

**Mono (channels=1):**
```
[S1, S2, S3, S4, ...]
```

**Stereo (channels=2):**
```
[L1, R1, L2, R2, L3, R3, ...]
```

### Sample Rate Considerations

Common rates sent by desktop app:
- **48000 Hz** (most common, modern audio interfaces)
- **44100 Hz** (CD quality, some devices)
- **16000 Hz** (telephony, if configured)

If transcription service requires different rate (e.g., AssemblyAI prefers 16000 Hz), implement server-side resampling.

## Next Steps

After implementing the endpoint:

1. **Test locally first:** Use `ws://localhost:3000/api/audio/stream`
2. **Verify logs appear:** Check server console for connection messages
3. **Test with desktop app:** Run `cargo run` and select a device
4. **Monitor transcription:** Ensure AssemblyAI integration works
5. **Deploy to production:** Update server URL in desktop `.env`
6. **Test production endpoint:** Verify HTTPS/WSS works correctly

## Support

For implementation questions or issues:

1. Review this specification carefully
2. Check the desktop app logs for error details (10-second timeout provides clear diagnostics)
3. Test WebSocket endpoint independently using `websocat` or `curl`
4. Verify server logs show expected connection flow
5. Compare your implementation against the examples provided

---

**Document Version:** 1.0
**Last Updated:** 2025-11-08
**Desktop App Version:** Compatible with current `src/server_streaming.rs`
