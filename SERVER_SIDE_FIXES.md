# Server-Side Diagnostic Code for WebSocket Compression Issue

## Problem
Client test shows no compression negotiated, but main app still fails with "Reserved bits are non-zero" error after ~19 seconds (1900 chunks).

## Hypothesis
Server may be setting reserved bits despite `perMessageDeflate: false` configuration.

---

## Fix 1: Add Frame Inspection to Server

**File:** `/mnt/c/Projects/voice_bird/server.ts`

Add this code in the WebSocket connection handler:

```typescript
import { WebSocketServer } from 'ws';

const wss = new WebSocketServer({
  server,
  path: '/api/audio/stream',
  maxPayload: 10 * 1024 * 1024,
  perMessageDeflate: false, // Already set
});

wss.on('connection', (ws, req) => {
  // Log client extensions request
  console.log('[WebSocket] Client requested extensions:', req.headers['sec-websocket-extensions']);
  console.log('[WebSocket] Connection established, compression:', ws.extensions || 'none');

  let frameCount = 0;

  ws.on('message', async (data: Buffer | string, isBinary: boolean) => {
    frameCount++;

    // Inspect raw WebSocket frame for reserved bits
    if (data instanceof Buffer && data.length > 0) {
      const firstByte = data[0];
      const fin = (firstByte & 0x80) !== 0;
      const rsv1 = (firstByte & 0x40) !== 0;
      const rsv2 = (firstByte & 0x20) !== 0;
      const rsv3 = (firstByte & 0x10) !== 0;
      const opcode = firstByte & 0x0F;

      // Log every 100th frame, or if reserved bits are set
      if (frameCount % 100 === 0 || rsv1 || rsv2 || rsv3) {
        console.log(`[Frame #${frameCount}] FIN=${fin}, RSV1=${rsv1}, RSV2=${rsv2}, RSV3=${rsv3}, opcode=${opcode}, size=${data.length} bytes`);
      }

      // Alert if ANY reserved bit is set
      if (rsv1 || rsv2 || rsv3) {
        console.error(`⚠️  RESERVED BITS DETECTED IN INCOMING FRAME #${frameCount}!`);
        console.error(`    RSV1=${rsv1} (compression), RSV2=${rsv2}, RSV3=${rsv3}`);
        console.error(`    This frame has reserved bits but server has perMessageDeflate: false`);
      }
    }

    // Your existing message handling code here
    // logger.info({ data, userId: validation.userId }, 'received WebSocket message');
  });
});
```

---

## Fix 2: Log Outgoing Frames (If Server Sends Messages)

If your server sends messages back to the client (transcription results, etc.), add inspection:

```typescript
// Before sending any message
const originalSend = ws.send.bind(ws);
ws.send = function(data: any, callback?: (err?: Error) => void) {
  if (data instanceof Buffer && data.length > 0) {
    const firstByte = data[0];
    const rsv1 = (firstByte & 0x40) !== 0;
    const rsv2 = (firstByte & 0x20) !== 0;
    const rsv3 = (firstByte & 0x10) !== 0;

    if (rsv1 || rsv2 || rsv3) {
      console.error(`⚠️  SERVER SENDING FRAME WITH RESERVED BITS!`);
      console.error(`    RSV1=${rsv1}, RSV2=${rsv2}, RSV3=${rsv3}`);
    }
  }

  return originalSend(data, callback);
};
```

---

## Fix 3: Check ws Library Version

Verify you're using the latest `ws` library:

```bash
cd /mnt/c/Projects/voice_bird
npm list ws
```

Current version: `8.18.3`
Latest version: Check with `npm show ws version`

If outdated, upgrade:
```bash
npm install ws@latest
```

---

## Fix 4: Disable Cloudflare WebSocket Compression

If using Cloudflare, check these settings:

1. **Cloudflare Dashboard** → Your domain → **Speed** → **Optimization**
   - Disable "Auto Minify" for JavaScript
   - Disable "Rocket Loader"

2. **Cloudflare Dashboard** → **Network**
   - Check "WebSockets" setting
   - Verify "HTTP/2 Server Push" isn't interfering

3. **Add Page Rule** to bypass Cloudflare for WebSocket endpoint:
   - URL: `voice-bird-app-ebrln.ondigitalocean.app/api/audio/stream`
   - Settings: Cache Level = Bypass, Performance = Off

---

## Fix 5: Test Without Cloudflare

Temporarily point directly to DigitalOcean origin to rule out Cloudflare:

1. Find your DigitalOcean app's direct IP:
```bash
dig voice-bird-app-ebrln.ondigitalocean.app
```

2. Test client with direct connection:
```bash
# Assuming origin is on port 3000 or 8080
SERVER_URL=ws://ORIGIN_IP:PORT cargo run --release --bin voice_bird_desktop
```

---

## Expected Output After Fixes

### Server Logs (Good):
```
[WebSocket] Client requested extensions: undefined
[WebSocket] Connection established, compression: none
[Frame #100] FIN=true, RSV1=false, RSV2=false, RSV3=false, opcode=2, size=1920 bytes
[Frame #200] FIN=true, RSV1=false, RSV2=false, RSV3=false, opcode=2, size=1920 bytes
...
[Frame #1900] FIN=true, RSV1=false, RSV2=false, RSV3=false, opcode=2, size=1920 bytes
```

### Server Logs (Bad - Identifies Problem):
```
[Frame #1895] FIN=true, RSV1=true, RSV2=false, RSV3=false, opcode=2, size=1920 bytes
⚠️  RESERVED BITS DETECTED IN INCOMING FRAME #1895!
    RSV1=true (compression), RSV2=false, RSV3=false
    This frame has reserved bits but server has perMessageDeflate: false
```

This would prove the **client** is sending compressed frames (shouldn't happen with our fixes).

OR:

```
⚠️  SERVER SENDING FRAME WITH RESERVED BITS!
    RSV1=true, RSV2=false, RSV3=false
```

This would prove the **server** is sending compressed frames (server-side bug).

---

## Alternative: Use Wireshark to Capture Frames

If server logs don't reveal the issue, capture raw WebSocket traffic:

1. **Install Wireshark**
2. **Capture filter:** `tcp.port == 443` (or 3000 if testing locally)
3. **Look for WebSocket frames** after HTTP upgrade
4. **Inspect first byte** of each frame:
   - Bit 1 (0x40): RSV1
   - Bit 2 (0x20): RSV2
   - Bit 3 (0x10): RSV3

---

## Debugging Checklist

- [ ] Added frame inspection logging to server
- [ ] Checked `ws` library version
- [ ] Verified server logs show `perMessageDeflate: false`
- [ ] Tested if error occurs at same chunk count (#1900)
- [ ] Checked Cloudflare settings for WebSocket interference
- [ ] Tested direct connection bypassing Cloudflare
- [ ] Captured Wireshark traffic to see raw frames
- [ ] Checked if server sends any messages back to client
- [ ] Verified both client and server are on same protocol version (13)

---

## Contact Points for Support

**ws Library Issues:**
- GitHub: https://github.com/websockets/ws/issues
- Check existing issues for "reserved bits" or "perMessageDeflate"

**Cloudflare WebSocket Issues:**
- Community: https://community.cloudflare.com/
- Search for "websocket compression" or "reserved bits"

**tokio-tungstenite Issues:**
- GitHub: https://github.com/snapview/tokio-tungstenite/issues

---

## Files to Share with GPT-5

1. `WEBSOCKET_COMPRESSION_INVESTIGATION.md` (this directory)
2. This file (`SERVER_SIDE_FIXES.md`)
3. Client logs from `cargo run --release --bin voice_bird_desktop`
4. Server logs after adding frame inspection code
5. Output from test script showing TEST PASSED

---

## Quick Start for GPT-5

**Context:** WebSocket "Reserved bits are non-zero" error after 19 seconds of audio streaming.

**What we know:**
- ✅ Client NOT requesting compression (test proves it)
- ✅ Server configured with `perMessageDeflate: false`
- ❌ Error still occurs in production
- ⚠️  Cloudflare proxy in the middle

**What we need:**
- Identify which side (client/server) is setting reserved bits
- Determine if Cloudflare is interfering
- Find root cause and implement fix

**Action:** Add server-side frame inspection logging (see Fix 1 above) and share output.
