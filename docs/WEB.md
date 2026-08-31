<!-- docs/WEB.md -->

# Web Bluetooth

Use `unified-ble-manager/web` to run the public UBM manager directly in a browser. The Web backend uses the browser’s device chooser instead of continuous scanning, then exposes the same connection and GATT handles as the other UBM hosts.

The complete runnable TypeScript/Vite application is in [`example-web/`](../example-web/).

This guide targets `4.0.11`. Web Bluetooth support still depends on the browser, operating system, adapter, and peripheral. UBM reports those runtime boundaries; it does not fabricate a fallback backend.

## Requirements

- Use Chrome or another browser that implements Web Bluetooth.
- Serve the page over HTTPS. `http://localhost` is also treated as a secure development context.
- Start the system chooser from a click or another transient user activation.
- If the page is inside an iframe, its container must grant the `bluetooth` Permissions Policy.
- Keep the requested peripheral near the browser and ensure another central is not already occupying its connection.

Web Bluetooth does not provide UBM’s continuous `scan()` capability, background monitoring after the page closes, or process-level restoration. `manager.scan()` therefore rejects with `capability.unsupported`; use `manager.choose()` (often named `ble.choose()` in smaller examples).

## Install and create the manager

```sh
pnpm add unified-ble-manager
```

```ts
import { createWebBleManager } from 'unified-ble-manager/web'

const manager = await createWebBleManager()
```

The factory returns the same host-neutral `BleManager` interface used elsewhere. Importing `unified-ble-manager/web` is explicit: failure to load Web Bluetooth never substitutes React Native, Node, a simulator, or a mock.

## Complete browser lifecycle

This example chooses a standard Heart Rate Service peripheral, grants optional access to Battery Service, connects, discovers GATT, reads Battery Level, and consumes Heart Rate notifications.

```ts
import { BleError, type BleManager } from 'unified-ble-manager'
import { createWebBleManager } from 'unified-ble-manager/web'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'

let manager: BleManager | null = null

async function chooseReadAndSubscribe(): Promise<void> {
  manager = await createWebBleManager()

  try {
    // Keep this call directly in the click-handler path.
    const peer = await manager.choose({
      filters: [{ serviceUuids: [HEART_RATE_SERVICE] }],
      optionalServices: [BATTERY_SERVICE],
      timeoutMs: 60_000
    })

    const connection = await manager.connect(peer, { timeoutMs: 60_000 })

    try {
      const gatt = await connection.discover({ timeoutMs: 20_000 })

      const battery = gatt.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
      const batteryPercent = parseBatteryLevel(await battery.read({ timeoutMs: 10_000 }))
      console.log('Battery', batteryPercent)

      const heartRate = gatt.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC)
      const subscription = await heartRate.subscribe({
        timeoutMs: 10_000,
        delivery: 'prefer-notification',
        stream: 'balanced'
      })

      try {
        for await (const event of subscription.values) {
          if (event.kind === 'value') {
            const measurement = parseHeartRateMeasurement(event.value.value)
            console.log('Heart rate', measurement.beatsPerMinute)
          } else if (event.kind === 'overflow') {
            console.warn('Dropped notification items', event.droppedItems)
          } else {
            console.log('Subscription ended', event.reason)
            break
          }
        }
      } finally {
        await subscription.remove()
      }
    } finally {
      await connection.disconnect()
    }
  } catch (error) {
    if (error instanceof BleError) {
      console.error({
        code: error.code,
        domain: error.domain,
        operation: error.operation,
        browserCause: error.platform?.metadata.browserErrorName,
        recovery: error.recovery
      })
    }
    throw error
  } finally {
    await manager.destroy()
    manager = null
  }
}

document.querySelector('#connect')?.addEventListener('click', () => {
  void chooseReadAndSubscribe()
})
```

For an application with separate Connect, Disconnect, Reconnect, and Destroy controls, use the runnable [`example-web/src/main.ts`](../example-web/src/main.ts).

## Filters and service permission

Chooser `filters` decide which devices the browser may display. Separate filter objects are OR branches; the fields inside one filter are AND constraints.

```ts
const peer = await manager.choose({
  filters: [{ serviceUuids: [HEART_RATE_SERVICE], namePrefix: 'My device' }],
  optionalServices: [BATTERY_SERVICE],
  timeoutMs: 60_000
})
```

`optionalServices` grants later GATT access to services that were not part of the matching filter. It accepts service UUIDs, not characteristic UUIDs. Request every service the application will access, but do not request unrelated services.

The chooser itself belongs to the browser. UBM cannot auto-select a new device, extend the browser’s native discovery UI, or tell whether Chrome’s `NotFoundError` came from an explicit Cancel action versus the chooser closing without a compatible selection. UBM preserves the browser cause in `error.platform` and reports the stable public code `chooser.cancelled`.

## Previously authorized devices

When the browser exposes `navigator.bluetooth.getDevices()`, UBM projects those origin grants through the peer directory:

```ts
const manager = await createWebBleManager()
const authorized = await manager.peers.authorized({ timeoutMs: 10_000 })

if (authorized.length === 1) {
  const connection = await manager.connect(authorized[0], { timeoutMs: 60_000 })
  await connection.disconnect()
}
```

An authorized Web peer means the current origin has permission to address that browser device. It is not an Android-style system bond, proof that the device is nearby, or proof that another central is not connected. Do not guess when several authorized peers exist; ask the user to choose explicitly or use an application-owned persisted `PeerReference` and `manager.peers.resolve()`.

Origin authorization can survive a reload, but an in-memory peer or connection handle cannot. Create a new manager after reload and query or resolve the peer again.

## Deadlines, cancellation, and safe retry

All public operations accept `timeoutMs`; cancellable operations also accept `signal`. Each timeout applies to that operation, not to an entire multi-step journey.

```ts
const abort = new AbortController()
const connection = await manager.connect(peer, {
  signal: abort.signal,
  timeoutMs: 60_000
})
```

There is one important browser boundary: `BluetoothRemoteGATTServer.connect()` returns a native promise with no cancellation API. If `manager.connect()` reaches its deadline, UBM reports `operation.timed-out` but retains pending ownership so a second native connection cannot race the first one.

Do not immediately call `connect()` again on that same manager and peer. That correctly fails with `connection.already-owned`. Instead, destroy the manager, await cleanup, then start a fresh user-driven attempt:

```ts
try {
  await manager.connect(peer, { timeoutMs: 60_000 })
} catch (error) {
  if (
    error instanceof BleError &&
    error.code === 'operation.timed-out' &&
    error.operation === 'web-connection.connect'
  ) {
    await manager.destroy()
    manager = await createWebBleManager()
  }
  throw error
}
```

Destroy compensation also handles a late native success by disconnecting it. Clearing ownership early would permit overlapping browser connections and is intentionally not supported.

## Notifications and cleanup

Subscription values are a bounded async stream. Handle all three event kinds:

- `value`: a notification or indication payload;
- `overflow`: visible data loss because the consumer did not drain fast enough;
- `terminal`: the stream ended because of removal, disconnect, adapter loss, or teardown.

Release resources from the inside out:

1. `await subscription.remove()`;
2. `await connection.disconnect()`;
3. `await manager.destroy()`.

Each cleanup returns a receipt. Production applications should verify `state === 'released'` and preserve any reported cleanup failures. Also destroy the manager during page teardown as a best-effort safeguard, while keeping primary cleanup in explicit application actions.

## Errors users can act on

Catch `BleError` from the root package and retain its structured fields. Do not replace every failure with one generic message.

| Field       | Meaning                                                                                        |
| ----------- | ---------------------------------------------------------------------------------------------- |
| `code`      | Stable UBM category such as `chooser.cancelled`, `operation.timed-out`, or `connection.failed` |
| `domain`    | UBM subsystem such as `chooser`, `connection`, or `gatt`                                       |
| `operation` | Exact operation boundary, for example `web-connection.connect`                                 |
| `platform`  | Safe browser detail, including `browserErrorName` when Chrome supplies one                     |
| `recovery`  | Deterministic recovery disposition and suggested actions                                       |

Common boundaries:

- `chooser.cancelled` + `NotFoundError`: the chooser ended without returning a compatible device;
- `connection.failed` + `NetworkError`: Chrome selected the device but could not open GATT;
- `operation.timed-out` at `web-connection.connect`: the bounded native connection did not settle;
- `gatt.not-found`: the connection opened, but a requested service or characteristic was unavailable or not granted;
- `operation.disconnected`: the link ended during another operation.

## Iframes and deployment

A top-level HTTPS page needs no extra header. An embedded application needs its container to allow Bluetooth, for example:

```html
<iframe src="https://example.test/app" allow="bluetooth"></iframe>
```

The embedding server may also restrict or grant access using the `Permissions-Policy` response header. Browser and administrator policies can still disable Web Bluetooth.

## Testing

Application tests should inject `createWebBleManagerWithEnvironment()` only at a test boundary. Production code should use `createWebBleManager()`.

The repository example builds against the public package entrypoints:

```sh
pnpm build:example:web
pnpm example:web
```

Open <http://localhost:5173>. A successful local run is useful evidence, but it is not by itself a cross-browser or cross-platform support claim.

## Maintainers

See [`PLATFORMS.md`](PLATFORMS.md), [`PEERS.md`](PEERS.md), and [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md).
