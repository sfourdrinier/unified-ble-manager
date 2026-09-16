# five0 — HOST-ANDROID bridge slice battery (UBM 5.0)

Track: trackourhealth/bun-mono#1188 · vehicle: U5/U9/U10.
Branch `codex/ubm5-host-android`. Nothing here is pushed by the slice.

## What this covers (11 records, 49/49 assertions green)

The Android GATT-callback → real Central path, end to end on a 5.0
artifact (the `bindings/jni` cdylib, `libubm5_jni_echo.so`, built for the
emulator ABI with explicit rustc targets on the pinned toolchain).
Committed record: `results-five0.json` (11 pass, 49/49 assertions; raw
`logs/` stay local — gitignored, never committed).

1. **AVD** (REAL-EMULATOR): task-owned AVD `ubm5-emu-five0` created in a
   temp home, headless emulator spawned on `emulator-5556`, boot detected
   via `sys.boot_completed` (30s on the reference run).
2. **PRE** (REAL-EMULATOR): device identity (`sdk_gphone64_x86_64`, API
   34, ABI x86_64), virtual bluetooth adapter queryable, package absent
   before install.
3. **F1 install** (REAL-EMULATOR): probe APK installs, package listed,
   v1/min24/target34.
4. **F2 native-lib surface** (REAL-EMULATOR): `primaryCpuAbi=x86_64`,
   native library dir declared, APK path queryable.
5. **F3 binding identity** (REAL-EMULATOR): launch accepted, process
   runs, no FATAL, no `UnsatisfiedLinkError`, in-APK self-test RESULT ok
   (41 checks through the real JNI natives).
6. **F4 setup-teardown** (REAL-EMULATOR): force-stop clears the process.
7. **F5 lifecycle** (REAL-EMULATOR): HOME keeps the process (same pid),
   activity re-resumes.
8. **F6 kill-relaunch** (REAL-EMULATOR): `kill -9` + relaunch gets a
   fresh pid and RESULT ok (no immortal handles).
9. **F7 permission-denied** (REAL-EMULATOR): revoked BLE permissions fail
   closed with the explicit
   `permission.denied|rn-android-boundary` identity; re-grant restores
   RESULT ok.
10. **F8 queued-cancel + release** (SIMULATED-injection): cold-started
    self-test proves queued-cancel-aborts, io-settled, release (real
    destroy transition), post-close-enqueue-rejects, adapter-reset,
    expire-sweep through the real JNI natives into the REAL Central.
11. **F9 notify bounds** (SIMULATED-injection): cold-started self-test
    proves notify-delivered, notify-oversize-rejects
    (`bytes.too-large`), stale-handle-fails-closed, path-registered.

Supporting layers (not emulator-executed, proven on HOST-JVM):

- **JNI bridge** (`bindings/jni/src/gatt_queue.rs` +
  `java/com/ubm/gatt/GattBridge.java`): binder threads enqueue only
  (`nativeEnqueueGattEvent`); a worker drains (`nativeDrainGattEvents`)
  into the REAL session-owned Central. Proven by
  `cargo test -p ubm5_jni_echo` (26 tests) and the `TestGatt` JVM
  exchange (44 checks) in `bindings/jni/run_jni_roundtrip.sh`.
- **Gradle wiring** (`android/build-rust-cdylib.sh`, invoked from
  `android/build.gradle#buildUbmRustCdylib` and from `probe-app`):
  pinned Rust 1.98.1, NDK 27.x linker, ABI list **x86_64 only** (KVM
  host-matched). No cargo-ndk.
- **Kotlin runtime** (`android/.../radio/GattCentralWire.kt`,
  `UbmGattCentralBridge.kt`): pure wire builders + permission-gated,
  never-driving post/drain bridge. HOST-JVM tests:
  `android/.../radio/GattCentralBridgeTest.kt` (11 tests).

## Running it

```sh
# 1. Build the probe APK (stages the cdylib via build-rust-cdylib.sh)
#    using the repo's own Gradle 8.13 wrapper (example/android/gradlew):
example/android/gradlew -p emulator-probe/five0/probe-app :app:assembleDebug --offline
# 2. Run the battery (task-owned AVD ubm5-emu-five0 on emulator-5556,
#    temp home /tmp/ubm5-five0-avd; every device command has a timeout):
node emulator-probe/five0/run-five0-battery.js
# Results: five0/results-five0.json (committed) + five0/logs/ (local only)
```

`--only PRE,F1,F3` narrows the run (needs a live emulator on the
serial); `--no-cleanup` leaves the owned emulator + AVD up for
inspection (default cleans them).

## Kotlin HOST-JVM tests (no emulator, no RN plugin)

The `android/` library module test task needs the RN Gradle plugin
(absent without `example/node_modules`). The identical test class runs
directly against cached jars (kotlinc-jvm 2.1.20, JUnit 4.13.2):

```sh
M2=~/.gradle/caches/modules-2/files-2.1
KC=$(find $M2/org.jetbrains.kotlin/kotlin-compiler-embeddable/2.1.20 -name "*.jar" | head -1)
STDLIB=$(find $M2/org.jetbrains.kotlin/kotlin-stdlib/2.1.20 -name "kotlin-stdlib-2.1.20.jar" | head -1)
CORO=$(find $M2/org.jetbrains.kotlinx/kotlinx-coroutines-core-jvm/1.9.0 -name "*.jar" | head -1)
DAEMON=$(find $M2/org.jetbrains.kotlin/kotlin-daemon-embeddable/2.1.20 -name "*.jar" | head -1)
TROVE=$(find $M2/org.jetbrains.intellij.deps/trove4j/1.0.20200330 -name "*.jar" | head -1)
ANNOT=$(find $M2/org.jetbrains/annotations/23.0.0 -name "*.jar" | head -1)
JUNIT=$(find $M2/junit/junit/4.13.2 -name "*.jar" | head -1)
HAMCREST=$(find $M2/org.hamcrest/hamcrest-core/1.3 -name "*.jar" | head -1)
rm -rf /tmp/ubm5-kt-test && mkdir -p /tmp/ubm5-kt-test/out
java -cp "$KC:$STDLIB:$CORO:$DAEMON:$TROVE:$ANNOT" \
  org.jetbrains.kotlin.cli.jvm.K2JVMCompiler \
  -no-stdlib -no-reflect -cp "$STDLIB:$ANNOT:$JUNIT:$HAMCREST" \
  -d /tmp/ubm5-kt-test/out \
  android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralWire.kt \
  android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCentralBridge.kt \
  android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralBridgeTest.kt
java -cp "/tmp/ubm5-kt-test/out:$STDLIB:$JUNIT:$HAMCREST" \
  org.junit.runner.JUnitCore com.sfourdrinier.unifiedblemanager.radio.GattCentralBridgeTest
# Expected: OK (11 tests)
```

## Boundaries (emulator cannot do these; not failures)

- No BLE peers exist: scan/connect/notify prove the bridge↔core path with
  synthetic wires (SIMULATED-injection), never physical radio behavior.
- arm64-v8a/armeabi-v7a unwired (no physical devices on this lane).
- API 24–30 location path / API 36 target: battery runs API 34.
- The probe app drives the JNI natives directly; the
  `UbmGattCentralBridge`/`GattCentralWire` Kotlin owner path is covered
  by the HOST-JVM unit tests above, not by any F-test.
- Launch waits poll for the self-test RESULT line (cold ART first launch
  is slow: F3 budget 45s, others 30s); budgets are timeouts, not sleeps.
