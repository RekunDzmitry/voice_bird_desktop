#!/bin/bash
set -e

cd "$(dirname "$0")/.."

# ── Load credentials ────────────────────────────────────────────────
if [ -f .env.release ]; then
  # shellcheck disable=SC1091
  set -a; source .env.release; set +a
fi

# Configure npm auth from token
if [ -n "${NPM_TOKEN:-}" ] && [ "$NPM_TOKEN" != "your_npm_token_here" ]; then
  echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" > "$HOME/.npmrc"
fi

# Configure cargo auth from token
if [ -n "${CARGO_TOKEN:-}" ] && [ "$CARGO_TOKEN" != "your_cargo_token_here" ]; then
  # In WSL, cargo.exe reads from Windows home
  if grep -qiE '(microsoft|wsl)' /proc/version 2>/dev/null; then
    WIN_USER=$(cmd.exe /C "echo %USERNAME%" 2>/dev/null | tr -d '\r')
    WIN_CARGO="/mnt/c/Users/${WIN_USER}/.cargo"
    mkdir -p "$WIN_CARGO"
    cat > "$WIN_CARGO/credentials.toml" <<CREDS
[registry]
token = "$CARGO_TOKEN"
CREDS
  else
    mkdir -p "$HOME/.cargo"
    cat > "$HOME/.cargo/credentials.toml" <<CREDS
[registry]
token = "$CARGO_TOKEN"
CREDS
  fi
fi

# Configure PyPI auth from token (maturin uses MATURIN_PYPI_TOKEN)
if [ -n "${PYPI_TOKEN:-}" ] && [ "$PYPI_TOKEN" != "your_pypi_token_here" ]; then
  export MATURIN_PYPI_TOKEN="$PYPI_TOKEN"
fi

# ── Helpers ──────────────────────────────────────────────────────────
red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

usage() {
  cat <<EOF
Usage: ./scripts/release.sh <command> [options]

Commands:
  build       Build CLI binary for the current platform
  npm         Publish npm packages (platform + main)
  pypi        Build and publish Python wheel to PyPI
  cargo       Publish stub crate to crates.io
  github      Create GitHub release with zip artifacts
  all         Run: build → github → npm → pypi → cargo

Options:
  --dry-run     Print commands without executing (npm/pypi/cargo)
  --skip-build  Skip build step when running 'all'
  --platform X  Force platform: windows, macos, linux (useful in WSL)

Prerequisites:
  npm:    npm login (or NPM_TOKEN env var)
  pypi:   pip install maturin, PyPI token configured
  cargo:  cargo login
  github: gh auth login
EOF
  exit 1
}

# ── Detect platform ──────────────────────────────────────────────────
set_platform_vars() {
  case "$1" in
    windows)
      PLATFORM="windows"
      RUST_TARGET="x86_64-pc-windows-msvc"
      NPM_PKG="@voice-bird/cli-win32-x64"
      BIN_NAME="voice-bird-cli.exe"
      ZIP_NAME="voice-bird-cli-x86_64-pc-windows-msvc.zip"
      ;;
    macos)
      PLATFORM="macos"
      local arch="$(uname -m)"
      if [ "$arch" = "arm64" ]; then
        RUST_TARGET="aarch64-apple-darwin"
        NPM_PKG="@voice-bird/cli-darwin-arm64"
        ZIP_NAME="voice-bird-cli-aarch64-apple-darwin.zip"
      else
        RUST_TARGET="x86_64-apple-darwin"
        NPM_PKG="@voice-bird/cli-darwin-x64"
        ZIP_NAME="voice-bird-cli-x86_64-apple-darwin.zip"
      fi
      BIN_NAME="voice-bird-cli"
      ;;
    linux)
      PLATFORM="linux"
      RUST_TARGET="x86_64-unknown-linux-gnu"
      NPM_PKG="@voice-bird/cli-linux-x64"
      BIN_NAME="voice-bird-cli"
      ZIP_NAME="voice-bird-cli-x86_64-unknown-linux-gnu.zip"
      ;;
    *)
      red "Unsupported platform: $1"
      exit 1
      ;;
  esac
}

detect_platform() {
  # Honour explicit --platform override
  if [ -n "$FORCE_PLATFORM" ]; then
    set_platform_vars "$FORCE_PLATFORM"
  else
    local os="$(uname -s)"
    case "$os" in
      MINGW*|MSYS*|CYGWIN*|Windows_NT)
        set_platform_vars windows ;;
      Darwin)
        set_platform_vars macos ;;
      Linux)
        # Auto-detect WSL → target Windows
        if grep -qiE '(microsoft|wsl)' /proc/version 2>/dev/null; then
          blue "WSL detected — targeting Windows"
          set_platform_vars windows
        else
          set_platform_vars linux
        fi
        ;;
      *)
        red "Unsupported OS: $os"
        exit 1
        ;;
    esac
  fi

  VERSION=$(grep '^version' voice-bird-cli/Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
  blue "Platform: $PLATFORM ($RUST_TARGET)"
  blue "Version:  $VERSION"
}

# ── Build ────────────────────────────────────────────────────────────
cmd_build() {
  bold "Building voice-bird-cli for $RUST_TARGET..."

  if [ "$PLATFORM" = "windows" ]; then
    # Use Windows cargo from WSL
    local cargo_cmd
    if command -v cargo.exe &>/dev/null; then
      cargo_cmd="cargo.exe"
    elif [ -f "/mnt/c/Users/$USER/.cargo/bin/cargo.exe" ]; then
      cargo_cmd="/mnt/c/Users/$USER/.cargo/bin/cargo.exe"
    else
      cargo_cmd="cargo"
    fi
    $cargo_cmd build --release --manifest-path voice-bird-cli/Cargo.toml --target "$RUST_TARGET" --target-dir target
  else
    cargo build --release --manifest-path voice-bird-cli/Cargo.toml --target "$RUST_TARGET" --target-dir target
  fi

  # Stage binary
  mkdir -p staging

  if [ "$PLATFORM" = "macos" ]; then
    # Create .app bundle for macOS
    mkdir -p staging/VoiceBirdCLI.app/Contents/MacOS

    local arch_dir
    if [ "$(uname -m)" = "arm64" ]; then
      arch_dir="cli-darwin-arm64"
    else
      arch_dir="cli-darwin-x64"
    fi

    cp "npm/@voice-bird/$arch_dir/VoiceBirdCLI.app/Contents/Info.plist" \
      staging/VoiceBirdCLI.app/Contents/Info.plist

    cp "target/$RUST_TARGET/release/$BIN_NAME" \
      staging/VoiceBirdCLI.app/Contents/MacOS/voice-bird-cli
    chmod +x staging/VoiceBirdCLI.app/Contents/MacOS/voice-bird-cli

    # Add Swift runtime rpath
    install_name_tool -add_rpath /usr/lib/swift \
      staging/VoiceBirdCLI.app/Contents/MacOS/voice-bird-cli 2>/dev/null || true

    # Ad-hoc codesign
    codesign --force --deep --sign - staging/VoiceBirdCLI.app

    # Create zip
    cd staging && zip -r "../$ZIP_NAME" VoiceBirdCLI.app && cd ..
  else
    cp "target/$RUST_TARGET/release/$BIN_NAME" staging/
    cd staging && zip "../$ZIP_NAME" "$BIN_NAME" && cd ..
  fi

  green "Build complete: staging/$BIN_NAME"
  green "Archive: $ZIP_NAME"
}

# ── GitHub Release ───────────────────────────────────────────────────
cmd_github() {
  bold "Creating GitHub release v$VERSION..."

  local release_tag="v${VERSION}-cli"
  local repo="RekunDzmitry/voice-bird-releases"

  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] gh release delete $release_tag --repo $repo --yes"
    blue "[dry-run] gh release create $release_tag $ZIP_NAME --repo $repo --title 'Voice Bird CLI v$VERSION'"
    return
  fi

  if [ ! -f "$ZIP_NAME" ]; then
    red "Missing: $ZIP_NAME (run 'build' first)"
    exit 1
  fi

  gh release delete "$release_tag" --repo "$repo" --yes 2>/dev/null || true
  gh release create "$release_tag" "$ZIP_NAME" \
    --repo "$repo" \
    --title "Voice Bird CLI v$VERSION" \
    --notes "Release v$VERSION for $PLATFORM"

  green "GitHub release created: $release_tag"
}

# ── npm ──────────────────────────────────────────────────────────────
cmd_npm() {
  bold "Publishing npm packages..."

  local pkg_dir="npm/@voice-bird/$(echo "$NPM_PKG" | sed 's|@voice-bird/||')"

  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] cd $pkg_dir && npm publish --access public"
    blue "[dry-run] cd npm/voice-bird-cli && npm publish --access public"
    return
  fi

  # Copy binary to platform package
  if [ "$PLATFORM" = "macos" ]; then
    if [ ! -d "staging/VoiceBirdCLI.app" ]; then
      red "Missing: staging/VoiceBirdCLI.app (run 'build' first)"
      exit 1
    fi
    rm -rf "$pkg_dir/VoiceBirdCLI.app"
    cp -R staging/VoiceBirdCLI.app "$pkg_dir/VoiceBirdCLI.app"
  else
    if [ ! -f "staging/$BIN_NAME" ]; then
      red "Missing: staging/$BIN_NAME (run 'build' first)"
      exit 1
    fi
    cp "staging/$BIN_NAME" "$pkg_dir/$BIN_NAME"
    [ "$PLATFORM" != "windows" ] && chmod +x "$pkg_dir/$BIN_NAME"
  fi

  # Publish platform package
  blue "Publishing $NPM_PKG@$VERSION..."
  (cd "$pkg_dir" && npm publish --access public) || true

  # Publish main package
  blue "Publishing voice-bird-cli@$VERSION..."
  (cd npm/voice-bird-cli && npm publish --access public)

  green "npm packages published"
}

# ── PyPI ─────────────────────────────────────────────────────────────
cmd_pypi() {
  bold "Building and publishing Python wheel..."

  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] maturin build --release --target $RUST_TARGET"
    blue "[dry-run] twine upload <wheel>"
    return
  fi

  local wheel_dir="voice-bird-cli/target/wheels"

  # In WSL, use Windows maturin for building, then twine for uploading
  if grep -qiE '(microsoft|wsl)' /proc/version 2>/dev/null; then
    local win_path
    win_path=$(wslpath -w "$(pwd)")
    cmd.exe /C "cd /d $win_path && maturin build --release --target $RUST_TARGET 2>&1" 2>/dev/null | tr -d '\r'
  else
    maturin build --release --target "$RUST_TARGET"
  fi

  local wheel
  wheel=$(ls -t "$wheel_dir"/voice_bird_cli-"$VERSION"*.whl 2>/dev/null | head -1)
  if [ -z "$wheel" ]; then
    red "No wheel found in $wheel_dir"
    exit 1
  fi

  blue "Uploading $wheel..."
  TWINE_USERNAME=__token__ TWINE_PASSWORD="${MATURIN_PYPI_TOKEN:-$PYPI_TOKEN}" \
    twine upload "$wheel"

  green "PyPI package published"
}

# ── Cargo ────────────────────────────────────────────────────────────
cmd_cargo() {
  bold "Publishing stub crate to crates.io..."

  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] cd voice-bird-cli-crate && cargo publish"
    return
  fi

  (cd voice-bird-cli-crate && cargo publish --allow-dirty)

  green "Cargo crate published"
}

# ── All ──────────────────────────────────────────────────────────────
cmd_all() {
  if [ "$SKIP_BUILD" != "1" ]; then
    cmd_build
  fi
  cmd_github
  cmd_npm
  cmd_pypi
  cmd_cargo
  echo ""
  green "All done! Published v$VERSION for $PLATFORM"
}

# ── Main ─────────────────────────────────────────────────────────────
COMMAND="${1:-}"
shift 2>/dev/null || true

DRY_RUN=0
SKIP_BUILD=0
FORCE_PLATFORM=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run)    DRY_RUN=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --platform)   FORCE_PLATFORM="$2"; shift ;;
  esac
  shift
done

[ -z "$COMMAND" ] && usage

detect_platform

case "$COMMAND" in
  build)   cmd_build ;;
  npm)     cmd_npm ;;
  pypi)    cmd_pypi ;;
  cargo)   cmd_cargo ;;
  github)  cmd_github ;;
  all)     cmd_all ;;
  *)       usage ;;
esac
