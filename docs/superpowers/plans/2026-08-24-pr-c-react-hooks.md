# Plan C — React hooks and adapter store

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Start only after Plan B is merged to `main`.

**Goal:** React adapter-state subscription cannot wedge or double-own a watch, `useDiscoveredPeers` remains explicitly bounded and honors optional lost-peer events, and connection/characteristic hooks leave loading with a fail-visible terminal outcome and exact cleanup.

**Architecture:** Fix `ManagerStore` ownership first. Put scan-retention constants and byte accounting in one package-internal policy module shared by the public scan controller and React. Treat `ScanSession.events` as optional: events own presence when available, while observations remain the overflow/rich-data source and the fallback when events are absent. Preserve abnormal connection terminal causes through the public projection, then give both terminal hooks one generation-guarded cleanup path.

**Tech Stack:** TypeScript, React external-store/effect hooks, Jest. No native radio.

**Spec:** [Master plan](./2026-08-24-lifecycle-correctness-master.md)

**Base:** `origin/main` after Plan B merge

**Locked designs:**

1. `#62` lifecycle states are `idle | starting | active | stopping | cleanup-failed`. Each run owns its creation promise, resolved watch, consumption promise, one stop attempt, cleanup record/error, and generation. Creation rejection owns no resource: publish the error, clear the run, and permit one retry only when a later subscriber calls `ensureWatch`. A cleanup failure retains the run; a later subscriber retries that same watch's stop and starts a replacement only after `released`.
2. `#65` uses package-internal limits of **256 peers** and **262144 retained bytes**. Create `src/public/scan-state-budget.ts` with these constants plus `estimatePublicPeerRetentionBytes(peer)`. Both `src/public/ble-manager.ts` and `src/react.ts` import the constants; the new module is not re-exported by `src/index.ts`, `src/advanced.ts`, or a host entrypoint.
3. When `session.events` exists, discovery events are authoritative for presence: `observed` upserts and refreshes insertion order, `lost` removes. Observations are consumed concurrently only for overflow and richer peer refresh, without a second presence insertion. When events are absent, observation values are the fallback presence source. One cleanup state machine returns both iterators and shares one `session.stop()` attempt; successful phases are memoized and only unresolved failures are retried.
4. Every natural connection-iterator end without an explicit expected terminal is `stream.closed`, even after values were received; keep the last state but set an error. Preserve expected `closed`/`owner-released` completion in a package-internal WeakMap keyed by the public lifecycle iterable so React can distinguish it from an arbitrary iterable's natural end without adding a public field.
5. Characteristic expected terminal reasons `closed` and `owner-released` end without error. Map `overflow`, `source-failed`, `connection-lost`, `service-changed`, `operation-aborted`, and `operation-timed-out` to typed public errors. Subscription removal has one in-flight attempt; only `released` marks it complete. Rejection/`release-failed` is reported and remains retryable by effect cleanup or manager destroy.
6. Effect-local generation tokens guard every async state update and cleanup continuation. Replacement and StrictMode cleanup from an older run cannot update the current result.

Do not add public hook result fields. Reuse `loading`, `error`, `state`, `value`, and `peers`.

---

## Trackers

| Issue                                                                | Title                                                                            |
| -------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [#62](https://github.com/sfourdrinier/unified-ble-manager/issues/62) | adapter-state store wedges or replaces a watch before failed cleanup is released |
| [#65](https://github.com/sfourdrinier/unified-ble-manager/issues/65) | `useDiscoveredPeers` unbounded; ignores lost events                              |
| [#66](https://github.com/sfourdrinier/unified-ble-manager/issues/66) | hooks stay loading after terminal completion                                     |

---

## File map

- Modify: `src/react.ts`
- Create: `src/public/scan-state-budget.ts`
- Modify: `src/public/ble-manager.ts`
- Modify: `__tests__/react.test.js`
- Modify: `__tests__/public-scan-query.test.js` for unchanged public scan budget/accounting

---

### Task C1: #62 adapter-store watch lifecycle

**Files:**

- Modify: `src/react.ts` `ManagerStore`
- Modify: `__tests__/react.test.js`

- [ ] **Step 1: Write RED tests**

Add these tests beside the existing adapter-watch tests:

- `watchState rejection clears the resource-free run and a later subscriber retries`
- `source terminal stops the owned watch before replacement`
- `unexpected watch iterator end stops the owned watch before replacement`
- `release-failed stop retains the old run and blocks replacement`
- `later subscriber retries failed cleanup before creating a watch`
- `manual unsubscribe and terminal race share one stop attempt`
- `StrictMode and rapid remount never own two watches`
- `adapter readiness and capability snapshots resume after recovery`
- `final unmount leaves zero React-owned watch runs`

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/react.test.js
```

Expected: creation rejection leaves `watchRun` wedged; terminal/end does not stop and clear ownership; `settleCleanup` hides `release-failed` and permits replacement.

- [ ] **Step 2: Implement the explicit run state**

Replace the current boolean `WatchRun` fields with `phase`, `generation`, `creation`, `watch`, `consumption`, `stopAttempt`, and `cleanupFailure`. `ensureWatch()` follows this table:

| Existing phase        | Action                                                    |
| --------------------- | --------------------------------------------------------- |
| `idle` / no run       | create one `starting` run                                 |
| `starting` / `active` | share it                                                  |
| `stopping`            | wait for its stop attempt; start only after `released`    |
| `cleanup-failed`      | retry that run's `watch.stop()`; never create in parallel |

On creation rejection, publish the error and remove the resource-free run. Do not loop automatically; a later subscription calls `ensureWatch()` again. On terminal/end, transition to `stopping` and call one `stopRun(run)`. `stopRun` returns the real `CleanupRecord`: only `released` removes the run; rejection/`release-failed` records `cleanup-failed`, reports it, and clears only `stopAttempt` so retry is possible. Guard callbacks with `this.watchRun === run` and `generation`.

- [ ] **Step 3: Run GREEN and commit**

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/react.test.js
```

Commit: `fix: unwedge React adapter-state watch ownership (#62)`

---

### Task C2: #65 bounded discovered peers with optional events

**Files:**

- Create: `src/public/scan-state-budget.ts`
- Modify: `src/public/ble-manager.ts`
- Modify: `src/react.ts` `useDiscoveredPeers()`
- Modify: `__tests__/react.test.js`
- Modify: `__tests__/public-scan-query.test.js`

- [ ] **Step 1: Write RED tests**

- `lost discovery event removes the peer when events are present`
- `observed discovery event refreshes one peer without duplication`
- `observations provide presence when events are absent`
- `observation values do not double-insert when events are present`
- `peer map evicts oldest observation at 256 entries`
- `peer map evicts oldest observation above 256 KiB`
- `cap eviction sets stream.overflow while scan remains active`
- `options change manager replacement and unmount clear retained state`
- `observation and optional event iterators are returned exactly once`
- `session stop is attempted exactly once after both iterator returns`
- `iterator-return and session-stop failures are all reported`
- `per-update array length never exceeds 256`

Use controllable async iterators and run the matrix with `events` present and `events: undefined`.

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/react.test.js \
  __tests__/public-scan-query.test.js
```

Expected: the current hook retains every observation, never consumes lost events, and has no item/byte budget.

- [ ] **Step 2: Add the shared internal budget authority**

Create `src/public/scan-state-budget.ts`:

```ts
export const MAX_PUBLIC_SCAN_STATE_ENTRIES = 256
export const MAX_PUBLIC_SCAN_STATE_BYTES = 256 * 1024

export function estimatePublicPeerRetentionBytes(peer: {
  readonly id: string
  readonly name: string | null
  readonly lastAdvertisement?: {
    readonly manufacturerData?: readonly { readonly data: Readonly<Uint8Array> }[] | null
    readonly serviceData?: readonly { readonly data: Readonly<Uint8Array> }[] | null
  } | null
}): number {
  let bytes = 64 + peer.id.length * 2 + (peer.name?.length ?? 0) * 2
  for (const entry of peer.lastAdvertisement?.manufacturerData ?? []) bytes += entry.data.byteLength
  for (const entry of peer.lastAdvertisement?.serviceData ?? []) bytes += entry.data.byteLength
  return bytes
}
```

Import the two constants into `public/ble-manager.ts` without exporting them from that module. Import all three symbols directly into `react.ts`. Keep existing public scan accounting green; do not add a package export.

- [ ] **Step 3: Implement one presence map and one cleanup state machine**

Store `Map<string, { peer: BlePeer; bytes: number }>` plus `retainedBytes`. Upsert deletes an existing entry before re-inserting it so map order is last-observation order, subtracts old bytes, adds `estimatePublicPeerRetentionBytes`, and evicts oldest entries until both caps hold. Each eviction sets one `streamOverflowError('react.useDiscoveredPeers.cap')`. Lost removes the entry and subtracts its bytes.

Create explicit observation and optional-event iterators. With events present, events perform presence mutations; observation values may refresh the stored peer only when that peer already exists and never insert a missing peer. With events absent, observation values upsert. Observation overflow and event iterator rejection set the result error without fabricating completeness.

Create `stopRun()` with one in-flight promise. It independently attempts observation iterator `return()`, optional event iterator `return()`, and `session.stop()` so one failure cannot skip another. It reports every failure, memoizes successful phases, and clears the map/bytes. Effect cleanup, options change, manager replacement, and natural/terminal completion call the same function. Guard all state updates with the run generation.

- [ ] **Step 4: Run GREEN and commit**

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/react.test.js \
  __tests__/public-scan-query.test.js
```

Commit: `fix: bound useDiscoveredPeers and drop lost peers (#65)`

---

### Task C3: #66 terminal hook outcomes and exact subscription removal

**Files:**

- Modify: `src/public/ble-manager.ts` `mapPublicConnectionEvents()` / `PublicConnectionEventBroadcast`
- Modify: `src/react.ts` `useConnectionState()`, `useCharacteristicValue()`
- Modify: `__tests__/react.test.js`

- [ ] **Step 1: Write RED tests**

- `connection done before a value sets stream.closed and loading false`
- `connection done after a value keeps the state and sets stream.closed`
- `expected owner-released completion clears loading without error`
- `connection source-failed terminal remains an error through public projection`
- `connection lost terminal keeps the last disconnected state and reports connection.lost`
- `replacement connection prevents stale terminal state updates`
- `characteristic expected owner close clears loading without error`
- `characteristic abnormal terminal maps to a typed error and keeps last value`
- `characteristic natural end sets stream.closed even after a value`
- `characteristic terminal and unmount share one remove attempt`
- `characteristic remove release-failed is reported and remains retryable`
- `replacement and StrictMode leave zero leaked iterators and subscriptions`

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/react.test.js
```

Expected: connection terminal causes are collapsed to `done`; both hooks can remain loading; characteristic cleanup waits for unmount and marks failed removal complete.

- [ ] **Step 2: Preserve abnormal connection terminal errors**

Add a private `ExpectedConnectionEventEnd` sentinel and a package-internal `WeakMap<AsyncIterable<BleConnectionEvent>, 'expected'>`. In `mapPublicConnectionEvents`, throw the sentinel for `closed`/`owner-released`; convert other terminal reasons to `contractError`: `overflow → stream.overflow`, `connection-lost → connection.lost`, `service-changed → gatt.stale-handle`, `operation-aborted → operation.aborted`, `operation-timed-out → operation.timed-out`, and `source-failed → stream.closed`. A raw underlying `done` throws `stream.closed`. `PublicConnectionEventBroadcast` catches the sentinel, records expected completion in the WeakMap, and closes subscribers normally; it retains other errors and rejects subscriber `next()` calls. Export only an internal-source function `connectionEventsEndedExpectedly(iterable)` from `public/ble-manager.ts` for `react.ts`; do not re-export it from a package entrypoint.

- [ ] **Step 3: Implement generation-guarded hook finalization**

For `useConnectionState`, a plain `done` checks `connectionEventsEndedExpectedly(connection.lifecycleEvents)`. Expected completion clears loading and keeps the last state without error. Any other `done` while a non-null connection is still the current generation sets `loading: false`, keeps the last state, and sets `stream.closed`; caught typed terminal errors do the same with their original error.

For `useCharacteristicValue`, map terminal reasons with the same table. `closed`/`owner-released` are expected; all other reasons set an error and retain the last value. Natural iterator completion without an explicit terminal always sets `stream.closed`, whether or not a value was seen.

Track `removeAttempt: Promise<CleanupRecord> | null` and `removeReleased`. Terminal, iterator end, replacement, and unmount call `removeSubscription()`. Return the same promise while active. Set `removeReleased` only on `{ state: 'released' }`; on rejection/`release-failed`, report the error, clear `removeAttempt`, and leave the subscription owned for the next cleanup call or manager destroy. Check the effect generation before every result update.

- [ ] **Step 4: Run GREEN and commit**

```sh
pnpm exec jest --config jest.config.js --runInBand __tests__/react.test.js
```

Commit: `fix: clear React hook loading when streams complete (#66)`

---

# Plan C merge gate

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/react.test.js \
  __tests__/public-scan-query.test.js
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

Hosted: all jobs that run must be green.

- [ ] Close C trackers on merge
- [ ] Master plan A/B/C checkboxes all checked
- [ ] Version bump / `4.0.3` prepare is a separate follow-up PR
