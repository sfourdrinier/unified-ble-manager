#!/usr/bin/env bash
# Full packed reference-consumer compilation, separate from Swift typechecking.
# Each target is built sequentially: Expo's dependency producer owns shared
# node_modules build state even when the app DerivedData roots are distinct.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ "$(uname -s)" != Darwin || "$(uname -m)" != arm64 ]]; then
  echo 'error: full tvOS consumer acceptance requires an Apple Silicon macOS/Xcode host' >&2
  exit 1
fi
if [[ -z "${TV_PACKAGE_TARBALL:-}" ]]; then
  echo 'error: TV_PACKAGE_TARBALL must name the exact release package tarball' >&2
  exit 1
fi
export TV_STAGE_DIR="${TV_STAGE_DIR:-$(mktemp -d /tmp/ubm-packed-tv.XXXXXX)}"
export TV_PACKAGE_TARBALL
TV_CONSUMER_LOG_DIR="$(mktemp -d /tmp/ubm-packed-tv-logs.XXXXXX)"
for step in stage install verify-identity prebuild build-simulator build-target; do
  bash "${ROOT}/example-expo/scripts/build-tv.sh" "${step}" >"${TV_CONSUMER_LOG_DIR}/${step}.log" 2>&1 || {
    echo "error: packed tvOS ${step} failed (${TV_STAGE_DIR})" >&2
    tail -80 "${TV_CONSUMER_LOG_DIR}/${step}.log" >&2
    exit 1
  }
done
echo "Full packed tvOS ARM64 simulator/physical-target compile passed: ${TV_STAGE_DIR}"
echo "Logs: ${TV_CONSUMER_LOG_DIR}"
echo 'Evidence level: compile/link only; simulator launch and physical radio remain distinct.'
