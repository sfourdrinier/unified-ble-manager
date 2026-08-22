# Android PHY Runtime Capability Truth Fix Implementation Plan

> **For agentic workers:** Execute this plan inline with TDD. Do not commit; preserve unrelated worktree changes.

**Goal:** Make React Native Android advertise and expose `connection:phy` only when the native runtime handshake reports PHY support, so API 24/25 fail with `capability.unsupported` before native dispatch while API 26+ retains limited deterministic semantics.

**Architecture:** Add an optional, versioned `phyAvailable` field to the existing protocol-v2 handshake result. The Android boundary starts fail-closed and exposes a dynamic PHY dispatch capability after handshake; the CoreBluetooth runtime feature registry is refreshed after the boundary opens, making the provider descriptor authoritative for the instantiated Android runtime. Existing Android API guards and protocol-v2 command/result records remain unchanged.

**Tech Stack:** TypeScript, Jest, Kotlin/Java Android protocol sources, generated React Native TurboModule source, existing native protocol contract/source-guard tests, pnpm.

---

### Task 1: Add RED handshake and boundary tests

**Files:**

- Modify: `__tests__/native-protocol/AndroidPhyProtocolBoundary.test.js`
- Modify: `__tests__/native-protocol/NativeProtocolV2Contract.test.js`

- [x] Add deterministic controls whose handshake returns `phyAvailable: false`, `phyAvailable: true`, and no `phyAvailable` field.
- [x] Assert the Android boundary reports PHY unavailable for false/missing handshakes, rejects `readPhy` and `requestPhy` with `capability.unsupported`, and submits no command.
- [x] Assert a true handshake preserves PHY command dispatch and result decoding.
- [x] Assert the contract fixture recognizes the optional handshake extension while the protocol command/result schema remains v2 and unchanged.
- [x] Run the focused Jest tests and confirm they fail because the boundary still hard-coded PHY available.

### Task 2: Add RED provider/public capability tests

**Files:**

- Modify: `__tests__/backends/reactnative/react-native-android-vertical-slice.test.js`
- Modify: `__tests__/public-link-controls.test.js` only if an isolated public-call regression fixture is required

- [x] Add provider fixtures for false and true native PHY handshake results.
- [x] Assert false/missing runtime handshakes produce an `unsupported` `connection:phy` descriptor and public calls reject `capability.unsupported` without adding `readPhy` or `requestPhy` to the runtime command list.
- [x] Assert true handshakes produce a `limited` descriptor and preserve existing PHY read/request behavior.
- [x] Run only these focused tests and confirm the new assertions fail before production edits.

### Task 3: Add RED Android native source guards

**Files:**

- Modify: `__tests__/backends/reactnative/android-phy-source.test.js`
- Modify: `android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherLifecycleTest.kt`

- [x] Assert the Java handshake result derives `phyAvailable` from `Build.VERSION.SDK_INT >= Build.VERSION_CODES.O`.
- [x] Assert dispatcher PHY entrypoints retain an explicit API-level guard before `OwnedAndroidGattRadio.readPhy`/`requestPhy`.
- [x] Assert no command/result enum or wire field is added for the handshake-only capability.
- [x] Run the focused JavaScript source guard and Android protocol unit test targets; confirm the new guards fail against current source.

### Task 4: Implement the minimal handshake and boundary fix

**Files:**

- Modify: `src/NativeUnifiedBleProtocolControl.ts`
- Modify: `src/native-protocol/rn-android-boundary.ts`
- Modify: `android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java`
- Modify: `android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt`

- [x] Add optional `phyAvailable?: boolean` to `NativeProtocolHandshakeResult` as a protocol-v2 handshake extension.
- [x] Initialize boundary PHY capability to false, set it only when `handshake.phyAvailable === true`, validate the optional field as boolean, and make `connectionControlCapabilities.phy` reflect that state.
- [x] Keep `readPhy` and `requestPhy` command/result records unchanged.
- [x] Return the API-level boolean from the Android Java module and retain a defensive dispatcher guard without casts or compatibility fallback.

### Task 5: Make provider feature registration runtime-truthful

**Files:**

- Modify: `src/backends/reactnative/react-native-connection-control-features.ts`
- Modify: `src/backends/corebluetooth/corebluetooth-runtime-capabilities.ts`
- Modify: `src/backends/corebluetooth/corebluetooth-backend.ts`
- Modify: `src/backends/reactnative/react-native-android-provider.ts`

- [x] Remove the static Android PHY registration that claims `limited` before handshake.
- [x] Add the shared runtime PHY registration from `CoreBluetoothBoundary.connectionControlCapabilities` and use `unsupported`/blocked evidence when absent or false, `limited`/deterministic evidence when true.
- [x] Add a focused backend runtime-feature refresh after `boundary.open()` and before returning the provider backend.
- [x] Preserve Apple’s explicit unsupported PHY registration and all existing non-PHY feature ownership.

### Task 6: GREEN and scoped verification

**Files:** No further files unless a failing focused test exposes a directly related contract fixture.

- [x] Run the focused boundary/provider/public/source-guard tests and verify all new assertions pass: 6 suites / 66 tests.
- [x] Run `pnpm typecheck`, `pnpm native-protocol:check`, the focused Android/RN tests, and the focused native-protocol contract tests.
- [x] Run `git diff --check` and changed-source lint. The repository-wide lint gate is rerun after the TCK slice is formatted; local Android Gradle remains blocked before project compilation because the checkout has no `:unified-ble-manager` Gradle project.
- [x] Inspect `git diff --check`, `git status --short`, and the scoped diff; Web, G6A, docs, and prior commits remain separate.
- [x] Record exact commit evidence in the branch history; deterministic/native-protocol evidence is not physical-radio proof, and hosted Android compilation remains required.
