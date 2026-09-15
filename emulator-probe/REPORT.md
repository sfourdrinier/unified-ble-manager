# U-ANDROID-EMULATOR slice report (UBM 5.0, Linux execution profile)

Branch `codex/ubm5-emu`. All work is local; nothing pushed. Raw evidence:
`results/battery-ubmprobe.json` (+ `results/logs/`), `results/example-crash.json`.

## 1. Environment record (read-only probes, `timeout 20` wrappers)

| Item | Observed |
|---|---|
| Emulator accel | `emulator -accel-check`: KVM (version 12) installed and usable, exit 0; `/dev/kvm` present (`crw-rw----+ root:kvm`) |
| Emulator | 36.6.11.0 (build 15507667); `adb`/`fastboot` 37.0.0-14910828 |
| SDK | platforms android-34/36/37.0; build-tools 35.0.0/36.0.0; NDK 27.0.12077973 + 27.1.12297006; cmake 3.22.1; cmdline-tools 19.0 |
| JDK / Gradle | OpenJDK 21.0.12; no system Gradle (used cached Gradle 8.13 dist); Kotlin plugin 2.1.20 via RN template |
| Pre-existing AVDs | none (`emulator -list-avds` empty; `~/.android/avd` absent) — nothing to wipe, nothing wiped |
| Attached devices at start | none (`adb devices` empty); all later commands used explicit `-s emulator-5554` |
| System images | none installed → installed `system-images;android-34;google_apis;x86_64` (rev 14) into the shared SDK cache |
| BlueZ / D-Bus | `bluetoothctl` 5.72 present; `org.bluez` absent on D-Bus (no daemon/device) — host BT irrelevant to the emulator slice, recorded only |
| Controller (nRF52840 fault-injection) | absent; per `docs/PLATFORMS.md` deferred to 4.1, not a 5.0 entrypoint |
| Node / pnpm / Rust | node 22.21.1, pnpm 10.14.0, cargo/rustc 1.97.1; 5.0 Rust workspace is `crates/ubm-core` only — **not Android-wired** (no JNI/AAR), so v1 is the retained-4.x-behavior baseline with a documented swap-in point |
| Host CPU/RAM | x86_64, 20 cores, 125 GB RAM → emulator ABI **x86_64 matched to host KVM** |

API levels (retained matrix: `android/gradle.properties` min 24 / target+compile 36;
`GETTING_STARTED.md` runtime-permission split at 31; `PLATFORMS.md`
`POST_NOTIFICATIONS` at 33): emulator runs **API 34**, covering the 31+ and
33+ paths. API 24–30 (location path), API 36 (target), other hosts →
follow-ups.

## 2. AVD identity (task-owned)

- Name `ubm5-emu-probe` (prefix-enforced by the harness), home `/tmp/ubm5-emu-avd`
  (temp dir; `~/.android` untouched)
- `system-images/android-34/google_apis/x86_64`, ABI x86_64 (auto-selected)
- Fingerprint `google/sdk_gphone64_x86_64/emu64xa:14/UE1A.230829.050/12077443:userdebug/dev-keys`,
  SDK 34, `sdk_gphone64_x86_64` / `emu64xa`, serial `emulator-5554` (explicit `-port 5554`)
- Launch: `-no-window -no-audio -no-boot-anim -gpu swiftshader_indirect -no-snapshot-save`
- Virtual BT adapter present (`dumpsys bluetooth_manager`: ON, emulator
  self-assigned address, redacted in logs) — **never claimed as physical radio**

## 3. Battery results (`results/battery-ubmprobe.json`)

Consumer: `com.ubmprobe` debug APK built from `emulator-probe/consumer`
(real 4.x path: `createReactNativeBleManager` → `UnifiedBleProtocolControl`
→ `OwnedAndroidGattRadio` → `libunified_ble_native_protocol.so` x86_64).
**14 tests, 57/57 assertions, 13 pass + 1 boundary (T11), 0 fail.**

| Test | Result | Assertions | What was proven (REAL-EMULATOR) |
|---|---|---|---|
| PRE identity/preconditions | pass | 3/3 | sdk_gphone64_x86_64, BT manager queryable, clean slate via uninstall |
| T1 install | pass | 3/3 | `install → Success`, package listed, v1/min24/target36 |
| T2 native-lib surface | pass | 4/4 | `primaryCpuAbi=x86_64`, nativeLibraryDir declared, `extractNativeLibs=false` (mmap-from-APK; hence device maps name segments `base.apk`, not the `.so` name), run-as debuggable access |
| T3 launch/identity | pass | 5/5 | activity starts + resumes (`topResumedActivity`, API-34 field name), process alive, no FATAL, no `UnsatisfiedLinkError` |
| T4 setup-teardown | pass | 3/3 | force-stop clears process, no UBM service lingers |
| T5 foreground-background | pass | 5/5 | HOME keeps PID, activity unr
esumes, relaunch re-resumes |
| T6 denied/revoked | pass | 7/7 | revoke → `granted=false`; denial ping → `BleError permission.denied: rn-android-boundary.enumerateBondedPeers` (explicit, never empty); grant restores; `bonded-count=0` |
| T6b revoke-kill | pass | 3/3 | revocation kills the process (platform-owned teardown); clean relaunch, new PID |
| T7 kill-relaunch | pass | 4/4 | `run-as kill -9` own PID → gone → relaunch with fresh PID (no immortal handles) |
| T8 JS-driven native battery | pass | 10/10 | Metro reachable; `app-mounted`; manager created; adapter round-trips (`unknown` on virtual adapter, never blocking); `bonded-count=0` (native bond-table read); pre-aborted find → `operation.aborted`; live scan settles with an explicit typed error |
| T9 service refusal | pass | 1/1 | external `start-foreground-service` refused (service undeclared in bare consumer; in-app lease needs JS) |
| T10 adapter loss | pass | 4/4 | `svc bluetooth disable` → OFF observable → process survives → enable restores |
| T11 bounded delivery | boundary | 0/0 | no burst drivable without peers; covered host-side (deterministic TCK overflow vector + unit suite) |
| T12 JS reload | pass | 5/5 | dev-menu Reload tapped; post-clear `app-mounted`; **same PID** (in-process rebind); no FATAL (only handled RN `ReactNoCrashSoftException` teardown noise) |

Notable designed-behavior evidence: aborting a starting scan yields
`AggregateError: BLE operation and cleanup both failed`
(`src/public/error-bridge.ts runWithCleanup`) — both errors preserved,
nothing swallowed; native `BtGatt unregisterScanner` ran. Mid-scan (pending)
cancellation needs a radio that holds a scan open — physical follow-up.

Supporting host-side runs: harness unit tests 17/17
(`node --test emulator-probe/tests/battery-lib.test.js`); android unit suite
**104/104 across 7 suites**
(`:unified-ble-manager:testDebugUnitTest`, HOST-JVM, not emulator);
APK contains `lib/x86_64/libunified_ble_native_protocol.so` (unzip -l).

## 4. Boundaries (emulator cannot do these; not failures)

- **Physical radio behavior** (scan/connect/notify/background reliability,
  OEM quirks): virtual adapter only → U-ANDROID-PHYSICAL.
- **Pending-scan cancellation & bounded-queue fill**: scans reject fast with
  no peers; needs a chatty physical peripheral.
- **Active-lease service loss**: service is plugin-managed and undeclared in
  the bare consumer; needs an in-app lease + physical run.
- **API 24–30 / API 36 / other hosts** (U-APPLE, U-WINDOWS): other AVDs/devices.
- **In-process instrumentation**: no `testInstrumentationRunner` in
  `android/build.gradle` (editing it is forbidden to this slice), and no
  Robolectric — so no new `android/src/*test/**` files were needed or added;
  the 104 existing unit tests cover the pure seams.
- **SIMULATED injections**: none performed in v1 (nothing to label); the
  origin taxonomy enforces labeling when they appear.

## 5. Findings for owning slices (not fixed here)

1. **`example/android` crash-loops on every launch** (any device/ABI):
   `MainApplication.kt:19-25` initializes `reactHost` eagerly with
   `applicationContext` (null pre-`attach()`) → NPE. Evidence:
   `results/example-crash.json` + `results/logs/EXAMPLE-CRASH-logcat.log`
   (unmodified tree, install Success → FATAL). My fix was reverted per scope;
   the probe consumer carries the `by lazy` pattern in a slice-owned file.
2. **UBM JS requires `TextDecoder`**: bare Hermes here lacks it, so
   native→JS record delivery fails closed with `ReferenceError`
   (surfaced, fail-closed — good projection, but the example app would hit it
   too if it ever launched). Probe consumer polyfills via `text-encoding`.
3. First post-boot `adb shell am` segfaulted once, then worked (emulator
   platform flake, single occurrence, recorded).
4. `pnpm start -- --port X` (double dash) silently binds 8081; use
   `npx react-native start --port X`. Metro `watchFolders` must stay narrow —
   repo-root watching exhausts inotify (ENOSPC) and kills the server.

## 6. Follow-ups

- U-ANDROID-PHYSICAL: pending-scan cancel, queue fill, lease-loss, OEM matrix.
- Re-run this identical battery against the 5.0 Rust artifact at the
  documented swap-in point (`README.md` §5.0 Rust swap-in) for U7 equivalence.
- Owning slice: `example/` Application NPE + TextDecoder host requirement.
- Consider `testInstrumentationRunner` (lane decision, edits `android/`) if
  in-process instrumentation is ever wanted.
