# Migrating UBM 4.0.28 to 5.x

Status: Current. This page is for apps already using UBM, not the historical
ble-plx rewrite. Install the exact release documented below and retain it in
your lockfile. Published release history and immutable source identities are
recorded in [`RELEASE.md`](RELEASE.md); source preparation is not publication.

```sh
pnpm add unified-ble-manager@5.0.0
```

Pin the exact release and rebuild native projects. Never mix a 4.0.28 binary
with 5.x JavaScript. After 5.0.0 publication, unpinned installs select npm
`latest` (5.0.0).
Package SemVer does not promote backend hardware-evidence labels.

## Mobile construction, permissions and restoration

React Native and Expo use `UnifiedBleRustCore` only: no Expo Go,
`legacyTypeScriptCore` or `control` option. Floors are RN 0.86, Expo SDK 57,
Android API 24 and iOS/tvOS 16.4. Apple simulators are arm64-only; generic
simulator builds select `ARCHS=arm64`.

Bare RN uses `createReactNativeBleManager`; Expo uses `createExpoBleManager`
for `readiness()` and `permissions.request`. Reading readiness or Apple
`adapter.state()` never prompts. Follow [Getting started](docs/GETTING_STARTED.md)
and [Expo authorization](docs/EXPO_PLUGIN.md), including the distinct
AccessorySetupKit path rather than requesting an unsupported global grant.

Restoration manager input is `{ restorationId, generation? }`; the Expo plugin
uses `background.ios.restoration: { id, generation }`. Do not pass
`applicationId` or call `deriveRestorationIdentity`: the native host derives
trusted identities. Record-only restoration is not automatic reconnect;
native reconnect/resubscribe is explicit. See [Background](docs/BACKGROUND.md).

Capability checks are `manager.capabilities.supports(id)` and
`manager.capabilities.get(id)`, not a static matrix or `manager.supports(id)`.
Android connected-device monitoring uses `manager.background.acquire`, not a
hand-rolled foreground service. [TV hosts](docs/TV.md) must not copy phone
background configuration.

## Values, errors and recovery

Values remain `Uint8Array`; cancellation remains `AbortSignal`. Notification
values describe `delivery: 'notification' | 'indication' | 'unknown'`, replacing
the boolean `indication` field. Apple and BlueZ report `unknown` where delivery
is not observable: a requested CCCD mode is not delivery evidence.

- A connect that never established a link is `connection.failed`, with
  `retryability: 'caller-decides'`. Android statuses including 133, 62 and 147
  remain in `platform.metadata.androidGattStatus`. The public operation is
  `connection.connect`, not `rn-android-boundary.connect`.
- Link loss during an operation is `connection.lost`. Do not hide authentication
  failures by treating every platform error as a sleeping device.
- Adapter power loss does not destroy the manager. Affected connections end
  `adapter-loss`. Observe `manager.adapter.watchState()`, resolve a fresh peer
  reference and reconnect under app policy; old GATT handles remain stale.
- A second connect to a connected peer joins the link.
  `connection.already-owned` represents teardown or conflicting ownership,
  not every existing connection.
- `intent: 'when-available'` without platform presence support fails with
  `capability.unsupported`; it does not silently substitute polling.

Apps adopt 5.x vocabulary; UBM does not grow 4.x error shims. For Android
restoration without a configured presence source, follow the explicit
unsupported result in the background guide, not `unsupportedRestoration`.

## Desktop and expert exports

Upgrade Tauri plugin/main and webview, and Electron main and renderer, together.
IPC protocol is 5: protocol-2 (4.0.28) or protocol-4 peers fail
`protocol.incompatible` before a radio call. The compatibility contract remains
`C-UBM.0.1.2-DRAFT`, not renamed merely for package SemVer.

`canonicalUuid` remains in `unified-ble-manager/advanced`, alongside expert
`createBleManagerFromProvider` construction. It does not select a radio or
authorize restoration identity fabrication. External Tauri consumers supply
the documented root Cargo patches; dependency-manifest patches are insufficient.

macOS desktop is arm64-only. Linux connection/GATT requires the maintained daemon
extension, not just a shipped native binary. Follow [Tauri](docs/TAURI.md) and
[BlueZ deployment](docs/BLUEZ_LE_GATT.md). No crates.io publish is required.
