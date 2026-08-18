<!-- docs/HELPERS.md -->

# Public manager helpers

Helpers sit on the host-neutral `BleManager`, `Connection`, `DiscoveredGattDatabase`, and `Subscription` handles. They do not pick a backend, retry connections, or hide cancellation.

```ts
import {
  collectNotifications,
  connectAndDiscover,
  find,
  firstNotification,
  scanUntil,
  withConnection
} from 'unified-ble-manager'
```

## Scan and connect

`scanUntil()` starts one scan, waits until the predicate matches, and always stops the session. `find()` is the same function.

```ts
import { capacity, deadline, scanUntil } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'

const abortController = new AbortController()
const until = deadline(manager.monotonicNow() + 15_000)

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
    signal: abortController.signal,
    sharing: { mode: 'owner', allowSharing: false }
  },
  matches: candidate =>
    candidate.localName.state === 'present' && candidate.localName.value.includes('Polar')
})

const connected = await connectAndDiscover(manager, observation.device.id, {
  signal: abortController.signal,
  deadline: until
})
```

`connectAndDiscover()` returns `{ connection, database, snapshot }`. If discovery fails, it releases the connection.

## Notifications

Resolve a path from the snapshot. Never build occurrences or generations by hand.

```ts
import { capacity, deadline, firstNotification } from 'unified-ble-manager'
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import { heartRateMeasurementSelector, parseHeartRateMeasurement } from 'unified-ble-manager/profiles/heart-rate'

const until = deadline(manager.monotonicNow() + 15_000)
const path = await resolveCharacteristicPath(connected.snapshot, heartRateMeasurementSelector())

const bytes = await firstNotification(connected.database, path, {
  signal: abortController.signal,
  deadline: until,
  delivery: {
    itemCapacity: capacity(16),
    byteCapacity: capacity(8 * 1024),
    reservedControlCapacity: capacity(2),
    overflowPolicy: 'drop-oldest'
  }
})

const measurement = parseHeartRateMeasurement(bytes)
```

`firstNotification()` removes the subscription before it returns. `collectNotifications()` does the same after at most `maximumValues` payloads.

## Scoped connection ownership

```ts
import { deadline, withConnection } from 'unified-ble-manager'
import { readBatteryLevel } from 'unified-ble-manager/profiles/standard-commands'

const until = deadline(manager.monotonicNow() + 15_000)
const batteryPercent = await withConnection(
  manager,
  observation.device.id,
  { signal: abortController.signal, deadline: until },
  async connection => {
    const database = await connection.discover({
      signal: abortController.signal,
      deadline: until
    })
    return readBatteryLevel(database, {
      signal: abortController.signal,
      deadline: until
    })
  }
)
```

The helper always releases the lease. It never reconnects.

## Host boundaries

Capabilities come from the instantiated backend. Web Bluetooth uses `createNavigatorWebBleManager()` and rejects `manager.scan()`. Electron renderers use `ElectronRendererBleClient`. Tauri uses `IpcBleManager`.

See [`PROFILES_AND_COMMANDS.md`](PROFILES_AND_COMMANDS.md), [`WEB.md`](WEB.md), [`ELECTRON.md`](ELECTRON.md), and [`PLATFORMS.md`](PLATFORMS.md).
