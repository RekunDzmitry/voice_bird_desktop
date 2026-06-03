#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")/.."

DRY_RUN=0
SKIP_BUILD=0

red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

usage() {
  cat <<EOF
Usage: ./scripts/release.sh <command> [options]

Commands:
  build       Build the voice-bird-cli binary
  github      Create a GitHub release for the current repository
  cargo       Publish the root Cargo crate
  npm         Publish the npm wrapper package
  pypi        Build and publish the PyPI wrapper package
  all         Run: build -> github -> cargo -> npm -> pypi

Options:
  --dry-run     Print publish commands without executing them
  --skip-build  Skip build when running 'all'

Prerequisites:
  cargo:  cargo login
  github: gh auth login
  npm:    npm login
  pypi:   python -m pip install build twine
EOF
  exit 1
}

run() {
  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] $*"
  else
    "$@"
  fi
}

version() {
  grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/'
}

archive_name() {
  local os arch target
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  target="${os}-${arch}"
  printf 'voice-bird-cli-%s.zip' "$target"
}

cmd_build() {
  bold "Building voice-bird-cli..."
  cargo build --release --bin voice-bird-cli
  mkdir -p staging
  cp target/release/voice-bird-cli staging/voice-bird-cli
  chmod +x staging/voice-bird-cli
  (cd staging && zip -q "../$(archive_name)" voice-bird-cli)
  green "Built staging/voice-bird-cli and $(archive_name)"
}

cmd_github() {
  local v tag archive
  v="$(version)"
  tag="v${v}"
  archive="$(archive_name)"

  [ -f "$archive" ] || { red "Missing $archive; run build first"; exit 1; }
  bold "Creating GitHub release $tag..."
  run gh release create "$tag" "$archive" --title "Voice Bird CLI $tag" --notes "Voice Bird CLI $tag"
}

cmd_cargo() {
  bold "Publishing Cargo crate..."
  run cargo publish
}

cmd_npm() {
  bold "Publishing npm wrapper..."
  run npm publish --access public --prefix npm/voice-bird-cli
}

cmd_pypi() {
  bold "Publishing PyPI wrapper..."
  if [ "$DRY_RUN" = "1" ]; then
    blue "[dry-run] python -m build"
    blue "[dry-run] python -m twine upload dist/*"
    return
  fi
  rm -rf dist
  python -m build
  python -m twine upload dist/*
}

cmd_all() {
  if [ "$SKIP_BUILD" != "1" ]; then
    cmd_build
  fi
  cmd_github
  cmd_cargo
  cmd_npm
  cmd_pypi
  green "Release complete."
}

COMMAND="${1:-}"
[ -z "$COMMAND" ] && usage
shift

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    *) usage ;;
  esac
  shift
done

case "$COMMAND" in
  build)  cmd_build ;;
  github) cmd_github ;;
  cargo)  cmd_cargo ;;
  npm)    cmd_npm ;;
  pypi)   cmd_pypi ;;
  all)    cmd_all ;;
  *)      usage ;;
esac
