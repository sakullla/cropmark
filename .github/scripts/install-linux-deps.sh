#!/usr/bin/env bash
# GitHub-hosted Ubuntu runners hang on azure.archive.ubuntu.com
# (actions/runner-images#11347, #12949, #14594). apt's own
# Acquire::Timeout often never fires after the Ign:/Get: fallback, so
# the job sits until Actions cancels it. Bypass the Azure mirror,
# drop unused third-party lists, and SIGTERM hung apt with `timeout`.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

sudo find /etc/apt -type f \( \
  -name 'apt-mirrors.txt' -o \
  -name 'sources.list' -o \
  -name '*.list' -o \
  -name '*.sources' \
\) -exec sed -i \
  -e 's|https\?://azure\.archive\.ubuntu\.com|http://archive.ubuntu.com|g' \
  -e 's|mirror+file:/etc/apt/apt-mirrors.txt|http://archive.ubuntu.com/ubuntu|g' \
  {} +

if [ -f /etc/apt/apt-mirrors.txt ]; then
  printf 'http://archive.ubuntu.com/ubuntu/\nhttp://security.ubuntu.com/ubuntu/\n' \
    | sudo tee /etc/apt/apt-mirrors.txt >/dev/null
fi

# Image extras (Azure CLI / Microsoft prod / Chrome) are unused by the
# Tauri build and independently stall apt-get update on the same runners.
sudo rm -f \
  /etc/apt/sources.list.d/azure-cli.list \
  /etc/apt/sources.list.d/azure-cli.sources \
  /etc/apt/sources.list.d/microsoft-prod.list \
  /etc/apt/sources.list.d/microsoft-prod.sources \
  /etc/apt/sources.list.d/google-chrome.list \
  /etc/apt/sources.list.d/google-chrome.sources \
  || true

sudo tee /etc/apt/apt.conf.d/99-gha-acquire >/dev/null <<'EOF'
Acquire::Retries "1";
Acquire::http::Timeout "15";
Acquire::https::Timeout "15";
Acquire::ftp::Timeout "15";
Acquire::ForceIPv4 "true";
Dpkg::Use-Pty "0";
EOF

APT_OPTS=(
  -o Acquire::Retries=1
  -o Acquire::http::Timeout=15
  -o Acquire::https::Timeout=15
  -o Acquire::ftp::Timeout=15
  -o Acquire::ForceIPv4=true
  -o Dpkg::Use-Pty=0
)

PACKAGES=(
  build-essential
  curl
  wget
  file
  pkg-config
  libssl-dev
  libxdo-dev
  libx11-dev
  libxcb1-dev
  libxcb-randr0-dev
  libwebkit2gtk-4.1-dev
  libgtk-3-dev
  libayatana-appindicator3-dev
  librsvg2-dev
  patchelf
  xdg-utils
)

# 124 = timeout(1) sent SIGTERM. apt's Timeout is unreliable (#14594).
run_apt() {
  local seconds="$1"
  shift
  sudo timeout --signal=TERM --kill-after=15s "${seconds}" \
    apt-get "${APT_OPTS[@]}" "$@"
}

for attempt in 1 2 3; do
  if run_apt 90 update \
    && run_apt 180 install -y --no-install-recommends "${PACKAGES[@]}"; then
    exit 0
  fi
  echo "apt failed (attempt ${attempt}/3), retrying..." >&2
  sleep $((attempt * 5))
done

echo "apt failed after 3 attempts" >&2
exit 1
