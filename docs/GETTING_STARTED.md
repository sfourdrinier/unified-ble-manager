<!-- docs/GETTING_STARTED.md -->

# Getting started

This page gets you to a first scan, connect, read, notify, and teardown on React Native. Other hosts are linked at the bottom. The root import does not turn Bluetooth on.

This source targets `4.0.24`; verify the published version in the npm registry.

## Pick a host

| You are building | Import | Next page |
| --- | --- | --- |
| Bare React Native | `unified-ble-manager/react-native` | this page |
| Expo / CNG v2 | `unified-ble-manager/expo` | [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md) |
| React provider / hooks | `unified-ble-manager/react` | [`README.md`](../README.md#react-provider-and-hooks) |
| Browser | `unified-ble-manager/web` | [`WEB.md`](WEB.md) |
| Electron | `unified-ble-manager/electron/main` + `unified-ble-manager/electron/renderer` | [`ELECTRON.md`](ELECTRON.md) |
| Node on macOS / Windows / Linux | `unified-ble-manager/node/corebluetooth`, `node/winrt`, or `node/bluez` | [`NODE.md`](NODE.md) |
| Tauri v2 | `unified-ble-manager/tauri` | [`TAURI.md`](TAURI.md) |

## React Native and Expo in one hour

### 1. Install

#### Bare React Native

Install the current published package and commit the resolved lockfile for a
known native rebuild:

```sh
pnpm add unified-ble-manager
```

Declare Android Bluetooth permissions and the BLE hardware feature yourself,
request runtime permissions on Android 12+, add
`NSBluetoothAlwaysUsageDescription` on iOS, run pods, and rebuild.

#### Expo / CNG v2

The Expo v2 schema and `unified-ble-manager/expo` factory are in this source.
After the npm registry lists `4.0.24`, install that exact version and keep it in
your lockfile while validating the native build:

```sh
pnpm add unified-ble-manager@4.0.24
```

The package does not run in Expo Go.

Expo also needs a native build and development client:

```sh
pnpm add expo@^57.0.0 expo-dev-client
```

Add the plugin (full option table: [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)):

```json
{
  "expo": {
    "plugins": [
      [
        "unified-ble-manager",
        {
          "requiredHardware": false,
          "permissions": {
            "bluetoothAlways": "Allow $(PRODUCT_NAME) to connect to Bluetooth devices",
            "android": {
              "neverForLocation": false,
              "legacyLocation": "none"
            }
          },
          "background": {
            "ios": {
              "mode": "central"
            },
            "android": {
              "mode": "none"
            }
          }
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

The packed-host proof is narrower than a full Expo app build. After `prepack`,
`node scripts/ci/packed-host-consumer-check.js` installs the tarball (not the
source tree) and checks the conditional `./expo`, `./react`, and `./tauri`
exports through CJS and ESM runtime imports/loadability, with TypeScript
imports compiled under Bundler and NodeNext resolution. The source-tree CNG
prebuild and Android debug APK/assembly are separate package/plugin and
Android compile evidence; Apple/Xcode, EAS, and physical-device proof are not
implied and require their own host- or device-specific runs.

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

#### Bare React Native

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager()
```

#### Expo

Expo uses its first-class factory, which adds Expo readiness, permission,
settings, background, association, and restoration surfaces:

```ts
import { createExpoBleManager } from 'unified-ble-manager/expo'

const manager = await createExpoBleManager()
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
- `requiredHardware: true` only marks the Android BLE hardware feature. It does not start a foreground service.

## Coming from react-native-ble-plx

This is a rewrite. There is no `new BleManager()` and no Base64 characteristic values. Start with [`MIGRATION_4.0.md`](../MIGRATION_4.0.md).

## Maintainers

Normative contract and evidence rules: [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
