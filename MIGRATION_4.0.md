<!-- MIGRATION_4.0.md -->

# Migrating from react-native-ble-plx

`unified-ble-manager@4.0.0-rc.2` is a new package and a new contract. It is **not a source-compatible rename**. There is no `new BleManager()` facade, no Base64 characteristic values, and no public transaction IDs.

This page is for a React Native app that already uses `react-native-ble-plx`. Web, Electron, Node, and Tauri are new hosts — use those pages after you understand the RN rewrite.

> This is a migration record, not the canonical 4.0 teaching page. The old
> API names and left-hand snippets are intentionally shown for comparison;
> copy current application recipes only from [`README.md`](README.md) and
> [`docs/HELPERS.md`](docs/HELPERS.md).

## What breaks on day one

| You used to write | You write now |
| --- | --- |
| `import { BleManager } from 'react-native-ble-plx'` | `import { createReactNativeBleManager } from 'unified-ble-manager/react-native'` |
| `new BleManager()` | `await createReactNativeBleManager({ instanceId: 'main' })` |
| `characteristic.value` as Base64 | `Uint8Array` |
| `cancelTransaction('tx-id')` | `AbortController` + `signal` |
| Immortal `Device` with methods | Scan observation → `Connection` lease → `snapshot()` paths |
| `manager.destroy()` fire-and-forget | `await manager.destroy()` and check `CleanupRecord` |

`instanceId` is an optional app-owned name for a distinct manager instance. For native restoration, pass `restoration: { applicationId, restorationId, generation? }`; the factory derives the trusted client, manager, and host-session identities internally. Do not pass `clientId`, `managerId`, or `hostSessionScope` to the RC2 factory.

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
  instanceId: 'main',
  restoration: {
    applicationId: 'com.example.app',
    restorationId: 'ble'
  }
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
import { capacity, deadline, scanUntil } from 'unified-ble-manager/advanced'
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
import { BleError } from 'unified-ble-manager'
import { encodeResetEnergyExpended, heartRateControlPointSelector } from 'unified-ble-manager/profiles/heart-rate'

try {
  const writablePath = await resolveCharacteristicPath(snapshot, heartRateControlPointSelector())
  await database.write(writablePath, encodeResetEnergyExpended(), {
    signal: abort.signal,
    deadline: journeyDeadline,
    mode: 'with-response'
  })
} catch (error) {
  if (!(error instanceof BleError) || (error.code !== 'gatt.not-found' && error.code !== 'gatt.property-not-supported')) {
    throw error
  }
}
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

## Advanced link controls, readiness, and GATT recovery

Advanced connection behavior lives under the generation-bound controls façade;
it is not added as methods on `connection` itself. The façade is capability
gated at runtime, so an unsupported control rejects with
`capability.unsupported` and a limited control carries its named limitation.
It never silently no-ops or reports a request as successful merely because the
method exists.

```ts
const rssi = await connection.controls.readRssi({ signal: abort.signal, deadline: journeyDeadline })
if (rssi.state === 'measured') {
  consumeRssi(rssi.rssi, rssi.connectionGeneration, rssi.observedAtMonotonicMs)
}

const mtu = await connection.controls.requestMtu(185, {
  signal: abort.signal,
  deadline: journeyDeadline
})
if (mtu.state === 'accepted' && mtu.observation?.state === 'measured') {
  consumeMtu(mtu.observation.attMtu, mtu.observation.payloadBytes)
}

const maxWrite = await connection.controls.maximumWriteLength('with-response')
if (maxWrite.state !== 'measured' || maxWrite.maximumWriteLength === null) {
  throw new Error('The backend did not provide a current write limit')
}
await database.writeLong(path, largeBytes, {
  signal: abort.signal,
  deadline: journeyDeadline,
  mode: 'with-response',
  chunkSize: maxWrite.maximumWriteLength
})
```

Every observation is typed and bound to the connection generation that produced
it. Its common metadata includes `state`, `connectionGeneration`,
`observedAtMonotonicMs`, `source`, `authority`, and `limitations`; values are
not naked integers whose meaning changes by host. A request and its observation
are different facts: an accepted MTU, priority, PHY, or subrate request means
the backend accepted the request, not that the controller or peer selected the
requested value. Use the returned observation or a separate observation stream
for measured state.

The canonical runtime capability IDs are:

| Control | Capability ID |
| --- | --- |
| `connection.controls.readRssi` | `connection:rssi` |
| `connection.controls.effectiveMtu` | `connection:effective-mtu` |
| `connection.controls.requestMtu` | `connection:request-mtu` |
| `connection.controls.requestPriority` | `connection:priority` |
| `connection.controls.parameters` / `parameterEvents` | `connection:parameters` |
| `connection.controls.readPhy` / `requestPhy` | `connection:phy` |
| `connection.controls.requestSubrate` | `connection:subrate` |
| `connection.controls.maximumWriteLength` | `gatt:maximum-write-length` |
| `connection.controls.writeReadiness` | `gatt:write-without-response-readiness` |

The operation scheduler is an implementation invariant, not a public command
queue: operations that must be serialized use one bounded queue per physical
connection, queued cancellation removes work before dispatch, and disconnect,
service change, backend reset, or destroy settles queued and in-flight work
exactly once. Work for different connections may proceed concurrently. A
bounded queue can reject new work with an explicit overflow/backpressure error;
callers must not assume an unbounded write loop.

Write-without-response readiness is unsupported until a backend advertises
`gatt:write-without-response-readiness`. A readiness stream is not evidence
that a payload was retained; use the authoritative maximum write length and
the write result's exact outcome. A late listener must be able to obtain the
current snapshot when the backend supports readiness, rather than relying only
on a missed edge event.

Use explicit GATT recovery when a service change is observed or when the
application deliberately wants a fresh database:

```ts
const afterServiceChange = await connection.rediscoverGatt({ reason: 'service-changed' })
const manualRefresh = await connection.rediscoverGatt({ reason: 'manual' })
```

Both reasons invalidate the prior database-generation paths and return a fresh
generation-bound database. Android recovery is supported disconnect/reconnect
and rediscovery; stable code does not call hidden `BluetoothGatt.refresh()`.
If a write was cancelled or otherwise has an uncertain commit state, cache
recovery never performs an uncertain-write replay. Resolve that ambiguity with
the product protocol after fresh discovery, with an explicit caller decision.

The package's deterministic tests, TCK, and host-compile checks prove only
their declared contract or host scope. They are not physical-radio
qualification; a platform support claim requires the matching retained live
evidence.

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
| `readRSSI` | `connection.controls.readRssi` when `connection:rssi` is advertised |
| `requestMTU` | `connection.controls.requestMtu` when `connection:request-mtu` is advertised; inspect its observation for the effective result |
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
| `requestConnectionPriority` | `connection.controls.requestPriority` when `connection:priority` is advertised; acceptance is not an observation of final parameters. |
| `checkBluetoothPermissions` helpers | `adapterState().authorization` + OS APIs. |
| `setLogLevel` | `manager.traces()` / `traceDocument()` if you need diagnostics. |
| Android `scanMode` / `callbackType` | `duplicatePolicy`, `filter`, `delivery`. |
| Expo `iosEnableRestoration` / `iosRestorationIdentifier` | `iosNativeProtocolRestoration` with five fields. |
| Expo `androidEnableForegroundService` | The app owns any FGS. |
| Static `supports()` matrix | `manager.supports(id)` after the backend exists. |

Restoration identity (`applicationId`, `restorationId`, `generation`, and Expo
`iosNativeProtocolRestoration`) does **not** auto-reconnect peripherals. You
still connect.

## Suggested order

1. Install `unified-ble-manager` next to the old package if you need a feature-flagged rollback. Only one stack may own the radio.
2. Create one RN manager with `instanceId` and, when restoration is required,
   the explicit `restoration` object.
3. Replace scan / connect / discover.
4. Convert Base64 reads and writes to `Uint8Array`.
5. Replace `cancelTransaction` with `AbortSignal`.
6. Move GATT state onto snapshot paths.
7. Await `destroy()`.
8. Remove `react-native-ble-plx`.

## RC1 → 4.0.0-rc.2 (PR1 — public contract reset)

`4.0.0-rc.2` is the pre-stable breaking window. There are **no compatibility aliases** — old names are removed, not deprecated.

| RC1 | RC2 |
|---|---|
| `import { BleManager, createBleManager, attachBleBackend, capacity, deadline } from 'unified-ble-manager'` | `import type { BleManager, BlePeer, BleConnection, OperationOptions } from 'unified-ble-manager'`  +  `import { createBleManager, attachBleBackend, capacity, deadline } from 'unified-ble-manager/advanced'` |
| `new BleManager(...generic)` / `BleManager<Attachment, Identity>` | `BleManager` is non-generic interface; `ApplicationBleManager` façade over internal generic core |
| `manager.scan({ deadline, signal, delivery: { itemCapacity, ... } })` | `manager.scan({ timeoutMs, signal, delivery: 'balanced' })` — `timeoutMs` normalizes once to `Deadline`; presets map to exact capacities |
| `createReactNativeBleManager({ clientId, managerId, hostSessionScope })` | `createReactNativeBleManager({ instanceId?, restoration?: { applicationId, restorationId, generation? } })` — identity derived internally, ephemeral vs deterministic restoration split |
| `createWebBleManager({ provider, clientId, managerId }) => { chooser, manager }` | `createWebBleManager(options?: BleManagerCreateOptions): Promise<BleManager>` — single manager; chooser is capability. Tests use `createWebBleManagerWithEnvironment` |
| `createTauriBleManager({ invoke, Channel })` | `createTauriBleManager(options?: BleManagerCreateOptions)` — imports `@tauri-apps/api/core` internally. Tests use `createTauriBleManagerWithEnvironment` |
| `Portable*` paths, `AttachmentId`, `ManagerId`, `Deadline`, `Capacity` from root | Moved to `unified-ble-manager/advanced` |
| `find`, `scanUntil`, `withConnection`, `defaultScanDelivery`, `throwIfCleanupFailed` from root | Moved to `unified-ble-manager/advanced` — application code should use façade `OperationOptions`/`StreamPreset` |
| `ReactNativeBleManagerAppOptions` type alias | Removed — use `BleManagerCreateOptions` directly |

Restoration identity is now deterministic via `deriveRestorationIdentity({ applicationId, restorationId, generation })` with domain `unified-ble-manager:restoration:v1` and golden fixtures at `__tests__/fixtures/restoration-identity.golden.json`. `instanceId` never affects restoration.

## Next

[`README.md`](README.md) · [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md) · [`docs/TUTORIALS.md`](docs/TUTORIALS.md) · [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md)

Maintainers: [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
