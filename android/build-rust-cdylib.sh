#!/bin/sh
# android/build-rust-cdylib.sh — HOST-ANDROID (UBM 5.0) Rust cdylib builder.
#
# Builds the bindings/jni cdylib (`libubm5_jni_echo.so`) for Android ABI(s)
# with EXPLICIT rustc targets on the PINNED toolchain (parsed from
# rust-toolchain.toml at runtime) — no cargo-ndk needed. Invoked by the
# `buildUbmRustCdylib` Gradle task in android/build.gradle and by the
# five0 probe-app build; this script is the single source of truth so the
# two call sites cannot drift.
#
# Supported ABI list (recorded): arm64-v8a (physical devices — the ABI the
# apps ship) plus x86_64 (KVM host-matched emulator ABI,
# system-images;android-34;google_apis;x86_64). armeabi-v7a stays unwired:
# no 32-bit ARM device exists on this lane, so it is a documented boundary,
# not a failure.
#
# Usage: build-rust-cdylib.sh --abi arm64-v8a|x86_64 --profile debug|release \
#          --libdir <dir> [--minsdk 24]
# Every failure is actionable: the exact missing piece (NDK dir, linker,
# toolchain target, cargo) plus the command that failed.
set -eu

ABI=""
PROFILE="debug"
LIBDIR=""
MINSDK="24"
# Pinned toolchain single-sourced from rust-toolchain.toml (derived below
# once ROOT is known) — never hardcode a version here.
PINNED_TOOLCHAIN=""

while [ $# -gt 0 ]; do
  case "$1" in
    --abi) ABI="$2"; shift 2 ;;
    --profile) PROFILE="$2"; shift 2 ;;
    --libdir) LIBDIR="$2"; shift 2 ;;
    --minsdk) MINSDK="$2"; shift 2 ;;
    *) echo "build-rust-cdylib: unknown argument $1" >&2; exit 2 ;;
  esac
done

fail() { echo "build-rust-cdylib: FAIL $1" >&2; exit 1; }

[ -n "$ABI" ] || fail "missing --abi (supported: arm64-v8a x86_64)"
[ -n "$LIBDIR" ] || fail "missing --libdir (staging directory)"
[ "$ABI" = "x86_64" ] || [ "$ABI" = "arm64-v8a" ] || fail "unsupported ABI '$ABI' (supported: arm64-v8a x86_64; armeabi-v7a is a documented boundary — no 32-bit ARM device on this lane)"
[ "$PROFILE" = "debug" ] || [ "$PROFILE" = "release" ] || fail "--profile must be debug|release, got '$PROFILE'"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PINNED_TOOLCHAIN="$(grep -E '^channel[[:space:]]*=' "$ROOT/rust-toolchain.toml" | sed -E 's/.*"([^"]+)".*/\1/' || true)"
CRATE="ubm5_jni_echo"
LIB="lib${CRATE}.so"

case "$ABI" in
  x86_64) TARGET="x86_64-linux-android" ;;
  arm64-v8a) TARGET="aarch64-linux-android" ;;
esac

# --- NDK resolution (actionable when absent) ---
if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
  NDK="$ANDROID_NDK_HOME"
else
  SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
  NDK=""
  for candidate in "$SDK/ndk/27.1.12297006" "$SDK/ndk/27.0.12077973"; do
    if [ -d "$candidate" ]; then NDK="$candidate"; break; fi
  done
  [ -n "$NDK" ] || fail "no NDK found (tried ANDROID_NDK_HOME='${ANDROID_NDK_HOME:-}' and $SDK/ndk/27.1.12297006 + 27.0.12077973). Install NDK 27.x via: sdkmanager 'ndk;27.1.12297006'"
fi
# NDK prebuilt dir is host-tagged: contributors build on Linux AND macOS.
HOST_TAG=""
case "$(uname -s)" in
  Linux) HOST_TAG="linux-x86_64" ;;
  Darwin) HOST_TAG="darwin-x86_64" ;;
  *) fail "unsupported build host '$(uname -s)' (Linux and macOS only)" ;;
esac
LINKER="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin/${TARGET}${MINSDK}-clang"
[ -x "$LINKER" ] || fail "NDK linker missing: $LINKER (NDK=$NDK host=$HOST_TAG). Reinstall NDK 27.x via: sdkmanager 'ndk;27.1.12297006'"

command -v rustup >/dev/null 2>&1 || fail "rustup not on PATH (needed to pin toolchain $PINNED_TOOLCHAIN)"
rustup target list --installed --toolchain "$PINNED_TOOLCHAIN" 2>/dev/null | grep -q "^${TARGET}$" \
  || fail "target $TARGET missing on toolchain $PINNED_TOOLCHAIN. Add it with: rustup target add --toolchain $PINNED_TOOLCHAIN $TARGET"

# Cargo resolves its workspace (and target dir) from the CALLER's working
# directory, but $BUILT below is $ROOT-relative: every Gradle call site
# inherits a foreign CWD (example/android, example-expo/android, probe-app),
# so without this cd a foreign checkout (Expo CNG's pnpm copy) builds the
# wrong workspace and the expected cdylib is "missing after a successful
# build". Anchor cargo at this script's own package root instead.
[ -f "$ROOT/Cargo.toml" ] || fail "no Cargo workspace at script root $ROOT (Expo CNG/packed copy without Rust sources?)"
cd "$ROOT" || fail "cannot cd to script root $ROOT"

echo "build-rust-cdylib: abi=$ABI target=$TARGET profile=$PROFILE ndk=$NDK minsdk=$MINSDK toolchain=$PINNED_TOOLCHAIN"

PROFILE_FLAG=""
[ "$PROFILE" = "release" ] && PROFILE_FLAG="--release"

# Linker env var is per-target (uppercase, hyphens to underscores).
LINKER_ENV="$(printf 'CARGO_TARGET_%s_LINKER' "$(printf '%s' "$TARGET" | tr '[:lower:]-' '[:upper:]_')")"
# Android 15+ requires 16 KB ELF alignment on every shipped .so (the CMake
# native lib already links -z max-page-size=16384 via AGP): pass the same
# flags through the clang linker driver so the cdylib is 16K-clean by
# construction. Appended ahead of any caller RUSTFLAGS, never replacing.
# shellcheck disable=SC2086
UBM_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384${RUSTFLAGS:+ $RUSTFLAGS}"
env "${LINKER_ENV}=${LINKER}" RUSTFLAGS="$UBM_RUSTFLAGS" \
  rustup run "$PINNED_TOOLCHAIN" cargo build -p "$CRATE" --locked --target "$TARGET" $PROFILE_FLAG \
  || fail "cargo build failed for $TARGET/$PROFILE (pinned $PINNED_TOOLCHAIN). See the cargo output above; common causes: stale Cargo.lock (run cargo update -p $CRATE on the host target first) or a missing NDK platform for minsdk $MINSDK."

BUILT="$ROOT/target/$TARGET/$PROFILE/$LIB"
[ -f "$BUILT" ] || fail "expected cdylib missing after a successful build: $BUILT"

if command -v nm >/dev/null 2>&1; then
  for sym in Java_com_ubm_gatt_GattBridge_nativeEnqueueGattEvent Java_com_ubm_gatt_GattBridge_nativeDrainGattEvents Java_com_ubm_echo_EchoBridge_nativeOpen; do
    nm -D --defined-only "$BUILT" 2>/dev/null | grep -q "$sym" \
      || fail "built cdylib $BUILT lacks JNI symbol $sym (wrong crate revision?)"
  done
  echo "build-rust-cdylib: JNI symbols verified (gatt + echo)"
else
  echo "build-rust-cdylib: WARN nm absent — JNI symbol check skipped (boundary, not a pass)" >&2
fi

mkdir -p "$LIBDIR"
cp -f "$BUILT" "$LIBDIR/$LIB"
echo "build-rust-cdylib: OK $LIBDIR/$LIB ($(wc -c < "$LIBDIR/$LIB") bytes)"
