<!-- MIGRATION_4.0.md -->

# Migrating to Unified BLE Manager 4.0

Unified BLE Manager 4.0 is a new package and public contract. It is not a source-compatible rename of `react-native-ble-plx` 3.x, and it intentionally does not ship a compatibility facade that hides the new ownership, byte, cancellation, or host-selection rules.

This guide covers migration to stable `unified-ble-manager@4.0.0`.
The current published package is `4.0.0-rc.0` on npm `latest`.

## Install the new package

```sh
pnpm remove react-native-ble-plx
pnpm add unified-ble-manager
```

For Linux/BlueZ consumers, add the optional host dependency explicitly:

```sh
pnpm add dbus-next@^0.10.2
```

Do not keep both packages merely to preserve a legacy manager API. Migrate one owning BLE integration at a time and remove the old package once no code path constructs or imports it.

## Choose an explicit host entrypoint

The root package is host-neutral and selects no radio.

| Host | Import |
| --- | --- |
| React Native Android / Apple | `unified-ble-manager/react-native` |
| Browser Web Bluetooth | `unified-ble-manager/web` |
| Electron main process | `unified-ble-manager/electron/main` |
| Electron renderer | `unified-ble-manager/electron/renderer` |
| macOS Node/CoreBluetooth | `unified-ble-manager/node/corebluetooth` |
| Windows Node/WinRT | `unified-ble-manager/node/winrt` |
| Linux Node/BlueZ | `unified-ble-manager/node/bluez` |
| Custom backend authors | `unified-ble-manager/backend-sdk` |

The explicit host boundary is intentional. A failed native backend does not silently turn into Web Bluetooth, Noble, or a simulated radio.

## Replace the legacy manager construction

There is no drop-in `new BleManager()` compatibility constructor in 4.0. Create the manager/provider for the host that actually owns the radio.

React Native example:

```ts
import { Platform } from 'react-native'
import {
  createReactNativeBleManager,
  getNativeUnifiedBleProtocolControl
} from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  platform: Platform.OS === 'ios' ? 'apple' : 'android',
  control: getNativeUnifiedBleProtocolControl(),
  now: () => performance.now(),
  clientId: 'signed-in-user-ble-client',
  managerId: 'main-mobile-ble-manager',
  hostSessionScope: 'com.example.app.mobile-ble'
})
```

Keep one clearly owned manager per host session. Await `manager.destroy()` before replacing that session or shutting it down.

## Base64 values become bytes

The normal 4.0 public BLE contract uses `Uint8Array` / `Readonly<Uint8Array>`.

Before:

```ts
const base64 = await characteristic.read()
```

4.0 code should operate on bytes at the BLE boundary. If an external protocol truly requires Base64, encode/decode explicitly through `unified-ble-manager/codecs` rather than treating text as the native BLE value type.

This makes byte ownership, validation, copying, and cross-host behavior explicit.

## Public transaction IDs become AbortSignal

Applications no longer invent transaction IDs for cancellation.

Use an `AbortController` and pass its signal to cancellable operations:

```ts
const controller = new AbortController()

const result = database.read(characteristicPath, {
  signal: controller.signal,
  deadline: null
})

controller.abort()
await result
```

The library owns opaque backend correlation and quarantines late completions according to the backend contract.

## Device objects become explicit connection/database ownership

Do not treat an arbitrary device object as an immortal handle to mutable GATT state.

The 4.0 lifecycle is explicit:

1. scan or choose a peer;
2. establish an owned connection;
3. discover an owned GATT database/revision;
4. read/write/subscribe through that database and its paths;
5. release subscriptions and connections;
6. await manager destruction at the owning host boundary.

A disconnect, Services Changed event, backend generation change, or manager teardown can invalidate previously discovered state. Handle the typed terminal/error instead of reusing a stale object graph.

## Capability checks are runtime/backend-owned

Do not infer support from a platform name or old static compatibility matrix. Instantiate the selected backend and inspect the capabilities it actually reports.

Package stability is also not the same thing as a support label. `4.0.0` is stable SemVer for the public package/API contract; Preview/Supported/Reliability labels remain bound to retained platform evidence.

See [`docs/PLATFORMS.md`](docs/PLATFORMS.md).

## React Native and Expo

React Native 4.0 uses the versioned `UnifiedBleProtocolControl` native boundary and the owned Android/CoreBluetooth implementations.

Current floors:

- React Native 0.86+
- Expo SDK 57+ when using Expo
- Android min SDK 24
- iOS deployment target 16.4

For Expo, configure the `unified-ble-manager` plugin and regenerate the native projects. The package cannot run in Expo Go.

See [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md).

## Restoration and host identity

Restoration identity is explicit and host-owned. `clientId`, `managerId`, and `hostSessionScope` are not disposable operation identifiers.

In particular, `hostSessionScope` should be a stable security/ownership scope for the host session. Do not derive it from a React render, request counter, scan, or connection attempt.

Restoration adoption remains manager-owned; configuring a restoration identifier does not create a second central manager or silently reconnect arbitrary peripherals.

## Electron

Only the trusted Electron main process owns or selects the radio. Renderers use the versioned renderer client and authenticated IPC lease; they do not load a native addon.

A renderer reload/replacement is an ownership boundary. Do not cache native/backend handles across the renderer lifecycle.

See [`docs/ELECTRON.md`](docs/ELECTRON.md).

## Node desktop hosts

The CoreBluetooth and WinRT integrations ship Node-API source, not a universal native binary. Build against the exact Node or Electron ABI and architecture that will load the addon.

BlueZ is isolated behind the `/node/bluez` entrypoint and optional `dbus-next` dependency.

## Error handling

Prefer typed errors and terminal events over string matching. In particular, treat unsupported, unavailable, permission, cancellation, deadline, adapter-loss, and stale-generation conditions as distinct control-flow outcomes where the API exposes them.

Do not add a fallback backend merely to make an error disappear; doing so changes the radio/security boundary.

## Suggested migration order

1. Install `unified-ble-manager` without changing production behavior yet.
2. Pick the correct explicit host entrypoint.
3. Replace legacy manager construction with explicit host ownership.
4. Convert Base64 BLE values to bytes.
5. Replace public transaction-ID cancellation with `AbortSignal`.
6. Move connection/GATT state to explicit connection/database lifetimes.
7. Update capability and error handling.
8. Update Expo/native host configuration where applicable.
9. Run package tests plus real-device validation for the host you ship.
10. Remove `react-native-ble-plx` and any local compatibility wrapper.

## Project lineage

The 4.0 work began in the `sfourdrinier/react-native-ble-plx` lineage and became the standalone `unified-ble-manager` package. The complete Git ancestry is preserved in the new canonical repository. `v4.0.0-alpha.40` is the repository-migration checkpoint; stable 4.0.0 and later 4.x work live in `sfourdrinier/unified-ble-manager`.

For the public architecture and current host documentation, start with [`README.md`](README.md), [`docs/PLATFORMS.md`](docs/PLATFORMS.md), and [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md).
