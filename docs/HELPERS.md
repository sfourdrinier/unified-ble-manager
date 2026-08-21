<!-- docs/HELPERS.md -->

# Application helpers

The normal application surface is the host-specific factory returning one
public `BleManager`. It owns operation cancellation, bounded streams, GATT
generation checks, and cleanup receipts. Custom backend authors can use the
typed helpers under `/advanced`; application code should use the façade methods
shown here.

## Find, connect, and discover

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'

const ble = await createReactNativeBleManager()
const abort = new AbortController()

try {
  const peer = await ble.find({
    query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] },
    signal: abort.signal,
    timeoutMs: 15_000,
    select: 'first'
  })

  await ble.withDiscoveredConnection(peer, { signal: abort.signal, timeoutMs: 15_000 }, async ({ gatt }) => {
    const characteristic = gatt.service(HEART_RATE_SERVICE).characteristic('2a37')
    const bytes = await characteristic.read({ signal: abort.signal, timeoutMs: 5_000 })
    consume(bytes)
  })
} finally {
  await ble.destroy()
}
```

`find()` owns and stops its scan. `withConnection()` and
`withDiscoveredConnection()` release their connection lease even when the
operation or callback fails. If both the callback and cleanup fail, the public
error bridge preserves both failures in an `AggregateError`. Scan observations
expose the advertised name as `localName`.

## Notifications

GATT objects are generation-bound views. Subscribe through the characteristic
object and always remove the returned subscription:

```ts
const characteristic = gatt.service(HEART_RATE_SERVICE).characteristic('2a37')
const subscription = await characteristic.subscribe({
  signal: abort.signal,
  timeoutMs: 10_000,
  stream: 'balanced'
})

try {
  for await (const event of subscription.values) {
    if (event.kind === 'value') consume(event.value.value)
    break
  }
} finally {
  await subscription.remove()
}
```

Values are `Uint8Array`; notification events retain their delivery mode,
monotonic observation time, and sequence number. Do not retain a GATT object
after disconnect, service change, or rediscovery.

## Host boundaries

Capabilities come from the instantiated backend. Web Bluetooth uses its
explicit chooser entrypoint. Electron renderers use
`createElectronRendererBleManager()` over the authenticated preload transport;
the low-level `ElectronRendererBleClient` is an internal boundary seam. Tauri
uses `createTauriBleManager()`; tests use
`createTauriBleManagerWithEnvironment()`.

See [`PROFILES_AND_COMMANDS.md`](PROFILES_AND_COMMANDS.md), [`WEB.md`](WEB.md),
[`ELECTRON.md`](ELECTRON.md), and [`PLATFORMS.md`](PLATFORMS.md).
