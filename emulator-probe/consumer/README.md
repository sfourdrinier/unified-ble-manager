# Probe consumer (`com.ubmprobe`)

Minimal UBM-owned React Native consumer that runs the **real 4.x native
module path** (`createReactNativeBleManager` → `UnifiedBleProtocolControl` →
`OwnedAndroidGattRadio` → `libunified_ble_native_protocol.so`) on the
task-owned emulator. It exists because the bare `example/` app cannot serve
as the v1 consumer (see "Why not the example app" below); everything the
battery needs from a consumer is duplicated here, in slice-owned files.

## UI contract (tapped by `scripts/run-battery.js` via uiautomator centers)

| Button (testID)          | Action |
|---|---|
| Init manager (`initButton`) | requests BT runtime permissions, creates the manager, reads `adapter.state()` |
| Scan 8s then cancel (`scanCancelButton`) | `find()` with a 1.5 s abort timer; reports the terminal outcome |
| Bonded peers (`bondedButton`) | `peers.bonded()`; reports `bonded-count=N` or the error shape |
| Aborted find (`abortedFindButton`) | `find()` with a pre-aborted signal; reports the cancellation receipt |
| RustCore session (`rustCoreButton`) | R01 producer probe: `openSession` → `central.status` + `echo.counter` invokes → `close` through `UnifiedBleRustCore`; reports `rustcore-ok` or `rustcore-error` |
| Teardown (`teardownButton`) | `manager.destroy()` |

Every outcome is logged to logcat with the `[UBM_PROBE]` tag
(`ReactNativeJS`), which is what the battery asserts on. Tap centers for AVD
`ubm5-emu-probe` are constants in `run-battery.js` (`PROBE_TAP`).

## Build

```sh
cd emulator-probe/consumer
pnpm install --no-frozen-lockfile   # needs .npmrc node-linker=hoisted (RN tooling expects flat paths)
GRADLE_BIN=$HOME/.gradle/wrapper/dists/gradle-8.13-all/*/gradle-8.13/bin/gradle
cd android
$GRADLE_BIN :app:assembleDebug -PreactNativeArchitectures=x86_64 --no-daemon --console=plain
```

`unified-ble-manager` resolves via `file:../..` (the worktree) plus
`react-native.config.js`; the UBM `lib/` build must exist (root
`pnpm install --frozen-lockfile` produces it). Debug signing uses a
throwaway `android/app/debug.keystore` (same convention as the example app).

## Run

```sh
cd emulator-probe/consumer
npx react-native start --port 8081 --host 127.0.0.1
adb -s emulator-5554 reverse tcp:8081 tcp:8081
adb -s emulator-5554 install -r android/app/build/outputs/apk/debug/app-debug.apk
adb -s emulator-5554 shell am start -n com.ubmprobe/.MainActivity
```

Notes learned on the lane (do not "fix" by reverting):

- `metro.config.js` watches **only** this consumer. Watching the repo root
  exhausts the host inotify budget (`ENOSPC`) and kills Metro: the tree holds
  several `node_modules` forests plus `example/android` `.cxx` build dirs.
- `index.js` polyfills `TextDecoder`/`TextEncoder` (`text-encoding`
  package). The UBM native-protocol JS boundary requires `TextDecoder`, and
  bare Hermes on this profile does not provide it; without the polyfill,
  native-to-JS delivery fails closed with a `ReferenceError` (good error
  projection, but no battery).
- `android/app/src/debug/AndroidManifest.xml` sets `usesCleartextTraffic`
  for debug builds (same as the RN template's example overlay); without it,
  the dev-server connection is refused (`CLEARTEXT ... not permitted`).
- Start Metro with `npx react-native start --port …` — `pnpm start -- --port …`
  (double dash) is silently ignored by the CLI and the server binds 8081.

## Why not the example app

`example/android` crash-loops at `MainApplication` construction on every
launch (property initializer calls `applicationContext` before `attach()` →
NPE; `MainApplication.kt:19-25`), so its JS never runs and the native module
never loads. That file is outside this slice's allowed paths, so the fix
belongs to the owning slice — this consumer carries the corrected pattern
(`by lazy`) in a slice-owned file instead. The unmodified example APK is
still used for one record: install succeeds, launch crashes (see
`results/example-crash.json`), which is the evidence handed to the owner.

## 5.0 Rust swap-in

See the parent `emulator-probe/README.md` ("5.0 Rust swap-in point"): the
Rust AAR dependency lands in `android/app/build.gradle` **here**, and the
same battery re-runs unchanged.
