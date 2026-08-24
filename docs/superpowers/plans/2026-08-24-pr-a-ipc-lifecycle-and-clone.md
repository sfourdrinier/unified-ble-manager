# Plan A — IPC lifecycle, stream close, and clone safety

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Complete phases **A1 → A2 → A3 → A4** in order. Run each phase gate before starting the next phase. Do not start Plan B until this PR is merged to `main`.

**Goal:** Close Plan A issues so cloned records cannot poison prototypes, owner `close()` discards queued values, public scan overflow/stop tears down source and native scan exactly once, public cleanup preserves `release-failed`, desktop IPC teardown cannot hang or leak, Web disconnect matches #56, and Tauri replay/retry is bounded.

**Architecture:** One PR, four gated phases. A1 is the clone boundary (#58) with a single shared constructor. A2 introduces the shared cleanup collector (#75), then stream close-after-finish (#59), then public scan stop + emit-overflow (#60). A3 is IPC ownership. A4 is Web Bluetooth and Tauri host integrity.

**Tech Stack:** TypeScript, Tauri Rust, Jest, Cargo.

**Spec:** [Master plan](./2026-08-24-lifecycle-correctness-master.md)

**Base:** `origin/main` @ `109ce0ca` (`v4.0.2`)

**Locked designs (no alternatives):**

1. Forbidden object keys are exactly `__proto__`, `constructor`, and `prototype`. Encountering any of them at any depth is `protocol.malformed`. They are never stored as data.
2. Every dynamically reconstructed output record starts as `Object.create(null)`; snapshot results are frozen, while Tauri encode/decode results retain their current mutable wire-object behavior. The prototype is always `null`, never `Object.prototype`.
3. Input records may have prototype `Object.prototype` or `null` only. Any other prototype is `protocol.malformed`.
4. Generic safe-record helpers in `src/backend-contract/serializable.ts` are the only record-construction path for snapshot/encode/decode. They accept the error domain explicitly so shared snapshots keep `boundary` errors while Tauri keeps `ipc` errors. The helpers are generic over entry value type; Tauri never forces decoded `unknown` through `SerializableValue` or a type assertion.
5. `#75` lands before `#60`. `#60` uses that collector for native `session.stop()` failures during overflow and explicit stop.
6. `CoreBoundedStream.closeWithReason` after `finishWithReason` discards the queue and uses the **close** reason as the terminal reason. Plain `finish` without a later `close` still drains.
7. Public scan has one shared cleanup state machine used by explicit `stop()`, local overflow, and manager `destroy()`. It independently tracks view/source-iterator closure and native `session.stop()`, shares concurrent attempts, retries only unresolved phases, and removes manager ownership only after both phases are `released`. `accept()` must inspect `emit().terminated` and enter that state machine.
8. Tauri replay: lease-scoped completed-correlation tombstones expire after **30 seconds** and the combined in-flight + unexpired-completed window is capped at **256**. Admission prunes expired entries, rejects a replay still in the window, and fails closed with `protocol.violation` when the window is full; it never evicts a live tombstone to admit new work. State dies with the lease. No public protocol change.
9. Tauri quarantine retries: one scheduler per lease, coalesced by `(cleanup_command, resource_handle)`, **8** attempts per resource and **4** active workers. Additional distinct cleanups remain in a bounded queue owned by the caller; lease release cancels workers and settles queued resources. Exhausted retries append a cleanup failure to the caller ledger so the next `release` returns `release-failed` and retries the still-owned resource.
10. Hung lifecycle admission: disconnect does **not** await admission; it cancels admission, attempts physical disconnect, quarantines late subscribe.

Keep green: `__tests__/ipc/connection-cleanup.test.js` (#56).

---

## Trackers

| Phase | Issue                                                                | Title                                                       |
| ----- | -------------------------------------------------------------------- | ----------------------------------------------------------- |
| A1    | [#58](https://github.com/sfourdrinier/unified-ble-manager/issues/58) | `__proto__` clone / decode                                  |
| A2    | [#59](https://github.com/sfourdrinier/unified-ble-manager/issues/59) | close after finish still drains                             |
| A2    | [#75](https://github.com/sfourdrinier/unified-ble-manager/issues/75) | public stop/destroy drop `release-failed`                   |
| A2    | [#60](https://github.com/sfourdrinier/unified-ble-manager/issues/60) | stop-after-resolve **and** `accept()` ignores emit terminal |
| A3    | [#72](https://github.com/sfourdrinier/unified-ble-manager/issues/72) | tombstone leaves live sink                                  |
| A3    | [#63](https://github.com/sfourdrinier/unified-ble-manager/issues/63) | event pump leaves children hanging                          |
| A3    | [#73](https://github.com/sfourdrinier/unified-ble-manager/issues/73) | malformed connect leak                                      |
| A3    | [#76](https://github.com/sfourdrinier/unified-ble-manager/issues/76) | events-admission unsubscribe swallowed                      |
| A3    | [#74](https://github.com/sfourdrinier/unified-ble-manager/issues/74) | expired deadlines still dispatch                            |
| A3    | [#79](https://github.com/sfourdrinier/unified-ble-manager/issues/79) | hung admission deadlocks disconnect                         |
| A4    | [#67](https://github.com/sfourdrinier/unified-ble-manager/issues/67) | Web disconnect skipped                                      |
| A4    | [#77](https://github.com/sfourdrinier/unified-ble-manager/issues/77) | Tauri completed-correlation replay                          |
| A4    | [#78](https://github.com/sfourdrinier/unified-ble-manager/issues/78) | unbounded cancelled-success retries                         |

---

## File map

- Modify: `src/backend-contract/serializable.ts`
- Modify: `src/tauri/transport.ts` — call serializable helpers; no local `{}` assignment loops
- Modify: `src/public/error-bridge.ts` — export `collectCleanupPhases`
- Modify: `src/core/bounded-stream.ts`
- Modify: `src/public/ble-manager.ts`
- Modify: `src/ipc/manager.ts`
- Modify: `src/web/web-bluetooth-backend.ts`
- Modify: `native/tauri/src/btleplug_dispatcher.rs`
- Modify: `native/tauri/Cargo.toml` — add Tokio `test-util` as a dev-only feature for deterministic replay/retry time
- Create: `__tests__/serializable-record-clone.test.js`
- Modify: `__tests__/TauriTransport.test.js`
- Create: `__tests__/core/bounded-stream-close-after-finish.test.js`
- Modify: `__tests__/public-scan-query.test.js`
- Modify: `__tests__/ipc/pending-stream-bounds.test.js`
- Create: `__tests__/ipc/event-pump-termination.test.js`
- Create: `__tests__/ipc/provisional-admission.test.js`
- Modify: `__tests__/ipc/connection-cleanup.test.js`
- Modify: `__tests__/web/web-bluetooth-lifecycle-hardening.test.js`

---

# Phase A1 — clone boundary (#58)

### Task A1.1: #58 failing tests

**Files:**

- Create: `__tests__/serializable-record-clone.test.js`
- Modify: `__tests__/TauriTransport.test.js`

- [ ] **Step 1: Write the failing tests**

`__tests__/serializable-record-clone.test.js` must include these tests:

- `rejects __proto__, constructor, and prototype keys at the top level`
- `rejects those keys in nested bootstrap, route, event, and platform-error metadata`
- `rejects custom prototypes while accepting Object.prototype and null-prototype records`
- `snapshot byteLength equals own enumerable data only`
- `Uint8Array byte wrappers remain siblings and are not walked as records`
- `output snapshot prototype is null and Object.keys sees only own data`
- `ordinary nested arrays null-prototype records bytes and serializable equality remain compatible`

Required test body excerpts:

```js
const { snapshotSerializableRecord } = require('../src/backend-contract/serializable')
const { encodeTauriWireValue, decodeTauriWireValue } = require('../src/tauri/transport')

test('rejects __proto__, constructor, and prototype keys at the top level', () => {
  for (const key of ['__proto__', 'constructor', 'prototype']) {
    expect(() => snapshotSerializableRecord({ [key]: 'x' })).toThrow(/protocol\.malformed/)
    expect(() => encodeTauriWireValue({ [key]: 'x' })).toThrow(/protocol\.malformed/)
    expect(() => decodeTauriWireValue({ [key]: 'x' })).toThrow(/protocol\.malformed/)
  }
})

test('rejects those keys in nested bootstrap, route, event, and platform-error metadata', () => {
  const nested = {
    bootstrap: { attachment: { adapter: { state: { ['__proto__']: { polluted: true } } } } },
    route: { payload: { constructor: { steal: 1 } } },
    event: { item: { prototype: { x: 1 } } },
    platform: { metadata: { ['__proto__']: 'x' } }
  }
  expect(() => snapshotSerializableRecord(nested.bootstrap)).toThrow(/protocol\.malformed/)
  expect(() => encodeTauriWireValue(nested.route)).toThrow(/protocol\.malformed/)
  expect(() => decodeTauriWireValue(nested.event)).toThrow(/protocol\.malformed/)
  expect(() => snapshotSerializableRecord(nested.platform)).toThrow(/protocol\.malformed/)
})

test('rejects custom prototypes while accepting Object.prototype and null-prototype records', () => {
  expect(() => snapshotSerializableRecord(Object.create({ inherited: 1 }))).toThrow(/protocol\.malformed/)
  const ordinary = { a: 1 }
  const nulled = Object.assign(Object.create(null), { a: 1 })
  expect(snapshotSerializableRecord(ordinary).value.a).toBe(1)
  expect(snapshotSerializableRecord(nulled).value.a).toBe(1)
  expect(Object.getPrototypeOf(snapshotSerializableRecord(ordinary).value)).toBe(null)
})

test('snapshot byteLength equals own enumerable data only', () => {
  const record = { a: 'x' }
  const snapshot = snapshotSerializableRecord(record)
  expect(snapshot.byteLength).toBe(new TextEncoder().encode(JSON.stringify(snapshot.value)).byteLength)
  expect(Object.getOwnPropertyNames(snapshot.value).sort()).toEqual(['a'])
})

test('Uint8Array byte wrappers remain siblings and are not walked as records', () => {
  const bytes = new Uint8Array([1, 2, 3])
  const encoded = encodeTauriWireValue({ bytes, label: 'ok' })
  expect(Object.keys(encoded).sort()).toEqual(['bytes', 'label'])
  expect(encoded.bytes).toEqual({ $__unifiedBleBytesV2: [1, 2, 3] })
  expect(decodeTauriWireValue(encoded).bytes).toEqual(bytes)
})
```

A byte-tag object with any sibling key, including a forbidden key created via `JSON.parse`, must be rejected rather than partially decoded.

Add to `__tests__/TauriTransport.test.js`:

- `rejects forbidden keys on encode and decode instead of mutating Object.prototype`
- `round-trips nested route envelopes through encode then decode with null prototypes`

- [ ] **Step 2: RED**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/serializable-record-clone.test.js \
  __tests__/TauriTransport.test.js
```

Expected: forbidden-key tests fail because `result[key] = value` on `{}` either poisons the prototype or does not throw. The custom-prototype snapshot test fails because `snapshotRecord()` currently copies only own keys without validating the input prototype. Current encode/decode also do not reject all three forbidden keys recursively.

- [ ] **Step 3: Implement**

In `src/backend-contract/serializable.ts` add and use only these:

```ts
const FORBIDDEN_SERIALIZABLE_KEYS = new Set<string>(['__proto__', 'constructor', 'prototype'])

export function assertAllowedSerializableKey(key: string, domain: 'boundary' | 'ipc', operation: string): void {
  if (FORBIDDEN_SERIALIZABLE_KEYS.has(key)) {
    throw contractError('protocol.malformed', domain, operation)
  }
}

export function assertSafeSerializablePrototype(value: object, domain: 'boundary' | 'ipc', operation: string): void {
  const prototype = Object.getPrototypeOf(value)
  if (prototype !== Object.prototype && prototype !== null) {
    throw contractError('protocol.malformed', domain, operation)
  }
}

export function createOwnedSerializableRecord<Value>(): Record<string, Value> {
  return Object.create(null)
}

export function setOwnedSerializableEntry<Value>(
  target: Record<string, Value>,
  key: string,
  value: Value,
  domain: 'boundary' | 'ipc',
  operation: string
): void {
  assertAllowedSerializableKey(key, domain, operation)
  target[key] = value
}
```

`snapshotRecord` must call `assertSafeSerializablePrototype(record, 'boundary', 'serializable.snapshot.prototype')`, use `createOwnedSerializableRecord<SerializableValue>()`, and call `setOwnedSerializableEntry(..., 'boundary', 'serializable.forbidden-key')`. Freeze the result as today.

`encodeTauriWireValue` / `decodeTauriWireValue` must call the same helpers with domain `ipc`, use `createOwnedSerializableRecord<unknown>()`, and use operations `tauri.transport.forbidden-key` and `tauri.transport.prototype`. Keep `TAURI_BYTES_WIRE_TAG` validation before generic key copy. Do not assign onto `{}` and do not add a type assertion.

- [ ] **Step 4: GREEN** same Jest command as Step 2. `Object.prototype` must be unmodified after the suite.

- [ ] **Step 5: Commit** `fix: reject prototype-mutating keys in serializable clone paths (#58)`

**Phase A1 gate:**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/serializable-record-clone.test.js \
  __tests__/TauriTransport.test.js
```

---

# Phase A2 — stream close and public cleanup (#75, #59, #60)

### Task A2.1: #75 shared cleanup collector

**Files:**

- Modify: `src/public/error-bridge.ts`
- Modify: `src/public/ble-manager.ts` `ScanSession.stop()` and `BleManager.destroy()`
- Modify: `__tests__/public-scan-query.test.js` for both stop and manager-destroy collector cases

- [ ] **Step 1: RED tests named**

- `scan stop preserves native release-failed when view close throws`
- `manager destroy preserves native release-failed when scan-view close throws`
- `native release-failed without a thrown local phase is returned unchanged`
- `all released phases return released without an AggregateError`

When view close throws and `session.stop()` / `internal.destroy()` returns `{ state: 'release-failed', failures }`, the thrown value is `AggregateError` whose `errors` include both the view error and a `BleCleanupError` wrapping that record.

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/public-scan-query.test.js
```

Expected: current `stop()` throws `AggregateError([controllerError])` and drops the native record.

- [ ] **Step 2: Implement**

Export from `src/public/error-bridge.ts`:

```ts
type CleanupResultLike = Pick<CleanupRecord, 'state' | 'failures'>

export function collectCleanupPhases(
  results: readonly { readonly error?: unknown; readonly cleanup?: CleanupResultLike }[]
): CleanupRecord {
  const thrown: unknown[] = []
  const cleanupFailures: CleanupRecord['failures'][number][] = []
  for (const result of results) {
    if (result.error !== undefined) thrown.push(result.error)
    if (result.cleanup?.state === 'release-failed') cleanupFailures.push(...result.cleanup.failures)
  }
  const cleanup: CleanupRecord =
    cleanupFailures.length === 0
      ? { state: 'released', failures: [] }
      : { state: 'release-failed', failures: cleanupFailures }
  if (thrown.length === 0) return cleanup
  const cleanupError = resolvedCleanupFailure(cleanup)
  throw new AggregateError(cleanupError === null ? thrown : [...thrown, cleanupError], 'BLE cleanup failed')
}
```

Keep `resolvedCleanupFailure` file-private and call it only from `runWithCleanup` and `collectCleanupPhases`. `ScanSession.stop` and `BleManager.destroy` must pass every thrown phase error and every resolved cleanup record to `collectCleanupPhases`. With no thrown phase, preserve the cleanup-record contract by returning `release-failed`; when another phase threw, raise one aggregate containing that error plus a `BleCleanupError` for the native record. The caller's lifecycle state machine, not the collector, retains unresolved ownership for retry.

- [ ] **Step 3: GREEN + commit** `fix: preserve native release-failed records in public cleanup (#75)`

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/public-scan-query.test.js
```

---

### Task A2.2: #59 owner close after finish

**Files:**

- Modify: `src/core/bounded-stream.ts`
- Create: `__tests__/core/bounded-stream-close-after-finish.test.js`

- [ ] **Step 1: RED tests named**

- `finish then close discards queued values and yields the close reason`
- `finish without close still drains queued values before terminal`
- `second close after owner close is a no-op`
- `close after finish zeros retained value and payload bytes`
- `close after finish clears pending overflow accounting exactly once`
- `pending reader and concurrent close observe one terminal and no value`
- `finish after owner close cannot resurrect draining state`

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/core/bounded-stream-close-after-finish.test.js
```

Expected: first test fails because `closeWithTerminal` returns early on `isTerminal()` after `finishWithReason`.

- [ ] **Step 2: Implement**

Add `private ownerClosed = false`. `finishWithReason` sets `terminalNotice` and does **not** set `ownerClosed`. `closeWithTerminal` returns only when `ownerClosed` or `terminalDelivered` is already true. Otherwise it escalates both `open` and `finishing` to owner-close: discard `values`, zero retained value/payload bytes, clear the pending overflow notice, apply the requested exact-zero policy, set `ownerClosed = true`, replace `terminalNotice` with the **close** reason, and flush consumers. `finishWithReason` is a no-op when `ownerClosed`, `terminalNotice !== null`, or `terminalDelivered`. `isTerminal()` remains true when a terminal is selected so `emit` after finish still rejects new values.

- [ ] **Step 3: GREEN + commit** `fix: honor owner close after stream finish (#59)`

---

### Task A2.3: #60 stop-after-resolve and emit overflow

**Files:**

- Modify: `src/public/ble-manager.ts` `PublicScanSessionController`, `PublicScanEventBroadcast`, `PublicBleManager.scan()`
- Modify: `__tests__/public-scan-query.test.js`

Depends on A2.1 and A2.2.

- [ ] **Step 1: RED tests named**

- `explicit stop does not deliver queued observations or discovery events after it resolves`
- `local observation overflow stops source consumption`
- `overflow stops the native scan session exactly once`
- `observation and discovery streams terminate together on overflow`
- `overflow clears timers, fingerprints, presence, iterator, and manager ownership`
- `overflow native stop release-failed remains retryable through a later stop/destroy`
- `concurrent explicit stop and overflow share one native stop attempt`
- `iterator return failure remains owned and is retried without repeating native success`
- `native stop success is not repeated when view cleanup needs retry`

Drive overflow with a tiny `itemCapacity` / `overflowPolicy: 'error'` delivery budget, then emit more observations than capacity.

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/public-scan-query.test.js
```

Expected: `accept()` ignores `emit().terminated` so the pump keeps consuming; `close()` uses `finishWithReason` so queued values drain after `stop()`.

- [ ] **Step 2: Implement**

1. `PublicScanEventBroadcast.close` calls `subscriber.closeWithReason(reason)`, not `finishWithReason`.
2. Split `PublicScanSessionController.closeView(reason)` into retryable phases: close both public streams once; return the source iterator; clear timers, presence, and fingerprints. Retain the iterator until `return()` succeeds, so a rejected return remains retryable. The method returns a real `CleanupRecord` and never marks a failed phase released.
3. In `PublicBleManager.scan()`, create one `stopScan(reason)` state machine shared by `publicSession.stop`, controller overflow callback, and manager `destroy`. It tracks `viewReleased`, `nativeReleased`, and one in-flight `stopPromise`. Each run attempts only unresolved phases, calls `collectCleanupPhases`, resets `stopPromise` after failure, and deletes `activeScan` only when both phases are released. A successful native `session.stop()` is never called again merely because view cleanup needs retry, and vice versa.
4. Add `pendingCleanupError: unknown | null` to `activeScan`. Pass an overflow callback that invokes `stopScan('overflow')` once and assigns any rejection to `pendingCleanupError` while marking scan state failed. The next explicit `stop()`/`destroy()` includes `pendingCleanupError` in its result, clears it only after all unresolved cleanup phases release, and never launches a second concurrent cleanup promise.
5. Change `PublicScanEventBroadcast.emit` to return `boolean` (`true` if any subscriber `emit().terminated`). `accept()`:

```ts
const observationPush = this.observationStream.emit(observation, bytes)
const eventTerminated = this.eventBroadcast.emit(
  Object.freeze({ kind: 'observed', peer: observation.peer }),
  eventBytes
)
if (observationPush.terminated || eventTerminated) {
  void this.terminateFromOverflow()
}
```

`terminateFromOverflow` sets ingress closed synchronously, closes observation and discovery streams with `overflow`, and invokes the injected shared `stopScan('overflow')` callback once. The shared state machine performs iterator return and native stop. Pump loop stops on the ingress-closed flag.

6. `publicSession.stop()` and manager `destroy()` both await `stopScan`. They expose every retained view/native failure and retry only the unresolved phase. Scan-state closure and active-manager removal occur only after complete release.

- [ ] **Step 3: GREEN + commit** `fix: stop public scans on owner close and local overflow (#60)`

**Phase A2 gate:**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/core/bounded-stream-close-after-finish.test.js \
  __tests__/public-scan-query.test.js
```

---

# Phase A3 — IPC ownership (#72 #63 #73 #76 #74 #79)

### Task A3.1: #72 tombstone must not keep a live sink

**Files:**

- Modify: `src/ipc/manager.ts` `registerStream()`
- Modify: `__tests__/ipc/pending-stream-bounds.test.js`

- [ ] **Step 1: RED** test named `tombstone registration does not retain the sink in the active map`

After register with a terminal tombstone, `inspectIpcPendingStreamAccountingForTests` / a test-only inspect of active sinks must not list that handle. A later event for that id must not be delivered.

Also add:

- `repeated tombstone registration keeps active pending item and byte counts bounded`
- `tombstone owner cleanup runs once and retains release failure for destroy`
- `destroy after tombstone returns all IPC stream accounting to zero`
- `non-evicted early events remain ordered and lossless`

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/ipc/pending-stream-bounds.test.js
```

- [ ] **Step 2: Implement** Consume the tombstone before publishing the sink. On tombstone hit, call `source.closeWithReason(tombstone.reason)`, call `onTerminal`, and return without ever calling `this.streams.set(handle, sink)`. Do not expose a transient active sink to synchronous terminal callbacks.

- [ ] **Step 3: GREEN + commit** `fix: drop IPC sinks when pending tombstones terminalize (#72)`

---

### Task A3.2: #63 event pump terminates children

**Files:**

- Modify: `src/ipc/manager.ts` `pumpEvents()`
- Create: `__tests__/ipc/event-pump-termination.test.js`

- [ ] **Step 1: RED tests named**

- `global terminal closes scan, notification, and lifecycle children`
- `natural completion is source-failed for all children`
- `malformed global event terminates children and does not leave an unobserved rejection`
- `pending pre-registration state is cleared and its terminal cause remains in the cleanup ledger`
- `route after pump death fails closed`
- `destroy after pump death is idempotent`
- `all child owner cleanups are attempted and failures are aggregated`
- `destroy retries only child cleanup phases that remain unresolved`

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/ipc/event-pump-termination.test.js
```

- [ ] **Step 2: Implement** `runEventPump` with `try/catch/finally`. On `event.kind === 'terminal'`, iterator end, or throw: snapshot sinks, `closeWithReason` each synchronously, invoke every `notifyOwnerTerminal` independently, clear pending maps with exact accounting, and mark manager unusable so `assertActive` fails. Store pump cause plus each owner-cleanup failure in a manager cleanup ledger consumed by `destroy`; do not let one owner failure prevent the other sinks from terminalizing. Attach an observation handler when the pump is created so its promise can never reject unobserved. `destroy` retains and retries unresolved owner cleanup rather than clearing the ledger on failure.

- [ ] **Step 3: GREEN + commit** `fix: terminate IPC child streams when the global event pump dies (#63)`

---

### Task A3.3: #73 and #76 provisional admission

**Files:**

- Modify: `src/ipc/manager.ts` `connect()`, `admitConnectionEvents()`
- Create: `__tests__/ipc/provisional-admission.test.js`

- [ ] **Step 1: RED tests named**

- `mismatched connect identity still disconnects the host handle`
- `missing connectionId still disconnects using the returned handle and remaining host identity`
- `missing connectionGeneration still disconnects using the returned handle and remaining host identity`
- `missing handle fails closed and manager destroy releases the host lease resources`
- `malformed events subscribe still unsubscribes`
- `unsubscribe release-failed is preserved on admission failure`
- `failed provisional cleanup is retried by connection release or manager destroy`
- `valid connect publishes one connection without compensating disconnect`
- `successful compensation and destroy return provisional resource counters to zero`
- `Electron and Tauri transport doubles exercise the same admission helper`

Never `.catch(() => undefined)` on compensation.

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/ipc/provisional-admission.test.js
```

- [ ] **Step 2: Implement** Decode the connect response into a provisional record that preserves every valid host-issued field without publishing an `IpcConnection`. When `handle`, `peerId`, `ownerLeaseId`, `connectionId`, and `connectionGeneration` are present, compensate validation failure with `connection.disconnect` using those host values, never the requested values. When the response omits a field required by `connection.disconnect`, record an unresolved provisional connection on the manager so `destroy()` releases the whole renderer lease; do not fabricate an identity. Aggregate the admission error with thrown or resolved `release-failed` compensation. Apply the same provisional-owner record to events subscribe/ready: failed admission always attempts unsubscribe, retains failed cleanup, and lets connection release/manager destroy retry it.

- [ ] **Step 3: GREEN + commit** `fix: compensate IPC connect and lifecycle-admission failures (#73 #76)`

---

### Task A3.4: #74 expired deadlines fail before dispatch

**Files:**

- Modify: `src/ipc/manager.ts` `route()`
- Modify: `__tests__/ipc/provisional-admission.test.js` add deadline cases

- [ ] **Step 1: RED tests named**

- `expired connect deadline does not invoke the transport`
- `expired scan.start deadline does not invoke the transport`
- `expired gatt.discover and gatt.subscribe deadlines do not invoke the transport`
- `already-aborted signal does not invoke the transport`
- `future deadline still dispatches`
- `deadline expiring after dispatch compensates a resource-bearing success`
- `Electron and Tauri transport doubles share the pre-dispatch deadline guard`

Count `transport.invoke` calls. Error code `operation.timed-out`.

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/ipc/provisional-admission.test.js
```

- [ ] **Step 2: Implement** At the start of `route()`, reject an already-aborted caller signal with `operation.aborted` before correlation allocation or `client.request`. After validating a present `deadline` is finite and **before** `client.request`:

```ts
if (deadline <= globalThis.performance.now()) {
  throw contractError('operation.timed-out', 'ipc', `ipc-manager.${command}`)
}
```

Keep the timer only for a still-future deadline. `Math.max(0, ...)` is not admission.

- [ ] **Step 3: GREEN + commit** `fix: reject expired IPC deadlines before host dispatch (#74)`

---

### Task A3.5: #79 hung admission cannot deadlock disconnect

**Files:**

- Modify: `src/ipc/manager.ts` `disconnectInternal()`, `ensureLifecycleAdmission()`, `admitConnectionEvents()`
- Modify: `__tests__/ipc/connection-cleanup.test.js`

- [ ] **Step 1: RED tests named**

- `release completes while connection.events.subscribe never settles`
- `connection.disconnect is still routed`
- `late subscribe success is unsubscribed and does not resurrect the connection`
- `concurrent release calls share one teardown`
- `manager destroy cannot hang behind lifecycle admission`
- `late admission compensation and destroy return connection-event counters to zero`
- existing #56 tests still pass

Use a transport whose `connection.events.subscribe` is controlled by a deferred promise. Use Jest fake timers and explicit microtask flushing; do not use a wall-clock timeout as correctness evidence.

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/ipc/connection-cleanup.test.js
```

- [ ] **Step 2: Implement** Do **not** `await this.lifecycleAdmission` unbounded. On disconnect: abort the admission signal (add an `AbortController` owned by the connection), attempt physical disconnect, record admission as failed/cancelled if still pending. If subscribe later succeeds, unsubscribe immediately (quarantine). `disconnectResult` sharing unchanged.

- [ ] **Step 3: GREEN + commit** `fix: bound IPC lifecycle admission during disconnect (#79)`

**Phase A3 gate:**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/ipc/pending-stream-bounds.test.js \
  __tests__/ipc/event-pump-termination.test.js \
  __tests__/ipc/provisional-admission.test.js \
  __tests__/ipc/connection-cleanup.test.js
```

---

# Phase A4 — Web and Tauri (#67 #77 #78)

### Task A4.1: #67 Web disconnect independence

**Files:**

- Modify: `src/web/web-bluetooth-backend.ts` `disconnectRecord()`
- Modify: `__tests__/web/web-bluetooth-lifecycle-hardening.test.js`

- [ ] **Step 1: RED tests named**

- `subscription release-failed still calls gatt.disconnect`
- `subscription throw still calls gatt.disconnect`
- `both failures are preserved`
- `gatt.disconnect failure after successful subscription cleanup is still reported`
- `every subscription cleanup is attempted before the result is assembled`
- `retry repeats only unresolved subscription or physical-disconnect phases`
- `local connection and database generations invalidate at terminal disconnect`
- `concurrent release and remote disconnect share one terminal outcome`
- `backend destroy retries unresolved phases and removes listeners and counters`

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/web/web-bluetooth-lifecycle-hardening.test.js
```

- [ ] **Step 2: Implement** Extend the existing `WebConnectionRecord` cleanup state with independently memoized subscription and physical-disconnect phases. Attempt every subscription cleanup, then attempt `record.device.gatt.disconnect()` regardless of earlier failures. Reuse Plan A's cleanup-record merge semantics locally in the backend without importing the public error bridge. Mark each successful phase released so retry repeats only unresolved work; invalidate connection/database generations at the first terminal disconnect transition. Remote-disconnect, explicit release, and backend destroy enter the same in-flight cleanup promise and retain failed phases plus listener ownership until release succeeds.

- [ ] **Step 3: GREEN + commit** `fix: disconnect Web Bluetooth even when subscription cleanup fails (#67)`

---

### Task A4.2: #77 Tauri completed-correlation replay

**Files:**

- Modify: `native/tauri/src/btleplug_dispatcher.rs`
- Modify: `native/tauri/Cargo.toml`
- Test: `#[cfg(test)]` module in that file (Cargo)

- [ ] **Step 1: RED** tests in Rust named:

- `completed_correlation_replay_is_protocol_violation`
- `completed_scan_and_subscribe_correlations_are_also_rejected`
- `in_flight_duplicate_correlation_is_still_rejected`
- `new_correlation_on_same_lease_succeeds`
- `replay_set_cleared_on_lease_drop`
- `expired completed correlation leaves the replay window after 30 seconds`
- `full replay window rejects new work without evicting a live tombstone`
- `in-flight plus completed correlations never exceed 256`

```sh
cargo test --manifest-path native/tauri/Cargo.toml completed_correlation_replay_is_protocol_violation -- --nocapture
```

Expected: second completed `c1` currently executes again because `operations.remove` already ran.

- [ ] **Step 2: Implement** Add Tokio's `test-util` feature under `[dev-dependencies]` in `native/tauri/Cargo.toml`. Add `completed_correlations: HashMap<String, Instant>` to each `CallerState`. Before route admission, prune entries older than 30 seconds, reject a correlation present in either `operations` or `completed_correlations`, and reject new work when `operations.len() + completed_correlations.len() >= 256`; never evict an unexpired tombstone to admit work. After every successful or failed non-cancel route completion, remove the in-flight entry and insert its completion time only when the same lease still owns the caller. Lease release drops both maps. Use Tokio paused time/advance in tests so expiry is deterministic.

- [ ] **Step 3: GREEN**

```sh
cargo fmt --manifest-path native/tauri/Cargo.toml -- --check
cargo test --manifest-path native/tauri/Cargo.toml
cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings
```

Commit: `fix: reject completed Tauri correlation replay (#77)`

---

### Task A4.3: #78 bound cancelled-success retries

**Files:**

- Modify: `native/tauri/src/btleplug_dispatcher.rs` `quarantine_cancelled_success`, `retry_quarantined_cleanup`

- [ ] **Step 1: RED** tests named:

- `persistent_cleanup_failure_stops_after_eight_attempts`
- `repeated_cancelled_success_for_same_handle_coalesces_to_one_worker`
- `lease_drop_cancels_quarantine_workers`
- `worker_count_per_lease_capped_at_four`
- `fifth distinct cleanup waits in the bounded queue rather than disappearing`
- `exhausted cleanup appears in release-failed and is retried by release`
- `ownership_denied_stops_retry_without_resurrecting_the_resource`

```sh
cargo test --manifest-path native/tauri/Cargo.toml persistent_cleanup_failure_stops_after_eight_attempts
```

- [ ] **Step 2: Implement** Add one `QuarantineScheduler` to `CallerState`: a deduplicating `HashSet<(command, handle)>`, a FIFO queue capped by the caller's maximum owned resource count, four worker permits, per-resource attempt count capped at eight, and a cancellation token tied to lease release. Repeated cleanup for the same key updates the existing entry without adding a worker. A fifth distinct key remains queued. On exhaustion, remove the active/queued key and append a normalized cleanup failure to `caller.quarantine_failures`; keep the underlying resource in the caller's ordinary scan/connection/subscription registry. `settle_caller()` cancels the scheduler, drains queued work, starts its failure list from `quarantine_failures`, and retries those ordinary resource registries. Clear a recorded quarantine failure only after that resource release succeeds.

- [ ] **Step 3: GREEN** full Cargo gate below. Commit: `fix: bound Tauri quarantined cleanup retries (#78)`

**Phase A4 gate:**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/web/web-bluetooth-lifecycle-hardening.test.js
cargo fmt --manifest-path native/tauri/Cargo.toml -- --check
cargo test --manifest-path native/tauri/Cargo.toml
cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings
cargo check --manifest-path example-tauri/src-tauri/Cargo.toml
```

---

# Plan A merge gate

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
cargo fmt --manifest-path native/tauri/Cargo.toml -- --check
cargo test --manifest-path native/tauri/Cargo.toml
cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings
cargo check --manifest-path example-tauri/src-tauri/Cargo.toml
```

- [ ] All hosted PR jobs that run are green
- [ ] Close A trackers on merge
- [ ] Do not start Plan B until this PR is on `main`
