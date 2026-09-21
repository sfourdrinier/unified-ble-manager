<!-- docs/BACKGROUND.md -->

# Background and restoration record

**Status:** the 4.0 record below is transitional behavior characterization, not normative runtime semantics; the 5.0 known-peer restoration section is current

**Architecture and sequencing authority:** [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

4.0 must specify restoration and background operation as typed backend features with evidence-bound limitations. The shared core owns portable lifecycle semantics; host integrations own OS permission, foreground service, and native restoration mechanics. A backend may not silently reconnect, and a product's reconnect policy remains outside the package.

Current Android foreground-service and Apple CoreBluetooth restoration material is useful audit evidence. It must be checked for native-before-JS ownership, serialization, adoption, lifecycle, cancellation, and cleanup behavior under `UB4-ADR-RN-BOOTSTRAP`. It does not prove 4.0 restoration, background reliability, or a published option shape.

Stable support labels require the stated live/background/reliability evidence. An app build, a simulator, an empty restoration callback, or a mock cannot replace the required physical-device and L5 proof. Current restoration identifiers, callbacks, manager APIs, and configuration flags are transitional and may not be preserved as compatibility requirements.

## 5.0 known-peer restoration (issue #212)

Design authority: [ADR 2026-09-5.0-restoration-known-peer-reconnect](ADR/2026-09-5.0-restoration-known-peer-reconnect.md).
Restoration reconnects directly to devices this app already connected to,
and nothing else: no background scanning for unknown devices, no
auto-reconnect, no subscription resume without an app call. The same public
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
   and replays subscriptions through `subscribe`. The library never
   auto-reconnects. `restoration.claim()` answers
   `capability.unsupported` on Android — there is no journal to adopt —
   so claim nothing; read the directory instead.

### iOS counterpart

Configure `background.ios.restoration` (`{ id, generation }` in the Expo
plugin, or the `restoration` manager option) and rebuild: the system
relaunches the terminated app on a BLE event, delivers the peripherals
through `willRestoreState`, and the app adopts them with
`restoration.claim()`. Then the same app-owned `connect` + `subscribe`
as on Android.

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
3. Run `restoration` `reconnect` with the recorded peer id and verify the same public events as a fresh connect (`connected` with a new connection generation, `subscribed`, `value`) in the same vocabulary.

Android (Samsung SM-A376U1):

1. Kill the app process: `adb shell am kill <package>`. Never `adb shell am force-stop`: a force-stop disables presence wake.
2. Move the strap out of range and back; Companion Device Manager binds `UbmCompanionPresenceService` on appearance, which installs the process radio owner and surfaces the associated peer as a `restored` record at once, rather than only persisting it for the next session open.
3. Run `restoration` `reconnect` with the recorded peer id and verify the same public events as iOS.

Expected on both phones: the reconnected peer id matches the recorded
known peer id, adoption happens once per process, nothing reconnected or
resumed before the app's `reconnect` call, and a platform that cannot
restore says so with `capability.unsupported` instead of failing silently.

## Related records

- [`ADR/2026-09-5.0-restoration-known-peer-reconnect.md`](ADR/2026-09-5.0-restoration-known-peer-reconnect.md)
- [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)
- [`GAPS.4.0.md`](GAPS.4.0.md)
- [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
