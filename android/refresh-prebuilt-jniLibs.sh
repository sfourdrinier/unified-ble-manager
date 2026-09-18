#!/bin/sh
# android/refresh-prebuilt-jniLibs.sh — HOST-ANDROID (UBM 5.0) prebuilt refresh.
#
# Rebuilds the COMMITTED release cdylibs shipped to packed consumers
# (android/src/main/jniLibs/<abi>/libubm5_jni_echo.so) plus the
# build-identity.json the Gradle prebuilt path verifies (presence, bytes,
# sha256 per ABI). PR210-18: the record also carries the sealed sourceDigest
# and bindingSchema the binaries were built with; the publish gate
# (`node scripts/release/native-build-identity.js --check-android-prebuilts`)
# fails whenever they differ from the current Rust sources, so run this on
# the pinned toolchain with NDK 27.x whenever those inputs change, and
# commit the refreshed tree.
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
IDENTITY="$OUT/build-identity.json"

[ -x "$BUILDER" ] || fail "builder missing: $BUILDER"
NODE="${NODE_BINARY:-node}"
command -v "$NODE" >/dev/null 2>&1 || fail "Node is required for the build identity (set NODE_BINARY or put node on PATH)"
IDENTITY_SCRIPT="$ROOT/scripts/release/native-build-identity.js"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT INT TERM

PINNED_TOOLCHAIN="$(grep -E '^channel[[:space:]]*=' "$ROOT/rust-toolchain.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
RUSTC_LINE="$(rustup run "$PINNED_TOOLCHAIN" rustc --version 2>/dev/null || echo unknown)"
# The digests every ABI is built with (the builder computes the same values
# and embeds them); recorded once, verified against the sources at the end.
IDENTITY_ENV="$("$NODE" "$IDENTITY_SCRIPT" --root "$ROOT" --write --print-env jni)" || fail "identity computation failed"
SOURCE_DIGEST="$(printf '%s\n' "$IDENTITY_ENV" | sed -n 's/^UBM_BUILD_SOURCE_DIGEST=//p')"
BINDING_SCHEMA="$(printf '%s\n' "$IDENTITY_ENV" | sed -n 's/^UBM_BUILD_BINDING_SCHEMA=//p')"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
NDK_VERSION="unknown"
if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
  NDK_VERSION="$(basename "$ANDROID_NDK_HOME") (ANDROID_NDK_HOME)"
else
  for candidate in "$SDK/ndk/27.1.12297006" "$SDK/ndk/27.0.12077973"; do
    if [ -d "$candidate" ]; then NDK_VERSION="$(basename "$candidate")"; break; fi
  done
fi

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
  echo "refresh-prebuilt-jniLibs: OK $abi ($(wc -c < "$OUT/$abi/$LIB" | tr -d ' ') bytes)"
done

"$NODE" "$IDENTITY_SCRIPT" --root "$ROOT" --write-android-identity \
  --dir "$OUT" \
  --source-digest "$SOURCE_DIGEST" \
  --binding-schema "$BINDING_SCHEMA" \
  --profile "$PROFILE" \
  --toolchain "$PINNED_TOOLCHAIN ($RUSTC_LINE)" \
  --ndk "$NDK_VERSION" || fail "could not write $IDENTITY"
"$NODE" "$IDENTITY_SCRIPT" --root "$ROOT" --check-android-prebuilts \
  || fail "the refreshed prebuilts do not match the current sources (did they change during the build?)"
echo "refresh-prebuilt-jniLibs: wrote $IDENTITY"
cat "$IDENTITY"
