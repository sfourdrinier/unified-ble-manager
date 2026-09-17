#!/bin/sh
# android/check-elf-16k-pages.sh — D2(iii) 16 KB page-size gate (UBM 5.0).
#
# Android 15+ requires every shipped native library to use 16 KB ELF
# alignment: each PT_LOAD segment's Align must be >= 0x4000 (16384).
# This script FAILS LOUD on any misaligned LOAD segment. It resolves
# llvm-readelf from the NDK first (same NDK candidates as
# build-rust-cdylib.sh), then PATH (llvm-readelf, readelf, eu-readelf).
#
# Usage: check-elf-16k-pages.sh --so <file> --abi <abi> [--offline-ok]
#   --offline-ok: exit 0 with a SKIP receipt when no readelf-class tool is
#     available (packed-consumer path: consumers must never need the NDK;
#     misalignment still fails loud whenever the tool IS available).
set -eu

SO=""
ABI=""
OFFLINE_OK="no"

while [ $# -gt 0 ]; do
  case "$1" in
    --so) SO="$2"; shift 2 ;;
    --abi) ABI="$2"; shift 2 ;;
    --offline-ok) OFFLINE_OK="yes"; shift ;;
    *) echo "check-elf-16k-pages: unknown argument $1" >&2; exit 2 ;;
  esac
done

fail() { echo "check-elf-16k-pages: FAIL $1" >&2; exit 1; }

[ -n "$SO" ] || fail "missing --so (ELF shared library to check)"
[ -n "$ABI" ] || fail "missing --abi (for diagnostics)"
[ -f "$SO" ] || fail "library missing: $SO (abi=$ABI)"

READELF=""
if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
  _ndk="$ANDROID_NDK_HOME"
else
  _sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
  _ndk=""
  for candidate in "$_sdk/ndk/27.1.12297006" "$_sdk/ndk/27.0.12077973"; do
    if [ -d "$candidate" ]; then _ndk="$candidate"; break; fi
  done
fi
if [ -n "${_ndk:-}" ]; then
  case "$(uname -s)" in
    Linux) _host="linux-x86_64" ;;
    Darwin) _host="darwin-x86_64" ;;
    *) _host="" ;;
  esac
  if [ -n "$_host" ] && [ -x "$_ndk/toolchains/llvm/prebuilt/$_host/bin/llvm-readelf" ]; then
    READELF="$_ndk/toolchains/llvm/prebuilt/$_host/bin/llvm-readelf"
  fi
fi
if [ -z "$READELF" ]; then
  for tool in llvm-readelf readelf eu-readelf; do
    if command -v "$tool" >/dev/null 2>&1; then READELF="$tool"; break; fi
  done
fi
if [ -z "$READELF" ]; then
  if [ "$OFFLINE_OK" = "yes" ]; then
    echo "check-elf-16k-pages: SKIP abi=$ABI so=$SO (no llvm-readelf/readelf available; packed consumers must never need the NDK)"
    exit 0
  fi
  fail "no ELF inspection tool (tried NDK llvm-readelf + PATH llvm-readelf/readelf/eu-readelf). Install NDK 27.x via: sdkmanager 'ndk;27.1.12297006'"
fi

# Every PT_LOAD Align must be >= 0x4000. Parse the wide (-W) program-header
# table; llvm-readelf and binutils readelf share the LOAD/Align columns.
LOADS="$("$READELF" -W -l "$SO" 2>/dev/null | awk '$1 == "LOAD" { print $NF }')"
[ -n "$LOADS" ] || fail "no PT_LOAD segments parsed from $SO via $READELF (abi=$ABI) — unparsable ELF"

BAD=""
COUNT=0
for align in $LOADS; do
  COUNT=$((COUNT + 1))
  # Align prints as 0x... hex; strip to compare numerically.
  _dec="$(printf '%d' "$align" 2>/dev/null || echo 0)"
  if [ "$_dec" -lt 16384 ]; then
    BAD="$BAD $align"
  fi
done
if [ -n "$BAD" ]; then
  fail "abi=$ABI so=$SO has $COUNT PT_LOAD segments with sub-16KB Align:$BAD (Android 15+ requires Align >= 0x4000 on every LOAD). Rebuild with -Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384."
fi
echo "check-elf-16k-pages: PASS abi=$ABI so=$SO ($COUNT PT_LOAD segments, all Align >= 0x4000, via $READELF)"
