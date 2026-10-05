<!-- docs/BACKGROUND.md -->

# Background and restoration record

**Status:** Current 5.0 configuration and lifecycle guidance. Backend qualification remains tied to the retained evidence for the tested artifact.

**Current authority:** [5.0 contracts, semantics and host guidance](README.md#current-50-authority).

Restoration and background operation use typed backend features with evidence-bound limitations. The shared core owns portable lifecycle semantics; host integrations own OS permission, foreground service, and native restoration mechanics. A backend may not silently reconnect; the opt-in standing declaration below is explicit application policy.

Configuration and ownership below describe the current public contract, not a promise of successful OS wake in every device state. Validate native-before-JavaScript ownership, adoption, cancellation, cleanup and positive sensor delivery on the actual consuming app and device.

Package SemVer and backend support labels are independent. Compilation, mocks, an empty restoration callback, and simulator-peripheral interoperability do not establish real-device compatibility or reliability qualification. Consult the [generated platform evidence](generated/PLATFORM_SUPPORT.md) for artifact-bound support claims.

## Finite mobile scans while JavaScript is suspended

On React Native, the native owner enforces a finite scan's original lifetime,
including time spent starting it. Cleanup does not depend on a JavaScript timer
firing or on the app draining events. Expiry ends that membership; another live
member of a shared scan keeps its own ownership. Failed physical cleanup remains
owned for retry rather than being forgotten when the expired stream ends.

This does not make JavaScript run while the OS suspends it. The app observes the
terminal when its runtime can process events again. Nor does a native deadline
grant permission to scan: platform background rules and the consuming app's
configuration still apply. Supply the actual service filter when one is known;
the library preserves residual predicates while lowering safe service filters
to the native scanner. It never invents a service or weakens the query to make
background scanning appear successful.

## 5.0 known-peer restoration (issue #212)

Design authority: [ADR 2026-09-5.0-restoration-known-peer-reconnect](ADR/2026-09-5.0-restoration-known-peer-reconnect.md).
Restoration reconnects directly to devices this app already connected to,
and nothing else: no background scanning for unknown devices. Restoration
alone does not reconnect or resume subscriptions. The opt-in native standing
order below explicitly authorizes those operations without an app callback. The same public
events and vocabulary names run on iOS and Android; a platform that cannot
do it reports `capability.unsupported` with a reason, never a fake.

On Apple mobile, direct reconnect can retrieve an OS-known peripheral using its
exact persisted, OS-issued identifier even when the new process has not scanned
it. Retrieval is not a connection or restoration event: a newly retrieved object
must still acquire the local central connection, and an unknown identifier fails.

- **iOS:** CoreBluetooth state restoration via `background.ios.restoration`
  (`{id, generation}` in the Expo plugin, or the `restoration` manager
  option). The trusted native host derives the restore identifier from it.
  The system relaunches the terminated app on a BLE event and delivers the
  restored peripherals through `willRestoreState`, announced as `restored`
  peers (`peers.restored`, `restoration-received`, `restoration.claim()`).
- **Android:** known-peer reconnect through `connectGatt(autoConnect = true)`
  (the existing intent `'when-available'`), armed per associated peer with
  Companion Device Manager device presence
  (`startObservingDevicePresence`, API 31+). The Expo plugin declares
  `UbmCompanionPresenceService` (see `EXPO_PLUGIN.md`); its appearance
  callback surfaces the same `restored` peers and `restoration-received`
  event as iOS. Android has no OS restoration journal. An explicitly configured
  restoration authority can claim the authenticated process-local presence queue
  with once-per-process consumption; this does not adopt an OS journal or promise
  survival across process death. Without a configured restoration authority,
  `restoration.claim()` answers `capability.unsupported`; use `peers.restored`
  to inspect the presence-fed directory. The driver exposes `restoration claim`
  and `restoration restored` as separate operations.
- **Capabilities:** `state:restoration-adoption` and
  `state:presence-observation`, reported verbatim by the backend. Apple
  presence observation is `unsupported`: restoration arrives through
  `willRestoreState` and there is nothing to arm. Android below API 31, Web,
  desktop, and tvOS report `capability.unsupported` with a reason for
  presence observation.
- **Shared example scenario:** `restoration` in `examples-shared` (`start`
  connects and subscribes; `reconnect` accepts a durable `peerReference` from
  `restored` across managers without scanning). A recorded manager-local peer id
  is not a portable reconnect identifier.

### Android presence chain, in task order

API 36+ uses association-ID observation requests and source-aware presence
events. BLE proximity, Bluetooth connection and self-managed presence are
tracked independently across the app's matching associations: only the last
remaining source disappearing releases the peer's foreground-service lease.
The compatibility callbacks Android also sends are ignored on API 36+;
API 31–35 retain the legacy callbacks because source-aware events are unavailable.
When multiple owned associations name the same address, observe selects the
lowest association ID; unobserve attempts every matching ID. A refused stop
retains cleanup ownership and does not report idle. A successful explicit
unobserve fences pending and late callbacks until observation is armed again.
UUID-only events cannot identify this API's associated-address peer and are
explicitly diagnosed, not guessed or converted into synthetic wakes.
Service rebinds share the process callback order; these mechanisms do not make
Android promise an appearance replay merely because a service was rebound.

On Android there is no OS journal, so every step below is explicit and
app-owned. Skip one and there is nothing to wake the app:

1. **Associate.** `await ble.association.associate({ name: 'Sensor' })`
   launches the Android system UI and returns an `associated`
   peer-directory record. Association is not a bond, a connection, or a
   scan-permission bypass.
2. **Permission.** The library manifest and the Expo plugin declare
   `REQUEST_OBSERVE_COMPANION_DEVICE_PRESENCE`; the app still requests the
   Android 12+ runtime permissions (`BLUETOOTH_SCAN`/`BLUETOOTH_CONNECT`)
   itself, as in [`GETTING_STARTED.md`](GETTING_STARTED.md).
3. **Persist wake policy.** Before observing, declare the intended native,
   headless-task or foreground-service continuation described below, or explicitly
   choose record-only for app-owned reconnect. This prevents an immediate
   appearance from executing a previously persisted policy.
4. **Arm presence.** `await ble.presence.observe({ peerId })` for the known
   peer id recorded by a previous connect. Without this call an appearance
   wakes nothing.
5. **OS wake.** Companion Device Manager binds
   `UbmCompanionPresenceService` on appearance, which installs the process
   radio owner and surfaces the associated peer as a `restored` record at
   once.
6. **Read, then reconnect.** `await ble.peers.restored()` lists the
   restored peer; use its `id` only on the same manager that returned that record,
   or pass its durable `reference` to a fresh manager:
   `await ble.connect(peer.reference, { intent: 'when-available', timeoutMs })`
   and replays subscriptions through `subscribe`, unless it has explicitly
   declared the native continuation below. Reading the directory does not require
   claiming the configured process presence queue. A raw persisted public MAC
   instead requires the explicit `{address, addressType: 'public'}` target and
   the backend's address-targeting capability; never pass it as an opaque peer id
   or assume a private/random address is public.

### iOS counterpart

`ble.restoration.claim()` preserves owner failures on the public operation
`expo.restoration.claim`, including the original `platform` details. Android
without a configured presence source reports `capability.unsupported`, with
`platform.domain: 'react-native-rust-core'` and
`platform.code: 'androidRestorationNeedsPresenceWake'`. It does not become
`capability.unavailable` or the removed `unsupportedRestoration` string.
Unconfigured Apple restoration remains `capability.unavailable`; callers must
not collapse malformed input or genuine platform failures into an unsupported case.

The process-owned restoration central is allocated on its serial radio queue,
including at native launch startup. Permission work and initial CoreBluetooth
callbacks therefore cannot race its nil/create/assignment sequence. This
queue-confinement invariant is not physical OS-relaunch qualification; that
requires the separately retained device procedure below.

A restoration identity configured in the application bundle opts into this
native launch bootstrap even without a continuation standing order. The default
is record-only: retain restoration callbacks before JavaScript starts; do not
invent a reconnect or subscription. ASK applications still wait for actual OS
accessory authorization on an ordinary launch, while a matching CoreBluetooth
restoration launch identity admits the original central directly. An application
without a native restoration identity remains inert, and tvOS reports restoration
as unsupported.

Configure `background.ios.restoration` (`{ id, generation }` in the Expo
plugin, or the `restoration` manager option) and rebuild: the system
relaunches the terminated app on a BLE event, delivers the peripherals
through `willRestoreState`, and the app adopts them with
`restoration.claim()`. iOS does not implement `intent: 'when-available'`.
After a relaunch, a direct app-owned reconnect must target a durable restored
`PeerReference`, not a peer id retained by the former manager instance, then
the app resubscribes. The shared driver accepts that reference for a direct
reconnect; the fresh-manager iOS path still needs a physical qualification run.

### What the other platforms answer instead

- **Android API < 31:** no presence wake exists; presence observation
  reports `capability.unsupported`.
- **tvOS:** no background Bluetooth mode and no state restoration, so the
  TV prebuild writes neither key; both capabilities report
  `capability.unsupported` / `capability.unavailable` with the native
  reason.
- **Desktop (CoreBluetooth, WinRT, BlueZ) and Tauri/Electron:** no OS
  restoration journal for a terminated app and no presence wake; the
  `restoration-received` event never fires and presence observation
  reports `capability.unsupported` with a reason.
- **Web:** Web Bluetooth has no background relaunch or presence wake; the
  event never fires and restoration reports `capability.unsupported`
  with a reason.

### Physical test procedure (owner, one strap per phone)

Prerequisites: the example app is built with the restoration authority
configured (iOS: `background.ios.restoration`; Android:
`background.android` connected-device mode, strap associated through
`ble.association.associate`). iPhone 16 Pro Max drives Polar H10 E9B93D29;
Samsung SM-A376U1 drives Polar H10 E997042F. Run the shared `restoration`
scenario `start`, connect, subscribe, and record the reported peer id.
This procedure is the **record-only baseline**: confirm no opt-in native standing
order is armed. If a previous native order still owns resources, claim/release
them before persisting `record-only`. Keep native autonomous continuation as a
separate test; it intentionally reconnects and runs declared setup without JS.

iOS (iPhone 16 Pro Max):

1. Put the app on the Home Screen with a normal swipe and disconnect any debugger.
   From the Mac, list current processes:
   `xcrun devicectl device info processes --device <device-id> --json-output -`.
   In `result.runningProcesses`, identify the **exact app executable path** in
   `executable` (match the installed app's `.app/<CFBundleExecutable>`, not a
   substring or a system process) and read its `processIdentifier`. Resolve this
   immediately before termination; a PID from an earlier launch can be stale.
   If no unique matching process is present, stop and investigate rather than
   guessing. Then terminate only that verified process:
   `xcrun devicectl device process terminate --device <device-id> --pid <verified-pid>`.
   The command accepts a PID, not a bundle identifier. Re-list processes and retain
   the lookup/termination results; successful termination alone is not restoration
   evidence. For this ordinary CoreBluetooth baseline, never swipe-kill the app:
   user force-quit is not a qualifying relaunch condition. The iOS 26
   AccessorySetupKit-specific cases below require separate setup and evidence.
2. Produce a BLE event (the strap sends heart rate or comes into range); the system relaunches the app and delivers `willRestoreState`.
3. Run `restoration` `restored` and verify that the relaunch reports the
   native-restored peer. Directory reads are non-consuming: this establishes
   reported restoration visibility, not exactly-once adoption or a fresh-manager
   reconnect. Qualify authenticated `restoration claim` separately where a
   restoration authority is configured; its consumption receipt is the evidence
   for once-per-process claim semantics.
4. Pass the restored peer's `reference` to `restoration` `reconnect` as
   `{ "peerReference": <reference>, "intent": "direct" }`, then verify a new
   connection and subscription values. Do not use `when-available` on iOS or
   pass a previous manager-local peer id to a fresh manager. This physical
   direct-reconnect proof remains open.

Android (Samsung SM-A376U1):

1. Kill the app process: `adb shell am kill <package>`. Never `adb shell am force-stop`: a force-stop disables presence wake.
2. Move the strap out of range and back; Companion Device Manager binds `UbmCompanionPresenceService` on appearance, which installs the process radio owner and surfaces the associated peer as a `restored` record at once, rather than only persisting it for the next session open.
3. Run `restoration` `restored`, then pass the returned durable reference to
   `restoration` `reconnect` as
   `{ "peerReference": <reference>, "intent": "when-available" }`; verify
   positive subscription values and the same public connection/event vocabulary
   as iOS. The command creates a fresh manager and rejects previous manager ids.

Expected for this record-only baseline on both phones: authenticated claims,
where configured, consume once per process; directory reads do not consume.
Nothing reconnects or resumes before an app call, and a platform that cannot restore says so with
`capability.unsupported` instead of failing silently.
Android also reconnects with the durable `peerReference` and
`intent: 'when-available'`; iOS direct reconnect qualification remains open
until the updated driver is run against the physical restoration path.
With an **opt-in native standing order**, the opposite reconnect expectation
applies: the native owner may reconnect, resubscribe and execute declared setup
before any app call. Qualify that path separately using continuation wake/recovery
outcomes and positive retained values; do not relabel this baseline's historical
proof as native continuation evidence.

## Apple accessory setup and relaunch eligibility

Apple's [TN3115](https://developer.apple.com/documentation/technotes/tn3115-bluetooth-state-restoration-app-relaunch-rules)
was updated for iOS 26 / iPadOS 26. Do not apply an unconditional
"force-quit always disables restoration" rule to every accessory setup path:
the table attaches AccessorySetupKit-specific qualification to user force-quit,
Control Center Bluetooth toggling and Airplane Mode cases. Ordinary
CoreBluetooth restoration and ASK-authorized accessories are separate paths.
Settings Bluetooth power-off is not a restoration relaunch trigger. Following
a device restart, the first passcode unlock is still required. Every relaunch
also requires a pending CoreBluetooth request and its matching physical event;
successful setup alone is not a relaunch receipt.

On configured iOS 18+ hosts, the ordinary React Native manager exposes ASK
through `choose()`. Name-prefix matching requires iOS 18.2+. It presents the
system picker and returns the OS-authorized accessory with an attachment-bound
peer id. It does not connect, read GATT, manufacture an advertisement, or emit
`restoration-received`. The initial `reference` is null until the peer has
actually entered the native known-peer directory (for example by connecting).
Cancellation never secretly revokes the person's persistent OS authorization.
If authorization races cancellation, the late choice is not returned; the
accessory may remain authorized in Apple's settings.

The consuming app must declare its actual accessory allowlists in Info.plist.
Apple documents a process crash when the picker uses undeclared identifiers;
UBM validates declarations before allocating an ASK session and reports a
refusal instead. For a service-and-name selection:

```xml
<key>NSAccessorySetupKitSupports</key><array><string>Bluetooth</string></array>
<key>NSAccessorySetupBluetoothServices</key><array><string>180D</string></array>
<key>NSAccessorySetupBluetoothNames</key><array><string>Polar H10</string></array>
```

```ts
const peer = await ble.choose({
  filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Polar H10' }],
  timeoutMs: 60000
})
const connection = await ble.connect(peer)
```

For manufacturer filtering, declare the hexadecimal company identifier in
`NSAccessorySetupBluetoothCompanyIdentifiers` and supply the matching
`manufacturerData` prefix. Public company identifiers must be integers in
`0..65535`; invalid values fail as `scan.filter-invalid` before host admission
on every backend. Each ASK filter needs a service or company identifier
and a name or manufacturer-data identifier. Filters are alternatives; fields
within one filter remain conjunctive. A service-only or accept-all request,
multiple required services/company identifiers in one filter, and undeclared
selectors cannot be represented by this picker and are refused, never widened.
`optionalServices` is a Web service-permission hint; ASK authorization covers
the selected accessory, so it does not gate later service access.

ASK is unavailable on tvOS, macOS, Mac Catalyst and Web. Android accessory
association uses Companion Device Manager, not Apple's framework. The consumer
can use the same public `choose()` API on a CDM-capable Android host. Android
OR alternatives preserve service UUID, escaped literal name prefix and
manufacturer-data prefix. Accept-all maps to an unconstrained CDM LE filter;
multiple required service/company IDs in one alternative are refused because
the native filter cannot express their conjunction. Selection returns a scoped
peer, not a GATT connection; an association display label is not an observed
advertisement name. Cancellation and deadlines use the existing tracked Rust
association request and release the pending activity on the UI queue.
Cancelling the operation does not itself confirm that this UI was released.
The originating session retains the exact CDM request until a native terminal
answer. `destroy()` retries its cancellation and observes a bounded one-second
drain; a held UI queue or refused activity release reports `release-failed`
with the obligation retained for another destroy attempt. Another manager's
session neither releases nor inherits that picker.

The consumer
still owns background mode, restoration identity and applicable entitlements.
The picker requires a foreground app; the native owner enforces its deadline,
single-flight admission and cancellation independently of a JS timer. A setup
receipt is not physical restoration/continuation qualification: retain an
ASK-configured device receipt for the specific iOS 26 state transition before
making a relaunch claim. Historical ordinary-restoration receipts do not count.

## 5.0 background continuation (the declared standing order)

Restoration above answers "the app is alive again — what was it connected to?".
Continuation answers the next question: **what may the wake itself do, before
any application code runs?**

The app declares a standing order while it is alive. When the OS wakes the
process, the wake executes **only what was declared** — the library never
invents a connect, never resubscribes to something the app did not name, and
never widens a declaration it could not validate. `record-only` is the default
and is exactly 5.0-before-this-feature behaviour, so an application that does
not opt in sees no change.

Contract: [`../src/backend-contract/background-continuation.ts`](../src/backend-contract/background-continuation.ts).
Event vocabulary: [`UNIFIED_SEMANTICS.md`](UNIFIED_SEMANTICS.md).

### The four strategies

| `onAppearance`       | What one wake does                                                                                                                       | Status in this release                                                                        |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `record-only`        | Install the process radio owner, record the restored peer, stop.                                                                         | **Default.** Implemented.                                                                     |
| `native`             | Reconnect the declared known peer and resubscribe the declared characteristics through the shared Rust core, with no JavaScript running. | Android and configured iOS restoration; trusted desktop process owners use the same executor. |
| `headless-task`      | Dispatch the registered headless JS task (Android).                                                                                      | Android mechanism; OS startup and task dispatch can refuse.                                   |
| `foreground-service` | Start the configured connected-device foreground service from the wake.                                                                  | Android mechanism; OS startup can refuse.                                                     |

Android task/service strategies are not substitutes for the native reconnect
and collection order. Starting a task or service does not prove any BLE data
arrived. Other platforms must report the actual availability of their mechanism,
not emulate Android services or claim an OS background exemption.

For `headless-task`, register the declared `headlessTaskName` with React Native's
`AppRegistry.registerHeadlessTask` in the application entrypoint, before mounting
the UI. The task receives `{ peerId, event: 'companion.appeared' }`. Cold React
startup and dispatch admission are bounded by 10 seconds; a timed-out admission
cannot later dispatch the task. The task bookkeeping budget is 60 seconds with
a bounded wake lock, and at most 16 admissions/tasks are retained. React Native
task finish/timeout releases bookkeeping; it does **not** prove application work
succeeded or cancel arbitrary JavaScript. Longer work must explicitly own an
appropriate execution mechanism, not assume a permanent headless wake lock.

The `foreground-service` strategy retains its connected-device lease for the
associated peer until disappearance or explicit `presence.unobserve`. Repeated
appearance does not multiply leases. Failed cleanup remains owned for retry.
It shares the same process lease registry as application background leases;
a wake cannot change an active owner's notification configuration or stop its
service by releasing an unrelated lease.
An explicit notification-text update changes the display, not the original
configuration used to admit compatible shared leases. Later acquisitions with
that original configuration remain compatible; different declarations still
require release first. Update titles must be nonblank; there is no invented
fallback title, and validation failure leaves existing ownership unchanged.

### Declaring it

The declaration is persisted host configuration: the OS must be able to read
it before JavaScript runs. A host may replace it only when doing so cannot
change an already owned continuation; claim and release that owner first.
Desktop status reads and claims queue behind an in-flight automatic recovery
attempt rather than depending on a scheduler gap. Claims then seal the
continuation before another retry can start. An explicit initial execution still
excludes concurrent claims and reports a busy lifecycle state. Confirmed link
loss retires that generation's physical subscription obligations; it does not
authorize releasing another owner or treating a service change as a link loss.
Adapter events observed while no native session is owned do not start recovery
work or compete with the first wake for admission. Once a session is owned,
events continue to request recovery through acquisition and failed cleanup;
only confirmed session disposal retires that ownership.

Native continuation and foreground clients hold independent leases on the same
physical connection. Concurrent discovery joins one physical snapshot; a new
lease attaches to that verified-current connection/database generation without
invalidating the other owner's logical handles. A later explicit rediscovery
by an already attached lease still refreshes the database. Service changes and
link loss invalidate the shared snapshot; a saved topology is never proof that
the native database remains current. Each caller keeps its own original
deadline and cancellation, including time spent waiting for discovery.
Releasing a foreground session or claiming the native continuation releases
only that owner's lease and subscriptions; another live owner's connection
and GATT work remain owned.
GATT requests carry their actual caller's lease even when another client first
discovered the physical topology. Each subscription also retains its parent
lease. Once scoped release starts, that lease admits no new GATT work; a failed
child unsubscribe keeps the lease and child cleanup retryable without blocking
unrelated owners' work.

A failed or cancelled connection attempt can still require native disconnect
cleanup. Its logical `lost` state does not prove that physical cleanup succeeded.
The shared native owner retains that generation's unresolved cleanup for retry;
a new attempt for the same peer cannot bypass it, and waiting for it remains
within the new caller's original deadline and cancellation. Other peers remain
independent. Shutdown reports unresolved connection cleanup with its native
failure details; a confirmed retry removes that obligation without erasing
unrelated failures or historical diagnostics.

**Expo apps** declare it in the config plugin, beside the other background
options, and the plugin validates it at prebuild time
([`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)):

```json
"background": {
  "continuation": {
    "onAppearance": "native",
    "resubscribe": [
      {
        "serviceUuid": "0000180d-0000-1000-8000-00805f9b34fb",
        "characteristicUuid": "00002a37-0000-1000-8000-00805f9b34fb"
      }
    ]
  }
}
```

`peerId` is optional: omitted, the order is scoped to whichever armed peer
appears. `serviceOccurrence` and `characteristicOccurrence` default to `1` and
only matter for a peer that exposes the same UUID more than once.
Android peers use MAC addresses; Apple peers use peripheral UUIDs. Declarations
accept at most 64 selectors. One native continuation owns one peer at a time;
a different peer is refused while that owner remains pinned.

On iOS, configure `background.ios.restoration` and the Bluetooth background
mode too. The plugin writes `UnifiedBleBackgroundContinuation` into Info.plist.
The native launch hook installs the process radio before React Native is
created; `willRestoreState` admits the restored peer to the shared executor.
Removing the option removes the generated key. tvOS does not receive a false
background/restoration declaration. A bare app must provide equivalent native
configuration; an option supplied only after JavaScript starts cannot bootstrap
a previously unconfigured cold wake.

**Bare React Native** passes the same shape to the host factory:

```ts
const host = await createReactNativeManagerHost({
  // …
  background: {
    continuation: {
      onAppearance: 'native',
      resubscribe: [
        /* … */
      ]
    }
  }
})
```

A non-`record-only` order without a persisting native owner **fails host
creation** with `capability.unsupported`. It never degrades quietly to
`record-only`: an app that believes a wake will reconnect, and is wrong, is
worse off than an app that was told no. `host.continuation` reports the
normalized declaration the host actually holds.

### Reading what happened

Expo apps read it through the manager's `continuation` namespace; bare React
Native hosts call the same two operations on `host.services`:

```ts
const status = await manager.continuation.status()
// strategy, peerId, resubscribe (count), malformedDeclarations, lastWake, lastRecovery
```

`lastWake` is the wake's own answer — `observedAtMs`, `event`
(`continuation.completed` or `continuation.failed`), `strategy`,
`peerAddress`, `code`, `reason` — not an inference from what was asked of it.
`malformedDeclarations` counts declarations the native side could not parse;
a non-zero value means a wake did **less** than the app believes it declared.

Android task/service success additionally identifies its acceptance stage:
`task-dispatched` means the headless task was admitted for dispatch, not that
its application work succeeded; `foreground-service-started` means the service
startup was accepted, not that it collected BLE data. Failed wakes can carry
the structured native `platform` refusal and never carry a success stage.

`lastRecovery` is separate from `lastWake`. It reports the latest autonomous
reconnect/resubscribe attempt, with its attempt number and either the recovered
peer/subscription count or the structured error and authoritative retryability.
An initial successful wake is not evidence that later recovery succeeded.
Only failures reported as `caller-decides` are retried, with bounded backoff;
claim handoff and host shutdown stop recovery. This is an opt-in declared
policy, not an implicit reconnect policy for ordinary connections.

`detail` is the host's own qualification of the declaration, when it has one.
Apple uses it to identify Android-specific task/service strategies that Apple
does not provide. This limitation does not apply to configured iOS `native`
continuation, which executes reconnect/resubscribe and setup through the native
host. A persisted declaration is not proof of an OS wake or successful recovery:
inspect `lastWake`, `lastRecovery` and actual delivered or recorded values.

### Draining what the wake collected

Values that arrived while no JavaScript was running are queued natively and
claimed afterwards:

```ts
const backlog = await manager.continuation.claim({ maxItems, maxBytes })
// selectors[], values[], streamEnds[], controlLost, afterCutoffLoss, disposed
```

This is a **stop-and-handoff** operation, not a non-disruptive periodic read.
The first prepared claim closes native collection admission. The foreground
owner must explicitly establish its own connection/subscriptions afterwards.
Without an explicit `recording` declaration, queues are bounded in memory:
process death can lose unclaimed values. The shared outbox retains at most 2,048 data records
and 4 MiB of encoded data, plus 1,024 control records. These are capacity limits,
not a guarantee of any recording duration; rates, payload sizes, and upstream
queues affect coverage. The opt-in durable journal below replaces the volatile
data queue; it does not remove OS background restrictions or prove uninterrupted
physical collection.

Each value carries the consumer that produced it, named
`ubm-continuation-{index}`. `selectors[index]` is the immutable selector that
the exact native session subscribed for that consumer. A recovery uses fresh
consumer indices while retaining the older mapping, so changing or reordering
the standing declaration cannot relabel an already queued value.

Claim first seals the Rust outbox under the same lock as native data admission.
Every record admitted before that cutoff remains drainable. An intake attempt
observed after the cutoff increments `afterCutoffLoss.items` and `.bytes`, so
the foreground handoff reports its gap explicitly instead of treating an
instantaneously empty queue as proof that no later value arrived.

Loss is reported, never hidden:

- a `streamEnd` carries `reason` (`overflow`, `invalidated`, `closed`) with
  `droppedItems` and `droppedBytes`;
- `controlLost` is cumulative; an increase means the app must run
  `session.reconcile` rather than infer the current state;
- `afterCutoffLoss` counts native intake observed after the sealed handoff
  cutoff;
- a broken ordinal chain or an unparseable batch **fails closed**. No partial
  backlog is ever handed over as though it were complete.

A native module without the claim answers `capability.unsupported` — never an
invented empty backlog, which would read as "the wake collected nothing".

**The claim has a duty.** `disposed` reports whether the wake's session was
released. When it is `false`, the session is **kept for the next claim, never
abandoned**, and `disposeFailure` says why the release did not complete — the
failures the platform reported, verbatim. The same is true when a drain is cut
short by the batch cap with more still queued: the unread tail is retained
rather than discarded. An application that sees `disposed: false` must claim
again until it sees `true`; treating one claim as the end of the backlog loses
data that the library deliberately kept for it. When a native drain batch is
malformed, only earlier strictly validated batches are returned and
`disposeFailure` reports the malformed boundary; destructive cleanup is not
authorized and the session remains owned for diagnosis or a follow-up claim.
If JavaScript decoded a handoff but its acknowledgement is rejected in transit
or returns a malformed receipt, the decoded backlog is still returned with
`disposed: false` and an acknowledgement uncertainty in `disposeFailure`.
After native cleanup succeeds it retains one empty acknowledgement receipt, so
the next claim can confirm release without delivering those already decoded
batches again.

**Execution and refusal outcomes are recorded.** Android also records an
unassociated appearance as `association.unknown`; an appearance outside a
`peerId`-scoped native order reports a scoped refusal. `record-only` does not
execute continuation and does not update `lastWake`. Therefore `lastWake: null`
means no continuation outcome has been recorded, not proof that the OS never
delivered a restoration/presence event. Read the restored peer directory for
those record-only events.

`malformedDeclarations` is persisted and counts distinct bad declarations, not
reads: asking twice does not inflate it, and a restart does not forget it.

### Per-generation device setup

Some devices require application commands in addition to GATT subscriptions.
For example, subscribing to Polar PMD data does not itself start ECG or ACC.
A native declaration can include up to 16 `setup` steps, executed sequentially
after all declared subscriptions are admitted. Each step names its canonical
selector, a nonempty `Uint8Array` payload (at most 512 bytes), and a
`timeoutMs` in 1–20,000 ms; the combined setup budget cannot exceed 60,000 ms.
Expo JSON configuration uses strictly validated byte-number arrays instead.

Each step uses an ATT write-with-response. An optional `response` identifies
the declared subscription index carrying its application acknowledgement,
a byte prefix, length bounds, and accepted status bytes. The observer is armed
before the write and does not consume the notification: the same record remains
available to collection. An early successful application acknowledgement cannot
override a failed ATT write. A matching malformed or rejected response fails
the step; unrelated prefixes are not acknowledgements. Optional `trailing`
validation can reject a response that says more fragments are still coming.
The step's `timeoutMs` covers observer admission, the ATT write, and any
application acknowledgement together; a delayed durable observer cannot send
the write after that deadline. A queued durable write rechecks the deadline
before native dispatch and receives only the remaining step budget. A write
already dispatched to the platform cannot be recalled by timeout; its outcome
remains uncertain and is not retried in the same generation.

Setup completion and uncertain failure are fenced by the authoritative
connection/database generation. A fresh generation repeats the setup; a timeout
does not authorize retrying a potentially committed write in the same generation.
This uncertainty belongs to the physical peer, not its continuation session:
claiming/releasing the native owner, replacing its declaration, or running an
order for another peer does not clear the failure while that peer's verified
connection/database generation is unchanged. A replacement session reports the
original failure without dispatching another setup write. Successful setup is
not inherited by a replacement declaration. The process retains at most 4,096
peers' unresolved setup failures; reaching that bound refuses new setup before
dispatch rather than evicting an uncertain generation. Observing a changed
authoritative generation for the affected peer retires its previous failure.
The executor never drops another owner's shared link to manufacture a fresh
generation. The shared H10 recipe and command encoders live in
[`examples-shared/driver/polar-continuation.ts`](../examples-shared/driver/polar-continuation.ts),
not in the library's generic native executor.

An optional `link.mtu` prerequisite runs before setup with `requested` (23–517),
`timeoutMs` (1–20,000), and an explicit `onUnsupported: 'continue' | 'fail'`.
`continue` permits only a genuine `capability.unsupported` answer; other errors
still fail. Completion reports the actual negotiated MTU or the unsupported
cause. It never interprets Apple's write-size limit as proof of an ATT MTU,
and continuing after unsupported does not establish a minimum usable MTU.

### Opt-in durable recording

A native order may declare `recording: { id, maxBytes, maxRecords }`. There are
no implicit quotas: `id` is 1–64 ASCII letters, digits, hyphens or underscores;
`maxBytes` is 1 MiB–1 GiB and `maxRecords` is 1–1,000,000. The ID is not a path.
The trusted host selects application-private storage; a renderer must never
choose an arbitrary filesystem destination.

The SQLite journal is **plaintext under OS storage protections**, and reports
`encrypted: false`. Quota accounting bounds logical database and rollback-file
bytes, not filesystem allocation units or backups. Storage and capacity failures
are explicit; there is no silent fallback to volatile collection. A terminal
`collectionFailure` stops admission and remains observable even when the disk
cannot persist that failure. Its `persisted` field distinguishes that situation.
An ordinary `runtimeFailure` is diagnostic history, not evidence that collection
was stopped. Committed-process-restart tests are not power-loss qualification.

Consumer registration metadata is committed before subscribing. Records retain
the peer, session epoch, connection/database generations, and exact selector
that produced them. Recovery does not relabel older records. Durable values
are not duplicated into the volatile claim queue and are not subject to its
2,048-record limit; the explicitly chosen journal quotas apply instead.
Journal work runs on awaited blocking workers rather than blocking the async
runtime. The shared executor offloads the entire recorded session operation,
including subscription, reconciliation and teardown; recording ownership is
retained until disposal succeeds. Independent notification collection also uses
an awaited blocking worker. Existing bounded ingress queues preserve order and backpressure;
application acknowledgement observations are published only after durable
commit. This is not a throughput or physical background-reliability guarantee.

A recording is scoped to its declared peer. Peer-bearing controls for another
device remain available through that device's ordinary event delivery, but do
not belong in this recording. Process-global controls without a peer identity
remain part of the recording. Ingress-loss deltas are durable evidence even
when the volatile delivery queue coalesces them or reaches its control limit;
later deltas never modify an already prepared prefix.

The current recording schema has no per-notification host receive timestamp.
The session start/epoch and durable ordinal establish context and ordering, not
arrival spacing. Device timestamps carried in payloads remain unchanged. Do not
derive an arrival time from export time or assume uniformly spaced arrivals.
Applications requiring host receive timing cannot obtain it from this schema;
adding it requires a separately specified native-ingress clock and schema
compatibility change. This corrective candidate does not invent that timing.

The recording controller has separate operations:

- `status(id)` reports capacity, retained records and failure state.
- `prepare(id, { maxItems, maxBytes })` reads a retained prefix without stopping
  collection. Bounds are 1–2,048 records and 1–4,194,304 bytes per batch. Public value
  payloads are owned `Uint8Array`s, never Base64 strings.
- `acknowledge(id, token)` explicitly retires that prepared prefix. Handle or
  export every record safely before acknowledging. A retry uses the same token;
  later arrivals are not part of the earlier prefix.
- `stop(id)` closes collection admission while retaining unread records. Its
  `radioRelease: 'not-requested'` receipt is not a disconnect receipt.
- `clear(id)` explicitly deletes retained records only after recording is stopped.

Logical clear does not reclaim physical allocation or securely erase payloads.
SQLite may retain freed pages in the recording file; `status().bytes` measures
retained logical records, not the file's filesystem allocation. Closing an
inactive handle does not delete the file. There is no implicit purge or vacuum.
For physical retention, the trusted application must first stop collection,
complete native ownership release, safely handle retained data, and close every
accessor in every process. Only after those owners are gone may its private
storage policy remove that recording's files. Never unlink an open journal or
remove a recording with unresolved cleanup or failure evidence. Mobile callers
have no public file-path authority; `clear` does not promise a mobile file purge.

Native `continuation.claim()` remains a separate radio stop-and-handoff. It
returns `recording: { id }` when applicable, but never reads, acknowledges, or
deletes the durable data. Complete radio cleanup and record handling separately.
Prepared prefixes survive native-session release and process restart.

All trusted accessors in one process share the authority for a canonical
recording path, including standalone desktop export stores. Export preparation
and live appends use the same journal mutex rather than competing SQLite writers.
A second process is refused before it opens SQLite while that authority is
retained, with `storage.busy`; access is nonblocking, not a wait-and-retry loop.
Use the owning process's authenticated recording bridge for concurrent export.
Cold access from a later process is supported after the earlier authority is
released. Unmanaged applications opening the SQLite file directly do not
participate in this admission protocol; a real conflicting database lock remains
an explicit storage failure, never a silently dropped value.

The process retains up to 16 inactive journal handles for reuse. Live accessors,
in-flight work, and diagnostics that could not be committed remain pinned.
A value arriving after an independent accessor stops collection is refused with
`storage.stopped`. This is a normal collection-admission refusal, not a new
uncommitted storage failure: once the producer is disposed, the stopped journal
can enter the bounded inactive cache. Earlier genuinely uncommitted storage failures remain pinned;
the stopped refusal neither replaces nor clears them. Eviction closes the handle,
not retained records or a prepared prefix/token on disk.
At 256 retained authorities, admission of a new identity refuses with
`storage.busy` rather than discarding those obligations; existing identities
remain accessible. Inactive cached handles still own their process admission
lock until eviction or process exit. A zero-byte `.authority` sidecar carries
the OS lock; it is not a recording payload or encryption. Never unlink that
sidecar while an authority exists, because doing so can admit a competing owner.
Process exit releases the OS lock without requiring deletion of the sidecar.

Trusted Node/Electron main code configures live storage with
`await continuation.recordings(privateDirectory)` before executing a recording
order. The explicit desktop process-host factories retain one shared backend:
ordinary manager borrowers release only their own resources, while native
continuation belongs to the process owner. `host.destroy()` seals admission and
reports retryable cleanup; it does not implicitly claim volatile values or
acknowledge durable records. Explicit status/claim remain available after close.
Trusted renderer bridges use the host's separate prepare/ack access, so a
renderer disappearing before delivery cannot silently consume the backlog.
Advanced host adapters can import `createNativeContinuationControl` and its
access/types from `unified-ble-manager/backend-sdk`. The supplied authenticated
access returns canonical execute/status/prepare/ack envelopes; the shared
controller validates and decodes values before ACK. It creates neither radio
nor filesystem authority. Optional `{ hostDomain, scope }` supplies transport
diagnostic context (for a mobile adapter, `ubm-mobile` and
`react-native-native`); omitted context preserves desktop diagnostics. Set
`format: 'mobile'` explicitly for native mobile controls: mobile peer IDs use
the existing MAC/UUID canonicalization, and status preserves the validated
`counters`, `native`, `process`, and `continuationOutcome` fields. Desktop status
keeps `queuedData`, `lastError`, and `continuationOutcome`; the structural
`NativeContinuationStatus` union does not invent desktop facts for mobile.
The separate mobile continuation posture (`continuationStatus`, exposed as
Expo `continuation.status()`) preserves a structured `startupFailure` independently of the
declared posture, `lastWake`, and `lastRecovery`. Accessory authorization startup
failure is not a completed wake/recovery and does not itself prevent claiming an
already-owned backlog. Its exact native domain/code remain observable until an
authoritative successful retry; Android/older hosts omit it or report null.
Native
failure identities remain authoritative. Callers sequence execute and claim;
failed disposal retains the cleanup obligation and decoded handoff for retry.
To retrieve retained data without Bluetooth admission, use
`openNativeContinuationRecordings(binding, privateDirectory)` from the explicit
desktop entrypoint. That path does not create a central or enumerate adapters.
The same private directory must be used after restart; the consuming application
owns its retention, export, consent and OS execution policy.

React Native and Expo expose `createReactNativeContinuationRecordings()` from
their explicit host entrypoints. It requires no manager, Bluetooth session, or
Bluetooth permission. Native code selects the app's private directory and the
binding checks the exact installed binary identity before storage operations.
Expo also exposes the same controller at `manager.continuation.recordings`;
it remains usable after `manager.destroy()` because the recording is not owned
by that manager's radio lease. Android uses no-backup app storage. Apple uses
Application Support excluded from backup with protection available after the
first device unlock; an OS refusal before that unlock remains explicit.

### Retained rc.13 journals with foreign-peer controls

An rc.13 mobile journal may contain a control whose peer differs from its
recording session. The typed controller continues to reject that prefix with
`protocol.malformed`; the correction prevents new foreign-peer rows but does
not rewrite existing evidence. Do not skip, relabel or automatically acknowledge
those rows. A normal typed export of such a prefix remains unavailable.

Preserve the journal and its prepared token. A trusted native accessor can
prepare the bounded raw prefix (at most 2,048 rows and 4 MiB) for explicit
diagnostic archival, retaining each row's original session and subject identity;
this is not a successfully decoded typed batch. Desktop hosts can use the
identity-checked `ContinuationRecordingStore.prepare` envelope without opening
a radio. On mobile, raw native storage controls belong to the trusted native
host, not a relaxed JavaScript decoder. No automatic migration, row deletion or
acknowledgement is performed. A consumer must make a separate explicit data
disposition decision after safe archival; retrying alone returns the same prefix.

### Capabilities

One capability per strategy, reported at runtime by the instantiated backend —
never a static platform matrix:

| Capability                      | Means                                                                           |
| ------------------------------- | ------------------------------------------------------------------------------- |
| `background:wake-on-appearance` | The OS wakes the dead process when the peer appears.                            |
| `background:native-resubscribe` | The wake reconnects and resubscribes through the Rust core, with no JavaScript. |
| `background:headless-task`      | The wake can run the registered headless JS task.                               |
| `background:wake-notification`  | The wake can start the configured foreground service with its notification.     |

### What the app owns, and what this package owns

Android task/service continuation is reported as `limited` only when the
running native binding exposes continuation declaration/status/claim ownership
and the observed Android API level is 31 or newer. This reports the mechanism,
not successful association, permissions, task registration, service configuration
or OS admission. A completed `task-dispatched` wake is not application completion;
`foreground-service-started` is not evidence of data collection. Apple has neither
Android Headless JS nor connected-device foreground services; its separate
restoration wake requires a configured restoration authority. Native resubscribe
also requires the running continuation binding on either host. These capability
registrations retain deterministic evidence labels, not hardware qualification.

This package implements the mechanism wherever the platform offers it. The
approvals are the application's business: Android Companion Device Manager
association plus `REQUEST_OBSERVE_COMPANION_DEVICE_PRESENCE` (presence
observation needs API 31+), a foreground-service type where one is used,
battery-optimisation exemptions, notification permission; on Apple, the
background modes and the restoration identifiers. We never withhold a mechanism
because an app might not be entitled to it, and we never substitute a lesser
path. When the platform refuses at runtime, the refusal is reported with the
platform's own reason under `platform`.

### Prerequisites on Android

Presence-triggered `native` execution requires the peer to be associated
through Companion Device Manager and its presence to be observed. After
obtaining the required permissions, use this order:
`ble.association.associate` → declare the continuation → `presence.observe`.
An appearance can arrive as soon as observation starts; persist the intended
policy first so that the callback does not execute the previous policy.
Association and declaration alone do not prove an appearance or successful
collection: inspect the actual wake receipt and positive recorded values.

Separately, trusted mobile controls permit explicit warm execution on the existing process owner.
The request must match the persisted declaration; this path does not require CDM association or observation
and does not fabricate a presence callback or update `lastWake`.
Neither declaring an order nor completing warm execution proves an OS wake.
Qualify presence-triggered execution separately using a fresh wake receipt and
positive collected values.

### Android observation succeeds but no new wake arrives

An accepted observation is not a new appearance. Android CDM can retain an
association in its connected-device set after the application's GATT connection
has closed. GATT teardown does not prove CDM disappearance, and toggling
observation does not necessarily clear that retained presence.

An Android 16 device trace showed `Device is already present. Triggering callback`
followed by `The association is already present`, service binding, and no new
native wake. In the corresponding [AOSP presence processor](https://android.googlesource.com/platform/frameworks/base/+/e33ae4e501c2c0aeb08d7a815881d8f1b4723859/services/companion/java/com/android/server/companion/devicepresence/DevicePresenceProcessor.java),
re-observation reaches the presence handler, but an already-present,
non-self-managed association does not pass its new-entry notification guard.
This is an observed OS admission condition, not native-wake qualification or
proof that the continuation executor failed.

For the exact associated test peer, retain CDM state and timestamped system/app
logs around observation: distinguish presence-handler entry, duplicate
suppression, service binding, callback delivery and continuation completion.
Check user-unlocked state (`RUNNING_UNLOCKED`) separately from keyguard
visibility; a showing lock screen alone does not establish credential-locked
user state. Require a genuine CDM disappearance/appearance transition and
a fresh `lastWake.observedAtMs` with the declared strategy and positive values
before qualifying collection. An unchanged historical receipt proves neither
a new success nor an executor failure. Do not fabricate callbacks, delete associations, or reset Bluetooth
to turn this diagnostic into a passing wake test; preserve the inconclusive
attempt and investigate the exact peer's OS event path.

## Related records

- [`ADR/2026-09-5.0-restoration-known-peer-reconnect.md`](ADR/2026-09-5.0-restoration-known-peer-reconnect.md)
- [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)
- [`GAPS.4.0.md`](GAPS.4.0.md)
- [Historical 4.0 architecture record](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
