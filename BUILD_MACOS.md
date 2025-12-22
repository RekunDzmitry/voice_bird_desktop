# Building Voice Bird Desktop for macOS

This guide explains how to build and distribute Voice Bird Desktop for macOS.

## Prerequisites

On a macOS machine (Intel or Apple Silicon), install:

1. **Xcode Command Line Tools**
   ```bash
   xcode-select --install
   ```

2. **Rust** (via rustup)
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source ~/.cargo/env
   ```

3. **Node.js** (for Tauri build process)
   ```bash
   brew install node
   ```

4. **AWS CLI** (for Wasabi upload)
   ```bash
   brew install awscli
   ```

## Quick Build

```bash
# Clone the repository
git clone <your-repo-url>
cd voice_bird_desktop

# Make scripts executable
chmod +x scripts/*.sh

# Build
./scripts/build-macos.sh

# Upload to Wasabi
./scripts/upload-to-wasabi.sh
```

## Manual Build Steps

### 1. Install Tauri CLI

```bash
cargo install tauri-cli
```

### 2. Build the Application

```bash
# For current architecture (recommended)
cargo tauri build

# For specific architecture
cargo tauri build --target aarch64-apple-darwin  # Apple Silicon
cargo tauri build --target x86_64-apple-darwin   # Intel
```

### 3. Find Build Artifacts

After successful build:
- **App Bundle**: `target/release/bundle/macos/Voice Bird Desktop.app`
- **DMG Installer**: `target/release/bundle/dmg/Voice Bird Desktop_*.dmg`

## Uploading to Wasabi

The upload script uses credentials from `.env`:

```bash
WASABI_ACCESS_KEY_ID=your_key
WASABI_SECRET_ACCESS_KEY=your_secret
WASABI_REGION=eu-central-2
WASABI_BUCKET_NAME=voice-bird-europe
WASABI_ENDPOINT=https://s3.eu-central-2.wasabisys.com
```

Run the upload:
```bash
./scripts/upload-to-wasabi.sh
```

Files are uploaded to:
- `releases/macos/v{version}/` - versioned release
- `releases/macos/latest/` - latest release symlink

## Code Signing (Optional)

For distribution outside the App Store, you can sign the app:

1. Get an Apple Developer ID certificate
2. Update `tauri.conf.json`:
   ```json
   "macOS": {
     "signingIdentity": "Developer ID Application: Your Name (TEAM_ID)"
   }
   ```

3. Notarize for Gatekeeper:
   ```bash
   xcrun notarytool submit "Voice Bird Desktop.dmg" \
     --apple-id "your@email.com" \
     --password "app-specific-password" \
     --team-id "TEAM_ID" \
     --wait
   ```

## Troubleshooting

### "screencapturekit" build errors
The macOS audio loopback uses ScreenCaptureKit. Ensure:
- macOS 12.3+ (Monterey or later)
- Screen Recording permission granted in System Preferences

### Rust target not installed
```bash
rustup target add aarch64-apple-darwin  # Apple Silicon
rustup target add x86_64-apple-darwin   # Intel
```

### AWS CLI authentication errors
Ensure `.env` file exists with valid Wasabi credentials.
