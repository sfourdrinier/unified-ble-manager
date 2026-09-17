#!/bin/sh
# bindings/napi/build-addon.sh — R03 NAPI dispatch addon builder (UBM 5.0).
#
# Builds the real `ubm5_napi_echo` cdylib (F01 UbmCentral dispatch over
# ubm-desktop) and stages it under bindings/napi with the platform-correct
# file name the jest converged-path tests require:
#   ubm_echo.<process.platform>-<process.arch>.node
# (linux-x64, darwin-arm64, win32-x64, ...). Single source of truth for the
# build+stage step: called by run_napi_roundtrip.sh and by the CI package
# job before jest. Every failure is actionable.
#
# Usage: sh bindings/napi/build-addon.sh   (from the repo root, or anywhere)
set -eu

fail() { echo "build-addon: FAIL $1" >&2; exit 1; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

# Pinned toolchain single-sourced from rust-toolchain.toml — never hardcode.
PINNED_TOOLCHAIN="$(grep -E '^channel[[:space:]]*=' "$ROOT/rust-toolchain.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$PINNED_TOOLCHAIN" ] || fail "could not parse channel from $ROOT/rust-toolchain.toml"

command -v rustup >/dev/null 2>&1 || fail "rustup not on PATH (needed to pin toolchain $PINNED_TOOLCHAIN)"

# Platform tag must match Node's process.platform-process.arch exactly.
case "$(uname -s)" in
  Linux) PLATFORM="linux" ;;
  Darwin) PLATFORM="darwin" ;;
  MINGW*|MSYS*|CYGWIN*|Windows*) PLATFORM="win32" ;;
  *) fail "unsupported build host '$(uname -s)' (Linux, macOS, Windows-git-bash only)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH="x64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) fail "unsupported build arch '$(uname -m)' (x86_64 and aarch64 only)" ;;
esac
case "$PLATFORM" in
  linux) ARTIFACT="libubm5_napi_echo.so" ;;
  darwin) ARTIFACT="libubm5_napi_echo.dylib" ;;
  win32) ARTIFACT="ubm5_napi_echo.dll" ;;
esac
STAGED="ubm_echo.${PLATFORM}-${ARCH}.node"

echo "build-addon: platform=$PLATFORM arch=$ARCH toolchain=$PINNED_TOOLCHAIN"

(cd "$ROOT" && rustup run "$PINNED_TOOLCHAIN" cargo build -p ubm5_napi_echo --locked) \
  || fail "cargo build -p ubm5_napi_echo failed on $PINNED_TOOLCHAIN (see output above)"

BUILT="$ROOT/target/debug/$ARTIFACT"
[ -f "$BUILT" ] || fail "expected cdylib missing after a successful build: $BUILT"
cp -f "$BUILT" "$HERE/$STAGED"
echo "build-addon: OK $HERE/$STAGED ($(wc -c < "$HERE/$STAGED") bytes)"
