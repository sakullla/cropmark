#!/usr/bin/env bash
# Builds Cropmark on Linux (AppImage and .deb).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Keep names and order aligned with src-tauri/.cargo/config.toml.
export_release_path_remap() {
  local repo="${root}"
  local home="${HOME}"
  local cargo_home="${CARGO_HOME:-${home}/.cargo}"
  local rustup_home="${RUSTUP_HOME:-${home}/.rustup}"
  set_remap_var CROPMARK_REMAP_HOME_VERBATIM "CROPMARK_REMAP_UNSET"
  set_remap_var CROPMARK_REMAP_HOME_ALT "${home}"
  set_remap_var CROPMARK_REMAP_HOME "${home}"
  set_remap_var CROPMARK_REMAP_RUSTUP_HOME_VERBATIM "CROPMARK_REMAP_UNSET"
  set_remap_var CROPMARK_REMAP_RUSTUP_HOME_ALT "${rustup_home}"
  set_remap_var CROPMARK_REMAP_RUSTUP_HOME "${rustup_home}"
  set_remap_var CROPMARK_REMAP_CARGO_HOME_VERBATIM "CROPMARK_REMAP_UNSET"
  set_remap_var CROPMARK_REMAP_CARGO_HOME_ALT "${cargo_home}"
  set_remap_var CROPMARK_REMAP_CARGO_HOME "${cargo_home}"
  set_remap_var CROPMARK_REMAP_REPO_VERBATIM "CROPMARK_REMAP_UNSET"
  set_remap_var CROPMARK_REMAP_REPO_ALT "${repo}"
  set_remap_var CROPMARK_REMAP_REPO "${repo}"
}

set_remap_var() {
  local name="$1"
  local value="$2"
  export "${name}=${value}"
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    printf '%s=%s\n' "${name}" "${value}" >> "${GITHUB_ENV}"
  fi
}

if [[ "${1:-}" == "--remap-only" ]]; then
  export_release_path_remap
  exit 0
fi

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

export_release_path_remap
npm run tauri -- build --bundles appimage,deb
echo "Cropmark Linux output: src-tauri/target/release/bundle/appimage/ and src-tauri/target/release/bundle/deb/"
