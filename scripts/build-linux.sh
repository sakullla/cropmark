#!/usr/bin/env bash
# Builds Cropmark on Linux (AppImage and .deb).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "scripts/build-linux.sh must run on Linux." >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required. Install Node.js, then retry." >&2
  exit 1
fi

if [[ ! -d node_modules ]]; then
  npm install
fi

npm run tauri -- build --bundles appimage,deb
echo "Cropmark Linux output: src-tauri/target/release/bundle/appimage/ and src-tauri/target/release/bundle/deb/"
