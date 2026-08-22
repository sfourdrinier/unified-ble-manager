<!-- docs/TUTORIALS.md -->

# Public API tutorials

These recipes assume you already constructed a public `BleManager` for your host. See [`GETTING_STARTED.md`](GETTING_STARTED.md). Web Bluetooth uses `ble.choose()` from a user gesture; it does not provide continuous scanning. All public operations use `timeoutMs` and `AbortSignal`. Catch `BleError` from `unified-ble-manager` when handling optional-feature absence.

Every cancellable call takes an `AbortSignal`. `BleManager.find`, `scan`, and `connect` accept operation-level `timeoutMs`; GATT objects accept the same public operation options.

## Scan, connect, and discover

```ts
import { BleError } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'

const controller = new AbortController()
const peer = await manager.find({
  query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] },
  timeoutMs: 20_000,
  signal: controller.signal,
  select: 'first'
})
const connection = await manager.connect(peer, { signal: controller.signal, timeoutMs: 15_000 })
const database = await connection.discover({ signal: controller.signal, timeoutMs: 15_000 })
const snapshot = database.snapshot()
```

The public `BlePeer` snapshot carries `name` (from normalized `localName`), `rssi`, a scoped reference when the host can issue one, and the last normalized advertisement.

## Read and write

Use generation-bound objects from the current discovery. Do not invent
occurrences or generations.

```ts
import { encodeResetEnergyExpended } from 'unified-ble-manager/profiles/heart-rate'

const battery = database.characteristic('180F', '2A19', {
  serviceOccurrence: 0,
  characteristicOccurrence: 0
})
const bytes = await battery.read({ signal: controller.signal, timeoutMs: 5000 })
```

Battery Level is optional relative to Heart Rate Service. Catch only `gatt.not-found` or `gatt.property-not-supported` if the peripheral may omit it.

Heart Rate Control Point is conditional (Energy Expended). Resolve it separately and write only after it exists:

```ts
try {
  const hrsControl = database.characteristic('180D', '2A39', {
    serviceOccurrence: 0,
    characteristicOccurrence: 0
  })
  const receipt = await hrsControl.write(encodeResetEnergyExpended(), {
    signal: controller.signal,
    timeoutMs: 5000,
    response: 'required'
  })
} catch (error) {
  if (!(error instanceof BleError) || (error.code !== 'gatt.not-found' && error.code !== 'gatt.property-not-supported')) {
    throw error
  }
}
```

A new connection or rediscovery needs a fresh snapshot and fresh paths. Stale paths throw `gatt.stale-handle`.

## Notifications

```ts
const measurement = database.characteristic('180D', '2A37')
const subscription = await measurement.subscribe({
  signal: controller.signal,
  timeoutMs: 15_000,
  stream: 'balanced'
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
