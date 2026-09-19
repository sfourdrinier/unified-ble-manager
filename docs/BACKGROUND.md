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
  event as iOS.
- **Capabilities:** `state:restoration-adoption` and
  `state:presence-observation`, reported verbatim by the backend. Apple
  presence observation is `unsupported`: restoration arrives through
  `willRestoreState` and there is nothing to arm. Android below API 31, Web,
  desktop, and tvOS report `capability.unsupported` with a reason for
  presence observation.
- **Shared example scenario:** `restoration` in `examples-shared` (`start`
  connects, subscribes, and records the known peer id; `reconnect` dials
  that id directly after a relaunch with no scan).

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
2. Move the strap out of range and back; Companion Device Manager binds `UbmCompanionPresenceService` on appearance.
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
