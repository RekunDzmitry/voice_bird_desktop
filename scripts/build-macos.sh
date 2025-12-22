#!/bin/bash
set -e

# Voice Bird Desktop - macOS Build Script
# Run this on a macOS machine with Xcode and Rust installed

echo "=== Voice Bird Desktop - macOS Build ==="

# Check prerequisites
check_prerequisites() {
    echo "Checking prerequisites..."

    if ! command -v cargo &> /dev/null; then
        echo "ERROR: Rust/Cargo not found. Install from https://rustup.rs"
        exit 1
    fi

    if ! command -v npm &> /dev/null; then
        echo "ERROR: Node.js/npm not found. Install from https://nodejs.org"
        exit 1
    fi

    if ! xcode-select -p &> /dev/null; then
        echo "ERROR: Xcode command line tools not found. Run: xcode-select --install"
        exit 1
    fi

    # Check for Apple Silicon vs Intel
    ARCH=$(uname -m)
    echo "Architecture: $ARCH"

    # Add Rust targets if needed
    if [ "$ARCH" = "arm64" ]; then
        rustup target add aarch64-apple-darwin 2>/dev/null || true
    else
        rustup target add x86_64-apple-darwin 2>/dev/null || true
    fi

    echo "Prerequisites OK"
}

# Install Tauri CLI if needed
install_tauri_cli() {
    if ! command -v cargo-tauri &> /dev/null; then
        echo "Installing Tauri CLI..."
        cargo install tauri-cli
    fi
}

# Build the application
build_app() {
    echo "Building Voice Bird Desktop for macOS..."

    # Build for current architecture (release mode)
    cargo tauri build --target $(rustc -vV | sed -n 's/host: //p')

    echo "Build complete!"
}

# Find and display build artifacts
show_artifacts() {
    echo ""
    echo "=== Build Artifacts ==="

    BUNDLE_DIR="target/release/bundle"

    if [ -d "$BUNDLE_DIR/macos" ]; then
        echo "App bundle:"
        ls -la "$BUNDLE_DIR/macos/"*.app 2>/dev/null || echo "  No .app found"
    fi

    if [ -d "$BUNDLE_DIR/dmg" ]; then
        echo ""
        echo "DMG installer:"
        ls -la "$BUNDLE_DIR/dmg/"*.dmg 2>/dev/null || echo "  No .dmg found"
    fi

    echo ""
    echo "Full path to artifacts: $(pwd)/$BUNDLE_DIR"
}

# Main execution
main() {
    cd "$(dirname "$0")/.."

    check_prerequisites
    install_tauri_cli
    build_app
    show_artifacts

    echo ""
    echo "=== Next Steps ==="
    echo "Run ./scripts/upload-to-wasabi.sh to upload the artifacts"
}

main "$@"
