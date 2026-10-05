#!/usr/bin/env bash
# Exact packed Expo SDK57/RN-TV ARM32 consumer. Compilation is not live BLE proof.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ -z "${TV_PACKAGE_TARBALL:-}" ]]; then
  echo 'error: TV_PACKAGE_TARBALL must name the exact package to test' >&2
  exit 1
fi
export TV_STAGE_DIR="${TV_STAGE_DIR:-$(mktemp -d /tmp/ubm-packed-android-tv.XXXXXX)}"
export TV_PACKAGE_TARBALL
# A packed consumer must consume sealed release bytes. Never build a substitute
# from the checkout, even if the caller is a source-build CI job.
export UBM_NATIVE_BUILD=prebuilt
for step in stage install verify-identity prebuild-android build-android; do
  bash "${ROOT}/example-expo/scripts/build-tv.sh" "${step}"
done
node "${ROOT}/scripts/ci/check-android-apk-abi.js" \
  "${TV_STAGE_DIR}/android/app/build/outputs/apk/debug/app-debug.apk" armeabi-v7a
echo "Packed Android TV ARM32 compile/native-graph proof passed: ${TV_STAGE_DIR}"
echo 'Physical Fire OS install, permissions and BLE scenarios remain separate qualification.'
