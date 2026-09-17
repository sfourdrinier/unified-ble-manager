# R01 Binding Producer (Native Session Facade) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the production `ReactNativeRustCoreBinding` producer: a `UnifiedBleRustCore` TurboModule per platform implementing exactly the F01 seam (`openSession(owner)` → `{contractRevision, invoke, close}`), so the R01 factory flip has a real target.

**Architecture:** Codegen spec in `src/` drives both platforms. Android module routes ops to the JNI cdylib (`EchoBridge`/`GattBridge` + 3 new first-class scan natives mirroring UniFFI U8); iOS module routes to the UniFFI `EchoSession`. A TS producer resolves the TurboModule and fails loud (`capability.unsupported`) when absent. Op coverage is explicit: session/echo/kernel/scan real on both; connect/subscribe fail loud (core follow-up, out of scope).

**Tech Stack:** React Native Codegen (TurboModules), Java (Android modules match existing style), Swift + ObjC++ (iOS), Rust (JNI externs), Jest, Gradle JVM tests, Android emulator battery.

---

## Scope boundary (read first)

- IN: spec, both native modules, JNI scan natives, TS producer, JVM tests, jest tests, Android emulator leg, CI compile on both platforms.
- OUT (explicitly sequenced later): the R01 factory-default flip (own package); the iOS simulator runtime vehicle (D2(v) T6 harness + R01 acceptance own it — no lane vehicle exists today); connect/subscribe core ops (core follow-up); `expo.ts` composition (R01 package).
- iOS proof in THIS plan: CI compile + linkage of the new module (existing apple legs), UniFFI Rust-gate parity, shared seam jest tests. Live iOS requests are proven at R01/(v), not here.

## File map

| File | Responsibility |
|---|---|
| `src/NativeUnifiedBleRustCore.ts` (create) | Codegen spec: `openSession`, `invoke`, `close`, `contractRevision` |
| `src/backends/reactnative/react-native-rust-core-binding.ts` (create) | Production binding: TurboModule resolution + fail-loud producer |
| `package.json` `codegenConfig.ios.modulesProvider` (modify) | Register `UnifiedBleRustCore` provider |
| `bindings/jni/src/core_backend.rs` (modify) | `ble_scan_start/take/stop` core fns (mirror UniFFI U8) |
| `bindings/jni/src/lib.rs` (modify) | 3 JNI externs + unit tests |
| `bindings/jni/java/com/ubm/echo/EchoBridge.java` (modify) | 3 native declarations |
| `android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModule.java` (create) | Android TurboModule: session store + op router |
| `android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java` (modify) | Register module (mirrors existing 6-line blocks) |
| `android/src/test/java/.../rustcore/UnifiedBleRustCoreModuleTest.java` (create) | JVM routing tests with fake bridge |
| `ios/UnifiedBleRustCore.swift` (create) | Swift session facade over UniFFI `EchoSession` |
| `ios/UnifiedBleRustCore.mm` (create) | `RCT_EXPORT_MODULE` + method exports |
| `unified-ble-manager.podspec` (modify) | Add the 2 files to `base_source_files` |
| `__tests__/backends/reactnative/rust-core-binding.test.js` (create) | Producer tests (present/missing/revision) |
| `emulator-probe/consumer/App.jsx` + battery (modify) | 5.0 leg: drive openSession/invoke/close, assert logcat |

## Op routing table (normative for this plan)

| `invoke` op | Android | iOS | Notes |
|---|---|---|---|
| `session.revision` | `nativeRevision()` | `EchoSession` init revision | Must equal `C-UBM.0.1.2-DRAFT` |
| `echo.bytes` / `echo.counter` | `nativeEchoBytes` / `nativeEchoCounter` | `echo_bytes` / `echo_counter` | Transport proof |
| `central.status` | `nativeCentralStatus` | `central_status` | Real kernel |
| `scan.start` / `scan.take` / `scan.stop` | NEW `nativeBleScanStart/Take/Stop` | `ble_scan_start/take/stop` | Real kernel admission, synthetic radio |
| `staged.step` / `staged.drain` / `staged.counters` | `nativeStaged*` | `staged_*` | Scripted surface |
| `session.close` | `nativeClose` | `close` | Idempotent |
| anything else | `capability.unsupported` | `capability.unsupported` | Fail loud, never legacy |

---

### Task 1: Codegen spec + provider registration

**Files:**
- Create: `src/NativeUnifiedBleRustCore.ts`
- Modify: `package.json` (`codegenConfig.ios.modulesProvider`)
- Test: `npx tsc --noEmit -p tsconfig.json` + Codegen dry-run via example Android sync

- [ ] **Step 1: Write the spec**

```ts
// src/NativeUnifiedBleRustCore.ts
import type { TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface RustCoreSessionHandle {
  sessionId: string
}

export interface RustCoreInvokeResult {
  ok: boolean
  value: string
  code: string
  domain: string
  operation: string
}

export interface Spec extends TurboModule {
  openSession(owner: string): Promise<RustCoreSessionHandle>
  invoke(sessionId: string, op: string, argsJson: string): Promise<RustCoreInvokeResult>
  close(sessionId: string): Promise<void>
  contractRevision(): string
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleRustCore')
```

- [ ] **Step 2: Register the iOS provider**

In `package.json` `codegenConfig.ios.modulesProvider`, add `"UnifiedBleRustCore": "UnifiedBleRustCore"` alongside the two existing entries.

- [ ] **Step 3: Typecheck**

Run: `npx tsc --noEmit -p tsconfig.json`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add src/NativeUnifiedBleRustCore.ts package.json
git commit -m "feat(rn): UnifiedBleRustCore Codegen spec + provider (lane only, no merge)"
```

### Task 2: JNI first-class scan natives (mirror UniFFI U8)

**Files:**
- Modify: `bindings/jni/src/core_backend.rs` (add `ble_scan_start/take/stop` mirroring `bindings/uniffi/src/core_backend.rs:524-560` semantics: staged `scan.start` line through the real Central, core-minted op id)
- Modify: `bindings/jni/src/lib.rs` (3 `pub extern "system"` fns on `EchoBridge`: `nativeBleScanStart(handle, owner, timeoutMs, nowMs) -> String`, `nativeBleScanTake(handle) -> String`, `nativeBleScanStop(handle, opId, nowMs) -> String`; decimal-string times; `EchoError` → thrown `EchoException`, same as existing fns)
- Modify: `bindings/jni/java/com/ubm/echo/EchoBridge.java` (3 declarations)
- Test: `cargo test -p ubm5_jni_echo --locked` (add `scan_slice_roundtrip` test: start → take → stop over synthetic radio, assert `op_id` present and stop observation)

- [ ] **Step 1: Add core fns + externs + Java declarations** (mirror the file-local patterns; no new dependencies)
- [ ] **Step 2: Add the roundtrip test in `core_backend.rs` tests mod**
- [ ] **Step 3: Run gates**

Run: `cargo test -p ubm5_jni_echo --locked && cargo clippy -p ubm5_jni_echo --all-targets --locked -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 4: Rebuild the cdylib and prove symbols**

Run: `sh android/build-rust-cdylib.sh --abi arm64-v8a --profile release --libdir /tmp/scanprobe && nm -D --defined-only /tmp/scanprobe/libubm5_jni_echo.so | grep -c BleScan`
Expected: `3` (Start/Take/Stop).

- [ ] **Step 5: Refresh committed prebuilts** (the cdylib changed)

Run: `sh android/refresh-prebuilt-jniLibs.sh`
Expected: 16K PASS both ABIs, identity rewritten.

- [ ] **Step 6: Commit**

```bash
git add bindings/jni/src/core_backend.rs bindings/jni/src/lib.rs bindings/jni/java/com/ubm/echo/EchoBridge.java android/src/main/jniLibs/
git commit -m "feat(jni): first-class ble_scan natives mirroring UniFFI U8 (lane only, no merge)"
```

### Task 3: Android TurboModule + JVM tests

**Files:**
- Create: `android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModule.java`
- Modify: `android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java` (register; mirror the existing `getModule` + `ReactModuleInfo` blocks)
- Test: `android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModuleTest.java`

Design (testable without loading the `.so`): the module owns a `RustCoreSessionStore` (sessionId → handle, `ConcurrentHashMap`, UUID ids) and routes `invoke` through a package-private `RustCoreBridge` interface whose default impl calls `EchoBridge` statics. Unit tests inject a fake bridge. Unknown ops reject with `capability.unsupported`; empty owner/sessionId reject `argument.invalid`; `contractRevision()` returns the pinned `C-UBM.0.1.2-DRAFT` and `openSession` verifies `nativeRevision()` equality first (`protocol.incompatible` on skew).

- [ ] **Step 1: Write the JVM test first** (routing table: open/close/invoke each op/unknown-op/empty-owner/revision-skew — 8 tests minimum)
- [ ] **Step 2: Run to verify it fails**

Run: `cd example/android && ./gradlew :unified-ble-manager:testDebugUnitTest --tests '*UnifiedBleRustCoreModuleTest*' --no-daemon`
Expected: FAIL (class under test missing).

- [ ] **Step 3: Implement the module + store + bridge interface + registration**
- [ ] **Step 4: Run full unit suite**

Run: `cd example/android && ./gradlew :unified-ble-manager:testDebugUnitTest --no-daemon`
Expected: BUILD SUCCESSFUL, 187/187 (179 existing + 8 new).

- [ ] **Step 5: Commit**

```bash
git add android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/ android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/
git commit -m "feat(android): UnifiedBleRustCore TurboModule + JVM tests (lane only, no merge)"
```

### Task 4: iOS TurboModule + podspec wiring

**Files:**
- Create: `ios/UnifiedBleRustCore.swift` (session facade: `[sessionId: EchoSession]`, op router per the table, `contractRevision()` pinned check at open)
- Create: `ios/UnifiedBleRustCore.mm` (`RCT_EXPORT_MODULE(UnifiedBleRustCore)` + `RCT_EXPORT_METHOD` for openSession/invoke/close/contractRevision bridging to Swift)
- Modify: `unified-ble-manager.podspec` (append the 2 files to `base_source_files`)
- Test: apple-ci compile (existing legs build the pod once merged); local proof unavailable on Linux (no Xcode) — prove by inspection + CI.

- [ ] **Step 1: Write Swift facade + ObjC++ bridge** (mirror `ios/UnifiedBleProtocolControl.mm:334` export pattern; Swiftelm init `EchoSession(revision:)` from `bindings/uniffi/generated/swift/ubm_echo.swift`)
- [ ] **Step 2: Wire podspec `base_source_files`**
- [ ] **Step 3: Validate Ruby syntax**

Run: `ruby -c unified-ble-manager.podspec`
Expected: `Syntax OK`.

- [ ] **Step 4: Run the podspec dual-mode gate**

Run: `node scripts/ci/check-podspec-rust-selection.js`
Expected: PASS (exact command per the script's usage).

- [ ] **Step 5: Commit**

```bash
git add ios/UnifiedBleRustCore.swift ios/UnifiedBleRustCore.mm unified-ble-manager.podspec
git commit -m "feat(ios): UnifiedBleRustCore TurboModule + podspec wiring (lane only, no merge)"
```

### Task 5: TS production binding + jest tests

**Files:**
- Create: `src/backends/reactnative/react-native-rust-core-binding.ts`
- Test: `__tests__/backends/reactnative/rust-core-binding.test.js`

Producer shape (implements `ReactNativeRustCoreBinding` from `react-native-rust-core.ts`):

```ts
import { TurboModuleRegistry } from 'react-native'
import type Spec from '../../../NativeUnifiedBleRustCore'
import { contractError } from '../../../backend-contract/errors'

export function createReactNativeRustCoreBinding(): ReactNativeRustCoreBinding {
  const native = TurboModuleRegistry.get<Spec>('UnifiedBleRustCore')
  if (native == null) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  return {
    openSession: async owner => {
      const { sessionId } = await native.openSession(owner)
      return {
        contractRevision: () => native.contractRevision(),
        invoke: (op, args) => native.invoke(sessionId, op, JSON.stringify(args)).then(r => r.value),
        close: () => native.close(sessionId)
      }
    }
  }
}
```

- [ ] **Step 1: Write jest tests** (missing module throws `capability.unsupported`; present module opens/invokes/closes through the mocked TurboModule; revision mismatch surfaces via `admitReactNativeRustCoreSession`)
- [ ] **Step 2: Run to verify they fail**

Run: `npx jest --config jest.package.config.js __tests__/backends/reactnative/rust-core-binding.test.js`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement the producer**
- [ ] **Step 4: Run tests + typecheck**

Run: `npx jest --config jest.package.config.js __tests__/backends/reactnative/rust-core-binding.test.js && npx tsc --noEmit -p tsconfig.json`
Expected: PASS, exit 0.

- [ ] **Step 5: Commit**

```bash
git add src/backends/reactnative/react-native-rust-core-binding.ts __tests__/backends/reactnative/rust-core-binding.test.js
git commit -m "feat(rn): production Rust core binding producer + tests (lane only, no merge)"
```

### Task 6: Android emulator leg (live proof)

**Files:**
- Modify: `emulator-probe/consumer/App.jsx` (add `rustCoreButton`: openSession → invoke `central.status` + `scan.start/take/stop` → close; log `[UBM_PROBE]` receipts)
- Modify: `emulator-probe/scripts/run-battery.js` (tap + assert the new receipts; add tap center constant)

- [ ] **Step 1: Add the probe leg + battery assertions**
- [ ] **Step 2: Build + run on the task-owned emulator**

Run: `cd emulator-probe/consumer && pnpm install --no-frozen-lockfile && cd android && <gradle8.13> :app:assembleDebug -PreactNativeArchitectures=x86_64 --no-daemon && node ../scripts/run-battery.js --only RUSTCORE`
Expected: receipts show real central status + scan observations from Rust; no `capability.unsupported`.

- [ ] **Step 3: Commit**

```bash
git add emulator-probe/consumer/App.jsx emulator-probe/scripts/run-battery.js
git commit -m "feat(probe): 5.0 RustCore emulator leg (lane only, no merge)"
```

### Task 7: Merge + full gates + push (push discipline applies)

- [ ] **Step 1: Full jest**

Run: `pnpm test:package`
Expected: 227+ suites green (new counts recorded in the merge message).

- [ ] **Step 2: tsc + lint**

Run: `npx tsc --noEmit -p tsconfig.json && pnpm lint`
Expected: exit 0.

- [ ] **Step 3: Gradle unit tests**

Run: `cd example/android && ./gradlew :unified-ble-manager:testDebugUnitTest --no-daemon`
Expected: BUILD SUCCESSFUL, 187/187.

- [ ] **Step 4: Merge to lane and push (ONLY when no Apple legs are in flight)**

```bash
git checkout 5.0.0 && git merge --no-ff r01-binding-producer -m "merge(5.0.0): R01 binding producer (lane only, no merge)" && git push origin 5.0.0
```

## Self-review

- Spec coverage: every F01 seam method has a task (spec→Task 1, Android→Task 3, iOS→Task 4, producer→Task 5, live proof→Task 6). JNI scan gap closed by Task 2. Codegen integration covered by provider/podspec/BlePlxPackage edits with CI compile as proof.
- No placeholders: all paths, commands, and expected outputs are exact; the one estimate (187 tests) is verified at runtime in Task 7.
- Type consistency: `RustCoreSessionHandle`/`RustCoreInvokeResult` field names are used identically in Tasks 1/3/4/5; op names match the routing table.
