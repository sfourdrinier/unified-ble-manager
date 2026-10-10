<!-- docs/GETTING_STARTED.md -->

# Getting started

This page gets you to a first scan, connect, read, notify, and teardown on React Native. Other hosts are linked at the bottom. The root import does not turn Bluetooth on.

This source targets `5.0.2`; verify the published version in the npm registry.

## Pick a host

| You are building                | Import                                                                        | Next page                                            |
| ------------------------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------- |
| Bare React Native               | `unified-ble-manager/react-native`                                            | this page                                            |
| Expo / CNG v2                   | `unified-ble-manager/expo`                                                    | [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)                   |
| React provider / hooks          | `unified-ble-manager/react`                                                   | [`README.md`](../README.md#react-provider-and-hooks) |
| Browser                         | `unified-ble-manager/web`                                                     | [`WEB.md`](WEB.md)                                   |
| Electron                        | `unified-ble-manager/electron/main` + `unified-ble-manager/electron/renderer` | [`ELECTRON.md`](ELECTRON.md)                         |
| Node on macOS / Windows / Linux | `unified-ble-manager/node/corebluetooth`, `node/winrt`, or `node/bluez`       | [`NODE.md`](NODE.md)                                 |
| Tauri v2                        | `unified-ble-manager/tauri`                                                   | [`TAURI.md`](TAURI.md)                               |

## React Native and Expo in one hour

### 1. Install

#### Bare React Native

Stable 5.0 publishes to npm `latest`; numbered 5.x RCs use `next`. Verify
the registry before installing: source preparation is not publication. Install
the exact release and commit the resolved lockfile for a known native rebuild:

```sh
pnpm add unified-ble-manager@5.0.2
```

Declare Android Bluetooth permissions and the BLE hardware feature yourself,
request runtime permissions on Android 12+, add
`NSBluetoothAlwaysUsageDescription` on iOS, run pods, and rebuild.

#### Expo / CNG v2

The Expo v2 schema and `unified-ble-manager/expo` factory are in this source.
Install `5.0.2` and keep that exact version in
your lockfile while validating the native build:

```sh
pnpm add unified-ble-manager@5.0.2
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
              "legacyLocation": "auto"
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

#### Apple simulator architecture

For both bare React Native and Expo, iOS/tvOS simulators are `arm64` only;
physical iPhone support is unchanged. Use an Apple Silicon Mac with native
ARM tools. A generic simulator destination can otherwise request both `arm64`
and `x86_64`, preventing CocoaPods from selecting the shipped XCFramework slice.
Configure your application's simulator build or CI explicitly with
`ARCHS=arm64`; UBM does not inject global architecture settings into your app.

After installing pods, a generic iOS simulator compile uses the following
command (replace `MyApp` with your workspace and scheme):

```sh
xcodebuild \
  -workspace ios/MyApp.xcworkspace \
  -scheme MyApp \
  -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  ARCHS=arm64 \
  CODE_SIGNING_ALLOWED=NO \
  build
```

For a tvOS simulator build, use `-sdk appletvsimulator` and
`-destination 'generic/platform=tvOS Simulator'` with the same `ARCHS=arm64`.
This restriction belongs to simulator builds, not physical-device signing or
Bluetooth permissions.

### 2. Request permission explicitly

#### Bare React Native

The consuming application owns its native permission flow. On Android, manifest
declarations do not grant runtime permission; call this helper before scanning
or connecting. Android below API 31 also needs the location declaration in the
application manifest. The helper is Android-only; it is not an Apple permission
request. On Apple, arrange the explicit native Bluetooth authorization flow in
your host. Reading `adapter.state()` does not ask the user for authorization.

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

#### Expo (phone and TV)

The Expo plugin writes native declarations, and the Expo manager exposes an
explicit permission request on both Android and Apple hosts. After creating
the Expo manager in step 3, inspect readiness, request permission, and wait for
the adapter before scan/connect. Reading `readiness()` or `adapter.state()` never prompts.
Denial must remain visible; follow readiness actions such as opening settings
or enabling Bluetooth rather than repeatedly requesting a refused permission.

The recipe below is the ordinary global-authorization path, not an AccessorySetupKit
(ASK) iOS recipe. In an ASK-configured app with global authorization still
`notDetermined`, the global permission request refuses with
`capability.unsupported` without allocating a central. Keep the user-initiated
`manager.choose` accessory flow available instead; its accessory grant is separate
from global Bluetooth permission. Do not retry a global request to obtain an
accessory grant. See [ASK authorization](EXPO_PLUGIN.md#permissions).

<!-- expo-permission-flow -->

```ts
const beforePermission = await manager.readiness()
// Present beforePermission.actions in your UI; inspecting them does not prompt.
// Ordinary global authorization only; ASK uses the separate chooser path above.
const permission = await manager.permissions.request({ purpose: 'scan-and-connect' })
if (!permission.granted.includes('bluetooth')) {
  throw new Error('Bluetooth permission was not granted.')
}
await manager.adapter.waitUntilReady({ timeoutMs: 15000 })
```

TV consumers use the same flow, with the platform-specific declarations and
limitations in [`TV.md`](TV.md). An Expo app using the bare React Native factory
instead owns the permission flow described above; no Expo surfaces are added
to that factory.

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

#### Optional native system chooser

Both factories also expose the same `manager.choose()` when the instantiated
host reports `discovery:system-chooser`: Android uses CompanionDeviceManager
on API 33+ devices with companion setup support; eligible iOS apps use
AccessorySetupKit on iOS 18+ (name prefixes require 18.2+). It is an explicit
alternative to scanning, not a fallback or a second manager:

```ts
const selected = await manager.choose({
  filters: [{ serviceUuids: ['180d'], localNamePrefix: 'SIM Polar H10' }],
  timeoutMs: 30000
})
const connection = await manager.connect(selected)
const database = await connection.discover()
// Use this database, then release the connection and eventually the manager.
```

Choose requires foreground system UI and the consuming app's declarations.
iOS apps must declare actual ASK service/name/company allowlists in Info.plist;
Expo consumers can use `ios.infoPlist`. See [the complete native setup guide](BACKGROUND.md#apple-accessory-setup-and-relaunch-eligibility)
and [the prepared reference-app qualification procedure](ACCESSORY_CHOOSER_QUALIFICATION.md).
An undeclared or unrepresentable filter is refused before OS picker allocation,
never silently broadened. A selected peer's name may be null; an Android
association label is not an advertisement name. Selection does not connect,
scan, or prove a relaunch. Abort/deadline suppresses late selection without
secretly revoking persistent OS authorization. Destroy cancels owned picker
work; retain and retry a refused cleanup receipt.

ASK is unavailable on tvOS, macOS and Mac Catalyst. On unavailable hosts the
public chooser reports `capability.unsupported`; use the runtime capability
report, not a static platform guess. Web uses its own user-activation and origin
permission rules. `optionalServices` governs Web service permissions, not ASK's
device-wide authorization.

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

Do not treat every value other than `granted` as a denial. `unknown` means the
platform exposes no per-application Bluetooth authorization concept (as on
BlueZ), or the host did not query one. `not-determined` means the user has not
been asked: complete the explicit permission flow in step 2 before radio work.
State reads never trigger that prompt; a radio operation may refuse undecided
authorization with `permission.not-determined` instead of asking implicitly.

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
- Reading a characteristic while it notifies works on every platform, but on Apple CoreBluetooth (iOS, macOS Node/Electron) the value cannot be told apart from a notification: `didUpdateValueFor` reports both. Use `characteristic.readReceipt()` when it matters: `provenance` is `read-response` when the platform attributed the value to your read, and `read-or-notification` when it may be a notification. Subscribers receive that value too. Android, WinRT, BlueZ and Web always answer `read-response`.
- `requiredHardware: true` only marks the Android BLE hardware feature. It does not start a foreground service.

### Reconnecting after the app was gone (restoration)

A known peer reconnects without a scan, but the wake-up differs per
platform: on Android associate (`ble.association.associate`), arm presence
(`ble.presence.observe({ peerId })`), then read `ble.peers.restored()` and
`connect` with intent `'when-available'`; on iOS configure
`background.ios.restoration` and adopt with `restoration.claim()`. iOS
reconnects directly through a durable restored `PeerReference`, never through
`'when-available'` (refused on iOS and tvOS); the shared driver now accepts that reference,
but its physical direct-reconnect qualification is still open. The full task-ordered chain, and what API<31,
tvOS, desktop and Web answer instead, is in [`BACKGROUND.md`](BACKGROUND.md).
Desktop initial acquisition implements `when-available` on macOS and Windows,
and on Linux with the optional maintained-daemon LE observer. This does not
provide process restoration or automatic post-loss reconnect.

## Coming from react-native-ble-plx

This is a rewrite. There is no `new BleManager()` and no Base64 characteristic
values. [`MIGRATION_4.0.md`](../MIGRATION_4.0.md) is historical comparison,
not a copyable setup guide. Apps already using UBM should start with
[`MIGRATION_4.0.28.md`](../MIGRATION_4.0.28.md) and the current host recipes here.

## Maintainers

Normative contract and evidence rules: [Current 5.0 authority](README.md#current-50-authority), [`PLATFORMS.md`](PLATFORMS.md).
