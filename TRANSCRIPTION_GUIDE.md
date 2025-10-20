# Voice Bird Desktop - Transcription Feature Guide

## Overview

Voice Bird Desktop now supports real-time speech-to-text transcription using AssemblyAI's streaming API. The application displays transcribed text in the terminal and saves both WAV audio files and TXT transcript files.

## Setup

### 1. Get AssemblyAI API Key

1. Sign up at https://www.assemblyai.com/
2. Get your API key from the dashboard
3. Free tier includes $50 credit (333 hours of transcription)

### 2. Configure API Key

#### Option A: Using .env File (Recommended)

This is the easiest method and works on all platforms (Windows, Linux, macOS).

1. **Copy the example file:**
   ```bash
   cp .env.example .env
   ```

   Or on Windows:
   ```powershell
   copy .env.example .env
   ```

2. **Edit the .env file** and replace `your-api-key-here` with your actual API key:
   ```
   ASSEMBLYAI_API_KEY=abc123your-actual-key-here
   ```

3. **Done!** The API key will be automatically loaded when you run the application.

**Note:** The `.env` file is already in `.gitignore`, so your API key won't be committed to version control.

#### Option B: Using Environment Variables

If you prefer not to use a `.env` file, you can set the environment variable manually:

**Windows (PowerShell):**
```powershell
$env:ASSEMBLYAI_API_KEY = "your-api-key-here"
```

**Windows (Command Prompt):**
```cmd
set ASSEMBLYAI_API_KEY=your-api-key-here
```

**Linux/macOS:**
```bash
export ASSEMBLYAI_API_KEY="your-api-key-here"
```

**Permanent Setup (Windows):**
- Right-click "This PC" → Properties → Advanced system settings
- Environment Variables → New (User variables)
- Name: `ASSEMBLYAI_API_KEY`
- Value: your API key

## Usage

### Build and Run

```bash
cargo build --release
cargo run --release
```

### Recording with Transcription

1. **Select Device Type**: Choose "Input Device (Microphone)" or "Output Device (Speaker)"
2. **Select Audio Device**: Choose your microphone or speaker
3. **Start Recording**: The application will:
   - Display transcribed text in real-time (if API key is set)
   - Show audio level bars (if no API key)
4. **Stop Recording**: Press `ESC` to stop and save files

### Output Files

When you press ESC, the application saves two files with matching timestamps:

1. **Audio File**: `recording_YYYY-MM-DD_HH-MM-SS.wav`
   - 32-bit float WAV format
   - Original audio quality preserved

2. **Transcript File**: `transcript_YYYY-MM-DD_HH-MM-SS.txt`
   - Plain text format
   - Contains all transcribed segments
   - Includes word count and metadata

### Example Transcript File

```
Voice Bird Desktop - Transcript
Timestamp: 2025-10-17 14-30-45
Segments: 12
==================================================

[1] Hello, this is a test recording.
[2] The transcription service is working correctly.
[3] You can see the real time text in your terminal.
...

==================================================
Total words: ~87
```

## Features

### Real-Time Display
- Shows last 5 transcript segments in terminal
- Newest segments appear in bold white
- Older segments appear dimmed
- Automatic text wrapping for long segments

### Fallback Mode
If no API key is set, the application falls back to audio level visualization (colored bars).

### Supported Audio Formats
- Input: F32, I16, U16 sample formats
- Sample rates: 16kHz minimum (higher rates supported)
- Channels: Mono or stereo (automatically handled)

### Windows Loopback Support
Capture system audio (games, music, browser) with the same transcription features on Windows.

## API Costs (AssemblyAI)

- **Price**: $0.15 per hour ($0.0025/minute)
- **Free tier**: $50 credit (333 hours)
- **Latency**: ~300ms median
- **Accuracy**: 91% on noisy audio, 21% better on alphanumerics

### Cost Examples
- 1 hour recording: $0.15
- 10 hours: $1.50
- 100 hours: $15.00

## Troubleshooting

### No Transcription Appearing
1. Check API key is set: `echo $env:ASSEMBLYAI_API_KEY` (PowerShell)
2. Check internet connection
3. Verify API key is valid on AssemblyAI dashboard
4. Check terminal for error messages

### Poor Transcription Quality
1. Use a better microphone or increase volume
2. Reduce background noise
3. Speak clearly and at normal pace
4. Ensure sample rate is 16kHz or higher

### Connection Errors
- Check firewall settings (allow outbound HTTPS)
- Verify network connectivity
- AssemblyAI uses WebSocket (wss://api.assemblyai.com)

## Architecture

### Audio Pipeline
```
Microphone → Audio Callback → [Parallel Paths]
                                   ↓
                    ┌──────────────┴──────────────┐
                    ↓                             ↓
              Audio Buffer                  Transcription
              (for WAV save)                Service
                    ↓                             ↓
              save_audio_file()         save_transcript_file()
```

### Transcription Flow
1. Audio callback converts samples to PCM16
2. Samples sent via channel to transcription thread
3. Transcription thread runs async WebSocket client
4. Base64-encoded audio chunks sent to AssemblyAI
5. Transcript segments received and displayed
6. All segments saved to TXT file on ESC

### Code Structure
- **Lines 41-182**: TranscriptionService struct (WebSocket client)
- **Lines 271-297**: display_transcript() (terminal display)
- **Lines 300-329**: save_transcript_file() (TXT export)
- **Lines 392-720**: stream_audio() (input device with transcription)
- **Lines 723-1070**: stream_output_audio() (Windows loopback with transcription)

## Alternative APIs

### Deepgram Nova-3
Better accuracy (6.84% WER), higher cost ($0.46/hour)

**Setup:**
```bash
export ASSEMBLYAI_API_KEY=""  # Clear AssemblyAI
# Requires code modification for Deepgram WebSocket endpoint
```

### Rev.ai
Lower cost ($0.10-0.20/hour), excellent Python SDK

Currently requires code modification. Future versions may support multiple providers.

## Performance

### Expected Latency
- AssemblyAI: 300-500ms
- Network: 50-100ms
- Display: <10ms
- **Total**: 350-610ms from speech to screen

### Resource Usage
- CPU: ~5-10% (depends on sample rate)
- Memory: ~50-100MB
- Network: ~100-200 KB/minute (compressed audio)

## Credits

- **Audio Engine**: cpal (cross-platform audio library)
- **Speech-to-Text**: AssemblyAI Real-Time API
- **WebSocket**: tokio-tungstenite
- **Terminal UI**: crossterm, console

---

For issues or feature requests, see the main README.md file.
