#!/bin/bash
set -e

# Voice Bird Desktop - Manual Release Script
# Usage: ./scripts/publish-release.sh [version]
# Example: ./scripts/publish-release.sh 0.1.0

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RELEASES_DIR="$(dirname "$PROJECT_DIR")/voice-bird-releases"
STUB_CRATE_DIR="$PROJECT_DIR/voice-bird-desktop-crate"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}=== Voice Bird Desktop - Release Script ===${NC}"

# Get version
if [ -n "$1" ]; then
    VERSION="$1"
else
    VERSION=$(grep '^version' "$PROJECT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo -e "${YELLOW}Using version from Cargo.toml: $VERSION${NC}"
fi

echo ""
echo "Version: $VERSION"
echo "Project: $PROJECT_DIR"
echo "Releases repo: $RELEASES_DIR"
echo ""

# Check if releases repo exists
if [ ! -d "$RELEASES_DIR" ]; then
    echo -e "${RED}Error: Releases repo not found at $RELEASES_DIR${NC}"
    echo "Clone it first: git clone git@github.com:RekunDzmitry/voice-bird-releases.git $RELEASES_DIR"
    exit 1
fi

# Menu
echo "What do you want to do?"
echo "1) Build Windows binary (run on Windows)"
echo "2) Build macOS binary (run on macOS)"
echo "3) Package existing builds into ZIPs"
echo "4) Create GitHub release (upload ZIPs)"
echo "5) Update stub crate version"
echo "6) Publish stub to crates.io"
echo "7) Full release (steps 4 + 5 + 6)"
echo ""
read -p "Enter choice [1-7]: " choice

case $choice in
    1)
        echo -e "${GREEN}Building Windows...${NC}"
        cd "$PROJECT_DIR"
        cargo tauri build
        echo -e "${GREEN}Done! Binary at: target/release/voice_bird_desktop.exe${NC}"
        ;;

    2)
        echo -e "${GREEN}Building macOS...${NC}"
        cd "$PROJECT_DIR"

        # Detect architecture
        ARCH=$(uname -m)
        if [ "$ARCH" = "arm64" ]; then
            TARGET="aarch64-apple-darwin"
        else
            TARGET="x86_64-apple-darwin"
        fi

        echo "Building for $TARGET..."
        rustup target add $TARGET 2>/dev/null || true
        cargo tauri build --target $TARGET
        echo -e "${GREEN}Done! App at: target/$TARGET/release/bundle/macos/${NC}"
        ;;

    3)
        echo -e "${GREEN}Packaging builds...${NC}"

        DIST_DIR="$PROJECT_DIR/dist"
        mkdir -p "$DIST_DIR"

        # Windows
        if [ -f "$PROJECT_DIR/target/release/voice_bird_desktop.exe" ]; then
            echo "Packaging Windows..."
            mkdir -p "$DIST_DIR/win-tmp"
            cp "$PROJECT_DIR/target/release/voice_bird_desktop.exe" "$DIST_DIR/win-tmp/voice-bird-desktop.exe"
            cd "$DIST_DIR/win-tmp"
            zip -r "../voice-bird-desktop-x86_64-pc-windows-msvc.zip" .
            rm -rf "$DIST_DIR/win-tmp"
            echo -e "${GREEN}Created: dist/voice-bird-desktop-x86_64-pc-windows-msvc.zip${NC}"
        fi

        # macOS ARM
        if [ -d "$PROJECT_DIR/target/aarch64-apple-darwin/release/bundle/macos" ]; then
            echo "Packaging macOS ARM..."
            cd "$PROJECT_DIR/target/aarch64-apple-darwin/release/bundle/macos"
            zip -r "$DIST_DIR/voice-bird-desktop-aarch64-apple-darwin.zip" "Voice Bird Desktop.app"
            echo -e "${GREEN}Created: dist/voice-bird-desktop-aarch64-apple-darwin.zip${NC}"
        fi

        # macOS Intel
        if [ -d "$PROJECT_DIR/target/x86_64-apple-darwin/release/bundle/macos" ]; then
            echo "Packaging macOS Intel..."
            cd "$PROJECT_DIR/target/x86_64-apple-darwin/release/bundle/macos"
            zip -r "$DIST_DIR/voice-bird-desktop-x86_64-apple-darwin.zip" "Voice Bird Desktop.app"
            echo -e "${GREEN}Created: dist/voice-bird-desktop-x86_64-apple-darwin.zip${NC}"
        fi

        echo ""
        echo -e "${GREEN}Packages in: $DIST_DIR/${NC}"
        ls -la "$DIST_DIR"/*.zip 2>/dev/null || echo "No ZIPs found"
        ;;

    4)
        echo -e "${GREEN}Creating GitHub release...${NC}"

        DIST_DIR="$PROJECT_DIR/dist"

        # Check if gh CLI is installed
        if ! command -v gh &> /dev/null; then
            echo -e "${RED}Error: GitHub CLI (gh) not installed${NC}"
            echo "Install: https://cli.github.com/"
            exit 1
        fi

        # Check for ZIP files
        if ! ls "$DIST_DIR"/*.zip 1> /dev/null 2>&1; then
            echo -e "${RED}Error: No ZIP files in dist/ directory${NC}"
            echo "Run option 3 first to package builds"
            exit 1
        fi

        cd "$RELEASES_DIR"

        # Create tag if needed
        git fetch --tags
        if git rev-parse "v$VERSION" >/dev/null 2>&1; then
            echo "Tag v$VERSION already exists"
        else
            git tag "v$VERSION"
            git push origin "v$VERSION"
        fi

        # Create release
        echo "Creating release v$VERSION..."
        gh release create "v$VERSION" \
            --title "v$VERSION" \
            --notes "Voice Bird Desktop v$VERSION" \
            "$DIST_DIR"/*.zip

        echo -e "${GREEN}Release created: https://github.com/RekunDzmitry/voice-bird-releases/releases/tag/v$VERSION${NC}"
        ;;

    5)
        echo -e "${GREEN}Updating stub crate version...${NC}"

        # Update version in stub Cargo.toml
        sed -i.bak "s/^version = \".*\"/version = \"$VERSION\"/" "$STUB_CRATE_DIR/Cargo.toml"
        rm -f "$STUB_CRATE_DIR/Cargo.toml.bak"

        echo "Updated $STUB_CRATE_DIR/Cargo.toml to version $VERSION"
        grep "^version" "$STUB_CRATE_DIR/Cargo.toml"
        ;;

    6)
        echo -e "${GREEN}Publishing to crates.io...${NC}"

        cd "$STUB_CRATE_DIR"

        # Check if logged in
        if ! cargo login --help > /dev/null 2>&1; then
            echo -e "${RED}Error: Not logged into crates.io${NC}"
            echo "Run: cargo login <your-token>"
            exit 1
        fi

        echo "Publishing voice-bird-desktop to crates.io..."
        cargo publish

        echo -e "${GREEN}Published! https://crates.io/crates/voice-bird-desktop${NC}"
        ;;

    7)
        echo -e "${GREEN}Running full release...${NC}"

        # Step 4: GitHub release
        echo ""
        echo "Step 1/3: Creating GitHub release..."
        $0 $VERSION <<< "4"

        # Step 5: Update stub version
        echo ""
        echo "Step 2/3: Updating stub crate version..."
        $0 $VERSION <<< "5"

        # Step 6: Publish to crates.io
        echo ""
        echo "Step 3/3: Publishing to crates.io..."
        $0 $VERSION <<< "6"

        echo ""
        echo -e "${GREEN}=== Full release complete! ===${NC}"
        echo "Users can now install with: cargo binstall voice-bird-desktop"
        ;;

    *)
        echo "Invalid choice"
        exit 1
        ;;
esac
