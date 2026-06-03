#!/bin/bash
set -e

VERSION=$1
if [ -z "$VERSION" ]; then
  echo "Usage: ./scripts/bump-version.sh 0.2.0"
  exit 1
fi

cd "$(dirname "$0")/.."

echo "Bumping public packages to v$VERSION..."

# Rust CLI Cargo.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
echo "  Updated Cargo.toml"

# pyproject.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" pyproject.toml
echo "  Updated pyproject.toml"

# Python wrapper
sed -i "s/^__version__ = \".*\"/__version__ = \"$VERSION\"/" python/voice_bird_cli/__init__.py
echo "  Updated python/voice_bird_cli/__init__.py"

# npm package
sed -i "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" npm/voice-bird-cli/package.json
echo "  Updated npm/voice-bird-cli/package.json"

echo ""
echo "All versions updated to $VERSION"
echo ""
echo "Next steps:"
echo "  1. git add -A && git commit -m 'release: v$VERSION'"
echo "  2. git tag v$VERSION"
echo "  3. git push origin main --tags"
echo "  4. ./scripts/release.sh all          # build + publish public packages"
echo "     # or run individual steps:"
echo "     #   ./scripts/release.sh build    # build binary"
echo "     #   ./scripts/release.sh github   # upload to GitHub releases"
echo "     #   ./scripts/release.sh npm      # publish npm wrapper"
echo "     #   ./scripts/release.sh pypi     # publish PyPI wrapper"
echo "     #   ./scripts/release.sh cargo    # publish Cargo crate"
