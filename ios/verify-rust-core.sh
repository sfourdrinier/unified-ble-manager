#!/bin/sh
# ios/verify-rust-core.sh — PR210-18 Apple RustCore staging verifier.
#
# Verifies a staged ios/RustCore (the XCFramework plus build-identity.json
# written by ios/build-rust-core.sh) before anything links it:
#   * Info.plist AvailableLibraries, parsed with plutil, is EXACTLY the
#     declared slice set below (LibraryIdentifier, platform, variant and
#     architecture set); a missing, extra or re-sliced library fails;
#   * the Info.plist sha256 equals the recorded one;
#   * every recorded slice exists at its LibraryPath with the recorded
#     sha256 and byte count, and no other archive rides the framework.
# It does not recompute the Rust source digest (that needs Node and the
# sources): `node scripts/release/native-build-identity.js --check-apple`
# does, in source mode and in the publish job.
#
# Runs in the macOS producer job (after ios/build-rust-core.sh) and in the
# pod "Verify staged RustCore" script phase in both build modes.
#
# Usage: sh ios/verify-rust-core.sh [--dir <RustCore dir>]   (macOS: plutil)
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
DIR="$ROOT/ios/RustCore"

# Declared slices: LibraryIdentifier|SupportedPlatform|SupportedPlatformVariant|SupportedArchitectures
# (device slices have no variant; architectures sorted, comma-joined).
# scripts/release/native-build-identity.js APPLE_DECLARED_LIBRARIES carries
# the same table; __tests__/NativeBuildIdentity.test.js keeps them equal.
DECLARED_LIBRARIES="
ios-arm64|ios||arm64
ios-arm64_x86_64-simulator|ios|simulator|arm64,x86_64
tvos-arm64|tvos||arm64
tvos-arm64-simulator|tvos|simulator|arm64
"
IDENTITY_SCHEMA="ubm-apple-rustcore-identity/1"

while [ $# -gt 0 ]; do
  case "$1" in
    --dir)
      if [ $# -lt 2 ]; then
        echo "verify-rust-core: --dir needs a value" >&2
        exit 2
      fi
      DIR="$2"; shift 2 ;;
    *) echo "verify-rust-core: unknown argument $1" >&2; exit 2 ;;
  esac
done

fail() {
  echo "verify-rust-core: FAIL $1" >&2
  echo "verify-rust-core: prebuilt mode (default) needs the CI-staged ios/RustCore from the published package: reinstall or upgrade unified-ble-manager. From source: UBM_NATIVE_BUILD=source pnpm --dir \"$ROOT\" native:apple:prepare (runs sh ios/build-rust-core.sh), then pod install." >&2
  exit 1
}

for tool in plutil shasum; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required tool $tool (macOS)"
done

FRAMEWORK="$DIR/RustCore.xcframework"
PLIST="$FRAMEWORK/Info.plist"
IDENTITY="$DIR/build-identity.json"
[ -d "$FRAMEWORK" ] || fail "missing $FRAMEWORK"
[ -f "$PLIST" ] || fail "missing $PLIST"
[ -f "$IDENTITY" ] || fail "missing $IDENTITY"

# Prints the value at a key path; exits non-zero when the key is absent.
extract() {
  plutil -extract "$2" raw -o - "$1" 2>/dev/null
}

sha256_of() {
  shasum -a 256 "$1" | cut -d' ' -f1
}

schema="$(extract "$IDENTITY" schema)" || fail "$IDENTITY has no schema"
[ "$schema" = "$IDENTITY_SCHEMA" ] || fail "$IDENTITY schema is '$schema', expected $IDENTITY_SCHEMA"

count="$(extract "$PLIST" AvailableLibraries)" || fail "$PLIST has no AvailableLibraries"
actual=""
paths=""
i=0
while [ "$i" -lt "$count" ]; do
  key="AvailableLibraries.$i"
  identifier="$(extract "$PLIST" "$key.LibraryIdentifier")" || fail "$key has no LibraryIdentifier"
  platform="$(extract "$PLIST" "$key.SupportedPlatform")" || fail "$identifier has no SupportedPlatform"
  variant="$(extract "$PLIST" "$key.SupportedPlatformVariant")" || variant=""
  library_path="$(extract "$PLIST" "$key.LibraryPath")" || fail "$identifier has no LibraryPath"
  arch_count="$(extract "$PLIST" "$key.SupportedArchitectures")" || fail "$identifier has no SupportedArchitectures"
  archs=""
  j=0
  while [ "$j" -lt "$arch_count" ]; do
    arch="$(extract "$PLIST" "$key.SupportedArchitectures.$j")" || fail "$identifier architecture $j unreadable"
    archs="$archs$arch
"
    j=$((j + 1))
  done
  archs="$(printf '%s' "$archs" | sort | paste -sd, -)"
  actual="$actual$identifier|$platform|$variant|$archs
"
  paths="$paths$identifier/$library_path
"
  i=$((i + 1))
done

declared_sorted="$(printf '%s' "$DECLARED_LIBRARIES" | sed '/^$/d' | sort)"
actual_sorted="$(printf '%s' "$actual" | sed '/^$/d' | sort)"
if [ "$declared_sorted" != "$actual_sorted" ]; then
  fail "XCFramework slices differ from the declared set.
  declared:
$declared_sorted
  staged:
$actual_sorted"
fi

want_plist="$(extract "$IDENTITY" infoPlistSha256)" || fail "$IDENTITY has no infoPlistSha256"
[ "$(sha256_of "$PLIST")" = "$want_plist" ] || fail "$PLIST sha256 does not match $IDENTITY"

recorded="$(extract "$IDENTITY" libraries)" || fail "$IDENTITY has no libraries"
[ "$recorded" = "$count" ] || fail "$IDENTITY records $recorded libraries, Info.plist declares $count"
k=0
while [ "$k" -lt "$recorded" ]; do
  key="libraries.$k"
  identifier="$(extract "$IDENTITY" "$key.libraryIdentifier")" || fail "$IDENTITY $key has no libraryIdentifier"
  library_path="$(extract "$IDENTITY" "$key.libraryPath")" || fail "$IDENTITY $key has no libraryPath"
  want_sha="$(extract "$IDENTITY" "$key.sha256")" || fail "$IDENTITY $key has no sha256"
  want_bytes="$(extract "$IDENTITY" "$key.bytes")" || fail "$IDENTITY $key has no bytes"
  printf '%s' "$paths" | grep -qxF "$identifier/$library_path" \
    || fail "$IDENTITY records $identifier/$library_path, which Info.plist does not declare"
  archive="$FRAMEWORK/$identifier/$library_path"
  [ -f "$archive" ] || fail "missing slice $archive"
  [ "$(sha256_of "$archive")" = "$want_sha" ] || fail "slice $identifier/$library_path sha256 does not match $IDENTITY (substituted or corrupted)"
  [ "$(wc -c < "$archive" | tr -d ' ')" = "$want_bytes" ] || fail "slice $identifier/$library_path size does not match $IDENTITY"
  k=$((k + 1))
done

archives="$(cd "$FRAMEWORK" && find . -name '*.a' -type f | sed 's|^\./||' | sort)"
for archive in $archives; do
  printf '%s' "$paths" | grep -qxF "$archive" || fail "undeclared archive $archive inside $FRAMEWORK"
done

echo "verify-rust-core: OK $count slices, Info.plist + every slice hash-verified ($DIR)"
