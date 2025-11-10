# WebSocket Compression Issue - Summary for GPT-5

## Quick Context

**Application:** Rust desktop app streaming audio to Node.js WebSocket server
**Error:** `WebSocket protocol error: Reserved bits are non-zero` after ~19 seconds (1900 chunks)
**Current Status:** Client compression DISABLED and verified, but error persists

---

## The Mystery

### ✅ What's Working
```bash
# Test script output:
✅ TEST PASSED: No compression extensions negotiated
   The client and server are both compression-free
```

**Test proves:**
- Client does NOT send `Sec-WebSocket-Extensions` header
- Server does NOT respond with compression extensions
- Handshake is clean

### ❌ What's Broken
```bash
# Main app output after 19 seconds:
⚠️  COMPRESSION MISMATCH DETECTED
   Reserved bits are non-zero
```

**Contradiction:** If no compression is negotiated, why are reserved bits set?

---

## Changes Summary

### Client Side (Rust)
| File | Change | Purpose |
|------|--------|---------|
| `Cargo.toml:15` | `default-features = false` | Disable compression at compile time |
| `server_streaming.rs:117` | Manual header removal | Strip `Sec-WebSocket-Extensions` |
| `server_streaming.rs:130-481` | Enhanced logging | Track every chunk, bytes sent, timing |
| `test_websocket_compression.rs` | New test script | Validate compression is disabled |

### Server Side (Node.js)
| File | Current Config | Status |
|------|---------------|--------|
| `server.ts:50-57` | `perMessageDeflate: false` | ✅ Already disabled |

---

## Key Evidence

### 1. Test Script (Direct Connection)
```
SERVER_URL=wss://voice-bird-app-ebrln.ondigitalocean.app
Request Headers:  [No Sec-WebSocket-Extensions]
Response Headers: [No Sec-WebSocket-Extensions]
Result: ✅ PASS
```

### 2. Main App (Audio Streaming)
```
Chunks sent: 1-1900 (19.0s audio)
Error at: Chunk #1987
Result: ❌ FAIL - Reserved bits non-zero
```

### 3. Server Response Headers (from test)
```
server: cloudflare
cf-cache-status: DYNAMIC
```
**⚠️ Cloudflare is in the middle**

---

## Hypotheses Ranked by Likelihood

### 1. Cloudflare Proxy Issue (80% confidence)
**Theory:** Cloudflare might be:
- Buffering frames and adding compression after 19 seconds
- Applying WebSocket compression transparently
- Interfering with binary frames over time

**Evidence:**
- Test succeeds immediately (< 1 second)
- Error appears after sustained streaming (19 seconds)
- Response shows Cloudflare: `cf-ray`, `cf-cache-status`

**Test:**
```bash
# Bypass Cloudflare - connect to origin directly
SERVER_URL=ws://ORIGIN_IP:PORT cargo run --release
```

### 2. Server Bug in ws Library (15% confidence)
**Theory:** Node.js `ws` v8.18.3 has a bug where it:
- Starts setting reserved bits after N frames
- Has race condition with `perMessageDeflate: false`
- Leaks compression state from other connections

**Evidence:**
- Error at consistent chunk count (~1900)
- Server configured correctly but behavior doesn't match

**Test:** Add server-side frame inspection (see SERVER_SIDE_FIXES.md)

### 3. Buffer Size Threshold (5% confidence)
**Theory:** After 10MB+ of data:
- Client or server buffer triggers compression
- Some middleware activates
- Memory corruption sets reserved bits

**Evidence:**
- 1900 chunks × 1920 bytes = ~3.6 MB
- Still less than 10MB limit

---

## Critical Question

**Why does a simple test pass but sustained streaming fail?**

Possible answers:
1. **Time-based:** Something activates after duration
2. **Size-based:** Something activates after data volume
3. **State-based:** Connection state changes after N frames
4. **Infrastructure:** Proxy behavior differs for sustained connections

---

## Next Steps (Prioritized)

### Step 1: Add Server Frame Inspection (CRITICAL)
**File:** `/mnt/c/Projects/voice_bird/server.ts`

```typescript
ws.on('message', async (data: Buffer, isBinary) => {
  if (data.length > 0) {
    const firstByte = data[0];
    const rsv1 = (firstByte & 0x40) !== 0;
    const rsv2 = (firstByte & 0x20) !== 0;
    const rsv3 = (firstByte & 0x10) !== 0;

    if (rsv1 || rsv2 || rsv3) {
      console.error(`Frame with reserved bits: RSV1=${rsv1}, RSV2=${rsv2}, RSV3=${rsv3}`);
    }
  }
});
```

**Why:** This will definitively show if client is sending compressed frames or if error is elsewhere.

### Step 2: Test Bypassing Cloudflare (HIGH PRIORITY)
Get DigitalOcean origin IP and test direct connection.

**Why:** Eliminates Cloudflare as variable.

### Step 3: Capture Wireshark Traffic (MEDIUM PRIORITY)
Capture WebSocket frames and inspect raw bytes.

**Why:** Shows exact frame contents, no ambiguity.

---

## Files for GPT-5 Context

1. **WEBSOCKET_COMPRESSION_INVESTIGATION.md** - Full technical analysis
2. **SERVER_SIDE_FIXES.md** - Ready-to-use server code
3. **This file** - Quick summary

### Code Locations
```
Client (Rust):
- /mnt/c/Projects/voice_bird_desktop/Cargo.toml
- /mnt/c/Projects/voice_bird_desktop/src/server_streaming.rs
- /mnt/c/Projects/voice_bird_desktop/test_websocket_compression.rs

Server (Node.js):
- /mnt/c/Projects/voice_bird/server.ts (lines 50-57)
- /mnt/c/Projects/voice_bird/package.json (ws version)
```

---

## Technical Background

### WebSocket Frame Bits
```
Byte 0: [FIN|RSV1|RSV2|RSV3|OPCODE(4 bits)]
        [1  |0   |0   |0   |0010] = Normal binary frame
        [1  |1   |0   |0   |0010] = Compressed binary frame
```

### Reserved Bits Purpose
- **RSV1:** Per-message compression (permessage-deflate extension)
- **RSV2:** Reserved for future extensions
- **RSV3:** Reserved for future extensions

### Error Condition
Error occurs when:
```
(Frame has RSV bits set) AND (Extension not negotiated)
```

---

## What We Need from GPT-5

1. **Analyze why test passes but main app fails**
   - Is it time-related? Size-related? State-related?

2. **Investigate Cloudflare WebSocket behavior**
   - Does Cloudflare compress WebSocket frames?
   - When/why would it start doing so mid-connection?

3. **Suggest additional diagnostics**
   - What else can we log/capture to identify root cause?

4. **Provide definitive fix**
   - Based on analysis, what's the actual solution?

---

## Test Results to Share with GPT-5

### Compression Test (Standalone)
```
🔬 WebSocket Compression Test
═══════════════════════════════════════════
📡 Connecting to: wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream

📤 Request Headers:
   host: "voice-bird-app-ebrln.ondigitalocean.app"
   connection: "Upgrade"
   upgrade: "websocket"
   sec-websocket-version: "13"
   sec-websocket-key: "+i2Gz2oPO0yNGGlGokldDA=="
   authorization: "vb_live_..."

✅ Connection successful!

📥 Response Headers:
   date: "Mon, 10 Nov 2025 13:05:41 GMT"
   sec-websocket-accept: "YZOOBfnEpRwThCJfsUj088ifSSY="
   server: "cloudflare"
   cf-ray: "99c5c349da4b350c-WAW"

✅ TEST PASSED: No compression extensions negotiated
   The client and server are both compression-free
```

### Main App (Audio Streaming) - FAILS
```
📋 WebSocket Config:
   Compression: DISABLED (via default-features = false)

✓ Connected to Voice Bird server!
📡 HTTP Response: 101 Switching Protocols
   Extensions: None (compression disabled)

📤 Chunk #1: 480 samples (1920 bytes)
   ✓ Chunk #1 sent successfully
...
📤 Chunk #1900: 480 samples (1920 bytes)
   ✓ Chunk #1900 sent successfully
...
✗ WebSocket error: WebSocket protocol error: Reserved bits are non-zero
⚠️  COMPRESSION MISMATCH DETECTED
```

---

## Environment Details

**Client:**
- OS: Windows (WSL2)
- Language: Rust
- WebSocket: tokio-tungstenite 0.23 (default-features = false)

**Server:**
- Platform: DigitalOcean App Platform
- Language: Node.js
- WebSocket: ws 8.18.3
- Proxy: Cloudflare

**Connection:**
- Protocol: WSS (secure WebSocket)
- Endpoint: `wss://voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream`
- Auth: Bearer token in Authorization header

---

## Expected Outcome

After implementing fixes, should see:
```
✅ Connection successful
✅ Extensions: None (compression disabled)
✅ Streamed 10000+ chunks (100+ seconds)
✅ No reserved bits errors
✅ Clean termination
```

---

## Questions?

If GPT-5 needs more information:
1. Run specific test/diagnostic
2. Add more logging
3. Capture network traffic
4. Check additional config

All files are in `/mnt/c/Projects/voice_bird_desktop/` and `/mnt/c/Projects/voice_bird/`
