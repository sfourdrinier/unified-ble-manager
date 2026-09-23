#!/usr/bin/env bash

set -euo pipefail

readonly profile="${1:-}"
readonly -a bluez_packages=(
  libdbus-1-dev
  pkg-config
)
readonly -a tauri_packages=(
  libwebkit2gtk-4.1-dev
  build-essential
  curl
  wget
  file
  libxdo-dev
  libssl-dev
  libayatana-appindicator3-dev
  librsvg2-dev
)
readonly -a electron_smoke_packages=(
  xvfb
  xauth
)

case "${profile}" in
  bluez)
    readonly -a packages=("${bluez_packages[@]}")
    ;;
  tauri)
    readonly -a packages=("${bluez_packages[@]}" "${tauri_packages[@]}")
    ;;
  desktop-prebuild)
    readonly -a packages=("${bluez_packages[@]}" "${electron_smoke_packages[@]}")
    ;;
  *)
    echo "Usage: $0 <bluez|tauri|desktop-prebuild>" >&2
    exit 64
    ;;
esac

if [[ "$(uname -s)" != 'Linux' ]]; then
  echo "Linux native system dependencies can only be installed on Linux" >&2
  exit 1
fi

sudo apt-get update
sudo env DEBIAN_FRONTEND=noninteractive \
  apt-get install --no-install-recommends -y "${packages[@]}"
