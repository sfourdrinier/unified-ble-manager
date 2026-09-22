#!/bin/sh
# ios/build-rust-core.sh — F01 Apple-lane Rust core builder (iOS + tvOS).
#
# Builds the pod-selected shared Rust core (`ubm5_uniffi_echo` staticlib)
# for the Apple matrix and assembles ios/RustCore/RustCore.xcframework plus
# build-identity.json (PR210-18): the sealed source digest and binding schema
# the binary was built with (scripts/release/native-build-identity.js, passed
# to cargo as UBM_BUILD_SOURCE_DIGEST / UBM_BUILD_BINDING_SCHEMA), the
# Info.plist sha256, and every slice parsed from Info.plist with its sha256.
# ios/verify-rust-core.sh then verifies the staging it just wrote.
#
# This is the canonical builder for both producers: the publish workflow's
# macOS `native-rustcore` job (prebuilt artifacts shipped in the package) and
# contributor source mode, run directly BEFORE pod install:
#   UBM_NATIVE_BUILD=source pnpm --dir <ubm checkout> native:apple:prepare
# No podspec hook runs it (CocoaPods skips prepare_command for :path pods);
# in source mode the pod script phase rejects a staging whose digests no
# longer match the sources and prints this command.
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
#     macOS-only: full staticlib build + XCFramework assembly + identity.
#     Fails loudly anywhere else (the Apple link needs Xcode tooling). Needs
#     Node (NODE_BINARY or `node` on PATH) for the identity digests.
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
PINNED_RUSTC="$(rustup which --toolchain "$PINNED_TOOLCHAIN" rustc)"
if [ ! -x "$PINNED_RUSTC" ]; then
  echo "build-rust-core: pinned rustc is not executable: $PINNED_RUSTC" >&2
  exit 1
fi
# `rustup run ... cargo` can otherwise inherit a Homebrew RUSTC from Cargo's
# environment on macOS. Bind Cargo to the exact compiler the repository pins.
export RUSTC="$PINNED_RUSTC"

# Cargo resolves a relative CARGO_TARGET_DIR from its working directory. All
# Cargo calls below run from ROOT, so mirror that rule when locating outputs.
case "${CARGO_TARGET_DIR:-}" in
  "") CARGO_TARGET_ROOT="$ROOT/target" ;;
  /*) CARGO_TARGET_ROOT="$CARGO_TARGET_DIR" ;;
  *) CARGO_TARGET_ROOT="$ROOT/$CARGO_TARGET_DIR" ;;
esac

TARGETS="$MATRIX_DEVICE $MATRIX_SIM $MATRIX_TVOS_DEVICE $MATRIX_TVOS_SIM"
for target in $TARGETS; do
  if ! rustup target list --installed --toolchain "$PINNED_TOOLCHAIN" 2>/dev/null | grep -q "^$target$"; then
    echo "build-rust-core: target $target missing on toolchain $PINNED_TOOLCHAIN. Add it with: rustup target add --toolchain $PINNED_TOOLCHAIN $target" >&2
    exit 1
  fi
done

if [ "$CHECK_ONLY" = "1" ]; then
  # Linux-runnable anchor coherence: the attested symbols below must still
  # be the exact surface the owned UDL generates. A regenerated binding that
  # renames them fails here (in the F01 packed proof), not silently in the
  # macOS-only attestation that consumes them.
  for anchor in ffi_ubm5_uniffi_echo_fn_constructor_echosession_new ffi_ubm5_uniffi_echo_fn_method_echosession_close ffi_ubm5_uniffi_echo_fn_method_echosession_central_status ffi_ubm5_uniffi_echo_fn_method_echosession_ble_scan_start; do
    if ! grep -q "$anchor" "$ROOT/bindings/uniffi/generated/swift/ubm_echo.swift"; then
      echo "build-rust-core: attested anchor $anchor missing from bindings/uniffi/generated/swift/ubm_echo.swift (regenerate bindings and re-pin anchors)" >&2
      exit 1
    fi
  done
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
need nm
need plutil

# PR210-18: seal the identity the binary carries. The digests are computed
# once, before any cargo run, and the same values are recorded in
# build-identity.json — a source edit during the build shows up as a
# mismatch in the verification below, never as a silently mixed artifact.
NODE="${NODE_BINARY:-node}"
command -v "$NODE" >/dev/null 2>&1 || { echo "build-rust-core: Node is required for the build identity (set NODE_BINARY or put node on PATH)" >&2; exit 1; }
IDENTITY_SCRIPT="$ROOT/scripts/release/native-build-identity.js"
[ -f "$IDENTITY_SCRIPT" ] || { echo "build-rust-core: missing $IDENTITY_SCRIPT" >&2; exit 1; }
IDENTITY_ENV="$("$NODE" "$IDENTITY_SCRIPT" --root "$ROOT" --write --print-env uniffi)"
UBM_BUILD_SOURCE_DIGEST="$(printf '%s\n' "$IDENTITY_ENV" | sed -n 's/^UBM_BUILD_SOURCE_DIGEST=//p')"
UBM_BUILD_BINDING_SCHEMA="$(printf '%s\n' "$IDENTITY_ENV" | sed -n 's/^UBM_BUILD_BINDING_SCHEMA=//p')"
[ -n "$UBM_BUILD_SOURCE_DIGEST" ] && [ -n "$UBM_BUILD_BINDING_SCHEMA" ] || { echo "build-rust-core: native-build-identity.js printed no digests" >&2; exit 1; }
export UBM_BUILD_SOURCE_DIGEST UBM_BUILD_BINDING_SCHEMA
echo "build-rust-core: identity sourceDigest=$UBM_BUILD_SOURCE_DIGEST bindingSchema=$UBM_BUILD_BINDING_SCHEMA"

# R02 Apple cutover: the assembled framework must PROVE it carries the real
# UniFFI core session. Slice counts and digests pass for any well-formed
# archive — including a stub or mismatched staticlib — so every assembled
# input is attested for the DEFINED session symbols below (open + close +
# real-Central status + first-class scan start, the exact symbols the
# generated Swift binding references). A synthetic archive fails HERE,
# never on device.
# shellcheck disable=SC2086
CORE_FFI_ANCHORS="ffi_ubm5_uniffi_echo_fn_constructor_echosession_new ffi_ubm5_uniffi_echo_fn_method_echosession_close ffi_ubm5_uniffi_echo_fn_method_echosession_central_status ffi_ubm5_uniffi_echo_fn_method_echosession_ble_scan_start"

attest_core_symbols() {
  # $1 = staticlib path. nm exits nonzero on an unrecognized archive; the
  # anchor loop rejects a well-formed archive carrying the wrong object
  # code (no defined core symbol, no XCFramework assembly).
  for anchor in $CORE_FFI_ANCHORS; do
    if ! nm -g "$1" 2>/dev/null | grep -q "T .*$anchor"; then
      echo "build-rust-core: $1 carries no defined core symbol $anchor (not the ubm-core UniFFI staticlib?)" >&2
      exit 1
    fi
  done
}

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
  BUILT_LIB="$CARGO_TARGET_ROOT/$1/$PROFILE_DIR/$LIB_NAME"
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
# Attest every assembled input BEFORE xcodebuild consumes it: a well-formed
# archive carrying stub or mismatched objects must fail here, never link
# into a framework that reports healthy digests.
echo "build-rust-core: attesting core symbols in assembled inputs"
attest_core_symbols "$IOS_DEVICE_LIB"
attest_core_symbols "$IOS_SIM_FAT"
attest_core_symbols "$TVOS_DEVICE_LIB"
attest_core_symbols "$TVOS_SIM_FAT"
echo "build-rust-core: assembling $FRAMEWORK_DIR"
xcodebuild -create-xcframework -output "$FRAMEWORK_DIR" \
  -library "$IOS_DEVICE_LIB" -headers "$HEADERS" \
  -library "$IOS_SIM_FAT" -headers "$HEADERS" \
  -library "$TVOS_DEVICE_LIB" -headers "$HEADERS" \
  -library "$TVOS_SIM_FAT" -headers "$HEADERS"

# Post-assembly identity (PR210-18): Info.plist parsed with plutil (never
# grepped), every slice hashed, the sealed digests recorded; then the
# independent shell verifier checks the declared slice set and hash chain.
if [ ! -f "$FRAMEWORK_DIR/Info.plist" ]; then
  echo "build-rust-core: assembly produced no $FRAMEWORK_DIR/Info.plist" >&2
  exit 1
fi
# The pre-PR210-18 text identity is superseded by build-identity.json.
rm -f "$OUT_DIR/build-identity.txt"
PLIST_JSON="$OUT_DIR/.Info.plist.json"
plutil -convert json -o "$PLIST_JSON" "$FRAMEWORK_DIR/Info.plist"
"$NODE" "$IDENTITY_SCRIPT" --root "$ROOT" --write-apple-identity \
  --dir "$OUT_DIR" \
  --plist-json "$PLIST_JSON" \
  --source-digest "$UBM_BUILD_SOURCE_DIGEST" \
  --binding-schema "$UBM_BUILD_BINDING_SCHEMA" \
  --profile "$PROFILE" \
  --toolchain "$(rustup run "$PINNED_TOOLCHAIN" rustc --version)" \
  --xcodebuild "$(xcodebuild -version | head -n 1)"
rm -f "$PLIST_JSON"
sh "$SCRIPT_DIR/verify-rust-core.sh" --dir "$OUT_DIR"
echo "build-rust-core: wrote $FRAMEWORK_DIR + build-identity.json"
