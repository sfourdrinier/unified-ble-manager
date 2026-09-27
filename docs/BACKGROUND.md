<!-- docs/BACKGROUND.md -->

# Background and restoration record

**Status:** the 4.0 record below is transitional behavior characterization, not normative runtime semantics; the 5.0 known-peer restoration section is current

**Current authority:** [5.0 contracts, semantics and host guidance](README.md#current-50-authority).

Restoration and background operation use typed backend features with evidence-bound limitations. The shared core owns portable lifecycle semantics; host integrations own OS permission, foreground service, and native restoration mechanics. A backend may not silently reconnect; the opt-in standing declaration below is explicit application policy.

Current Android foreground-service and Apple CoreBluetooth restoration material is useful audit evidence. It must be checked for native-before-JS ownership, serialization, adoption, lifecycle, cancellation, and cleanup behavior under `UB4-ADR-RN-BOOTSTRAP`. It does not prove 4.0 restoration, background reliability, or a published option shape.

Stable support labels require the stated live/background/reliability evidence. An app build, a simulator, an empty restoration callback, or a mock cannot replace the required physical-device and L5 proof. Current restoration identifiers, callbacks, manager APIs, and configuration flags are transitional and may not be preserved as compatibility requirements.

## 5.0 known-peer restoration (issue #212)

Design authority: [ADR 2026-09-5.0-restoration-known-peer-reconnect](ADR/2026-09-5.0-restoration-known-peer-reconnect.md).
Restoration reconnects directly to devices this app already connected to,
and nothing else: no background scanning for unknown devices. Restoration
alone does not reconnect or resume subscriptions. The opt-in native standing
order below explicitly authorizes those operations without an app callback. The same public
events and vocabulary names run on iOS and Android; a platform that cannot
do it reports `capability.unsupported` with a reason, never a fake.

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
  event as iOS. What Android does NOT have is an OS restoration journal to
  adopt, so `restoration.claim()` answers `capability.unsupported` there and
  the restored peer is read from `peers.restored` instead — the same words,
  the platform's own mechanism. The example app labels the two actions
  "Claim native restoration (iOS)" and "Show restored peers (Android)"; the
  test driver exposes them as `restoration claim` and `restoration restored`.
- **Capabilities:** `state:restoration-adoption` and
  `state:presence-observation`, reported verbatim by the backend. Apple
  presence observation is `unsupported`: restoration arrives through
  `willRestoreState` and there is nothing to arm. Android below API 31, Web,
  desktop, and tvOS report `capability.unsupported` with a reason for
  presence observation.
- **Shared example scenario:** `restoration` in `examples-shared` (`start`
  connects, subscribes, and records the known peer id; `reconnect` dials
  that id directly after a relaunch with no scan).

### Android presence chain, in task order

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
3. **Arm presence.** `await ble.presence.observe({ peerId })` for the known
   peer id recorded by a previous connect. Without this call an appearance
   wakes nothing.
4. **OS wake.** Companion Device Manager binds
   `UbmCompanionPresenceService` on appearance, which installs the process
   radio owner and surfaces the associated peer as a `restored` record at
   once.
5. **Read, then reconnect.** `await ble.peers.restored()` lists the
   restored peer; the app reconnects with
   `await ble.connect(peerId, { intent: 'when-available', timeoutMs })`
   and replays subscriptions through `subscribe`, unless it has explicitly
   declared the native continuation below. `restoration.claim()` answers
   `capability.unsupported` on Android — there is no journal to adopt —
   so claim nothing; read the directory instead.

### iOS counterpart

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

iOS (iPhone 16 Pro Max):

1. From the Mac, terminate the app: `xcrun devicectl device process terminate --device <device-id> <bundle-id>`. Never swipe-kill the app: a user force-quit disables restoration.
2. Produce a BLE event (the strap sends heart rate or comes into range); the system relaunches the app and delivers `willRestoreState`.
3. Run `restoration` `restored` and verify that the relaunch reports the
   native-restored peer once. This proves wake and adoption; it does not prove
   a fresh-manager reconnect.
4. Pass the restored peer's `reference` to `restoration` `reconnect` as
   `{ "peerReference": <reference>, "intent": "direct" }`, then verify a new
   connection and subscription values. Do not use `when-available` on iOS or
   pass a previous manager-local peer id to a fresh manager. This physical
   direct-reconnect proof remains open.

Android (Samsung SM-A376U1):

1. Kill the app process: `adb shell am kill <package>`. Never `adb shell am force-stop`: a force-stop disables presence wake.
2. Move the strap out of range and back; Companion Device Manager binds `UbmCompanionPresenceService` on appearance, which installs the process radio owner and surfaces the associated peer as a `restored` record at once, rather than only persisting it for the next session open.
3. Run `restoration` `reconnect` with the recorded peer id and
   `{ "intent": "when-available" }`; verify the same public connection and
   subscription event vocabulary as iOS.

Expected on both phones: adoption happens once per process, nothing reconnects
or resumes before an app call, and a platform that cannot restore says so with
`capability.unsupported` instead of failing silently. Android additionally
qualifies reconnect with its recorded known-peer id and
`intent: 'when-available'`; iOS direct reconnect qualification remains open
until the updated driver is run against the physical restoration path.

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
| `headless-task`      | Run the registered headless JS task (Android).                                                                                           | Not implemented; explicit refusal.                                                            |
| `foreground-service` | Start the configured connected-device foreground service from the wake.                                                                  | Not implemented; explicit refusal.                                                            |

The two deferred strategies retain their validated option shape but answer
`capability.unsupported` with _"not implemented in this release"_ — deliberately
distinct from _"the platform cannot"_. A reader must never have to guess which
of the two they are looking at.

### Declaring it

The declaration is persisted host configuration: the OS must be able to read
it before JavaScript runs. A host may replace it only when doing so cannot
change an already owned continuation; claim and release that owner first.

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

`lastRecovery` is separate from `lastWake`. It reports the latest autonomous
reconnect/resubscribe attempt, with its attempt number and either the recovered
peer/subscription count or the structured error and authoritative retryability.
An initial successful wake is not evidence that later recovery succeeded.
Only failures reported as `caller-decides` are retried, with bounded backoff;
claim handoff and host shutdown stop recovery. This is an opt-in declared
policy, not an implicit reconnect policy for ordinary connections.

`detail` is the host's own qualification of the declaration, when it has one.
Apple uses it to say that a declared strategy was validated but is not
implemented in this release — the status reports what was actually declared and
validated, and says plainly that nothing will execute it, rather than reporting
plausible nulls that read as a working standing order.

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
Queues are bounded in memory, not a durable recording database: process death
can lose unclaimed values. The shared outbox retains at most 2,048 data records
and 4 MiB of encoded data, plus 1,024 control records. These are capacity limits,
not a guarantee of any recording duration; rates, payload sizes, and upstream
queues affect coverage. Applications requiring durable, uninterrupted collection
must qualify their persistence and background execution policy separately.

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

A `native` order only executes if the peer can wake the process at all, which
means the peer is associated through Companion Device Manager and presence is
being observed. In order: `ble.association.associate` → `presence.observe` →
declare the continuation. A peer that is not associated cannot appear, and the
status will show it.

## Related records

- [`ADR/2026-09-5.0-restoration-known-peer-reconnect.md`](ADR/2026-09-5.0-restoration-known-peer-reconnect.md)
- [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)
- [`GAPS.4.0.md`](GAPS.4.0.md)
- [Historical 4.0 architecture record](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
