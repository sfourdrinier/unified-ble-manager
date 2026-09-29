# Trusted reference process continuation

The app-only `UBMReferenceContinuation` module runs the shared
`process-continuation` scenario on the existing Android/iOS native process owner.
The Expo plugin copies these tracked templates and registers both native modules
on prebuild. No production TurboModule method, second central, caller-selected
storage directory, permission grant, or fabricated appearance is introduced.

First explicitly use `continuation declare` with the exact native peer, recipe,
and (if needed) recording ID/bounds. Then use `process-continuation execute` with
the same arguments. A missing, record-only, or different persisted declaration
refuses execution; execute does not secretly change the standing order. Existing
owned sessions also retain their declaration/release authority.

This proves **direct warm native execution**, never a CDM appearance, CoreBluetooth
restoration, cold-process launch, or permission to run indefinitely in background.
It does not update `lastWake`. Generic HR/ECG/ACC recipes remain in the shared
scenario. Background execution requires its independently acquired host capability.

All raw operations verify the loaded library identity before invocation. Native
errors and values remain canonical envelopes, decoded by the shared controller.
At most 16 accepted app-module controls are outstanding, including asynchronous
native callbacks. React reload does not abandon accepted process-owned work.

For cleanup, seal the exact durable recording, explicitly claim the volatile
backlog (decoded before its handoff acknowledgement), then disarm the standing
order with `continuation stop`. A refused disposal remains retryable; do not infer
release from an empty queue. Durable pages use the separate recording controller:
there is no automatic journal acknowledgement or clear. Retain all loss receipts.

Focused Android module/host tests, without app generation, install, or radio:

```sh
cd example/android
./gradlew -I ../../example-expo/native/test-android.gradle \
  :unified-ble-manager:testDebugUnitTest --no-daemon --console=plain
```

The init script compiles the real tracked app templates into the existing library
test fixture and validates actual emitted failure envelopes with the current
TypeScript decoder. The canonical `pnpm test:native-protocol:android` command
includes this init script, so the existing CI and preflight Android lanes execute
these checks automatically. Apple app-template compilation and emitted-envelope checks
run in `src/driver/__tests__/native-continuation-apple.test.mjs` on macOS. The
canonical `pnpm test:native-protocol:apple` gate invokes that compilation and
actual emitted-envelope decoder validation automatically.
