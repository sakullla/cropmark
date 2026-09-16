#!/usr/bin/env bash
# Builds Cropmark on macOS (.app and .dmg).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "scripts/build-macos.sh must run on macOS." >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required. Install Node.js, then retry." >&2
  exit 1
fi

if [[ ! -d node_modules ]]; then
  npm install
fi

npm run tauri -- build --bundles app,dmg
echo "Cropmark macOS output: src-tauri/target/release/bundle/macos/ and src-tauri/target/release/bundle/dmg/"
