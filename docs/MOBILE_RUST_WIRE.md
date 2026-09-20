# Mobile Rust owner and wire `ubm-mobile-wire/1`

This document describes how React Native (Android and iOS) reaches the Rust
core. It is the frozen interface between four parts:

- the Rust owner: `crates/ubm-mobile`;
- the native facades: JNI `com.ubm.core.MobileCoreBridge` and the UniFFI
  `Mobile*` types in `bindings/uniffi/src/ubm_echo.udl`;
- the platform radio adapters: Kotlin over `OwnedAndroidGattRadio`, and Swift
  over `OwnedCoreBluetoothProtocolRadio`;
- the TypeScript codec: `src/backends/reactnative/rust-core-wire.ts`.

The schema exists in two places only, Rust and TS. Golden vectors bind the two
(see [Golden vectors](#golden-vectors)). Java and Swift never parse
JavaScript arguments.

## Ownership (one owner per process)

```
JS manager ─┐                          ┌─ PlatformRadio (Kotlin / Swift adapter)
JS manager ─┼─ MobileSession (lease) ─ MobileHost ─ DesktopCentral<ForeignRadio>
JS manager ─┘                          └─ WakeSink (JS wakeup)
```

- **`MobileHost`** is installed once per process. It holds one platform radio
  and one real core central, `DesktopCentral<ForeignRadio>`, which runs on the
  shared executor (`ubm_desktop::executor::desktop_runtime`). The host outlives
  every JS manager. This is what the following need:
  - iOS state restoration: the OS hands restored peripherals to the process's
    one `CBCentralManager`.
  - Android foreground-service and companion leases.
  - Several managers sharing the platform radio (one GATT owner, one
    CoreBluetooth delegate).
  - Background operation.
- **Identity.** <a id="identity"></a>The shared central names no host: the
  owner that opens it supplies its identity (`ubm_desktop::HostIdentity`,
  `crates/ubm-desktop/src/identity.rs`). `MobileHost` passes
  `MobileIdentity` (`crates/ubm-mobile/src/identity.rs`), which names every
  scope in the formats the legacy React Native backends published (origin/main
  `corebluetooth-attachment-lifecycle.ts` with the React Native identity
  options): backend and adapter generations `"1"` (then `"2"`… per adapter
  reset), backend instance `react-native-{android|apple}-backend-{n}`,
  attachment `{instance}:{backend gen}:{adapter gen}`, adapter
  `android-default-adapter` / `apple-corebluetooth-default-adapter`. Its own
  failures and log lines use `ubm-mobile.host` / `ubm-mobile`. No desktop
  name reaches the wire; `crates/ubm-mobile/tests/golden.rs` and
  `rust-core-wire.golden.test.js` fail on one. Only the generations cross the
  wire; the TypeScript backend names its own instance and attachment in the
  same legacy formats, and its public resource names (peer, scan, connection,
  lease, connection and database generations, database, subscription) in the
  legacy `corebluetooth-*` formats, mapping each public generation to the
  owner's generation that the wire carries (`cg-{n}`, `db-{n}`).
- **`MobileSession`** is one RN manager's lease on the host.
  - Everything it acquires on the shared central is namespaced by session id
    (`s{id}:` for leases and consumers): scan membership, connection leases
    and subscription consumers.
  - `session.dispose` releases exactly those resources.
- **Background leases belong to a background scope.**
  - On Android every session a React Native module instance opens shares
    that module's scope (`nativeOpenSession(…, backgroundScope)`). A
    foreground-service lease outlives the manager that acquired it, any
    session of the scope may update or release it, and module invalidation
    (`nativeReleaseBackgroundScope`) ends it, exactly as the legacy module
    held its leases until `invalidate()`. Another scope never reaches it
    (`ownership.denied`).
  - A session opened without a scope (UniFFI, tests) is its own scope, so
    `session.dispose` releases its background leases.
  - Host shutdown releases every scope. A failed release is reported and the
    lease stays held for a retry.
- **Scans are shared.**
  - One physical scan serves every member.
  - A member whose service or address filter the running scan does not cover
    restarts it once with the union filter. Any empty filter makes the union a
    broad scan.
  - Each member still receives only the advertisements that match its own
    filter.
  - Members must use equal Android scan settings. Otherwise the second start
    fails with `scan.already-active`.
- **Restoration.**
  - The adapter reports restored peers with `RadioIngress::Restored`.
  - `peers.restored` lists them, and a `restored` drain record announces them.
    Listing is a read: it never consumes anything.
  - `peers.claim-restored` hands the restored peers no other session has
    claimed to the asking session, once per process. This is the legacy
    rule: the legacy module cleared the process radio's restoration
    identifiers on the first successful adoption
    (`consumeRestorationPeerIdentifiers`). A later claim finds none, and so
    does a claim by another manager, even after the claimant was disposed.
    The TS journal adopts through this op. A second manager's adoption
    therefore replays the adapter record only, as a later legacy attachment
    did.
  - `connection.connect` on a restored peer id adopts it into a core
    connection: the adapter answers `Connect` at once from the live OS link.
  - An app that configures restoration only in its Info.plist gets its
    identity from `restorationIdentity('{}')` (the TurboModule answers the
    configured identity, or `null` when there is none; Android always
    answers `null`). No JS option is needed, as with the legacy module,
    which read the authority when it was initialised.
- **Shutdown** is explicit and belongs to the process owner:
  `nativeShutdownHost()` / `MobileCoreHost.shutdown()`.
  1. Every session is disposed.
  2. The central shuts down, and the radio `Close` is bounded by
     `LIVENESS_CLEANUP`.
  3. Every waiting request is failed.
  4. The cleanup record is returned.

The production surface has no path to `ubm-fake-radio`/`StagedDriver`. Three
tests guard this:

- `crates/ubm-mobile/tests/no_fake_radio.rs` runs `cargo tree`;
- `bindings/jni/src/mobile.rs` has a source guard;
- `bindings/uniffi/src/mobile.rs` has a source guard.

## Threading and wakeups

- **Rust → platform.** `PlatformRadio::submit` is called from executor
  threads. It must not block: post to the adapter's own thread or queue.
  Answer each request exactly once with `complete(requestId, completion)`.
  Answering synchronously inside `submit` is allowed.
- **Platform → Rust.**
  - `complete` and `ingest` never block. They push into bounded queues or
    oneshots.
  - A completion for a request that was cancelled or abandoned is counted in
    `lateRadioCompletions`. It is never silently discarded.
  - A completion with the wrong shape for its request fails that request with
    `protocol.malformed`.
- **Wake arming.**
  - While a session's outbox is empty and armed, the first record disarms it
    and calls `WakeSink.wake(sessionId)` once.
  - JS then drains with `drain(256, 65536)` until `more` is false and its
    backlog is empty (see Delivery).
  - A drain that empties the outbox re-arms it, then re-checks. A record that
    raced in makes the drain answer `more: true` and wakes no one, so no
    wakeup is lost.
  - An idle session costs zero calls and zero wakes (tested).
- **Delivery.** Data records (`adv`, `value`) reach JS one per native→JS
  task, as the legacy boundary delivered one native callback per record. A
  stream reader re-arms through several promise hops, so data records emitted
  in one synchronous loop overflow a `latest` (one-item) stream before its
  reader runs.
  - Taken records wait in a FIFO backlog. Each pass delivers the backlog up
    to, not including, its second data record. Control records keep their
    drained order and their neighbours: a `link` and the `stream-end`s behind
    it are delivered in one pass.
  - The boundary before the next pass is the next drain call, which takes one
    record (`drain(1, 65536)`) while a backlog remains, so the backlog never
    grows. A TurboModule promise resolves through the JS call invoker as a
    task of its own (bridgeless `RuntimeScheduler`), and it runs while the app
    is in the background.
  - No JS timer is a boundary. React Native 0.86 timers stop with the host:
    Android `JavaTimerManager` fires only on Choreographer frames and skips
    them while the host is paused; iOS `RCTTiming` runs on `CADisplayLink`,
    which it stops in the background. A `setTimeout(0)` boundary held
    notifications ~26 s with the screen off. Bridgeless `setImmediate` is a
    `queueMicrotask` shim, which is not a task boundary.
  - Cost: one extra drain call per data record after the first in a burst,
    the same count as legacy's one native event per record (an ECG stream at
    130 Hz is 130 calls/s).
  - Stopping the drain (destroy) runs after `session.dispose`; the owner keeps
    its outbox, and the router drains and delivers every record it still
    holds before it resolves. A value whose consumer was released meanwhile
    surfaces as the `unmatched-notification` diagnostic warning, never
    silently. A drain failure delivers the backlog before every stream ends
    with the failure.

## Envelope and codec

`invoke(op, argsJson, completion)` validates the arguments synchronously. Every
rejection happens before any effect. The completion receives exactly one of
these:

```json
{"ok":true,"value":…}
{"ok":false,"error":{"code","domain","operation","detail","platform"},"commit":null|"not-dispatched"|"uncertain","retryability":"never"|"caller-decides"}
```

- **`platform`** is the radio's own error identity, or `null` when the
  failure did not come from the radio: `{domain,code,message,metadata}`, with
  integer, text or boolean metadata values (finding 113). It is legacy React
  Native's identity (4.x `rn-android-boundary.ts` `nativeOperationFailure`):

  | Platform failure | `code` / `domain` | `platform` |
  |---|---|---|
  | Android GATT status, busy or other radio failure | `platform.failure` / `platform` | `{domain:"android", code:<legacy native code of the verb>, metadata:{androidGattStatus}}` (status only when the platform reported one) |
  | Link loss: Android `not-connected` or GATT status 19; Apple `not-connected` | `connection.lost` / `connection` | Android `{domain:"android", code:"connectionLost", …}`; Apple the `NSError` domain and code (owned radio 1016/1020, `CBErrorDomain` 7) |
  | Apple radio failure | `platform.failure` / `platform` | the `NSError` domain and decimal code, else `{domain:"corebluetooth", code:<legacy native code of the verb>}`; `metadata:{}` |
  | Refused for lack of security on a GATT operation: Android GATT 5/8/12/15/137, Apple `CBATTErrorDomain` 5/8/12/15 or `CBErrorDomain` 14/15 | `platform.security` / `platform` | as above |
  | A failed `connection.connect` (any radio failure above) | `connection.failed` / `connection` | as above |
  | A link operation cut off while the app's own release was underway | `operation.disconnected` / `connection` | as above, when the platform answered first |
  | Adapter state read failure | `adapter.unavailable` / `adapter` | as above |
  | cancel, permission, adapter state, stale path, unknown peer, unsupported | their contract codes | `null` |

  The security, connect and release rows are the one-name-per-event rules
  of 5.0 (`docs/UNIFIED_SEMANTICS.md`), applied by the core for mobile and
  desktop alike. The legacy native codes per verb are `RequestKind::android_native_code` and
  `apple_native_code` in `crates/ubm-mobile/src/radio.rs`, for example
  `readFailed`, `writeFailed`, `subscriptionFailed`, `connectionFailed`. The
  React Native provider hands `platform` on as the error's platform detail
  `{domain, code, safeMessage, metadata}`.

- **`commit`** is non-null for `gatt.write` and `gatt.write-descriptor` only:
  - `not-dispatched`: the platform never received the write, or the platform
    answered `Failed{dispatched:false}` (refused before sending, e.g. a full
    write-without-response queue or an oversize value).
  - `uncertain`: the write was submitted. This applies to an abort or timeout
    after submission, and to a GATT failure after dispatch.
- **`retryability`** is on every failure envelope: the owner's own answer
  (`DesktopError::retryability`), which the provider reports unchanged and
  never re-derives from the code. A write whose `commit` is `uncertain` is
  always `never`; the TS parser refuses an envelope that says otherwise.
  A `connection.connect` whose link the platform could not establish —
  Android GATT status 133, 62 (HCI 0x3E) or 147, CoreBluetooth
  `CBErrorDomain` 6 (`connectionTimeout`) or 10 (`connectionFailed`) — is
  `caller-decides` with the platform's answer kept (owner decision, 5.0;
  the same rule answers the desktop hosts, `is_transient_establishment_failure`
  in `crates/ubm-desktop/src/errors.rs`). The owner never retries it.
- **Bytes** travel as strict RFC 4648 §4 padded base64 in `…B64` fields.
  - Whitespace, the URL alphabet and non-zero pad bits are all rejected.
  - Length is checked before decoding: at most `4*ceil(524288/3)` characters,
    otherwise `bytes.too-large`.
  - Args text over 1 MiB is `bytes.too-large` before parsing.
- **Key sets are exact.** A missing or unknown key is `argument.invalid`.
- **Integers** are safe integers. Fractions are refused, never rounded.
- **Budgets** travel as a relative `budgetMs`, measured from the moment the
  owner received the call (fix-plan decision 2). `null` or absent means the
  liveness backstops apply: `LIVENESS_OP` 120 s, `LIVENESS_CLEANUP` 10 s and
  `LIVENESS_SCAN_START` 30 s. A `connection.connect` without a budget has no
  backstop (finding 112): like legacy Android `autoConnect` and a pending
  CoreBluetooth connect, it waits until the OS answers or it is cancelled.
  The same holds for the waits on the user or the OS (finding 123):
  `security.pair`, `security.cancel-pairing` and `companion.associate`.
  Legacy waited for the bond dialog, its cancellation and the companion
  chooser without a deadline.
  - Every other op keeps its backstop, because it is either an immediate
    platform readout or a link exchange that a link loss ends:
    `adapter.state`, `peers.bonded`, `security.state`, `background.*`,
    `connection.rssi`/`effective-mtu`/`request-mtu`/`request-priority`/`read-phy`/`request-phy`/`maximum-write-length`,
    `session.reconcile`, and the core's discover and GATT verbs.

## Op table

`selector` is
`{serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence}`.
Descriptor ops add `descriptorUuid` and `descriptorOccurrence`. Occurrences
are required. UUIDs are canonicalised by the core.

`operationId` is caller-minted. It registers before the first await, and
`op.cancel` targets exactly that operation.

Every invoke whose `operationId` names a new operation also carries
`admission`: a positive integer, strictly increasing per session in the
order the client sends invokes (legacy's dispatch epoch). `scan.stop`'s
`operationId` names a scan membership and carries none; `op.cancel` carries
its target's. An admission at or below the highest one the session has seen
is `argument.invalid`, and so is an `operationId` without one (or the
reverse). The owner consumes an admission when the invoke arrives, even if
it then refuses it. The keys are not listed again in each row below.

The "Δ vs spec §2" column marks where this table departs from the original
spec.

| op | args (`?` optional) | ok `value` | Δ vs spec §2 |
|---|---|---|---|
| `adapter.state` | `{}` | `{availability,authorization,power,safeReason,updatedAt,backendGeneration,adapterGeneration}`, read from the platform (`AdapterState` request). Generations are the core attachment's, named by the owner's [identity](#identity): `"1"` at open, as legacy React Native reported them. | — |
| `counters.describe` | `{}` | `{counters:{13 keys},native:{pendingRadioRequests,liveOps},process:{counters:{13 keys},native:{pendingRadioRequests,lateRadioCompletions,ingressDrops:{advertisement,notification,control},liveOps,connectSections}}}`. `counters`/`native` describe this session; `process` describes the whole owner. Answered after `session.dispose` too. | see [Counters](#counters) |
| `scan.start` | `{serviceUuids[],duplicatePolicy:"all",operationId,deviceAddresses?[],platform?{mode?,callbackType?,legacy?,phy?,reportDelayMs?},budgetMs?}` (`phy`/`reportDelayMs` → `capability.unsupported` `scan.start.platform-options` before any effect, as legacy) | `{operationId}`, the session's **membership** id (`s{n}-scan-{k}`) | `timeoutMs` → `budgetMs` (start budget). Adds `deviceAddresses` and Android `platform`: on Apple either one is `capability.unsupported`. `callbackType:"match-lost"` is `capability.unsupported`. `duplicatePolicy` other than `all` is `capability.unsupported` (first/merged are applied above the radio). |
| `scan.stop` | `{operationId,budgetMs?}` | cleanup record. A stop failure keeps the membership (retry). | unknown id → `lifecycle.invalid-state` (detail `scan-not-active`; the contract has no `scan.not-active` code) |
| `peers.resolve` | `{reference:{opaqueId,version?,backendId?,scope?}}` | peer record \| `null` | — |
| `peers.known` / `peers.connected` | `{}` | `[peer record]` | — |
| `peers.bonded` | `{operationId,budgetMs?}` | `[peer record]` (`source:"system-bonded"`) | **new** |
| `peers.restored` | `{}` | `[peer record]` (`source:"restored"`) | **new** |
| `peers.claim-restored` | `{maxPeers}` | `{peers:[peer record]}`: the restored peers no session claimed before, now claimed by this one (once per process). More unclaimed peers than `maxPeers` → `bytes.too-large` with nothing claimed. Android → `capability.unsupported` | **new** (PR210-52) |
| `connection.connect` | `{peerId,lease,operationId,budgetMs?,intent?:"direct"\|"when-available",transport?:"auto"\|"le",preferredPhy?:["le-1m"\|"le-2m"\|"le-coded"]}` | `{peerKey,connectionGeneration}` | `when-available` = Android `autoConnect`; Apple → unsupported. `preferredPhy` (PR210-54): on Android the link is established on those PHYs (`connectGatt(…, TRANSPORT_LE, phyMask)`, API 26+); it is refused with `capability.unsupported` (`connection.connect.preferred-phy`) before any effect on Apple, with `when-available` (Android ignores the connect PHY with `autoConnect`), and when the link is already up. The radio refuses API < 26 the same way. |
| `connection.disconnect` | `{peerId,lease,budgetMs?,operationId?}` | cleanup record. A failure answers `release-failed` and keeps the lease for retry. A lease an adapter loss ended answers `released` once, as legacy's adapter-loss cleanup left it. | — |
| `connection.rssi` | `{peerId,lease,operationId,budgetMs?}` | `{rssi}` (core-admitted: foreign lease → `ownership.denied`) | **new** |
| `connection.effective-mtu` | `{peerId,lease,operationId,budgetMs?}` | `{mtu\|null}` (Android) | **new**, Apple → unsupported |
| `connection.request-mtu` | `{peerId,lease,mtu:0..517,operationId,budgetMs?}` (below 23 goes to the platform, which refuses it: `platform.failure` `requestMtuFailed`, as legacy) | `{mtu}` | **new**, Apple → unsupported |
| `connection.request-priority` | `{peerId,lease,priority:"low-power"\|"balanced"\|"high-throughput",operationId,budgetMs?}` | `{accepted}` (dispatch acceptance only) | **new**, Apple → unsupported |
| `connection.read-phy` | `{peerId,lease,operationId,budgetMs?}` | `{tx,rx}` | **new**, Apple → unsupported |
| `connection.request-phy` | `{peerId,lease,operationId,tx?,rx?,budgetMs?}` (at least one, else `argument.invalid` in the `connection` domain, as legacy) | `{accepted,observation:{tx,rx}\|null}` | **new**, Apple → unsupported |
| `connection.maximum-write-length` | `{peerId,lease,mode:"with-response"\|"without-response",operationId,budgetMs?}` | `{maximumWriteLength}` (1..512): the platform's `ReadWriteLimits` answer for `mode`, bounded by the ATT maximum attribute value through the core's `connection_maximum_write_length`, the limit every `gatt.write` of that mode is admitted against. Core-admitted: foreign lease → `ownership.denied`; a withheld platform answer → `capability.unavailable`. No discovered database is needed. | **new** (5.0 `gatt:maximum-write-length`), both platforms |
| `security.state` | `{peerId,operationId?,budgetMs?}` | `{bond,encryption,authentication,secureConnections,pairingPossible}` | **new** |
| `security.pair` | `{peerId,transport:"auto"\|"le",operationId,budgetMs?}` | `{outcome:"paired"\|"already-paired"\|"rejected",state}`. `auto` on an already-bonded peer runs no ceremony (legacy rule). | **new** |
| `security.cancel-pairing` | `{peerId,operationId?,budgetMs?}` | `{state:"requested"}` | **new** |
| `gatt.discover` | `{peerId,lease,operationId,budgetMs?}` | `{connectionGeneration,databaseGeneration,services:[…]}` from the core's `discovered_paths` | The database registers whole or the discovery fails; nothing is skipped. A malformed platform UUID fails `protocol.malformed` (`discovery.snapshot.uuid`), a database past the ATT handle space `capability.limited` (`discovery.database-bound`), as the legacy backends failed |
| `gatt.read` | `{peerId,selector,operationId,budgetMs?}` | `{valueB64,provenance}` | no `lease` (core reads are not lease-scoped); `provenance` is the radio's own answer: `read-response` (Android `onCharacteristicRead`; CoreBluetooth while the characteristic cannot notify) or `read-or-notification` (CoreBluetooth while it can notify: the value may be a notification). A reply without it is `protocol.malformed` |
| `gatt.read-descriptor` | `{peerId,selector,operationId,budgetMs?}` | `{valueB64}` | no `lease` |
| `gatt.write` / `gatt.write-descriptor` | `+{valueB64,mode}` | `{commitState:"confirmed"}` with-response / `"unknown"` without | descriptor `without-response` → unsupported |
| `gatt.subscribe` | `{peerId,selector,consumer,operationId,deliveryMode?,budgetMs?}` | `{consumer,delivery:"notification"\|"indication"\|"unknown"}` | see [Delivery modes](#delivery-modes) |
| `gatt.unsubscribe` | `{peerId,selector,consumer,operationId,budgetMs?}` | `{state:"released",physicalDisabled}`. A failure keeps the consumer (L7). A consumer an adapter loss ended answers released once. | — |
| `background.acquire` | `{kind:"connected-device",reason,operationId?,budgetMs?}` | `{leaseId}` | **new** (replaces the legacy protocol-control `acquireBackground`) |
| `background.release` | `{leaseId,budgetMs?}` | cleanup record | **new** |
| `background.update-notification` | `{leaseId,title,body?,budgetMs?}` | `{state:"updated"}` | **new** |
| `companion.associate` | `{name?,serviceUuid?,operationId?,budgetMs?}` | `{source:"associated",associationId,peerId\|null,displayName\|null}` | **new**, Apple → unsupported |
| `op.cancel` | `{operationId,admission}` (the target's) | `{state:"cancellation-requested"\|"already-terminal"}` | see [Cancellation](#cancellation) |
| `session.reconcile` | `{}` | `{adapter,links,subscriptions,security,restored,scan}`, see [Control-record loss](#control-record-loss) | **new** |
| `session.dispose` | `{}` | cleanup record `{state:"released"\|"release-failed",failures:[{resourceKind,code,domain,operation,detail}]}` | `resourceKind` ∈ `scan`, `subscription`, `connection`, `background` (`background` only for a session that is its own background scope) |

`scan.take`, `notifications.take` and `events.take` are removed. Data flows
through `drain` only.

A **peer record** is
`{peerId,name,rssi,source,reachability,connection,bond,lastSeenAtMonotonicMs}`.
This extends spec §2 with `source`, `reachability` and `bond`, per
COORDINATION #3.

- `connection` is `connected` from the core, and `disconnected` for a
  terminal core connection.
- Otherwise `connection` is `unknown`.
- `reachability` is `reachable` only while connected.
- `bond` comes from the last security fact.

## Drain records

`drain(maxItems, maxBytes)` answers `{more, records, controlLost}`. Every
record carries an `ordinal` that increases monotonically within the session.
`controlLost` is the cumulative count of control records ever refused past
the control queue's cap (X-R5): a running total, not a per-call delta, so a
drain that keeps arriving behind data still exposes the gap within a bounded
number of drains, without waiting for the queues to empty.

| `t` | fields | Δ |
|---|---|---|
| `adv` | `peerId,localName,rssi,txPower,serviceUuids,manufacturerData[{companyId,payloadB64}],serviceData[{uuid,payloadB64}],connectable,solicitedServiceUuids,overflowServiceUuids,appearance,rawRecordB64,observedAtMs`. Empty collections → `null`; `connectable`, `appearance` (0..65535) and `rawRecordB64` are `null` when the platform does not report them. | adds `connectable`, `solicitedServiceUuids`, `overflowServiceUuids` (legacy CoreBluetooth advertisement parity), and `appearance`/`rawRecordB64` (legacy Android parity, PR210-69; CoreBluetooth reports neither) |
| `scan-end` | `operationId` (membership), `reason` | emitted with `source-failed` when the OS stops the scan or a widening restart fails |
| `value` | `consumer,valueB64,delivery` | — |
| `stream-end` | `consumer,reason:"overflow"\|"invalidated"\|"closed",droppedItems,droppedBytes` | — |
| `adapter` | `state` (as `adapter.state`) | from platform `AdapterState` ingress |
| `link` | `peerId,connectionGeneration,databaseGeneration\|null,reason:"local"\|"peer"\|"adapter"` | — |
| `db-changed` | `peerId,connectionGeneration,databaseGeneration` | `databaseGeneration` is the generation the change invalidated; the database is undiscovered until the next `gatt.discover` |
| `ingress-drop` | `class:"advertisement"\|"notification"\|"control",count` | consecutive drops of one class coalesce |
| `security` | `peerId,state` | **new** |
| `restored` | `peers:[{peerId,name,connected}]` | **new** |

Ordering per peer:

1. values that arrived before a lifecycle transition;
2. the transition record (`link` / `db-changed`);
3. the `stream-end` it caused.

The outbox has two bounded queues:

- **data** (`adv`, `value`): 2048 items and 4 MiB. On overflow, a value
  stream ends with `stream-end overflow`, and an advertisement counts in
  `ingress-drop`.
- **control**: 1024 items. On overflow, records count and surface as one
  `ingress-drop{class:"control"}` once the earlier records have drained.

### Control-record loss

A lost control record is re-read, never inferred. The React Native provider
watches the drain response's cumulative `controlLost` and calls
`session.reconcile` as soon as it increases — a prompt reconcile that does not
wait for the in-band `ingress-drop{class:"control"}` record to drain behind
whatever data is ahead of it. `session.reconcile` answers every fact a control
record carries from the owner's own state (the adapter is re-read from the
platform):

- `adapter`: the adapter state (as `adapter.state`).
- `links`: every link the owner holds connected
  `{peerId,connectionGeneration,state:"connected",reason:null,databaseGeneration,databaseChange,databaseState}`,
  where `databaseChange` is the generation the latest `db-changed` on this
  link invalidated (else `null`), plus the latest link end per peer
  `{…,state:"ended",reason,databaseGeneration,databaseChange:null,databaseState:null}`,
  which is what its `link` record said. A held generation the owner reports
  neither connected nor ended was ended by an earlier record that was also
  lost, for example before a reconnect under a new generation.
- `subscriptions`: this session's consumers, `{consumer,state:"live"}` or
  `{consumer,state:"ended",reason,droppedItems,droppedBytes}` (what its
  `stream-end` said). An ended consumer is kept until it is unsubscribed or
  the session is disposed.
- `security`: the last security report per peer (`{peerId,state}`).
- `restored`: the restored peers (as the `restored` record).
- `scan`: this session's scan membership, or `null`.

The React Native provider turns each difference into the transition the lost
record would have caused: `link` ended (`disconnected`/`connection-lost`),
database invalidation, `stream-end` with its reason and drop counts,
`bond-security-changed`, `restoration-received`, and `scan-end`
`source-failed`. A platform fact the ingress queue refused before the owner
saw it (a radio-side `ingress-drop`) is counted, but the owner cannot re-read
what it never received.

## Cancellation

- **Live id.** The op's `OpTicket` is cancelled through
  `DesktopCentral::cancel`.
  - Before core admission, the request is recorded and the op ends
    `operation.aborted` with no radio call.
  - After admission, exactly that core op is cancelled.
  - Dropping the radio wait calls `PlatformRadio.cancel(requestId)` exactly
    once.
  - An op the core queued behind another op on the same link is cancelled
    in the queue, so it never reaches the radio.
- **Exact classification (finding 109).** Without a live operation of that
  id and admission, the answer follows from the admission alone, with no
  memory of finished operations:
  - at or below the highest admission the session has seen: the operation
    was admitted and has ended, so the answer is `already-terminal`;
  - above it: the invoke has not arrived yet. The answer is
    `cancellation-requested`, and when that invoke arrives it fails
    `operation.aborted` with no effect. Pending cancels are kept only above
    the highest admission and dropped as it passes them.
  - An admission more than 65,536 (`ADMISSION_WINDOW`) above the highest is
    `argument.invalid`. A client assigns admissions in send order, so a
    cancel can only precede invokes it has already sent.
  - A live id named with a different admission is `argument.invalid`.
- **Client side (React Native provider).** It tracks every cancellable
  operation until it settles. A cancel of a settled operation answers
  `already-terminal` locally. One that has not been sent yet is refused
  locally when it would be sent: `operation.aborted`, and nothing reaches
  the owner. One that was sent is cancelled on the owner with its
  admission.

## Memory bounds

Every queue and memory the mobile owner keeps is bounded; each bound is at
least legacy React Native's. Legacy queued 512 records and 1 MiB per binding
across every record class (Android `UnifiedBleProtocolJsiBinding.cpp`, Apple
`UnifiedBleProtocolAppleExecutionState.hpp`), capped one control record at
256 KiB and one binary payload at 512 KiB, and held 1024 pending operations
per attachment. Tested in `crates/ubm-mobile/tests/caps.rs` and
`tests/session.rs`.

| Bound | Value | Past it |
|---|---|---|
| Platform ingress, advertisements | 512 | the new one is refused and counted (`ingress-drop`) |
| Platform ingress, notifications | 1024 values and 1 MiB | the new one is refused and counted |
| Platform ingress, control facts | 512 | the new one is refused and counted |
| Session outbox, data (`adv`, `value`) | 2048 records and 4 MiB | value stream ends `stream-end overflow`; advertisement counted |
| Session outbox, control | 1024 records | counted; one `ingress-drop{class:"control"}`, then `session.reconcile` |
| Host signal queue, between ingress and the pump (X-R6) | 1024 signals | current-state facts (adapter, security, restored set, scan outcome, reset, ingress-drop counts) merge per scope, latest wins; lifecycle transitions never coalesce; past the bound they are counted and the pump broadcasts the overflow, driving `session.reconcile` |
| Args / envelope text | 1 MiB | `bytes.too-large` before parsing |
| One operation's bytes | 512 KiB (`MAX_OPERATION_BYTES`) | `bytes.too-large` |
| Cancellation state | the highest admission, plus pending cancels above it (at most `ADMISSION_WINDOW` apart) | see [Cancellation](#cancellation) |
| Link ends, database changes kept for reconcile | one per peer | replaced by the peer's next |

Sessions per host, subscriptions per session, scan members and in-flight
radio requests have no owner-side count: the core's own bounds apply
(`CentralConfig::default()`). A drain call takes at most the requested items
and bytes but always at least one record, so no record is too large to drain.

## Delivery modes

`deliveryMode` accepts `prefer-notification`, `prefer-indication`,
`require-notification` and `require-indication`.

- **Every platform (legacy rule).** A `require-*` whose characteristic lacks
  the property bit fails `gatt.property-not-supported` before any effect.
- **Android.** `require-*` is sent to the radio (`requested`). The radio writes
  that CCCD mode or refuses before any effect. `prefer-*` is a soft preference
  (`preferred`). `delivery` is the mode the radio actually wrote.
- **Apple.** CoreBluetooth picks the mode and never reports it, so the
  requirement is decided in Rust and is not sent to the radio; `delivery` is
  `unknown` (fix-plan decision 3). Decision C (owner decision pending; one
  policy point, `unenforceable_requirement` in `crates/ubm-mobile/src/session.rs`):
  `require-indication` on a characteristic that can both notify and indicate
  fails `capability.limited` (`gatt.subscribe.delivery`) before any effect,
  because CoreBluetooth enables notifications there. On an indicate-only
  characteristic it stands.

## Counters

`counters`/`native` describe the resources the asking session holds: its lease
namespace on the shared owner. Each manager therefore reports its own
resources and returns to baseline on its own, as the legacy per-manager
counters did (PR210-53). The whole owner is reported separately, under
`process`.

| key | session (`counters`) | process (`process.counters`) |
|---|---|---|
| `activeScanControllers` | 1 while the session holds a scan membership | core scan owned (0/1) |
| `scanConsumers` | the session's scan membership (0/1) | scan members |
| `chooserSessions` | 0 (no chooser on mobile) | 0 |
| `connectionLeases` | the session's leases | every session's leases |
| `physicalLinks` | leased peers the core reports connected | core live connections |
| `databaseSnapshots` | leased peers with a current database | core connections with a current database |
| `physicalCccdEnablements` | distinct characteristic instances the session subscribes | routed per-instance subscriptions |
| `subscriptionConsumers` | the session's consumers | core live consumers |
| `queuedOperations` | the session's live ops with no platform request outstanding | core queued ops |
| `dispatchedOperations` | the session's live ops with a platform request outstanding | core dispatched ops |
| `retainedByteBuffers` | the session's queued data records | every session's queued data records + queued central advertisements |
| `restorationRecords` | restored peers this session claimed | restored peers the owner holds |
| `orphanedIpcOwners` | 0 (no IPC) | 0 |

`counters.describe` never counts itself.

- `native`: the session's `pendingRadioRequests` (platform requests its ops
  have outstanding) and `liveOps`.
- `process.native`: `pendingRadioRequests`, `lateRadioCompletions` (which
  also counts completions with the wrong shape), `ingressDrops` per class,
  and `liveOps`. Late completions and ingress drops are radio facts that no
  single session owns.
- `connectSections`: live entries of the per-peer connect-section table
  (never removed; a soak can prove no growth from garbage peer ids).

A disposed session still answers `counters.describe`. After a clean dispose it
reports nothing held.

## Platform radio interface

The full shapes are in `crates/ubm-mobile/src/radio.rs`,
`android/src/main/java/com/ubm/core/MobileCoreBridge.java` and
`bindings/uniffi/src/ubm_echo.udl`.

**Requests and their expected completions**

- `AdapterState` → `Adapter`
- `StartScan{serviceUuids,deviceAddresses,android{mode,callbackType,legacy}}` → `Unit`
- `StopScan` → `Unit`
- `Connect{peerId,autoConnect,preferredPhy}` → `Unit`. `preferredPhy`
  (Android) is the set of PHYs to establish the link on; a radio that
  cannot do that answers `Failed{unsupported, dispatched:false}` and never
  connects without them.
- `Disconnect` → `Unit`
- `Discover` → `Discovered`
- `Read` → `Bytes`
- `Write{value,withResponse}` → `Unit`
- `ReadDescriptor` → `Bytes`
- `WriteDescriptor` → `Unit`
- `EnableNotifications{epoch,requested,preferred}` → `NotifyEnabled(delivery written)`
- `DisableNotifications` → `Unit`
- `ReadMtu` → `Mtu(n|none)`. On Apple this is `maximumWriteValueLength(.withResponse)+3`.
- `ReadWriteLimits` → `WriteLimits{withResponse,withoutResponse}`: the
  largest single write the OS accepts per mode, both positive. It bounds
  every `gatt.write`/`gatt.write-descriptor` (`bytes.too-large`,
  `not-dispatched`, above it; a failed read fails the write
  `capability.unavailable` before dispatch). Android answers with-response
  512 (the ATT maximum attribute value; the stack performs the long write)
  and without-response one ATT payload of the MTU `onMtuChanged` reported,
  or of the ATT default MTU 23 before any exchange. Apple answers
  `maximumWriteValueLength(for:)` per type. `connection.maximum-write-length`
  reports the same answer. Android's with-response 512 rests on AOSP: the
  stack sends a prepared write when a value exceeds one ATT payload
  (`system/stack/gatt/gatt_cl.cc` `gatt_act_write`), and
  `BluetoothGatt.writeCharacteristic` throws for a value over
  `GATT_MAX_ATTR_LEN` (512) from API 33; the ATT specification caps any
  attribute value at 512 octets (Core Spec Vol 3 Part F §3.2.9).
- `RequestMtu` → `Mtu(n)`
- `ReadRssi` → `Rssi`
- `RequestConnectionPriority` → `Accepted`
- `ReadPhy` → `Phy`
- `RequestPhy` → `PhyRequest{accepted,observation}`
- `SecurityState` → `Security`
- `CreateBond` → `Security`
- `CancelBond` → `Unit`
- `BondedPeers` → `BondedPeers`
- `AcquireBackground` → `Lease`
- `ReleaseBackground` → `Unit`
- `UpdateBackgroundNotification` → `Unit`
- `AssociateCompanion` → `Companion`
- `Close` → `Closed([scope failures])`

Any request may instead be answered with
`Failed{kind,gattStatus,nativeDomain,nativeCode,detail,dispatched}`
(`nativeDomain`/`nativeCode`: the Apple `NSError` domain and code; absent on
Android, where the GATT status is the identity). Android may also name the
failure (`nativeCompleteFailure(…, nativeCode)`, finding 133): a
foreground-service code such as `foregroundServiceNotConfigured` or a
companion-chooser code such as `associationCancelled`. That name becomes the
failure's `platform.code` for any `kind`, and the Expo layer maps it to
legacy Expo's codes. An operation pending when the Android link goes down
(peer or app disconnect, failed connect, close timeout, reconnect) is
answered `not-connected` with the disconnect's GATT status and
`dispatched:false` for work still queued (finding 132), so it reports
`connection.lost` as legacy's dispatcher did. Service discovery is one of
those operations: the Android driver hands its failure through
(`OwnedAndroidGattRadio.discover` answers a `Result`), so a link that drops
while discovery is pending is `connection.lost`, not
`platform.failure: gatt.discover`. An adapter loss, a database
change and destroy keep `platform`. `dispatched:false` means the
platform refused before sending anything to the peer. `kind` is one of
(contract code in parentheses where fixed):

- `not-connected` (`connection.lost` on both platforms, with the platform's
  answer kept; 5.0 — legacy Apple, and finding 132, reported
  `platform.failure`)
- `peer-unknown` (`peer.not-found`)
- `path-stale` (`gatt.stale-handle`)
- `busy`
- `permission-denied` (`permission.denied`)
- `permission-restricted` (`permission.restricted`)
- `permission-not-determined` (`permission.not-determined`)
- `adapter-off` (`adapter.powered-off`)
- `adapter-unavailable` (`adapter.unavailable`)
- `adapter-resetting` (`adapter.resetting`)
- `gatt-status`
- `cancelled` (`operation.aborted`)
- `unsupported` (`capability.unsupported`)
- `platform`

`busy`, `gatt-status` and `platform` are `platform.failure` with the
platform's own identity (see [Envelope and codec](#envelope-and-codec)),
except Android GATT status 19 (`connection.lost`) and the Apple owned
radio's read/notify refusals.

An Apple operation pending at a disconnect (the owned radio's 1016 or 1020,
CoreBluetooth `peripheralDisconnected` 7) is `not-connected`, so it reports
`connection.lost` like Android, with the `NSError` domain and code as
`platform` (owner decision, 5.0: every platform reports one word for one
fact; this supersedes finding 132's Apple identity rule).

Rust maps the kind to the contract identity once, per request kind.

**Ingress**

- `Advertisement`
- `Connection{peerId,connected,status}`: `status` is the platform's reason
  for a disconnect (Android GATT status; the CoreBluetooth disconnect
  `NSError` code, `null` when it gave none). A disconnect with a non-zero
  status ended for a reason other than this app's release, so it is a link
  loss (`link` reason `peer`) even while a release is pending; a clean
  disconnect (status 0 or `null`) confirms a pending release (`local`).
- `ServicesChanged`
- `Notification{instance,epoch,value}`: stamp the value with the epoch from
  the enable.
- `AdapterState`
- `ScanFailed`
- `SecurityChanged`
- `Restored{peers}`
- `Dropped{class,detail}`: use this for a platform fact the adapter cannot
  translate, such as a manufacturer section shorter than 2 bytes.

The host refuses and counts malformed advertisements:

- an RSSI or TX power outside i8;
- a non-UUID service;
- an empty peer id.

The advertisement also carries `connectable`, the solicited and overflow
service UUID lists, and (Android) `appearance` and the raw record bytes.
"Not reported" is `None`. Over JNI, `connectable` is `-1` for unknown,
`appearance` is `ABSENT_INT`, and a UUID array or the raw record is `null`
when not reported. UniFFI carries neither `appearance` nor the raw record:
CoreBluetooth reports neither.

**JNI (`MobileCoreBridge`).**

- **Host lifecycle.**
  - `nativeInstallHost(RadioHost, WakeListener, platform, owner, adapterLabel)`
    installs the process host.
  - `nativeShutdownHost()` returns the cleanup JSON.
  - `nativeHostInstalled()`.
- **Sessions.**
  - `nativeOpenSession(owner, expectedWireRevision, backgroundScope)` returns
    `{sessionId,contractRevision,wireRevision,buildIdentity}`. A foreign wire
    revision fails `protocol.incompatible` before any session exists.
    `backgroundScope` (non-empty) is the module instance whose background
    leases the session shares.
  - `nativeReleaseBackgroundScope(backgroundScope)` returns the cleanup JSON
    after releasing every background lease of the scope.
  - `nativeInvoke(sessionId, op, argsJson, InvokeCallback)` is asynchronous.
  - `nativeDrain(sessionId, maxItems, maxBytes)`.
- **Answers and facts.** Typed `nativeComplete*` and `nativeIngest*` calls
  return a status:
  - `0` delivered or accepted;
  - `1`, `2`, `3` late or mismatched completion, or a dropped ingress class;
  - `4` closed;
  - `-1` no host.
- **Build identity.**
  - `nativeBuildIdentityJson()` returns the `ubm-native-build-identity/1`
    record emitted by `bindings/jni/build.rs`.
  - `nativeContractRevision()` returns the core's `CONTRACT_REVISION`.
  - `nativeWireRevision()`.
- **Discovery encoding.** `nativeCompleteDiscovered` takes a pre-order tree:
  - levels 0/1/2 for service, characteristic and descriptor;
  - Android `PROPERTY_*` bits for characteristics.

**UniFFI.**

- Namespace functions:
  - `mobile_host_install(radio, wake, platform, owner, adapter_label)` throws;
  - `mobile_host_current()`;
  - `mobile_build_identity_json()`;
  - `mobile_contract_revision()`;
  - `mobile_wire_revision()`.
- `MobileCoreHost.{complete, ingest, open_session, shutdown}`.
- `MobileCoreSession.{session_id, admission_json, invoke, drain}`.
- Callback interfaces: `MobilePlatformRadio`, `MobileWakeSink`,
  `MobileInvokeCompletion`.
- The generated Swift, Kotlin and Python in `bindings/uniffi/generated/` are
  regenerated with the pinned `uniffi-bindgen 0.32.1`
  (`bindings/uniffi/run_uniffi_roundtrip.sh`).

## Golden vectors

- **Source.** `crates/ubm-mobile/golden/wire-vectors.json` is envelope and
  drain text produced by the owner over the scripted radio
  (`crates/ubm-mobile/tests/golden.rs`). Clock fields are normalized to 0.
- **Check.** `cargo test -p ubm-mobile` fails when the file drifts.
- **Regenerate.**
  `UBM_MOBILE_GOLDEN_WRITE=1 cargo test -p ubm-mobile --test golden`.
- **Replay.** `__tests__/backends/reactnative/rust-core-wire.golden.test.js`
  runs every vector through the TS parsers with `Buffer`/`atob` removed.
  - It requires every TS op to be covered.
  - It checks the bytes exactly.

## Core facts read

`crates/ubm-mobile/src/compat.rs` is the one place that reads:

- `PeerRecord::database_generation`;
- `LifecycleEvent::database_generation`, read before the transition;
- `CentralResourceCounters::{queued_operations, dispatched_operations}`.

A transition that arrives without a generation still surfaces, as
`ingress-drop{class:"control"}`. It is never given an invented generation.

## Evidence level

Evidence level: deterministic only. That covers:

- the scripted-radio Rust tests;
- the JVM JNI exchange (`bindings/jni/run_mobile_roundtrip.sh`);
- the UniFFI unit exchange;
- the golden replay.

None of this is physical-radio proof. The Polar H10 rows need the ANDROID and
APPLE adapters on devices.
