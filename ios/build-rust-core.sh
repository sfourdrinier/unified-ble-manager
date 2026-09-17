#!/bin/sh
# ios/build-rust-core.sh — F01 Apple-lane Rust core builder (iOS + tvOS).
#
# Builds the pod-selected shared Rust core (`ubm5_uniffi_echo` staticlib)
# for the Apple matrix and assembles ios/RustCore/RustCore.xcframework plus
# a build-identity.txt (mirroring the Android jniLibs pattern). Invoked by
# the podspec `prepare_command` on the 5.x lane, so `pod install` on a
# macOS host always links the shipped sources — never a stale prebuilt.
#
# Declared matrix (device + simulator, physical-target load in macOS CI):
#   iOS     device    aarch64-apple-ios
#           simulator aarch64-apple-ios-sim, x86_64-apple-ios (Intel sims keep
#                     the historic triple; there is no x86_64-apple-ios-sim)
#   tvOS    device    aarch64-apple-tvos
#           simulator aarch64-apple-tvos-sim, arm64-only: x86_64-apple-tvos
#                     ships no prebuilt std on the pinned stable toolchain
#                     (Tier 3 — `-Zbuild-std`/nightly only), so no Intel
#                     tvOS-simulator slice is declared.
# The podspec serves both iOS and tvOS (`s.platforms`), so the XCFramework
# must carry slices for both — an iOS-only framework would fail the tvOS
# link in consumers.
#
# Each `-library` passed to `xcodebuild -create-xcframework` defines exactly
# one platform slice, so a platform's simulator archs are first merged with
# `lipo -create` into one fat archive per simulator slice (a duplicate
# platform slice is rejected by xcodebuild). Every `-library` input keeps the
# same basename (`libubm5_uniffi_echo.a`, staged in per-slice dirs) because
# CocoaPods rejects a vendored XCFramework whose slices carry differing
# static-library names. All paths are quoted throughout — consumer checkouts
# may live under directories with spaces — and the xcodebuild invocation is
# issued directly, never accumulated in a string.
#
# Usage:
#   sh ios/build-rust-core.sh --check
#     Linux-runnable: `cargo check` the core for every matrix target (no
#     Apple SDK needed — type/metadata only). Runs in the F01 packed proof.
#   sh ios/build-rust-core.sh [--profile release] [--out <dir>]
#     macOS-only: full staticlib build + XCFramework assembly. Fails loudly
#     anywhere else (the Apple link needs Xcode tooling).
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
CRATE="ubm5_uniffi_echo"
LIB_NAME="libubm5_uniffi_echo.a"
MATRIX_DEVICE="aarch64-apple-ios"
MATRIX_SIM="aarch64-apple-ios-sim x86_64-apple-ios"
MATRIX_TVOS_DEVICE="aarch64-apple-tvos"
MATRIX_TVOS_SIM="aarch64-apple-tvos-sim"

CHECK_ONLY=0
PROFILE="release"
OUT_DIR="$ROOT/ios/RustCore"

usage() {
  echo "usage: sh ios/build-rust-core.sh [--check] [--profile release|debug] [--out <dir>]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --profile)
      if [ $# -lt 2 ]; then
        echo "build-rust-core: --profile needs a value (release|debug)" >&2
        exit 2
      fi
      PROFILE="$2"; shift 2 ;;
    --out)
      if [ $# -lt 2 ]; then
        echo "build-rust-core: --out needs a value (output directory)" >&2
        exit 2
      fi
      OUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "build-rust-core: unknown argument $1" >&2; usage; exit 2 ;;
  esac
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "build-rust-core: missing required tool: $1" >&2
    exit 1
  fi
}

need rustup

PINNED_TOOLCHAIN="$(sed -n 's/^channel *= *"\(.*\)" *$/\1/p' "$ROOT/rust-toolchain.toml" | head -n 1)"
if [ -z "$PINNED_TOOLCHAIN" ]; then
  echo "build-rust-core: cannot parse pinned channel from rust-toolchain.toml" >&2
  exit 1
fi

TARGETS="$MATRIX_DEVICE $MATRIX_SIM $MATRIX_TVOS_DEVICE $MATRIX_TVOS_SIM"
for target in $TARGETS; do
  if ! rustup target list --installed --toolchain "$PINNED_TOOLCHAIN" 2>/dev/null | grep -q "^$target$"; then
    echo "build-rust-core: target $target missing on toolchain $PINNED_TOOLCHAIN. Add it with: rustup target add --toolchain $PINNED_TOOLCHAIN $target" >&2
    exit 1
  fi
done

if [ "$CHECK_ONLY" = "1" ]; then
  for target in $TARGETS; do
    echo "build-rust-core: checking $CRATE for $target"
    (cd "$ROOT" && rustup run "$PINNED_TOOLCHAIN" cargo check --locked -p "$CRATE" --target "$target")
  done
  echo "build-rust-core: matrix check clean ($TARGETS)"
  exit 0
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "build-rust-core: full staticlib/XCFramework build needs macOS+Xcode (have $(uname -s)); use --check here" >&2
  exit 1
fi
need xcodebuild
need lipo

case "$PROFILE" in
  release) CARGO_PROFILE="--release"; PROFILE_DIR="release" ;;
  debug) CARGO_PROFILE=""; PROFILE_DIR="debug" ;;
  *) echo "build-rust-core: unknown profile $PROFILE" >&2; exit 2 ;;
esac

# The -headers dir must exist before xcodebuild runs: without it the
# assembly fails deep inside xcodebuild instead of here with a cause.
HEADERS="$ROOT/bindings/uniffi/generated/swift"
if [ ! -d "$HEADERS" ]; then
  echo "build-rust-core: missing generated Swift headers dir $HEADERS" >&2
  exit 1
fi
for generated in ubm_echoFFI.h ubm_echoFFI.modulemap ubm_echo.swift; do
  if [ ! -f "$HEADERS/$generated" ]; then
    echo "build-rust-core: missing generated binding $HEADERS/$generated" >&2
    exit 1
  fi
done

# Fresh consumer checkouts have no ios/RustCore yet (the framework is built,
# never shipped); create the output root before writing into it.
mkdir -p "$OUT_DIR"

build_target() {
  # $1 = target triple. Builds the staticlib and sets BUILT_LIB to its
  # verified path (fail-loud when cargo produces no archive).
  echo "build-rust-core: building $CRATE ($PROFILE) for $1"
  # shellcheck disable=SC2086
  (cd "$ROOT" && rustup run "$PINNED_TOOLCHAIN" cargo build --locked -p "$CRATE" $CARGO_PROFILE --target "$1")
  BUILT_LIB="$ROOT/target/$1/$PROFILE_DIR/$LIB_NAME"
  if [ ! -f "$BUILT_LIB" ]; then
    echo "build-rust-core: missing staticlib $BUILT_LIB" >&2
    exit 1
  fi
}

build_sim_slice() {
  # $1 = fat-archive output path; $2.. = simulator triples for ONE platform.
  # create-xcframework takes exactly one -library per platform slice, so the
  # per-arch archives are merged with lipo -create first (a single-arch
  # platform still goes through lipo so every simulator slice is fat-built
  # the same way). Per-arch paths are collected one argv word per file and
  # only ever expanded quoted, so roots with spaces survive; the triples
  # themselves never contain whitespace.
  FAT_LIB="$1"; shift
  TRIPLES="$*"
  mkdir -p "$(dirname -- "$FAT_LIB")"
  set --
  # shellcheck disable=SC2086
  for triple in $TRIPLES; do
    build_target "$triple"
    set -- "$@" "$BUILT_LIB"
  done
  if [ $# -eq 0 ]; then
    echo "build-rust-core: no simulator triples for slice $FAT_LIB" >&2
    exit 1
  fi
  echo "build-rust-core: merging $# simulator arch(s) into $FAT_LIB"
  lipo -create "$@" -output "$FAT_LIB"
  if [ ! -f "$FAT_LIB" ]; then
    echo "build-rust-core: lipo produced no fat archive $FAT_LIB" >&2
    exit 1
  fi
  lipo -info "$FAT_LIB"
}

build_target "$MATRIX_DEVICE"
IOS_DEVICE_LIB="$BUILT_LIB"
# Fat simulator slices stage under the same lib basename as the device
# slices (CocoaPods: one binary name per vendored XCFramework).
# shellcheck disable=SC2086
build_sim_slice "$OUT_DIR/slices/ios-sim/$LIB_NAME" $MATRIX_SIM
IOS_SIM_FAT="$FAT_LIB"
build_target "$MATRIX_TVOS_DEVICE"
TVOS_DEVICE_LIB="$BUILT_LIB"
# shellcheck disable=SC2086
build_sim_slice "$OUT_DIR/slices/tvos-sim/$LIB_NAME" $MATRIX_TVOS_SIM
TVOS_SIM_FAT="$FAT_LIB"
# Drop staging archives from the pre-fix layout, if a rerun overlays them.
rm -f "$OUT_DIR"/fat-ios-simulator-*.a "$OUT_DIR"/fat-tvos-simulator-*.a

FRAMEWORK_DIR="$OUT_DIR/RustCore.xcframework"
rm -rf "$FRAMEWORK_DIR"
echo "build-rust-core: assembling $FRAMEWORK_DIR"
xcodebuild -create-xcframework -output "$FRAMEWORK_DIR" \
  -library "$IOS_DEVICE_LIB" -headers "$HEADERS" \
  -library "$IOS_SIM_FAT" -headers "$HEADERS" \
  -library "$TVOS_DEVICE_LIB" -headers "$HEADERS" \
  -library "$TVOS_SIM_FAT" -headers "$HEADERS"

# Post-assembly proof: the framework must exist with exactly one slice per
# platform (device + simulator for iOS and tvOS). A count drift means the
# matrix and the assembly disagree — fail here, not at consumer link time.
if [ ! -f "$FRAMEWORK_DIR/Info.plist" ]; then
  echo "build-rust-core: assembly produced no $FRAMEWORK_DIR/Info.plist" >&2
  exit 1
fi
SLICE_COUNT="$(grep -c "<key>LibraryIdentifier</key>" "$FRAMEWORK_DIR/Info.plist" || true)"
if [ "$SLICE_COUNT" != "4" ]; then
  echo "build-rust-core: expected 4 platform slices in $FRAMEWORK_DIR, found $SLICE_COUNT" >&2
  exit 1
fi

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "build-rust-core: need sha256sum or shasum to record slice digests" >&2
  exit 1
fi
{
  echo "# UBM 5.0 Apple Rust core. Maintained by ios/build-rust-core.sh — do not hand-edit."
  echo "profile=$PROFILE"
  echo "targets=$TARGETS"
  echo "toolchain=$(rustup run "$PINNED_TOOLCHAIN" rustc --version)"
  echo "xcodebuild=$(xcodebuild -version | head -n 1)"
  # Best-effort: packed-tarball consumers have no .git checkout, so a
  # missing source-sha must not fail the install build.
  # shellcheck disable=SC2162
  (cd "$ROOT" && git rev-parse HEAD 2>/dev/null | sed 's/^/source-sha=/')
  find "$FRAMEWORK_DIR" -name '*.a' | sort | while IFS= read -r lib; do
    if command -v sha256sum >/dev/null 2>&1; then
      digest="$(sha256sum "$lib" | cut -d' ' -f1)"
    else
      digest="$(shasum -a 256 "$lib" | cut -d' ' -f1)"
    fi
    echo "slice=$lib sha256=$digest bytes=$(wc -c < "$lib" | tr -d ' ')"
  done
} > "$OUT_DIR/build-identity.txt"
echo "build-rust-core: wrote $FRAMEWORK_DIR + build-identity.txt"
