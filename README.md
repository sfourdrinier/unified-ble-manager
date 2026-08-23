<!-- README.md -->

# Unified BLE Manager

`unified-ble-manager` is a Bluetooth Low Energy **central** library. You pick a host — React Native, Web, Electron, Tauri, or Node — create one manager, talk to a peripheral in bytes, cancel work with `AbortSignal`, and destroy what you create.

It is an evolution of `react-native-ble-plx`, rewritten as a **cross-platform unified product**. One bytes-first BLE model and lifecycle semantics across hosts, with host-specific construction and ownership. The root package never picks a radio for you, and it will not quietly fall back to a simulator or a different backend.

**RC3 candidate:** `4.0.0-rc.3` is prepared in this source tree. The tag-driven trusted-publishing workflow publishes it to npm `latest` after the exact-main release gates pass. Package SemVer and backend support labels are independent: each radio backend stays Experimental until artifact-bound physical-hardware validation says otherwise. See [`docs/PLATFORMS.md`](docs/PLATFORMS.md).

> Sponsored by [Imagi Explain](https://imagiexplain.com) — researched, narrated whiteboard explainers from a prompt, a PDF, or your notes.

## Documentation map

| Start here                                                                                                                                                                               | What it is                                                       |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| This README                                                                                                                                                                              | Product, install, one React Native loop, method index            |
| [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)                                                                                                                                     | Host chooser + first-hour React Native / Expo path               |
| [`docs/TUTORIALS.md`](docs/TUTORIALS.md)                                                                                                                                                 | Scan, connect, read, write, subscribe, tear down                 |
| [`docs/HELPERS.md`](docs/HELPERS.md)                                                                                                                                                     | Public `find`, scoped connection, GATT, and notification recipes |
| [`MIGRATION_4.0.md`](MIGRATION_4.0.md)                                                                                                                                                   | Side-by-side map from `react-native-ble-plx`                     |
| [`docs/WEB.md`](docs/WEB.md) · [`docs/ELECTRON.md`](docs/ELECTRON.md) · [`docs/NODE.md`](docs/NODE.md) · [`docs/TAURI.md`](docs/TAURI.md) · [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md) | Host construction                                                |
| [`docs/PEERS.md`](docs/PEERS.md)                                                                                                                                                         | Scoped peer directories, persistence, and reconnect-by-reference |
| [`docs/PROFILES_AND_COMMANDS.md`](docs/PROFILES_AND_COMMANDS.md)                                                                                                                         | Heart Rate, Battery, DIS, and path helpers                       |

## Install

```sh
pnpm add unified-ble-manager
```

Installable with npm, yarn, or Bun. This repository uses pnpm. Bun as a runtime is not a tested host.

Linux BlueZ also needs the optional D-Bus peer in the **application**:

```sh
pnpm add unified-ble-manager dbus-next@^0.10.2
```

React Native, Web, macOS CoreBluetooth, and Windows WinRT do not need `dbus-next`.

## Public entrypoints

The root import selects no radio. Import the host you actually run.

| Import                                   | Purpose                                                                        |
| ---------------------------------------- | ------------------------------------------------------------------------------ |
| `unified-ble-manager`                    | Host-neutral manager, handles, helpers, and shared types                       |
| `unified-ble-manager/react-native`       | React Native Android / Apple manager                                           |
| `unified-ble-manager/web`                | Web Bluetooth chooser + matched manager                                        |
| `unified-ble-manager/electron/main`      | Trusted Electron-main radio + IPC router                                       |
| `unified-ble-manager/electron/renderer`  | Public `BleManager` factory over an authenticated IPC transport; never a radio |
| `unified-ble-manager/tauri`              | Tauri v2 zero-plumbing `BleManager` factory                                    |
| `unified-ble-manager/node/corebluetooth` | macOS CoreBluetooth Node provider                                              |
| `unified-ble-manager/node/winrt`         | Windows WinRT Node provider                                                    |
| `unified-ble-manager/node/bluez`         | Linux BlueZ D-Bus provider                                                     |
| `unified-ble-manager/backend-sdk`        | Backend authoring contract                                                     |
| `unified-ble-manager/testing`            | Deterministic backend and TCK utilities                                        |
| `unified-ble-manager/codecs`             | Byte/`DataView` helpers and IEEE-11073 numbers — not Base64                    |
| `unified-ble-manager/cli`                | Node CLI                                                                       |

Profile subpaths: `profiles/commands`, `profiles/standard-commands`, `profiles/heart-rate`, `profiles/battery-service`, `profiles/device-information`, `profiles/health-thermometer`, `profiles/blood-pressure`, `profiles/ieee-11073`.

Deep imports are unsupported.

## Create a React Native manager

Requirements: React Native 0.86+, Expo SDK 57+ when using Expo, Android min SDK 24, iOS 16.4. The package contains native code and does not run in Expo Go.

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  instanceId: 'main'
})
```

On Android 12+ the app must request `BLUETOOTH_SCAN` and `BLUETOOTH_CONNECT` itself. The library does not call `PermissionsAndroid`.

### Expo plugin

Expo plugin v2 configuration and native restoration wiring are intentionally
tracked for PR10. Do not copy the retired RC1 keys from older examples. Use an
Expo development build, never Expo Go, and follow [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md)
for the current release-train status.

## One complete loop

Values are `Uint8Array`. Cancellable work takes `AbortSignal` and bounded
operations use `timeoutMs`. Advertised names live on `localName`, not
`device.name`.

```ts
// @ubm-recipe finite-hrs
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import { BATTERY_LEVEL_CHARACTERISTIC, parseBatteryLevel } from 'unified-ble-manager/profiles/battery-service'

const manager = await createReactNativeBleManager()
const abort = new AbortController()

try {
  const peer = await manager.find({
    query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] },
    timeoutMs: 10_000,
    signal: abort.signal,
    select: 'first'
  })
  await manager.withDiscoveredConnection(peer, { timeoutMs: 15_000, signal: abort.signal }, async ({ gatt }) => {
    const battery = gatt.characteristic('180F', BATTERY_LEVEL_CHARACTERISTIC, {
      serviceOccurrence: 0,
      characteristicOccurrence: 0
    })
    const bytes = await battery.read({ timeoutMs: 10_000, signal: abort.signal })
    consume(parseBatteryLevel(bytes))
  })
} finally {
  await manager.destroy()
}
```

Battery Level and Heart Rate Control Point are optional or conditional; see [`docs/TUTORIALS.md`](docs/TUTORIALS.md). Persistent subscriptions also live there.

Web Bluetooth replaces the scan with `ble.choose(...)` from a user gesture. Tauri and the Electron renderer use different host entrypoints — see those host pages.

## Why the API looks like this

| Shape                                 | Benefit                                                                      |
| ------------------------------------- | ---------------------------------------------------------------------------- |
| `Uint8Array`, not Base64              | BLE is binary. Encode text at the HTTP boundary yourself.                    |
| `AbortSignal` + `timeoutMs`           | Cancel the way you cancel `fetch`. The library owns operation correlation.   |
| Observation → `Connection` → snapshot | A peer id is not a live link. After disconnect, old objects would lie.       |
| Paths from `snapshot()`               | The same UUID can appear twice. Generations make stale handles fail closed.  |
| Verbose scan `delivery`               | Overflow is visible. A second scan is `scan.already-active` unless you join. |
| Explicit host import                  | A failed native backend must not become Web Bluetooth or a mock.             |
| Await `destroy()`                     | The radio and every lease have an owner. Fire-and-forget leaks them.         |

## Method index

### `BleManager`

| Member                                         | Use                                                  |
| ---------------------------------------------- | ---------------------------------------------------- |
| `scan(options)`                                | Start a bounded `ScanSession`. You must `stop()` it. |
| `find(options)`                                | Find one normalized `BlePeer` and stop its scan.     |
| `choose(options)`                              | Use a system chooser where the backend supports it.  |
| `connect(peer, { signal, timeoutMs })`         | Open a connection lease.                             |
| `destroy()`                                    | Async teardown. Await it. Inspect `CleanupRecord`.   |
| `adapter.state()` / `adapter.waitUntilReady()` | Readiness of this instantiated backend.              |
| `capabilities` / `discovery`                   | Runtime feature and discovery truth from the host.   |

### `ScanSession`

| Member         | Use                                                                                      |
| -------------- | ---------------------------------------------------------------------------------------- |
| `observations` | Bounded stream: `value`, `overflow`, or `terminal`                                       |
| `plan`         | Host-owned native/residual planning diagnostics, or `null` when this host has no planner |
| `stop()`       | End the scan and return a cleanup receipt. `find` already does this.                     |

`AdvertisementObservation.device` is identity (`id`, address, stability). The advertised name is `observation.localName`.

### `Connection`

| Member                                                   | Use                                                                                                                   |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `discover({ signal, timeoutMs })`                        | Discover GATT and return a generation-bound database                                                                  |
| `release()`                                              | Drop the lease (happy-path cleanup)                                                                                   |
| `disconnect()`                                           | Ask the radio to disconnect                                                                                           |
| `connection.controls.readRssi(options)`                  | RSSI when the instantiated backend advertises `connection:rssi`                                                       |
| `connection.controls.requestMtu(n, options)`             | Request an ATT MTU when `connection:request-mtu` is advertised; inspect the returned observation                      |
| `connection.controls.maximumWriteLength(mode)`           | Authoritative mode-specific write limit when `gatt:maximum-write-length` is advertised                                |
| `connection.controls.writeReadiness('without-response')` | Bounded readiness only when `gatt:write-without-response-readiness` is advertised; otherwise `capability.unsupported` |
| `events`                                                 | Lifecycle stream for this generation                                                                                  |

Controls report the truth of the instantiated host backend, including
`supported`, `limited`, `unavailable`, or `unsupported`; host family alone is
not evidence of support. Readiness is unsupported until the backend advertises
the readiness capability, and a readiness event does not prove a later payload
was retained.

Current PR8 host truth is detailed in the [semantics host matrix](docs/UNIFIED_SEMANTICS.md#172-current-pr8-host-matrix). In
particular, React Native Android exposes MTU request/effective observation and
PHY read/request as `limited` / deterministic controls: effective MTU is
unavailable before a successful `onMtuChanged` callback, and PHY request
`accepted` plus its observation come from the native callback result. Direct
CoreBluetooth Node/Electron-main readiness is also `limited` / deterministic
when both native readiness hooks are bridged. `parameters`, `subrate`,
`connection:parameters`, and `connection:subrate` remain unsupported.
`writeWhenReady` is available only when the instantiated backend advertises
authoritative write-without-response readiness; otherwise it rejects
`capability.unsupported` (or `capability.unavailable` when the registered
capability cannot currently be used). It accepts only `{ signal, timeoutMs }`,
waits at the connection FIFO head, rechecks the generation-bound database path
and readiness stream before dispatch, and never replays an uncertain write.
Cancellation and teardown retain readiness cleanup failures for the manager's
cleanup receipt. The separate
`writeReadiness('without-response')` stream is an observation surface, not an
automatic write helper.

### `GattDatabase`

| Member                                                                        | Use                                                                              |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `snapshot()`                                                                  | Immutable services / characteristics / descriptors                               |
| `service(uuid, selector?)`                                                    | Generation-bound `GattService` object                                            |
| `characteristic(serviceUuid, characteristicUuid, selector?)`                  | Generation-bound `GattCharacteristic` object                                     |
| `characteristic.read(options)`                                                | `Uint8Array`                                                                     |
| `characteristic.write(value, { response, signal, timeoutMs })`                | `response` is `'required'`, `'not-required'`, or `'automatic'`                   |
| `characteristic.writeWhenReady(value, { signal, timeoutMs })`                 | Bounded write-without-response helper when authoritative readiness is advertised |
| `characteristic.writeLong(value, { response, signal, timeoutMs, chunkSize })` | Chunked write when supported                                                     |
| `characteristic.subscribe({ signal, timeoutMs, stream })`                     | Notification / indication stream                                                 |
| `characteristic.descriptor(uuid).read/write(...)`                             | Descriptor bytes through the generation-bound characteristic object              |

Use the generation-bound service and characteristic objects returned by the
public database. Do not manufacture advanced portable paths or retain objects
after disconnect, service change, or rediscovery.

### `Subscription`

| Member     | Use                                                                                                                      |
| ---------- | ------------------------------------------------------------------------------------------------------------------------ |
| `values`   | Bounded stream of `value` / `overflow` / `terminal` items. A value item carries bytes, delivery, timestamp, and sequence |
| `remove()` | Always, including after abort; inspect the cleanup receipt                                                               |

### Scoped façade methods

| Helper                       | Use                                             |
| ---------------------------- | ----------------------------------------------- |
| `withConnection`             | Run a function and always `release()` the lease |
| `withDiscoveredConnection`   | Connect, discover, run, then `release()`        |
| `withScan`                   | Start a scan, run a function, then stop it      |
| `connection.lifecycleEvents` | Observe generation-bound lifecycle transitions  |

### Host factories

| Factory                                                                                | Returns                                                                                         |
| -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `createReactNativeBleManager`                                                          | Zero-plumbing public React Native manager                                                       |
| `createReactNativeBleManagerWithEnvironment`                                           | Injectable RN factory for tests                                                                 |
| `createWebBleManager`                                                                  | Zero-plumbing public Web manager; use `ble.choose()` from a user gesture                        |
| `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`    | One-call Node managers                                                                          |
| `createElectronMainCoreBluetoothBackendProvider` / `WinRt`                             | Main-process provider; you still build a `BleManager`                                           |
| `createElectronRendererBleManager` / `createElectronRendererBleManagerWithEnvironment` | Public renderer `BleManager` over a preload transport; the renderer never loads a radio backend |
| `createTauriBleManager`                                                                | Zero-plumbing Tauri `BleManager`; tests use `createTauriBleManagerWithEnvironment`              |
| `createBleManagerFromProvider`                                                         | Advanced provider construction                                                                  |

## Other hosts

- **Web:** user-gesture `ble.choose()`, then the same `connect` / GATT handles. No continuous scan. [`docs/WEB.md`](docs/WEB.md)
- **Electron:** main owns the radio; the renderer creates the public manager from its authenticated preload transport. [`docs/ELECTRON.md`](docs/ELECTRON.md)
- **Node:** `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`, or list adapters and `createBleManagerFromProvider`. Published releases ship Node-API v8 prebuilds for macOS and Windows `arm64`/`x64`. [`docs/NODE.md`](docs/NODE.md)
- **Tauri:** `createTauriBleManager()` returns the public `BleManager`; test transports use `createTauriBleManagerWithEnvironment`. [`docs/TAURI.md`](docs/TAURI.md)

`4.0.0-rc.*` versions publish to npm `latest` so a bare install gets the current 4.0 line. After the first stable `4.0.0`, later prereleases publish to `next`. Publication uses npm trusted publishing/OIDC with provenance.

## Migrating from react-native-ble-plx

This is a rewrite, not a rename. There is no drop-in BleManager constructor, no Base64 characteristic values, no public transaction IDs, and no compatibility shim.

Read [`MIGRATION_4.0.md`](MIGRATION_4.0.md) before changing a shipping app.

## Examples

- [`example/`](example/) — classic React Native fixture (`file:..`).
- [`example-expo/`](example-expo/) — Expo SDK 57 CNG fixture; requires a native prebuild.
- [`example-electron/`](example-electron/) — deterministic package/IPC smoke, not a live-radio claim.
- [`example-web/`](example-web/) — Chrome + physical Heart Rate Service harness.
- [`example-tauri/`](example-tauri/) — Tauri v2 public-manager proof.

## Development

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
```

## Maintainers

Contract, evidence, and release process live in [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`docs/PLATFORMS.md`](docs/PLATFORMS.md), [`RELEASE.md`](RELEASE.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`GOVERNANCE.md`](GOVERNANCE.md), [`SECURITY.md`](SECURITY.md), and [`SUPPORT.md`](SUPPORT.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE) and [`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json).
