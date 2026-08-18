<!-- docs/TUTORIALS.md -->

# Public API tutorials

These recipes assume you already constructed a `BleManager` for your host. See [`GETTING_STARTED.md`](GETTING_STARTED.md). Web Bluetooth replaces the scan with `chooser.choose()`. Tauri and the Electron renderer use different client types. On `BleManager`, pass `deadline(...)` into `scan` and `connect`; GATT methods accept that same value.

Every cancellable call takes an `AbortSignal`. Scan and connect deadlines on `BleManager` use `deadline()`. GATT helpers on `Connection` / `DiscoveredGattDatabase` accept the same `deadline` value.

## Scan, connect, and discover

```ts
import { capacity, deadline, scanUntil } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'

const controller = new AbortController()
const until = deadline(manager.monotonicNow() + 20_000)

const observation = await scanUntil(manager, {
  scan: {
    filter: {
      serviceUuids: [HEART_RATE_SERVICE],
      manufacturerData: [],
      localNamePrefix: null
    },
    duplicatePolicy: 'merged',
    timestampPolicy: 'source-then-receipt',
    delivery: {
      itemCapacity: capacity(32),
      byteCapacity: capacity(16 * 1024),
      reservedControlCapacity: capacity(2),
      overflowPolicy: 'drop-oldest'
    },
    deadline: until,
    signal: controller.signal,
    sharing: { mode: 'owner', allowSharing: false }
  },
  matches: candidate =>
    candidate.localName.state === 'present' && candidate.localName.value.includes('Polar')
    // Polar often puts the full name in the scan response. Use 'merged' so that
    // packet is not dropped after a nameless first advertisement.
})

const connection = await manager.connect(observation.device.id, {
  signal: controller.signal,
  deadline: until
})

const database = await connection.discover({
  signal: controller.signal,
  deadline: until
})
const snapshot = await database.snapshot()
```

The advertised name is `observation.localName`. `observation.device` is identity only (`id`, address, stability).

## Read and write

Resolve paths from the current snapshot. Do not invent occurrences or generations.

```ts
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import { batteryLevelSelector } from 'unified-ble-manager/profiles/battery-service'
import { encodeResetEnergyExpended, heartRateControlPointSelector } from 'unified-ble-manager/profiles/heart-rate'

const batteryPath = await resolveCharacteristicPath(snapshot, batteryLevelSelector())
const bytes = await database.read(batteryPath, {
  signal: controller.signal,
  deadline: until
})

const hrsControlPath = await resolveCharacteristicPath(snapshot, heartRateControlPointSelector())
const receipt = await database.write(hrsControlPath, encodeResetEnergyExpended(), {
  signal: controller.signal,
  deadline: until,
  mode: 'with-response'
})
```

A new connection or rediscovery needs a fresh snapshot and fresh paths. Stale paths throw `gatt.stale-handle`.

## Notifications

```ts
import { capacity } from 'unified-ble-manager'
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import { heartRateMeasurementSelector } from 'unified-ble-manager/profiles/heart-rate'

const measurementPath = await resolveCharacteristicPath(snapshot, heartRateMeasurementSelector())
const subscription = await database.subscribe(measurementPath, {
  signal: controller.signal,
  deadline: until,
  delivery: {
    itemCapacity: capacity(64),
    byteCapacity: capacity(64 * 1024),
    reservedControlCapacity: capacity(2),
    overflowPolicy: 'drop-oldest'
  }
})

try {
  for await (const item of subscription.values) {
    if (item.kind === 'value') {
      consumeBytes(item.value.value)
    } else if (item.kind === 'overflow') {
      reportDataLoss(item)
    } else {
      handleTerminalNotice(item)
      break
    }
  }
} finally {
  const cleanup = await subscription.remove()
  if (cleanup.state === 'release-failed') {
    throw new Error('The notification subscription did not release cleanly.')
  }
}
```

The stream is bounded. Overflow is a real event, not silent loss.

## Disconnect and destroy

```ts
const connectionCleanup = await connection.release()
if (connectionCleanup.state === 'release-failed') {
  throw new Error('The connection did not release cleanly.')
}

const managerCleanup = await manager.destroy()
if (managerCleanup.state === 'release-failed') {
  throw new Error('The manager did not release cleanly.')
}
```

After `destroy()`, the manager admits no new operation.

## See also

[`HELPERS.md`](HELPERS.md), [`PROFILES_AND_COMMANDS.md`](PROFILES_AND_COMMANDS.md), [`CONNECTION_MANAGER.md`](CONNECTION_MANAGER.md).
