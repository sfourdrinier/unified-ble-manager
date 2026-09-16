#!/bin/sh
# ios/build-rust-core.sh — F01 iOS lane Rust core builder.
#
# Builds the pod-selected shared Rust core (`ubm5_uniffi_echo` staticlib)
# for the Apple matrix and assembles ios/RustCore/RustCore.xcframework plus
# a build-identity.txt (mirroring the Android jniLibs pattern). Invoked by
# the podspec `prepare_command` on the 5.x lane, so `pod install` on a
# macOS host always links the shipped sources — never a stale prebuilt.
#
# Declared matrix (device + simulator, physical-target load in macOS CI):
#   device    aarch64-apple-ios
#   simulator aarch64-apple-ios-sim, x86_64-apple-ios (Intel sims keep the
#             historic triple; there is no x86_64-apple-ios-sim)
# tvOS stays Owned-radio-only in this slice: the pod serves tvOS, but no
# tvOS Rust slice is declared, so this script refuses tvOS targets loudly
# instead of shipping an unchecked one.
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

CHECK_ONLY=0
PROFILE="release"
OUT_DIR="$ROOT/ios/RustCore"

while [ $# -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --profile) PROFILE="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    *) echo "build-rust-core: unknown argument $1" >&2; exit 2 ;;
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

TARGETS="$MATRIX_DEVICE $MATRIX_SIM"
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

LIB_DIRS=""
for target in $TARGETS; do
  echo "build-rust-core: building $CRATE ($PROFILE) for $target"
  (cd "$ROOT" && rustup run "$PINNED_TOOLCHAIN" cargo build --locked -p "$CRATE" $CARGO_PROFILE --target "$target")
  LIB="$ROOT/target/$target/$PROFILE_DIR/$LIB_NAME"
  if [ ! -f "$LIB" ]; then
    echo "build-rust-core: missing staticlib $LIB" >&2
    exit 1
  fi
  LIB_DIRS="$LIB_DIRS $LIB"
done

HEADERS="$ROOT/bindings/uniffi/generated/swift"
FRAMEWORK_DIR="$OUT_DIR/RustCore.xcframework"
rm -rf "$FRAMEWORK_DIR"
CREATE="xcodebuild -create-xcframework -output $FRAMEWORK_DIR"
set -- $LIB_DIRS
# $1=device slice, $2+=simulator slices.
CREATE="$CREATE -library $1 -headers $HEADERS"
shift
for sim in "$@"; do
  CREATE="$CREATE -library $sim -headers $HEADERS"
done
echo "build-rust-core: assembling $FRAMEWORK_DIR"
$CREATE

{
  echo "# UBM 5.0 iOS Rust core. Maintained by ios/build-rust-core.sh — do not hand-edit."
  echo "profile=$PROFILE"
  echo "targets=$TARGETS"
  echo "toolchain=$(rustup run "$PINNED_TOOLCHAIN" rustc --version)"
  echo "xcodebuild=$(xcodebuild -version | head -n 1)"
  # shellcheck disable=SC2162
  (cd "$ROOT" && git rev-parse HEAD 2>/dev/null | sed 's/^/source-sha=/')
  find "$FRAMEWORK_DIR" -name '*.a' | sort | while read -r lib; do
    if command -v sha256sum >/dev/null 2>&1; then
      digest="$(sha256sum "$lib" | cut -d' ' -f1)"
    else
      digest="$(shasum -a 256 "$lib" | cut -d' ' -f1)"
    fi
    echo "slice=$lib sha256=$digest bytes=$(wc -c < "$lib" | tr -d ' ')"
  done
} > "$OUT_DIR/build-identity.txt"
echo "build-rust-core: wrote $FRAMEWORK_DIR + build-identity.txt"
