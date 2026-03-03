#!/bin/bash
set -e

VERSION=$1
if [ -z "$VERSION" ]; then
  echo "Usage: ./scripts/bump-version.sh 0.2.0"
  exit 1
fi

cd "$(dirname "$0")/.."

echo "Bumping all packages to v$VERSION..."

# Rust CLI Cargo.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" voice-bird-cli/Cargo.toml
echo "  Updated voice-bird-cli/Cargo.toml"

# Rust stub crate Cargo.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" voice-bird-cli-crate/Cargo.toml
echo "  Updated voice-bird-cli-crate/Cargo.toml"

# pyproject.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" pyproject.toml
echo "  Updated pyproject.toml"

# npm main package
sed -i "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" npm/voice-bird-cli/package.json
# Update optionalDependencies versions
sed -i "s/\"@voice-bird\/cli-\([^\"]*\)\": \"[^\"]*\"/\"@voice-bird\/cli-\1\": \"$VERSION\"/" npm/voice-bird-cli/package.json
echo "  Updated npm/voice-bird-cli/package.json"

# npm platform packages
for pkg in npm/@voice-bird/cli-*/package.json; do
  sed -i "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" "$pkg"
  echo "  Updated $pkg"
done

echo ""
echo "All versions updated to $VERSION"
echo ""
echo "Next steps:"
echo "  1. git add -A && git commit -m 'release: v$VERSION'"
echo "  2. git tag v$VERSION"
echo "  3. git push origin main --tags"
echo "  4. ./scripts/release.sh all          # build + publish for current platform"
echo "     # or run individual steps:"
echo "     #   ./scripts/release.sh build    # build binary"
echo "     #   ./scripts/release.sh github   # upload to GitHub releases"
echo "     #   ./scripts/release.sh npm      # publish npm packages"
echo "     #   ./scripts/release.sh pypi     # publish Python wheel"
echo "     #   ./scripts/release.sh cargo    # publish stub crate"
