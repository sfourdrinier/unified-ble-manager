#!/bin/sh
# android/refresh-prebuilt-jniLibs.sh — HOST-ANDROID (UBM 5.0) prebuilt refresh.
#
# Rebuilds the COMMITTED release cdylibs shipped to packed consumers
# (android/src/main/jniLibs/<abi>/libubm5_jni_echo.so) plus the
# build-identity.txt provenance file the Gradle packed path verifies
# (presence, bytes, sha256 per ABI). Run this on the pinned toolchain with
# NDK 27.x whenever the Rust inputs change (bindings/jni, crates, Cargo
# manifests/lock, toolchain pin); commit the refreshed tree.
#
# This is a MAINTAINER step, not part of `prepack`: packing and consuming
# must never require NDK/Rust. The committed tree is the source of truth;
# `__tests__/AndroidPrebuilds.test.js` and the tarball proof assert its
# coherence (offline, no toolchain).
#
# Usage: sh android/refresh-prebuilt-jniLibs.sh   (from the repo root)
set -eu

ABIS="arm64-v8a x86_64"
PROFILE="release"
LIB="libubm5_jni_echo.so"

fail() { echo "refresh-prebuilt-jniLibs: FAIL $1" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/android/src/main/jniLibs"
BUILDER="$ROOT/android/build-rust-cdylib.sh"
IDENTITY="$OUT/build-identity.txt"

[ -x "$BUILDER" ] || fail "builder missing: $BUILDER"
command -v git >/dev/null 2>&1 || fail "git not on PATH (needed for source-sha provenance)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT INT TERM

PINNED_TOOLCHAIN="$(grep -E '^channel[[:space:]]*=' "$ROOT/rust-toolchain.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
RUSTC_LINE="$(rustup run "$PINNED_TOOLCHAIN" rustc --version 2>/dev/null || echo unknown)"
SOURCE_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
SOURCE_DESC="$(git -C "$ROOT" describe --tags --always --dirty 2>/dev/null || echo unknown)"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
NDK_VERSION="unknown"
if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
  NDK_VERSION="$(basename "$ANDROID_NDK_HOME") (ANDROID_NDK_HOME)"
else
  for candidate in "$SDK/ndk/27.1.12297006" "$SDK/ndk/27.0.12077973"; do
    if [ -d "$candidate" ]; then NDK_VERSION="$(basename "$candidate")"; break; fi
  done
fi

{
  echo "# Committed UBM 5.0 Android prebuilts. Maintained by"
  echo "# android/refresh-prebuilt-jniLibs.sh — do not hand-edit."
  echo "profile=$PROFILE"
  echo "abis=$(printf '%s' "$ABIS" | tr ' ' ',')"
  echo "toolchain=$PINNED_TOOLCHAIN ($RUSTC_LINE)"
  echo "ndk=$NDK_VERSION"
  echo "source-sha=$SOURCE_SHA"
  echo "source-describe=$SOURCE_DESC"
} > "$IDENTITY.tmp"

for abi in $ABIS; do
  echo "refresh-prebuilt-jniLibs: building $abi/$PROFILE"
  sh "$BUILDER" --abi "$abi" --profile "$PROFILE" --libdir "$STAGE/$abi" \
    || fail "builder failed for $abi (see output above)"
  BUILT="$STAGE/$abi/$LIB"
  case "$abi" in
    arm64-v8a) want_machine="AArch64" ;;
    x86_64) want_machine="Advanced Micro Devices X86-64" ;;
  esac
  if command -v readelf >/dev/null 2>&1; then
    machine="$(readelf -h "$BUILT" | sed -n 's/^ *Machine: *//p')"
    [ "$machine" = "$want_machine" ] \
      || fail "$BUILT targets $machine, expected $want_machine for $abi"
    echo "refresh-prebuilt-jniLibs: $abi machine verified ($machine)"
  else
    echo "refresh-prebuilt-jniLibs: WARN readelf absent — machine check skipped" >&2
  fi
  # D2(iii): hard 16 KB gate — a maintainer refresh must never ship a
  # 4 KB-aligned library (Android 15+ install-time requirement).
  sh "$ROOT/android/check-elf-16k-pages.sh" --so "$BUILT" --abi "$abi" \
    || fail "16 KB page-size check failed for $abi (see output above)"
  mkdir -p "$OUT/$abi"
  cp -f "$BUILT" "$OUT/$abi/$LIB"
  sha="$(sha256sum "$OUT/$abi/$LIB" | cut -d' ' -f1)"
  bytes="$(wc -c < "$OUT/$abi/$LIB")"
  echo "abi=$abi sha256=$sha bytes=$bytes file=$LIB" >> "$IDENTITY.tmp"
  echo "refresh-prebuilt-jniLibs: OK $abi ($bytes bytes sha256:$sha)"
done

mv -f "$IDENTITY.tmp" "$IDENTITY"
echo "refresh-prebuilt-jniLibs: wrote $IDENTITY"
cat "$IDENTITY"
