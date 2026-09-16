# UBM 5.0 Android-emulator probe (`emulator-probe/`)

App-running slice for UBM 5.0 (trackourhealth/bun-mono#1188), Linux execution
profile, gate-ledger row **U-ANDROID-EMULATOR**. This directory is the slice's
exclusive workspace: the harness, the minimal consumer, and the results all
live here. Nothing outside `emulator-probe/` (plus additive
`android/src/androidTest/**` / `android/src/test/**` files, of which v1 needs
none — see "Why no new android tests") was modified for this slice.

## What it proves

The **real 4.x native module path** — `UnifiedBleProtocolControl`
(`BlePlxPackage`, `libunified_ble_native_protocol.so`, x86_64 ABI matched to
host KVM) — installed and run on a task-owned emulator, driven through the
public JS contract (`createReactNativeBleManager`). Not Expo Go (which cannot
contain the native module), not a JS mock.

Every result is labeled at the source:

- `REAL-EMULATOR` — observed on the task-owned AVD through `adb -s`.
- `SIMULATED-injection` — synthetic radio-boundary events fed to a seam.
  v1 injects nothing; the label exists so any future injection is explicit.
- `HOST-JVM` — host-side execution (unit tests, APK content checks).

A virtual adapter is never presented as a physical radio. Anything that hangs
is recorded `HUNG-SKIPPED` with the exact command, attempted once. Anything
the installed emulator cannot do is a documented boundary, not a failure.

## Layout

- `lib/battery-lib.js` — pure harness helpers (serial selection, origin
  labels, redaction, AVD naming, battery summary). stdlib only.
- `tests/battery-lib.test.js` — test-first contract for the helpers.
  Run: `node --test emulator-probe/tests/battery-lib.test.js`
- `scripts/run-battery.js` — the adb battery. Targeted serial only, every
  call timeout-bounded, device commands contain no pipes (host matches on
  saved logs). Writes `results/battery-*.json` + `results/logs/`.
- `consumer/` — minimal UBM-owned RN consumer (`com.ubmprobe`) that runs the
  real native module. See `consumer/README.md`.
- `results/` — battery JSON (test names, assertion counts, results, origins)
  plus redacted per-step logs.

## Exact commands (from the worktree root)

```sh
# 0. Env (all read-only, timeout-bounded)
timeout 20 emulator -accel-check
ls -l /dev/kvm
timeout 20 adb devices -l
timeout 20 emulator -list-avds

# 1. System image (once; shared SDK cache, not task-owned)
export ANDROID_HOME=$HOME/Android/Sdk ANDROID_SDK_ROOT=$HOME/Android/Sdk
yes | sdkmanager --install "system-images;android-34;google_apis;x86_64"

# 2. Task-owned AVD in a temp dir (never touches existing AVDs)
export ANDROID_AVD_HOME=/tmp/ubm5-emu-avd; mkdir -p $ANDROID_AVD_HOME
echo no | avdmanager create avd -n ubm5-emu-probe \
  -k "system-images;android-34;google_apis;x86_64" -d pixel

# 3. Boot headless, explicit adb port (serial becomes emulator-5554)
emulator -avd ubm5-emu-probe -no-window -no-audio -no-boot-anim \
  -gpu swiftshader_indirect -no-snapshot-save -port 5554

# 3b. Wait for boot completion before any install/launch (never skip:
#     installs against a half-booted emulator flake or hang the battery)
timeout 300 adb -s emulator-5554 wait-for-device
timeout 300 adb -s emulator-5554 shell 'while [ "$(getprop sys.boot_completed)" != "1" ]; do sleep 2; done'
adb -s emulator-5554 shell getprop sys.boot_completed

# 4. Build the probe consumer for the emulator ABI (needs consumer/node_modules;
#    see consumer/README.md). Uses the cached Gradle 8.13 distribution so no
#    wrapper binary is duplicated into this dir.
GRADLE_BIN=$HOME/.gradle/wrapper/dists/gradle-8.13-all/*/gradle-8.13/bin/gradle
cd emulator-probe/consumer/android
$GRADLE_BIN :app:assembleDebug -PreactNativeArchitectures=x86_64 \
  --no-daemon --console=plain

# 5. Probe Metro (narrow watches: repo-root watching exhausts inotify/ENOSPC)
cd emulator-probe/consumer
npx react-native start --port 8081 --host 127.0.0.1
adb -s emulator-5554 reverse tcp:8081 tcp:8081

# 6. Harness unit tests, then the battery
node --test emulator-probe/tests/battery-lib.test.js
node emulator-probe/scripts/run-battery.js --serial emulator-5554 \
  --apk emulator-probe/consumer/android/app/build/outputs/apk/debug/app-debug.apk \
  --package com.ubmprobe \
  --out emulator-probe/results/battery-ubmprobe.json --attempt-metro
```

API level choice: the retained matrix is minSdk 24 / runtime-permission split
at 31 / `POST_NOTIFICATIONS` at 33 / target+compile 36
(`android/gradle.properties`, `docs/GETTING_STARTED.md`,
`docs/PLATFORMS.md`). API 34 (x86_64, Google APIs) covers the 31+ and 33+
paths on the stable emulator profile. API 24–30 (location-permission path),
API 36 (target), and physical radios are follow-ups (see REPORT.md).

## 5.0 Rust swap-in point

The v1 baseline runs the retained 4.x native module (the 5.0 Rust core is
not yet Android-wired). The swap-in is explicit and single-pointed:

1. Build the 5.0 Rust Android artifact (AAR / JNI `.so` set).
2. Point the consumer at it: `emulator-probe/consumer/android/app/build.gradle`
   gains the Rust AAR dependency **in this directory**, and
   `consumer/README.md` records the exact dependency coordinates.
3. Re-run the identical battery: `scripts/run-battery.js` asserts behavior
   (install → load → bind → lifecycle → permissions → cancellation), not
   artifact provenance, so a behavior delta between the 4.x baseline
   (`results/battery-ubmprobe.json`) and the Rust run is the equivalence
   signal for gate U7.

Until then, `consumer/` deliberately contains no Rust references beyond this
section — no stub JNI, no mock native module.

## Why no new android tests in v1

`android/src/test/**` already holds 104 unit tests covering the pure
dispatcher/radio seams (arbiters, classifiers, ownership, foreground-service
lifecycle). Dispatcher-instance behavior (pending-scan cancellation, callback
ownership under a live radio) needs either a live `Context` (Robolectric is
not wired) or `connectedAndroidTest` (the library declares no
`testInstrumentationRunner`, and adding one means editing the forbidden
`android/build.gradle`). Both are documented boundaries; the emulator battery
covers the same behaviors at the installed-app level instead.

## Cleanup (only resources this slice owns)

```sh
adb -s emulator-5554 emu kill          # task-owned emulator
pkill -f "emulator-probe/consumer"     # task-owned Metro (or kill its PID)
rm -rf /tmp/ubm5-emu-avd               # task-owned AVD home (after REPORT)
```

Never touched: existing AVDs (none existed), other adb devices (none
attached), `package.json`, `docs/**`, `.github/**`, `src/**`, existing
`android/**` sources or build files.
