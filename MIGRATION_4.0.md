<!-- MIGRATION_4.0.md -->

# Migrating from react-native-ble-plx

`unified-ble-manager@4.0.0-rc.1` is a new package and a new contract. It is **not a source-compatible rename**. There is no `new BleManager()` facade, no Base64 characteristic values, and no public transaction IDs.

This page is for a React Native app that already uses `react-native-ble-plx`. Web, Electron, Node, and Tauri are new hosts — use those pages after you understand the RN rewrite.

## What breaks on day one

| You used to write | You write now |
| --- | --- |
| `import { BleManager } from 'react-native-ble-plx'` | `import { createReactNativeBleManager } from 'unified-ble-manager/react-native'` |
| `new BleManager()` | `await createReactNativeBleManager({ clientId, managerId, hostSessionScope })` |
| `characteristic.value` as Base64 | `Uint8Array` |
| `cancelTransaction('tx-id')` | `AbortController` + `signal` |
| Immortal `Device` with methods | Scan observation → `Connection` lease → `snapshot()` paths |
| `manager.destroy()` fire-and-forget | `await manager.destroy()` and check `CleanupRecord` |

`hostSessionScope` is a stable security/ownership scope for the host session, not a request id.

## Install

```sh
pnpm add unified-ble-manager
```

Both packages may be installed temporarily. Only one BLE stack may own the radio/session. Feature-flag the new stack, migrate one owning session, then `pnpm remove react-native-ble-plx`.

## Construct

```ts
// react-native-ble-plx
const manager = new BleManager()

// unified-ble-manager
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  clientId: 'com.example.app.ble-client',
  managerId: 'com.example.app.ble-manager',
  hostSessionScope: 'com.example.app'
})
```

Keep one manager for the session. Await `manager.destroy()` before replacing it.

## Adapter state

```ts
// react-native-ble-plx
manager.onStateChange((state) => {
  if (state === 'PoweredOn') start()
}, true)

// unified-ble-manager — public API is a snapshot
const state = await manager.adapterState()
if (state.power !== 'on' || state.authorization !== 'granted' || state.availability !== 'available') {
  // request OS permissions / ask the user to enable Bluetooth
}
```

There is no public `onStateChange`. Use `adapterState()` for a snapshot, or `manager.adapterStates()` for a bounded stream of transitions. Stop the stream.

## Scan

```ts
// react-native-ble-plx
manager.startDeviceScan(['180d'], { allowDuplicates: false }, (error, device) => {
  manager.stopDeviceScan()
})

// unified-ble-manager
import { capacity, deadline, scanUntil } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'

const abort = new AbortController()
const journeyDeadline = deadline(manager.monotonicNow() + 20_000)
const observation = await scanUntil(manager, {
  scan: {
    filter: { serviceUuids: [HEART_RATE_SERVICE], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'merged',
    timestampPolicy: 'source-then-receipt',
    delivery: {
      itemCapacity: capacity(32),
      byteCapacity: capacity(64 * 1024),
      reservedControlCapacity: capacity(2),
      overflowPolicy: 'drop-oldest'
    },
    deadline: journeyDeadline,
    signal: abort.signal,
    sharing: { mode: 'owner', allowSharing: false }
  },
  matches: candidate => candidate.localName.state === 'present'
})
const peerId = observation.device.id
```

`startDeviceScan` / `stopDeviceScan` map to `manager.scan()` + `session.stop()`, or to `scanUntil` / `find`. The advertised name is `localName`, not `device.name`.

Scan options are verbose on purpose: overflow is visible, and a second scan is `scan.already-active` unless you join.

## Connect and discover

```ts
// react-native-ble-plx
const device = await manager.connectToDevice(id, { timeout: 15000 })
await device.discoverAllServicesAndCharacteristics()
const services = await device.services()
const characteristics = await device.characteristicsForService(hrsUuid)

// unified-ble-manager
const connection = await manager.connect(peerId, { signal: abort.signal, deadline: journeyDeadline })
const database = await connection.discover({ signal: abort.signal, deadline: journeyDeadline })
const snapshot = await database.snapshot()
```

There is no `autoConnect`, `refreshGatt`, or connect-time MTU option. After disconnect, create a new connection and a new snapshot. Old `Device` objects would have looked valid; stale 4.x paths throw `gatt.stale-handle`.

`cancelDeviceConnection` maps to `connection.disconnect()` or `connection.release()`.

## Read and write

```ts
// react-native-ble-plx
const ch = await device.readCharacteristicForDevice(id, service, char)
const bytes = Buffer.from(ch.value, 'base64')
await device.writeCharacteristicWithResponseForDevice(id, service, char, Buffer.from([1]).toString('base64'))

// unified-ble-manager
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import { batteryLevelSelector } from 'unified-ble-manager/profiles/battery-service'

const readablePath = await resolveCharacteristicPath(snapshot, batteryLevelSelector())
const value = await database.read(readablePath, { signal: abort.signal, deadline: journeyDeadline })
```

```ts
const writablePath = await resolveCharacteristicPath(snapshot, applicationWriteSelector())
await database.write(writablePath, new Uint8Array([1]), {
  signal: abort.signal,
  deadline: journeyDeadline,
  mode: 'with-response'
})
```

`value` is `Uint8Array`. If an HTTP API wants Base64, encode at that boundary yourself. `unified-ble-manager/codecs` is IEEE-11073 and byte views, not a Base64 helper. Battery Level is read-oriented; do not write it. Use a distinct application characteristic (or Heart Rate Control Point only after it exists) for writes.

Or use `readBatteryLevel` from `unified-ble-manager/profiles/standard-commands`. `journeyDeadline` is one budget for the whole sample, not 20 seconds per call.

## Notify

```ts
// react-native-ble-plx
device.monitorCharacteristicForDevice(id, '180d', '2a37', (error, c) => {
  parseHr(Buffer.from(c.value, 'base64'))
}, 'hr-monitor')
manager.cancelTransaction('hr-monitor')

// unified-ble-manager
import { subscribeHeartRateMeasurements } from 'unified-ble-manager/profiles/standard-commands'
import { parseHeartRateMeasurement } from 'unified-ble-manager/profiles/heart-rate'

const sub = await subscribeHeartRateMeasurements(database, {
  signal: abort.signal,
  deadline: journeyDeadline,
  delivery: {
    itemCapacity: capacity(16),
    byteCapacity: capacity(32 * 1024),
    reservedControlCapacity: capacity(2),
    overflowPolicy: 'drop-oldest'
  }
})
const stop = setTimeout(() => abort.abort(), 5_000)
try {
  for await (const item of sub.values) {
    if (item.kind === 'value') consume(parseHeartRateMeasurement(item.value.value))
    else if (item.kind === 'overflow') reportLoss(item)
    else break
  }
} finally {
  clearTimeout(stop)
  await sub.remove()
}
```

`monitorCharacteristicForDevice` + `cancelTransaction` become `database.subscribe` + `AbortSignal` + `subscription.remove()`.

## RSSI, MTU, long write

```ts
const rssi = (await connection.readRssi({ signal: abort.signal, deadline: journeyDeadline })).rssi
if (manager.supports('connection:request-att-mtu')) {
  await connection.requestMtu(185, { signal: abort.signal, deadline: journeyDeadline })
}
const maxWrite = await database.maximumWriteLength(path, 'without-response')
await database.writeLong(path, largeBytes, { signal: abort.signal, deadline: journeyDeadline, mode: 'with-response' })
```

## Destroy

```ts
// react-native-ble-plx
manager.destroy()

// unified-ble-manager
const gone = await manager.destroy()
if (gone.state === 'release-failed') {
  throw new Error('Manager cleanup failed.')
}
```

## Method map

| react-native-ble-plx | unified-ble-manager |
| --- | --- |
| `new BleManager()` | `createReactNativeBleManager(...)` |
| `state()` / `onStateChange` | `adapterState()` snapshot or `adapterStates()` stream |
| `startDeviceScan` / `stopDeviceScan` | `manager.scan` + `ScanSession.stop`, or `scanUntil` / `find` |
| `connectToDevice` | `manager.connect(peerId, { signal, deadline })` |
| `discoverAllServicesAndCharacteristics` | `connection.discover` |
| `services` / `characteristicsForService` | `database.snapshot()` |
| `readCharacteristicForDevice` | `database.read(path, options)` → `Uint8Array` |
| `writeCharacteristicWithResponseForDevice` | `database.write(..., { mode: 'with-response' })` |
| `writeCharacteristicWithoutResponseForDevice` | `database.write(..., { mode: 'without-response' })` |
| `monitorCharacteristicForDevice` | `database.subscribe` + `for await` + `remove()` |
| `cancelTransaction` | `AbortController.abort()` |
| `readRSSI` | `connection.readRssi` |
| `requestMTU` | `connection.requestMtu` if `supports('connection:request-att-mtu')` |
| `destroy` | `await manager.destroy()` |

## Gone on purpose

| 3.x | Why it is gone |
| --- | --- |
| Immortal `Device` handle | A disconnect made the object a lie. Use a lease + snapshot. |
| Base64 `.value` | BLE is binary. |
| Caller `transactionId` | One `AbortSignal` per policy. The library owns correlation. |
| `createBond` / `removeBond` / `bondedDevices` | OS pairing only. See [`docs/BONDING.md`](docs/BONDING.md). |
| `enable()` / `disable()` | The library does not toggle the adapter. |
| `devices()` / `isDeviceConnected` / `connectedDevices()` | Own the `Connection` you created. |
| `requestConnectionPriority` | No direct 4.0 replacement. Inspect `manager.capabilities()` and apply host-specific policy only where explicitly supported. |
| `checkBluetoothPermissions` helpers | `adapterState().authorization` + OS APIs. |
| `setLogLevel` | `manager.traces()` / `traceDocument()` if you need diagnostics. |
| Android `scanMode` / `callbackType` | `duplicatePolicy`, `filter`, `delivery`. |
| Expo `iosEnableRestoration` / `iosRestorationIdentifier` | `iosNativeProtocolRestoration` with five fields. |
| Expo `androidEnableForegroundService` | The app owns any FGS. |
| Static `supports()` matrix | `manager.supports(id)` after the backend exists. |

Restoration identity (`clientId`, `hostSessionScope`, Expo `iosNativeProtocolRestoration`) does **not** auto-reconnect peripherals. You still connect.

## Suggested order

1. Install `unified-ble-manager` next to the old package if you need a feature-flagged rollback. Only one stack may own the radio.
2. Create one RN manager with a stable `hostSessionScope`.
3. Replace scan / connect / discover.
4. Convert Base64 reads and writes to `Uint8Array`.
5. Replace `cancelTransaction` with `AbortSignal`.
6. Move GATT state onto snapshot paths.
7. Await `destroy()`.
8. Remove `react-native-ble-plx`.

## Next

[`README.md`](README.md) · [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md) · [`docs/TUTORIALS.md`](docs/TUTORIALS.md) · [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md)

Maintainers: [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
