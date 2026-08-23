<!-- docs/GETTING_STARTED.md -->

# Getting started

**Current package:** `4.0.0-rc.3` on npm `latest`.

This page gets you to a first scan, connect, read, notify, and teardown on React Native. Other hosts are linked at the bottom. The root import does not turn Bluetooth on.

## Pick a host

| You are building | Import | Next page |
| --- | --- | --- |
| React Native / Expo | `unified-ble-manager/react-native` | this page |
| Browser | `unified-ble-manager/web` | [`WEB.md`](WEB.md) |
| Electron | `electron/main` + `electron/renderer` | [`ELECTRON.md`](ELECTRON.md) |
| Node on macOS / Windows / Linux | `node/corebluetooth`, `node/winrt`, or `node/bluez` | [`NODE.md`](NODE.md) |
| Tauri v2 | `unified-ble-manager/tauri` | [`TAURI.md`](TAURI.md) |

## React Native / Expo in one hour

### 1. Install

Three native setup paths:

**Expo / CNG.** Install with `npx expo install unified-ble-manager` (or `pnpm add` plus Expo 57). Add the plugin, run `npx expo prebuild`, then a development client or production binary. Config-plugin changes require a native rebuild. The package does not run in Expo Go.

**Bare React Native adopting Expo modules.** Install Expo modules first (`npx install-expo-modules@latest`), then add this plugin and prebuild.

**Bare React Native without Prebuild.** Declare Android Bluetooth permissions and the BLE hardware feature yourself, request runtime permissions on Android 12+, add `NSBluetoothAlwaysUsageDescription` on iOS, run pods, and rebuild.

```sh
pnpm add unified-ble-manager
```

Expo also needs a native build. The package does not run in Expo Go.

```sh
pnpm add expo@^57.0.0
```

Add the plugin (full option table: [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)):

```json
{
  "expo": {
    "plugins": [
      [
        "unified-ble-manager",
        {
          "requiresBluetoothLeHardware": false,
          "modes": ["central"],
          "neverForLocation": false,
          "bluetoothAlwaysPermission": "Allow $(PRODUCT_NAME) to connect to Bluetooth devices"
        }
      ]
    ]
  }
}
```

Then generate native projects and run a development build:

```sh
npx expo prebuild
npx expo run:ios
# or: npx expo run:android
```

### 2. Ask Android for runtime permission

The plugin writes the manifest. Android 12+ still needs a runtime request. The library will not do this for you.

```ts
import { PermissionsAndroid, Platform } from 'react-native'

async function ensureAndroidBluetoothPermission(): Promise<void> {
  if (Platform.OS !== 'android') {
    return
  }
  if (Platform.Version < 31) {
    const location = await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.ACCESS_FINE_LOCATION)
    if (location !== PermissionsAndroid.RESULTS.GRANTED) {
      throw new Error('Location permission was not granted. Android 11 and below need it to scan.')
    }
    return
  }
  const result = await PermissionsAndroid.requestMultiple([
    PermissionsAndroid.PERMISSIONS.BLUETOOTH_SCAN,
    PermissionsAndroid.PERMISSIONS.BLUETOOTH_CONNECT
  ])
  const scan = result[PermissionsAndroid.PERMISSIONS.BLUETOOTH_SCAN]
  const connect = result[PermissionsAndroid.PERMISSIONS.BLUETOOTH_CONNECT]
  if (scan !== PermissionsAndroid.RESULTS.GRANTED || connect !== PermissionsAndroid.RESULTS.GRANTED) {
    throw new Error('Bluetooth permission was not granted.')
  }
}
```

### 3. Create one manager and keep it

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager()
```

The host factory owns ephemeral identity generation. Restoration-bound identity comes from the trusted native host and native configuration; application code does not pass client, manager, or host-session IDs.

### 4. Check the adapter, then run the loop

```ts
const adapter = await manager.adapter.state()
if (
  adapter.power !== 'on' ||
  adapter.availability !== 'available' ||
  ['denied', 'restricted', 'unavailable'].includes(adapter.authorization)
) {
  throw new Error(`Bluetooth is not ready: ${adapter.power} / ${adapter.authorization}`)
}
```

Never gate on a bare `authorization !== 'granted'`. Only an explicit refusal — `'denied'`,
`'restricted'`, `'unavailable'` — blocks. The other values are not refusals:
`'unknown'` means the platform exposes no per-application Bluetooth
authorization concept, as BlueZ on Linux does, or that the host did not query
one; `'not-determined'` means the user has not been asked yet, and since the
prompt is raised by *using* the radio rather than by reading the state, blocking
on it would stop the prompt from ever appearing.

Then run the finite public journey (`find` → `withDiscoveredConnection` → GATT read → `destroy`) from the root [`README.md`](../README.md):

```ts
import { HEART_RATE_SERVICE, parseHeartRateMeasurement } from 'unified-ble-manager/profiles/heart-rate'

try {
  const peer = await manager.find({
    query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] },
    timeoutMs: 20_000,
    select: 'first'
  })

  await manager.withDiscoveredConnection(peer, { timeoutMs: 15_000 }, async ({ gatt }) => {
    const bytes = await gatt.characteristic(HEART_RATE_SERVICE, '2A37').read({ timeoutMs: 10_000 })
    consume(parseHeartRateMeasurement(bytes))
  })
} finally {
  await manager.destroy()
}
```

Each `timeoutMs` is scoped to its public operation. More recipes: [`TUTORIALS.md`](TUTORIALS.md) and [`HELPERS.md`](HELPERS.md). Use `manager.adapter.waitUntilReady()` when an operation should wait for readiness.

### What will hurt you

- Expo Go has no native module. You need a prebuild / dev client.
- Android 12 without the runtime permission fails the first scan with `permission.denied`. Android 11 and below need `ACCESS_FINE_LOCATION`. `neverForLocation: true` is only honest if you do not use BLE for location.
- Bare React Native still needs `NSBluetoothAlwaysUsageDescription` in Info.plist. The Expo plugin writes that for Expo apps.
- Creating a new manager on every render leaks the radio. Create one, await `destroy()` when the session ends.
- `requiresBluetoothLeHardware: true` only marks the Android BLE hardware feature. It does not start a foreground service.

## Coming from react-native-ble-plx

This is a rewrite. There is no `new BleManager()` and no Base64 characteristic values. Start with [`MIGRATION_4.0.md`](../MIGRATION_4.0.md).

## Maintainers

Normative contract and evidence rules: [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
