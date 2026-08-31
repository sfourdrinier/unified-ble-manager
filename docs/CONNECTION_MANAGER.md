<!-- docs/CONNECTION_MANAGER.md -->

# Connection ownership and reconnect policy

Unified BLE 4.0 has one public `BleManager` and generation-bound `Connection`
handles. The shared core owns connection admission, cancellation, timeouts,
late-completion quarantine, disconnect invalidation, and deterministic cleanup.
Applications own product retry and reconnect policy.

There is no hidden retry engine or global radio singleton. The optional
`createConnectionSupervisor()` API is an explicit PR6E application-owned
supervisor with visible retry/gate state; it never runs implicitly.

## One connection lease

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const abortController = new AbortController()
const manager = await createReactNativeBleManager()
const options = { signal: abortController.signal, timeoutMs: 15_000 }

try {
  await manager.withConnection(peerId, options, async connection => {
    const database = await connection.discover(options)
    const characteristic = database.service('180f').characteristic('2a19')
    await characteristic.read(options)
  })
} finally {
  await manager.destroy()
}
```

Every database and attribute path is bound to the connection/database
generation that produced it. A reconnect creates a new generation; rediscover
and resolve fresh paths instead of reusing the previous snapshot.

## Scoped helper

`withConnection()` owns one lease and releases it on every terminal path:

```ts
const value = await manager.withConnection(peerId, options, async connection => {
  const database = await connection.discover(options)
  return readBatteryLevel(database, options)
})
```

It does not retry or reconnect.

## Application-owned retry loop

Retry only normalized errors that your product policy explicitly classifies as
transient. Create a fresh deadline for each attempt, retain cancellation across
the entire policy, and release every acquired connection before waiting.

```ts
async function connectWithProductPolicy(manager, peerId, signal) {
  let delayMilliseconds = 1_000

  for (let attempt = 1; attempt <= 5; attempt += 1) {
    if (signal.aborted) {
      throw new Error('Connection policy was aborted.')
    }

    try {
      return await manager.connect(peerId, { signal, timeoutMs: 15_000 })
    } catch (error) {
      if (!isProductRetryableBleError(error) || attempt === 5) {
        throw error
      }
      await waitForProductBackoff(delayMilliseconds, signal)
      delayMilliseconds = Math.min(delayMilliseconds * 2, 30_000)
    }
  }

  throw new Error('Connection policy exhausted without a terminal result.')
}
```

`isProductRetryableBleError()` and `waitForProductBackoff()` are intentionally
application functions: retry budgets, user intent, session state, and medical
workflow policy do not belong in the generic BLE package.

## Disconnects and lifecycle loss

Consume the public connection lifecycle stream using bounded delivery. Adapter
loss, permission loss, backend restart, and disconnect invalidate affected
resources through the shared core. Do not keep a path alive by catching an
invalidation error, and do not create a second manager to work around the
owner's lifecycle.

Apple restoration is an explicit native-owner adoption flow, not automatic
reconnect. See [`BACKGROUND.md`](BACKGROUND.md) and
[`EXPO_PLUGIN.md`](EXPO_PLUGIN.md). Electron renderer resources remain owned by
their authorized renderer lease; the main process owns the physical backend.

## Adapter interruption and recovery

An adapter power transition is not a peer disappearance. UBM publishes the
typed transition through `manager.adapter.watchState()` and invalidates active
connection generations while the adapter is unavailable. This behavior is
shared by the React Native Android, React Native Apple, BlueZ, WinRT, Electron,
Tauri, and Web backends wherever the platform exposes adapter state.

The application remains the reconnect authority:

```ts
const watch = await manager.adapter.watchState()
let prior = watch.initial
for await (const item of watch.values) {
  if (item.kind !== 'value') continue
  const current = item.value
  const becameReady = prior.power !== 'on'
    && current.power === 'on'
    && current.availability === 'available'
    && current.authorization === 'granted'
  prior = current
  if (becameReady) {
    // Resolve a fresh peer/reference and invoke the caller-owned retry policy.
  }
}
await watch.stop()
```

Do not create a second manager or reuse a connection/database from before the
adapter loss. A backend may report a cleanup retry while native operations are
settling; wait for a ready state and retry the application operation with a new
generation. Backends that cannot observe a given adapter transition expose the
truthful snapshot they have and never fabricate reconnect behavior.

## Cleanup failures

Cleanup records are data. A `release-failed` result retains retry ownership and
must be surfaced or retried by the current owner. Never discard a cleanup error
because the primary operation also failed; public helpers preserve both errors
with `AggregateError` where required.

See [`UNIFIED_SEMANTICS.md`](UNIFIED_SEMANTICS.md) for normative state-machine
rules and [`HELPERS.md`](HELPERS.md) for the helper surface.
