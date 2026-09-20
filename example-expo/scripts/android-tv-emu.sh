#!/usr/bin/env bash
# example-expo/scripts/android-tv-emu.sh — Google TV emulator as a test host.
#
# Boots the pinned Google TV AVD, installs the example APK, reverses the
# driver and Metro ports, and launches the app. The phone APK installs and
# runs on the TV emulator unmodified, so this never forks the example: it
# only drives adb/emulator with pinned arguments. Launch goes through
# `am start`, not the leanback launcher (the phone manifest has no TV
# launcher intent, and none is needed for a test host).
#
# The emulator reaches the host through 10.0.2.2, but this mirrors the
# physical phone setup instead: adb reverse maps the emulator's localhost
# ports onto the host, so neither the driver server (8795) nor the phone
# Metro (8082) is touched. The proven phone pair is tcp:8795->tcp:8795 and
# tcp:8081->tcp:8082; other work depends on both, so never restart them.
#
# Usage:
#   bash example-expo/scripts/android-tv-emu.sh boot     # start AVD (no-op when booted), wait for boot
#   bash example-expo/scripts/android-tv-emu.sh install  # adb install -r the example APK
#   bash example-expo/scripts/android-tv-emu.sh reverse  # reverse driver + Metro ports
#   bash example-expo/scripts/android-tv-emu.sh launch   # am start the example MainActivity
#   bash example-expo/scripts/android-tv-emu.sh status   # emulator + app state
#   bash example-expo/scripts/android-tv-emu.sh all      # boot..launch
#
# Env (defaults match this repo's setup):
#   ANDROID_TV_AVD=TV_IMAGIBOOKS_GOOGLE_TV_API_36_arm64_v8a
#   ANDROID_TV_SERIAL=emulator-5554 ANDROID_TV_PACKAGE=com.sfourdrinier.bleplxexample
#   ANDROID_TV_METRO_PORT=8082 ANDROID_TV_DRIVER_PORT=8795
#   ADB=adb ANDROID_EMULATOR_BIN=$ANDROID_HOME/emulator/emulator
#   ANDROID_TV_APK=<repo>/example-expo/android/app/build/outputs/apk/debug/app-debug.apk
#   ANDROID_TV_EMU_LOG=/tmp/ubm-android-tv-emu.log ANDROID_TV_BOOT_TIMEOUT=300
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

AVD="${ANDROID_TV_AVD:-TV_IMAGIBOOKS_GOOGLE_TV_API_36_arm64_v8a}"
SERIAL="${ANDROID_TV_SERIAL:-emulator-5554}"
PACKAGE="${ANDROID_TV_PACKAGE:-com.sfourdrinier.bleplxexample}"
METRO_PORT="${ANDROID_TV_METRO_PORT:-8082}"
DRIVER_PORT="${ANDROID_TV_DRIVER_PORT:-8795}"
ADB="${ADB:-adb}"
APK="${ANDROID_TV_APK:-${ROOT}/example-expo/android/app/build/outputs/apk/debug/app-debug.apk}"
EMU_LOG="${ANDROID_TV_EMU_LOG:-/tmp/ubm-android-tv-emu.log}"
BOOT_TIMEOUT="${ANDROID_TV_BOOT_TIMEOUT:-300}"

emulator_bin() {
  if [[ -n "${ANDROID_EMULATOR_BIN:-}" ]]; then
    printf '%s' "${ANDROID_EMULATOR_BIN}"
  elif [[ -n "${ANDROID_HOME:-}" ]]; then
    printf '%s' "${ANDROID_HOME}/emulator/emulator"
  else
    echo "error: no emulator binary (set ANDROID_EMULATOR_BIN or ANDROID_HOME)" >&2
    return 1
  fi
}

booted() {
  [[ "$("${ADB}" -s "${SERIAL}" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]
}

cmd_boot() {
  local emu
  emu="$(emulator_bin)" || exit 1
  if [[ ! -x "${emu}" ]]; then
    echo "error: emulator binary is not executable: ${emu}" >&2
    exit 1
  fi
  if booted 2>/dev/null; then
    echo "emulator ${SERIAL} already booted"
    return 0
  fi
  "${emu}" -avd "${AVD}" -no-snapshot -no-boot-anim >>"${EMU_LOG}" 2>&1 &
  echo "emulator ${SERIAL} booting (AVD ${AVD}, log ${EMU_LOG})"
  local waited=0
  while ! booted 2>/dev/null; do
    if [[ "${waited}" -ge "${BOOT_TIMEOUT}" ]]; then
      echo "error: emulator ${SERIAL} did not boot within ${BOOT_TIMEOUT}s (log ${EMU_LOG})" >&2
      exit 1
    fi
    sleep 5
    waited=$((waited + 5))
  done
  echo "emulator ${SERIAL} booted"
}

cmd_install() {
  if [[ ! -f "${APK}" ]]; then
    echo "error: example APK not found: ${APK} (build example-expo/android first)" >&2
    exit 1
  fi
  "${ADB}" -s "${SERIAL}" install -r "${APK}"
}

cmd_reverse() {
  # Same pair as the physical phone: the dev bundle is served on the Metro
  # port while the app requests localhost:8081, and the driver server stays
  # shared on its own port.
  "${ADB}" -s "${SERIAL}" reverse "tcp:${DRIVER_PORT}" "tcp:${DRIVER_PORT}"
  "${ADB}" -s "${SERIAL}" reverse tcp:8081 "tcp:${METRO_PORT}"
  echo "reversed tcp:${DRIVER_PORT}->tcp:${DRIVER_PORT} tcp:8081->tcp:${METRO_PORT} on ${SERIAL}"
}

cmd_launch() {
  "${ADB}" -s "${SERIAL}" shell am start -n "${PACKAGE}/.MainActivity"
}

cmd_status() {
  "${ADB}" -s "${SERIAL}" shell 'getprop sys.boot_completed; getprop ro.product.model'
  "${ADB}" -s "${SERIAL}" shell "pidof ${PACKAGE}" || echo "app ${PACKAGE} not running"
  "${ADB}" -s "${SERIAL}" reverse --list
}

case "${1:-all}" in
  boot) cmd_boot ;;
  install) cmd_install ;;
  reverse) cmd_reverse ;;
  launch) cmd_launch ;;
  status) cmd_status ;;
  all) cmd_boot; cmd_install; cmd_reverse; cmd_launch ;;
  *) echo "usage: $0 [boot|install|reverse|launch|status|all]" >&2; exit 1 ;;
esac
