# WebSocket "Reserved Bits Are Non-Zero" Error Investigation

## Problem Summary

Desktop Rust application experiences WebSocket protocol error after streaming ~1900 audio chunks (19 seconds):
```
✗ WebSocket error: WebSocket protocol error: Reserved bits are non-zero
Failed to send audio chunk #1987: IO error: An established connection was aborted by the software in your host machine. (os error 10053)
```

**Key Finding:** Test shows client is NOT requesting compression, but error still occurs.

---

## System Configuration

### Server Configuration
- **File:** `/mnt/c/Projects/voice_bird/server.ts`
- **WebSocket Library:** `ws` v8.18.3
- **Configuration:**
```typescript
const wss = new WebSocketServer({
  server,
  path: '/api/audio/stream',
  maxPayload: 10 * 1024 * 1024, // 10MB
  perMessageDeflate: false,  // ✅ Compression disabled
});
```

### Client Configuration (Before Fixes)
- **File:** `/mnt/c/Projects/voice_bird_desktop/src/server_streaming.rs`
- **WebSocket Library:** `tokio-tungstenite` v0.23
- **Issue:** Client was using `..Default::default()` which may enable compression

---

## Changes Made

### 1. Cargo.toml - Disable Compression Feature
**File:** `/mnt/c/Projects/voice_bird_desktop/Cargo.toml`

**Before:**
```toml
tokio-tungstenite = { version = "0.23", features = ["native-tls"] }
```

**After:**
```toml
tokio-tungstenite = { version = "0.23", features = ["connect", "native-tls"], default-features = false }
```

**Rationale:**
- `default-features = false` removes compression support from compilation
- Explicitly add `connect` feature (required for `connect_async_with_config`)
- Keep `native-tls` for SSL/TLS support

### 2. Manual Header Removal (Defensive)
**File:** `/mnt/c/Projects/voice_bird_desktop/src/server_streaming.rs`

**Added after line 113:**
```rust
// CRITICAL FIX: Remove Sec-WebSocket-Extensions header to prevent compression negotiation
// This ensures no compression is requested even if the library tries to add it
request.headers_mut().remove("Sec-WebSocket-Extensions");

println!(
    "{}",
    style("🔧 Manually removed Sec-WebSocket-Extensions header (prevents compression)").yellow()
);
```

### 3. Enhanced Diagnostic Logging

**Added throughout `server_streaming.rs`:**

#### Connection Phase Logging (Lines 130-149)
```rust
println!("📋 WebSocket Config:");
println!("   Max message size: {} bytes", ...);
println!("   Max frame size: {} bytes", ...);
println!("   Write buffer size: {} bytes", ...);
println!("   Compression: DISABLED (via default-features = false)");
```

#### HTTP Response Headers (Lines 157-183)
```rust
// Log HTTP response details for debugging
println!("📡 HTTP Response: {}", stream.1.status());

// Check for WebSocket extension headers (especially compression)
if let Some(extensions) = stream.1.headers().get("Sec-WebSocket-Extensions") {
    println!("   Extensions negotiated: {:?}", extensions);
} else {
    println!("   Extensions: None (compression disabled)");
}
```

#### Init Message Details (Lines 241-273)
```rust
println!("📤 Sending init message ({} bytes)...", init_json.len());
println!("   Session ID: {}", session_id);
println!("   Sample Rate: {} Hz", sample_rate);
println!("   Channels: {}", channels);
// ... success/error logging
```

#### Per-Chunk Streaming Metrics (Lines 370-481)
```rust
let mut total_bytes_sent = 0u64;
let start_time = std::time::Instant::now();

// Log first 10 chunks, then every 100th
let should_log = chunk_count <= 10 || chunk_count % 100 == 0;

if should_log {
    println!(
        "📤 Chunk #{}: {} samples ({} bytes) | Total: {:.1}s audio, {:.2} MB sent | Elapsed: {:.1}s",
        chunk_count, audio_chunk.len(), bytes_len, duration_secs,
        total_bytes_sent as f64 / 1_000_000.0, elapsed.as_secs_f32()
    );
}
```

#### Enhanced Error Detection (Lines 343-363)
```rust
Err(e) => {
    eprintln!("✗ WebSocket error: {}", e);
    eprintln!("   Error details: {:?}", e);

    // Detect compression mismatch
    let error_msg = format!("{}", e);
    if error_msg.contains("Reserved bits") {
        eprintln!("⚠️  COMPRESSION MISMATCH DETECTED");
        eprintln!("   Server has: perMessageDeflate: false (compression disabled)");
        eprintln!("   Action: Check 'Sec-WebSocket-Extensions' header in connection log above");
    }
    break;
}
```

---

## Test Results

### Compression Test Script
**File:** `/mnt/c/Projects/voice_bird_desktop/test_websocket_compression.rs`

**Test Output:**
```
✅ TEST PASSED: No compression extensions negotiated
   The client and server are both compression-free
```

**Request Headers Sent:**
```
host: "voice-bird-app-ebrln.ondigitalocean.app"
connection: "Upgrade"
upgrade: "websocket"
sec-websocket-version: "13"
sec-websocket-key: "+i2Gz2oPO0yNGGlGokldDA=="
authorization: "vb_live_LPSHLXC-FU2FeOhjAM1FcI0qJdRe-J90BUcvCBGXQJc"
```

**Key Observation:** No `Sec-WebSocket-Extensions` header present ✅

**Response Headers Received:**
```
sec-websocket-accept: "YZOOBfnEpRwThCJfsUj088ifSSY="
```

**Key Observation:** No `Sec-WebSocket-Extensions` in response ✅

---

## Current Status

### ✅ What's Working
1. Client no longer requests compression during handshake
2. Server confirms no compression extensions
3. Test script passes compression validation

### ❌ Still Broken
Main application (`cargo run --bin voice_bird_desktop`) still shows:
```
⚠️  COMPRESSION MISMATCH DETECTED
```

After ~1900 chunks (19 seconds of audio streaming).

---

## Hypothesis: Server-Side Issue

Since the test proves no compression is negotiated, but the error still occurs in production, the problem is likely:

### Theory 1: Server Middleware Adding Compression
Despite `perMessageDeflate: false`, something in the server stack might be compressing frames:
- Cloudflare proxy
- Reverse proxy (nginx, etc.)
- Express middleware
- Some server-side WebSocket middleware

**Evidence:**
- Response headers show Cloudflare: `server: "cloudflare"`
- Response has cache headers: `cf-cache-status: "DYNAMIC"`

### Theory 2: Reserved Bits Set by Non-Compression Extension
WebSocket frames have 3 reserved bits (RSV1, RSV2, RSV3):
- RSV1 = compression (per-message-deflate)
- RSV2 & RSV3 = other extensions

Something might be setting RSV2 or RSV3.

### Theory 3: Server Bug in ws Library
The server's `ws` library v8.18.3 might have a bug where it sets reserved bits even with `perMessageDeflate: false`.

---

## Files Modified

### Client (Rust Desktop App)
1. `/mnt/c/Projects/voice_bird_desktop/Cargo.toml`
   - Line 15: Changed tokio-tungstenite configuration

2. `/mnt/c/Projects/voice_bird_desktop/src/server_streaming.rs`
   - Lines 115-122: Manual header removal
   - Lines 130-149: WebSocket config logging
   - Lines 157-183: HTTP response header inspection
   - Lines 241-273: Init message logging
   - Lines 343-363: Enhanced error detection
   - Lines 370-481: Per-chunk streaming metrics

3. `/mnt/c/Projects/voice_bird_desktop/test_websocket_compression.rs` (NEW)
   - Full test script for compression validation

4. `/mnt/c/Projects/voice_bird_desktop/Cargo.toml`
   - Lines 6-8: Added test binary configuration

### Server (No changes made yet)
- `/mnt/c/Projects/voice_bird/server.ts` (verified configuration only)

---

## Recommended Next Steps

### 1. Server-Side Logging (HIGHEST PRIORITY)
Add logging to `/mnt/c/Projects/voice_bird/server.ts` to inspect incoming frames:

```typescript
wss.on('connection', (ws, req) => {
  // Log negotiated extensions
  console.log('Client extensions:', req.headers['sec-websocket-extensions']);

  ws.on('message', async (data: Buffer | string, isBinary: boolean) => {
    // Log frame metadata
    if (data instanceof Buffer) {
      const firstByte = data[0];
      const rsv1 = (firstByte & 0x40) !== 0;
      const rsv2 = (firstByte & 0x20) !== 0;
      const rsv3 = (firstByte & 0x10) !== 0;

      console.log(`Frame: RSV1=${rsv1}, RSV2=${rsv2}, RSV3=${rsv3}, size=${data.length}`);

      if (rsv1 || rsv2 || rsv3) {
        console.error(`⚠️  Reserved bits detected! RSV1=${rsv1}, RSV2=${rsv2}, RSV3=${rsv3}`);
      }
    }
  });
});
```

### 2. Check Cloudflare Settings
- Verify Cloudflare isn't applying WebSocket compression
- Check if "Auto Minify" or "Rocket Loader" affects WebSocket traffic

### 3. Test Direct Connection (Bypass Cloudflare)
Try connecting directly to DigitalOcean origin:
```bash
SERVER_URL=ws://direct-origin-ip:port cargo run --release
```

### 4. Update Server ws Library
Try upgrading from `ws` v8.18.3 to latest:
```bash
npm install ws@latest
```

### 5. Add Frame-Level Logging to Client
Capture raw WebSocket frames in Rust to see reserved bits:

```rust
// In server_streaming.rs, before parsing message
match msg {
    Ok(Message::Binary(ref data)) => {
        if !data.is_empty() {
            let first_byte = data[0];
            let rsv1 = (first_byte & 0x40) != 0;
            let rsv2 = (first_byte & 0x20) != 0;
            let rsv3 = (first_byte & 0x10) != 0;

            if rsv1 || rsv2 || rsv3 {
                eprintln!("⚠️  Server sent frame with reserved bits: RSV1={}, RSV2={}, RSV3={}", rsv1, rsv2, rsv3);
            }
        }
    }
}
```

---

## Technical Details

### WebSocket Frame Structure
```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-------+-+-------------+-------------------------------+
|F|R|R|R| opcode|M| Payload len |    Extended payload length    |
|I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
|N|V|V|V|       |S|             |   (if payload len==126/127)   |
| |1|2|3|       |K|             |                               |
+-+-+-+-+-------+-+-------------+-------------------------------+
```

- **RSV1** (bit 1): Per-message compression
- **RSV2** (bit 2): Reserved for future extensions
- **RSV3** (bit 3): Reserved for future extensions

### Error Occurs When:
- Client/server sends frames with RSV bits set to 1
- But the other side hasn't negotiated that extension
- Protocol violation → connection closes with error

---

## Complete File Listings

### Modified: Cargo.toml
```toml
[package]
name = "voice_bird_desktop"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "test_compression"
path = "test_websocket_compression.rs"

[dependencies]
cpal = "0.15"
dialoguer = "0.11"
console = "0.15"
crossterm = "0.27"
anyhow = "1.0"
hound = "3.5"
chrono = "0.4"
tokio = { version = "1.0", features = ["full"] }
tokio-tungstenite = { version = "0.23", features = ["connect", "native-tls"], default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
base64 = "0.22"
futures-util = "0.3"
dotenvy = "0.15"
http = "1.0"
ratatui = "0.29"
uuid = { version = "1.0", features = ["v4"] }

[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Media_Audio",
    "Win32_Media_Multimedia",
    "Win32_System_Com",
    "Win32_System_Com_StructuredStorage",
    "Win32_System_Threading",
    "Win32_Foundation",
    "Win32_System_ProcessStatus",
    "Win32_UI_Shell_PropertiesSystem",
    "Win32_Devices_FunctionDiscovery",
] }
windows-core = "0.58"
```

### Key Code Sections

#### Header Removal (server_streaming.rs:115-122)
```rust
// CRITICAL FIX: Remove Sec-WebSocket-Extensions header to prevent compression negotiation
// This ensures no compression is requested even if the library tries to add it
request.headers_mut().remove("Sec-WebSocket-Extensions");

println!(
    "{}",
    style("🔧 Manually removed Sec-WebSocket-Extensions header (prevents compression)").yellow()
);
```

#### Extension Header Inspection (server_streaming.rs:163-174)
```rust
// Check for WebSocket extension headers (especially compression)
if let Some(extensions) = stream.1.headers().get("Sec-WebSocket-Extensions") {
    println!(
        "{}",
        style(format!("   Extensions negotiated: {:?}", extensions)).dim()
    );
} else {
    println!(
        "{}",
        style("   Extensions: None (compression disabled)").dim()
    );
}
```

---

## Questions for GPT-5

1. **Why does the test pass but the main app fails?**
   - Test shows no compression negotiated
   - Main app still gets "Reserved bits are non-zero" error after 19 seconds

2. **Could Cloudflare be adding compression?**
   - Response headers show Cloudflare proxy
   - Might Cloudflare be compressing WebSocket frames transparently?

3. **Is this a timing issue?**
   - Error occurs consistently around chunk #1900 (19 seconds)
   - Could this be a buffer overflow or state management issue?

4. **Should we capture raw frame bytes?**
   - Need to see which reserved bits are actually set (RSV1, RSV2, or RSV3)
   - This would definitively identify the source

5. **Is the server's ws library buggy?**
   - ws v8.18.3 with `perMessageDeflate: false`
   - Could it still be setting reserved bits incorrectly?

---

## Summary

**Client-side compression is now DISABLED and verified working.** The "Reserved bits are non-zero" error persists, suggesting the issue is either:

1. Server sending compressed frames despite configuration
2. Proxy/CDN (Cloudflare) interfering with WebSocket frames
3. Non-compression extension setting reserved bits
4. Bug in server's `ws` library

**Next step:** Add server-side frame inspection logging to identify which reserved bit is being set and by whom.
