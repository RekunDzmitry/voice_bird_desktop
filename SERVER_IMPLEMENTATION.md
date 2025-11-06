# Server Implementation Guide (Next.js)

This guide explains how to implement the server-side WebSocket endpoint for Voice Bird audio streaming.

## Quick Start

### Option 1: Next.js Custom Server (Recommended)

Next.js doesn't support WebSockets natively in API routes, so we need a custom server.

#### 1. Install Dependencies

```bash
npm install ws @types/ws
npm install prisma @prisma/client  # For database
```

#### 2. Create Custom Server

Create `server.js` in your project root:

```javascript
const { createServer } = require('http');
const { parse } = require('url');
const next = require('next');
const { WebSocketServer } = require('ws');

const dev = process.env.NODE_ENV !== 'production';
const hostname = 'localhost';
const port = 3000;

const app = next({ dev, hostname, port });
const handle = app.getRequestHandler();

app.prepare().then(() => {
  const server = createServer(async (req, res) => {
    try {
      const parsedUrl = parse(req.url, true);
      await handle(req, res, parsedUrl);
    } catch (err) {
      console.error('Error occurred handling', req.url, err);
      res.statusCode = 500;
      res.end('internal server error');
    }
  });

  // WebSocket Server
  const wss = new WebSocketServer({
    server,
    path: '/api/audio/stream'
  });

  wss.on('connection', handleAudioStreamConnection);

  server.listen(port, (err) => {
    if (err) throw err;
    console.log(`> Ready on http://${hostname}:${port}`);
    console.log(`> WebSocket endpoint: ws://${hostname}:${port}/api/audio/stream`);
  });
});

// Handler implementation (see below)
function handleAudioStreamConnection(ws, req) {
  // ... implementation ...
}
```

#### 3. Update package.json

```json
{
  "scripts": {
    "dev": "node server.js",
    "build": "next build",
    "start": "NODE_ENV=production node server.js"
  }
}
```

### Option 2: Vercel Deployment

Vercel doesn't support WebSockets. Use one of these alternatives:

1. **Deploy WebSocket server separately** on Railway, Render, or DigitalOcean
2. **Use Pusher/Ably** for real-time communication
3. **Use Server-Sent Events (SSE)** for server-to-client only

## WebSocket Handler Implementation

### Complete Handler

```javascript
const { PrismaClient } = require('@prisma/client');
const prisma = new PrismaClient();

// Store active sessions
const activeSessions = new Map();

async function handleAudioStreamConnection(ws, req) {
  console.log('New WebSocket connection');

  let sessionData = null;
  let audioBuffer = [];
  let transcriptionService = null;

  ws.on('message', async (data) => {
    try {
      // Handle text messages (JSON)
      if (typeof data === 'string' || Buffer.isBuffer(data) && isJSON(data)) {
        const message = JSON.parse(data.toString());

        if (message.type === 'init') {
          sessionData = await handleInit(ws, message);
          if (sessionData) {
            transcriptionService = await startTranscription(sessionData);
          }
        }
        else if (message.type === 'terminate') {
          await handleTerminate(sessionData, audioBuffer, transcriptionService);
        }
      }
      // Handle binary messages (audio data)
      else {
        if (sessionData) {
          await handleAudioData(
            sessionData,
            data,
            audioBuffer,
            transcriptionService
          );
        }
      }
    } catch (error) {
      console.error('WebSocket message error:', error);
      ws.send(JSON.stringify({
        type: 'error',
        error: error.message
      }));
    }
  });

  ws.on('close', async () => {
    console.log('WebSocket connection closed');
    if (sessionData) {
      await handleTerminate(sessionData, audioBuffer, transcriptionService);
    }
  });

  ws.on('error', (error) => {
    console.error('WebSocket error:', error);
  });
}

// Helper to check if buffer contains JSON
function isJSON(buffer) {
  const str = buffer.toString().trim();
  return str.startsWith('{') || str.startsWith('[');
}

// Handle initialization message
async function handleInit(ws, message) {
  const { api_key, session_id, device_name, sample_rate, channels } = message;

  // Validate API key
  const user = await prisma.user.findUnique({
    where: { apiKey: api_key }
  });

  if (!user) {
    ws.send(JSON.stringify({
      type: 'error',
      error: 'Invalid API key'
    }));
    ws.close(4001, 'Unauthorized');
    return null;
  }

  // Create session record
  const stream = await prisma.stream.create({
    data: {
      userId: user.id,
      sessionId: session_id,
      deviceName: device_name,
      sampleRate: sample_rate,
      channels: channels,
      startTime: new Date(),
    }
  });

  const sessionData = {
    streamId: stream.id,
    userId: user.id,
    sessionId: session_id,
    deviceName: device_name,
    sampleRate: sample_rate,
    channels: channels,
    ws: ws,
  };

  activeSessions.set(session_id, sessionData);

  // Send acknowledgment
  ws.send(JSON.stringify({
    type: 'connected',
    message: `Audio streaming session started for ${device_name}`
  }));

  console.log(`Session started: ${session_id} for user ${user.id}`);

  return sessionData;
}

// Handle audio data
async function handleAudioData(sessionData, data, audioBuffer, transcriptionService) {
  // Convert Buffer to Float32Array
  const float32Array = new Float32Array(
    data.buffer,
    data.byteOffset,
    data.byteLength / 4
  );

  // Store in buffer (optional - for saving later)
  audioBuffer.push(float32Array);

  // Forward to transcription service
  if (transcriptionService) {
    await transcriptionService.sendAudio(float32Array);
  }

  // Optional: Log progress
  if (audioBuffer.length % 100 === 0) {
    const totalSamples = audioBuffer.reduce((sum, arr) => sum + arr.length, 0);
    const duration = totalSamples / (sessionData.sampleRate * sessionData.channels);
    console.log(`Session ${sessionData.sessionId}: ${duration.toFixed(1)}s of audio received`);
  }
}

// Handle termination
async function handleTerminate(sessionData, audioBuffer, transcriptionService) {
  if (!sessionData) return;

  console.log(`Terminating session: ${sessionData.sessionId}`);

  // Stop transcription
  if (transcriptionService) {
    await transcriptionService.stop();
  }

  // Calculate total duration
  const totalSamples = audioBuffer.reduce((sum, arr) => sum + arr.length, 0);
  const duration = totalSamples / (sessionData.sampleRate * sessionData.channels);

  // Update database
  await prisma.stream.update({
    where: { id: sessionData.streamId },
    data: {
      endTime: new Date(),
      duration: duration,
    }
  });

  // Optional: Save audio file
  // await saveAudioFile(sessionData, audioBuffer);

  activeSessions.delete(sessionData.sessionId);

  console.log(`Session ${sessionData.sessionId} terminated. Duration: ${duration.toFixed(1)}s`);
}
```

## Transcription Service Integration

### Using AssemblyAI

```javascript
const { RealtimeTranscriber } = require('assemblyai');

async function startTranscription(sessionData) {
  const transcriber = new RealtimeTranscriber({
    token: process.env.ASSEMBLYAI_API_KEY,
    sampleRate: sessionData.sampleRate,
  });

  transcriber.on('open', () => {
    console.log('AssemblyAI connection opened');
  });

  transcriber.on('transcript', async (transcript) => {
    if (transcript.message_type === 'FinalTranscript') {
      // Save to database
      await prisma.transcript.create({
        data: {
          streamId: sessionData.streamId,
          text: transcript.text,
          timestamp: new Date(),
        }
      });

      // Send to client
      sessionData.ws.send(JSON.stringify({
        type: 'transcription',
        message: transcript.text,
      }));
    }
  });

  transcriber.on('error', (error) => {
    console.error('Transcription error:', error);
  });

  await transcriber.connect();

  return {
    async sendAudio(float32Array) {
      // Convert f32 to i16 for AssemblyAI
      const pcm16 = new Int16Array(float32Array.length);
      for (let i = 0; i < float32Array.length; i++) {
        pcm16[i] = Math.max(-32768, Math.min(32767, float32Array[i] * 32767));
      }

      // Downmix to mono if needed
      let monoData;
      if (sessionData.channels === 1) {
        monoData = pcm16;
      } else {
        const frameCount = pcm16.length / sessionData.channels;
        monoData = new Int16Array(frameCount);
        for (let i = 0; i < frameCount; i++) {
          let sum = 0;
          for (let ch = 0; ch < sessionData.channels; ch++) {
            sum += pcm16[i * sessionData.channels + ch];
          }
          monoData[i] = Math.floor(sum / sessionData.channels);
        }
      }

      await transcriber.sendAudio(monoData);
    },

    async stop() {
      await transcriber.close();
    }
  };
}
```

## Database Schema (Prisma)

### schema.prisma

```prisma
datasource db {
  provider = "postgresql"
  url      = env("DATABASE_URL")
}

generator client {
  provider = "prisma-client-js"
}

model User {
  id        String   @id @default(cuid())
  email     String   @unique
  name      String?
  apiKey    String   @unique @default(cuid())
  createdAt DateTime @default(now())
  streams   Stream[]
}

model Stream {
  id          String       @id @default(cuid())
  userId      String
  sessionId   String       @unique
  deviceName  String
  sampleRate  Int
  channels    Int
  startTime   DateTime     @default(now())
  endTime     DateTime?
  duration    Float?       @default(0)
  audioUrl    String?
  transcripts Transcript[]

  user        User         @relation(fields: [userId], references: [id])

  @@index([userId])
  @@index([sessionId])
}

model Transcript {
  id        String   @id @default(cuid())
  streamId  String
  text      String   @db.Text
  timestamp DateTime @default(now())

  stream    Stream   @relation(fields: [streamId], references: [id])

  @@index([streamId])
}
```

### Migrations

```bash
npx prisma migrate dev --name init
npx prisma generate
```

## API Routes (REST)

### Get User Streams

```typescript
// app/api/streams/route.ts
import { NextRequest, NextResponse } from 'next/server';
import { PrismaClient } from '@prisma/client';

const prisma = new PrismaClient();

export async function GET(request: NextRequest) {
  const apiKey = request.headers.get('authorization');

  if (!apiKey) {
    return NextResponse.json({ error: 'Missing API key' }, { status: 401 });
  }

  const user = await prisma.user.findUnique({
    where: { apiKey }
  });

  if (!user) {
    return NextResponse.json({ error: 'Invalid API key' }, { status: 401 });
  }

  const streams = await prisma.stream.findMany({
    where: { userId: user.id },
    include: {
      transcripts: {
        orderBy: { timestamp: 'asc' }
      }
    },
    orderBy: { startTime: 'desc' }
  });

  return NextResponse.json(streams);
}
```

### Get Stream by ID

```typescript
// app/api/streams/[id]/route.ts
import { NextRequest, NextResponse } from 'next/server';
import { PrismaClient } from '@prisma/client';

const prisma = new PrismaClient();

export async function GET(
  request: NextRequest,
  { params }: { params: { id: string } }
) {
  const apiKey = request.headers.get('authorization');

  if (!apiKey) {
    return NextResponse.json({ error: 'Missing API key' }, { status: 401 });
  }

  const user = await prisma.user.findUnique({
    where: { apiKey }
  });

  if (!user) {
    return NextResponse.json({ error: 'Invalid API key' }, { status: 401 });
  }

  const stream = await prisma.stream.findFirst({
    where: {
      id: params.id,
      userId: user.id
    },
    include: {
      transcripts: {
        orderBy: { timestamp: 'asc' }
      }
    }
  });

  if (!stream) {
    return NextResponse.json({ error: 'Stream not found' }, { status: 404 });
  }

  return NextResponse.json(stream);
}
```

## Environment Variables

Create `.env` file:

```env
DATABASE_URL="postgresql://user:password@localhost:5432/voicebird"
ASSEMBLYAI_API_KEY="your-assemblyai-api-key"
NODE_ENV="development"
```

## Deployment

### DigitalOcean App Platform

1. Create a new App
2. Connect your Git repository
3. Set environment variables
4. Deploy

The WebSocket server will be available at your app's URL.

### Railway

```bash
railway login
railway init
railway add
railway up
```

### Docker

```dockerfile
FROM node:18-alpine

WORKDIR /app

COPY package*.json ./
RUN npm ci

COPY . .
RUN npx prisma generate
RUN npm run build

EXPOSE 3000

CMD ["npm", "start"]
```

## Testing

### Manual Testing with wscat

```bash
npm install -g wscat

# Connect
wscat -c wss://your-server.com/api/audio/stream -H "Authorization: your-api-key"

# Send init message
{"type":"init","api_key":"your-api-key","session_id":"test-123","device_name":"Test Device","sample_rate":48000,"channels":2}
```

### Automated Testing

```javascript
const WebSocket = require('ws');

async function testAudioStream() {
  const ws = new WebSocket('ws://localhost:3000/api/audio/stream');

  ws.on('open', () => {
    console.log('Connected');

    // Send init
    ws.send(JSON.stringify({
      type: 'init',
      api_key: 'test-api-key',
      session_id: 'test-session',
      device_name: 'Test Device',
      sample_rate: 48000,
      channels: 2
    }));

    // Send dummy audio data
    const audioData = new Float32Array(4800); // 0.1s at 48kHz
    for (let i = 0; i < audioData.length; i++) {
      audioData[i] = Math.sin(2 * Math.PI * 440 * i / 48000); // 440Hz tone
    }

    ws.send(Buffer.from(audioData.buffer));

    // Terminate after 1 second
    setTimeout(() => {
      ws.send(JSON.stringify({
        type: 'terminate',
        session_id: 'test-session'
      }));
      ws.close();
    }, 1000);
  });

  ws.on('message', (data) => {
    console.log('Received:', data.toString());
  });
}

testAudioStream();
```

## Monitoring

### Active Sessions

```javascript
// Add endpoint to check active sessions
app.get('/api/admin/sessions', (req, res) => {
  const sessions = Array.from(activeSessions.values()).map(s => ({
    sessionId: s.sessionId,
    userId: s.userId,
    deviceName: s.deviceName,
    duration: calculateDuration(s)
  }));

  res.json(sessions);
});
```

### Metrics

Track:
- Active connections
- Audio data received (bytes)
- Transcription latency
- Error rates

Use tools like:
- **Prometheus** + Grafana
- **DataDog**
- **New Relic**

## Security Best Practices

1. **Rate Limiting**: Limit connections per user
2. **API Key Rotation**: Implement key expiration
3. **Audio Encryption**: Encrypt stored audio
4. **HTTPS/WSS Only**: Disable HTTP in production
5. **Input Validation**: Sanitize all inputs
6. **CORS**: Configure allowed origins
7. **Monitoring**: Track suspicious activity
