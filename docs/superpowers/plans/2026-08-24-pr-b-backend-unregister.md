# Plan B — backend unregister and overflow native release

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Start only after Plan A is merged to `main`.

**Goal:** Consumer close, terminal, and overflow always unregister backend-owned streams. Overflow that terminalizes a consumer releases native scan/notify owners when that consumer set is empty. Failed native cleanup stays owned and retryable. Adapter-state watches stay in the core set until close succeeds.

**Architecture:** Extract WinRT's existing owned-stream pattern into one core-internal `OwnedCoreBoundedStream`: close, immediate close, graceful terminal, and overflow terminal all invoke one idempotent ownership-release callback. Core `adapterStates().stop()` deletes from `adapterStateWatches` only after close is `released`. BlueZ security and BlueZ scan/notify overflow remain regression controls.

**Tech Stack:** TypeScript backends, `UnifiedBleCore`, Jest, native-protocol scripts.

**Spec:** [Master plan](./2026-08-24-lifecycle-correctness-master.md)

**Base:** `origin/main` after Plan A merge

**Locked designs:**

1. Create `src/core/owned-bounded-stream.ts` by moving/generalizing `WinRtOwnedStream`. Its idempotent `releaseOwnership()` runs from `close()`, `closeWithReason()`, and `finishWithReason()`; overflow-policy `error` reaches the overridden close path through dynamic dispatch. Registries use this class directly, so iterator/public close and producer terminal cannot drift. `Map<string, Set<Stream>>` callbacks also prune empty peer keys.
2. `#61`: store a named abort listener on the session. `stop()` has one in-flight attempt shared by manual stop, abort, and destroy; successful release is memoized, while rejection/`release-failed` clears only the in-flight promise so a later stop retries. Remove the abort listener when the attempt settles, including failure. Destroy aggregates every remaining watch through the existing core lifecycle observer.
3. `#68`: fix RN Android, WinRT, deterministic security. **BlueZ `src/backends/bluez/bluez-security.ts` is the control** — add the same close/overflow tests there and expect them already green.
4. `#69`: close, terminal, and overflow must unregister. Web Bluetooth and WinRT adapter/event behavior are regression controls. Add a module-internal WeakMap-backed `inspectDeterministicStreamOwnershipForTests(backend)` in `deterministic-backend-base.ts`; tests import that internal file directly, and no root/testing entrypoint re-exports it.
5. `#70`/`#71`: honor `emit().terminated`; drop that consumer; if the group is empty, stop the native watcher/notify. Failed native stop is retained on the group/owner and retried by a later stop/destroy. A second consumer in the same group must keep receiving. One shared Jest scenario module used for CoreBluetooth (JS double), WinRT (JS double), and BlueZ (control).

---

## Trackers

| Issue                                                                | Title                                                 |
| -------------------------------------------------------------------- | ----------------------------------------------------- |
| [#61](https://github.com/sfourdrinier/unified-ble-manager/issues/61) | adapter-state watch dropped before close succeeds     |
| [#68](https://github.com/sfourdrinier/unified-ble-manager/issues/68) | security watch close does not unregister              |
| [#69](https://github.com/sfourdrinier/unified-ble-manager/issues/69) | adapter/event streams remain registered after close   |
| [#70](https://github.com/sfourdrinier/unified-ble-manager/issues/70) | CoreBluetooth overflow does not release native owners |
| [#71](https://github.com/sfourdrinier/unified-ble-manager/issues/71) | WinRT scan overflow does not release scan ownership   |

---

## File map

- Modify: `src/core/unified-ble-core.ts`
- Create: `src/core/owned-bounded-stream.ts`
- Modify: `src/backends/reactnative/react-native-android-security.ts`
- Modify: `src/backends/winrt/winrt-security.ts`
- Modify: `src/testing/deterministic/deterministic-security.ts`
- Modify: `src/backends/corebluetooth/corebluetooth-backend.ts`
- Modify: `src/backends/corebluetooth/corebluetooth-gatt-operations.ts` `emitNotification()`
- Modify: `src/backends/bluez/bluez-backend-runtime.ts`
- Modify: `src/backends/winrt/winrt-backend.ts` `handleAdvertisement()`
- Test-only control: `src/web/web-bluetooth-backend.ts`
- Create: `__tests__/core/adapter-state-watch-cleanup.test.js`
- Create: `__tests__/backends/security-watch-unregister.test.js`
- Create: `__tests__/backends/adapter-event-stream-unregister.test.js`
- Create: `__tests__/backends/scan-overflow-native-release.test.js`
- Modify: `__tests__/manager/public-helpers.test.js` for adapter-state retry assertions

---

### Task B1: #61 adapter-state watch ownership

**Files:**

- Modify: `src/core/unified-ble-core.ts` `adapterStates()`
- Create: `__tests__/core/adapter-state-watch-cleanup.test.js`

- [ ] **Step 1: RED tests named**

- `stop does not drop the watch before close succeeds`
- `destroy retries a watch whose close returned release-failed`
- `manual stop, abort, and destroy share one in-flight close`
- `abort after successful stop is a no-op`
- `abort listener is removed on success and on failure`
- `destroy aggregates remaining watch cleanup failures`

Use a fake `backend.adapter.watchState()` whose `transitions.close()` returns `release-failed` on first call and `released` on second.

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/core/adapter-state-watch-cleanup.test.js \
  __tests__/public-adapter-watch.test.js \
  __tests__/manager/public-helpers.test.js
```

Expected: `stop` deletes from `adapterStateWatches` before close, so destroy does not retry.

- [ ] **Step 2: Implement**

The session stores `stopAttempt: Promise<CleanupRecord> | null` and `released: CleanupRecord | null`. `stop()` returns `released` after success, returns `stopAttempt` while one is active, and otherwise starts `closeAdapterStateStream`. In `finally`, remove the named `onAbort` listener and clear `stopAttempt`. Delete the session from `adapterStateWatches` and set `released` only when the cleanup state is `released`; retain it after rejection or `release-failed`. `destroy()` already iterates the retained set through `lifecycleObserver.captureCleanup`, so do not import the public error bridge into core.

- [ ] **Step 3: GREEN** same Jest command. Commit: `fix: retain adapter-state watches until close succeeds (#61)`

---

### Task B2: #68 security watch unregister

**Files:**

- Modify: `src/backends/reactnative/react-native-android-security.ts`
- Modify: `src/backends/winrt/winrt-security.ts`
- Modify: deterministic security backend
- Create: `__tests__/backends/security-watch-unregister.test.js`
- Control: `src/backends/bluez/bluez-security.ts` (already deletes on `emit().terminated` and has `streams.delete(stream)`)

- [ ] **Step 1: RED tests named**

- `android security close removes the stream from the registry`
- `winrt security close removes the stream from the registry`
- `deterministic security close removes the stream from the registry`
- `bluez security close already unregisters (control)`
- `later security event does not iterate a closed stream`
- `repeated open/close does not grow the map`
- `close terminal reset and destroy race unregisters exactly once`
- `deterministic retained and reserved stream bytes return to zero`

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/backends/security-watch-unregister.test.js
```

- [ ] **Step 2: Implement** Construct an `OwnedCoreBoundedStream` in each affected `watch()`. Its release callback deletes the stream from the peer set and deletes the map key when the set becomes empty. Keep broadcast-loop terminal deletion as a defensive invariant check. BlueZ remains unchanged and must pass the same behavioral tests.

- [ ] **Step 3: GREEN + commit** `fix: unregister security watches on consumer close (#68)`

---

### Task B3: #69 adapter/event stream unregister

**Files:**

- Modify: `src/backends/corebluetooth/corebluetooth-backend.ts` `stateStreams` / `eventStreams`
- Modify: `src/backends/bluez/bluez-backend-runtime.ts`
- Modify: `src/testing/deterministic/deterministic-backend-base.ts` `stateWatchers` / `eventStreams`
- Create: `__tests__/backends/adapter-event-stream-unregister.test.js`

- [ ] **Step 1: RED tests named**

- `corebluetooth state and event close unregisters`
- `bluez state and event close unregisters`
- `deterministic state and event close unregisters`
- `deterministic counts return to zero after close`
- `terminal reason unregisters`
- `overflow terminal unregisters`
- `web adapter/event close remains unregistered (control)`
- `winrt adapter/event close remains unregistered (control)`
- `repeated watch and event-stream churn keeps every registry bounded`
- `deterministic aggregate quota and zero-resource evidence include stream ownership`
- `core retry after release-failed backend close unregisters only after success`

For deterministic, assert inspect counts (`stateWatchers.size`, `eventStreams.size`) are 0 after close.

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/backends/adapter-event-stream-unregister.test.js
```

- [ ] **Step 2: Implement** Replace raw registered streams in CoreBluetooth, BlueZ, and deterministic state/event factories with `OwnedCoreBoundedStream`. Each release callback deletes the exact instance from its set. Move WinRT's local `WinRtOwnedStream` implementation to the shared core file and import it back without changing behavior. Keep Web's existing managed stream as a control; do not edit Web production code. Broadcast loops delete terminal streams as a backstop, not as the only path. Update the deterministic internal WeakMap counters on add/release.

- [ ] **Step 3: GREEN + commit** `fix: unregister backend adapter and event streams on close (#69)`

---

### Task B4: #70 and #71 overflow native release

**Files:**

- Modify: `src/backends/corebluetooth/corebluetooth-backend.ts` `handleAdvertisement()`
- Modify: `src/backends/corebluetooth/corebluetooth-gatt-operations.ts` `emitNotification()`
- Modify: `src/backends/winrt/winrt-backend.ts` `handleAdvertisement()`
- Create: `__tests__/backends/scan-overflow-native-release.test.js`
- Control: BlueZ `emitNotification` / scan consumer path already checks `emit().terminated`

- [ ] **Step 1: RED tests named** (same names, parameterized by backend id `corebluetooth` | `winrt` | `bluez`)

- `overflow error policy terminalizes the overflowing consumer only`
- `sibling consumer in the same group still receives`
- `last consumer overflow stops the native scan or notify owner`
- `failed native stop is retained and retried on a later stop`
- `later native callbacks do not copy into the dead consumer`
- `overflow racing abort deadline and explicit stop releases the consumer once`
- `resource counters return to their exact pre-consumer values`
- `CoreBluetooth final notification overflow disables native notification`

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/backends/scan-overflow-native-release.test.js
```

BlueZ rows must already pass (control). CoreBluetooth and WinRT rows fail because `emit()` results are ignored.

- [ ] **Step 2: Implement** After `const push = consumer.stream.emit(...)`, route `push.terminated` through the backend's existing idempotent consumer-release function—the same function used by abort, deadline, and explicit stop. That function removes only the affected logical consumer and releases its timers/listeners. When the group becomes empty, start one memoized native watcher/notify stop phase. Retain a thrown or `release-failed` native result on the group/owner, clear the in-flight promise, and retry it from later stop/destroy without re-releasing successful logical consumers. Keep the native source active while a healthy sibling remains. Drop/quarantine callbacks for released consumers before normalization or byte copying, and update resource counters only on confirmed phase transitions.

- [ ] **Step 3: GREEN JS.** Then native-protocol:

```sh
pnpm test:native-protocol
pnpm test:native-protocol:apple
pnpm test:native-protocol:winrt
```

Android security (#68) also:

```sh
pnpm test:native-protocol:android
```

Commit: `fix: release CoreBluetooth and WinRT owners on consumer overflow (#70 #71)`

---

# Plan B merge gate

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/core/adapter-state-watch-cleanup.test.js \
  __tests__/backends/security-watch-unregister.test.js \
  __tests__/backends/adapter-event-stream-unregister.test.js \
  __tests__/backends/scan-overflow-native-release.test.js
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
pnpm test:native-protocol
pnpm test:native-protocol:apple
pnpm test:native-protocol:winrt
pnpm test:native-protocol:android
```

Hosted: all jobs that run must be green (JS matrix, Tauri Rust, Classic RN Android, Expo CNG if scheduled).

- [ ] Close B trackers on merge
- [ ] Do not start Plan C until this PR is on `main`
