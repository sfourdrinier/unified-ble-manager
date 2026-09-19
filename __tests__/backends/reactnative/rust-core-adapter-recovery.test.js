// __tests__/backends/reactnative/rust-core-adapter-recovery.test.js
//
// Physical Samsung/Polar H10 run (2026-09-18): Bluetooth off for 12 s under a
// `createConnectionSupervisor`. The link loss was reported, the reconnect
// failed `adapter.resetting`, the supervisor waited for the adapter — and
// stayed in `waiting-for-gate` after Bluetooth came back, because its
// readiness wait timed out (10 s default) before the adapter returned and it
// then parked until a manual wake. Then `manager.destroy()` answered
// `release-failed` for the subscription the loss had already ended.
//
// Driven through the ordinary factory, the real binding and codec, the
// deterministic owner in the owner's adapter-loss order
// (crates/ubm-mobile/tests/adapter_loss.rs), the public manager and the
// public supervisor.

const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createConnectionSupervisor } = require('../../../src/public/connection-supervisor')

const RETRY = Object.freeze({ initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 })

let debug = null
async function eventually(what, predicate, timeoutMs = 3000) {
  const started = Date.now()
  while (!predicate()) {
    if (Date.now() - started > timeoutMs) throw new Error(`timed out waiting for ${what}: ${JSON.stringify(debug?.())}`)
    await new Promise(resolve => setTimeout(resolve, 5))
  }
}

async function open(platform) {
  const clock = { now: 1000 }
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => clock.now)
  const scan = await manager.scan(scanOptions())
  harness.native.emitAdvertisement()
  const advertisement = await scan.observations[Symbol.asyncIterator]().next()
  await scan.stop()
  return { clock, native: harness.native, manager, publicManager, peerId: advertisement.value.value.device.id }
}

/** The owner's order: the lost state, the link and stream ends, then the generation advance. */
function loseAdapter(native, generation) {
  native.setAdapter({ power: 'off' })
  native.dropLink(undefined, 'adapter')
  native.setAdapter({ backendGeneration: generation, adapterGeneration: generation })
}

describe.each(['android', 'apple'])('%s: adapter off and on under a supervised connection', platform => {
  test('an outage longer than the readiness window still reconnects when the adapter returns', async () => {
    const { clock, native, manager, publicManager, peerId } = await open(platform)
    const supervisor = createConnectionSupervisor(publicManager, peerId, { retry: RETRY })
    supervisor.start()
    debug = () => ({ snapshot: supervisor.snapshot, ops: native.calls.map(call => call[1]).slice(-12) })
    await eventually('the first connection', () => supervisor.snapshot.state === 'connected')

    native.failNext('connection.connect', 'adapter.resetting', 'adapter', 'connection.connect')
    loseAdapter(native, '2')
    await eventually(
      'the adapter wait',
      () =>
        supervisor.snapshot.state === 'waiting-for-gate' && supervisor.snapshot.lastError?.code === 'adapter.resetting'
    )
    // The adapter stays off past the public readiness window (10 s).
    clock.now += 11_000
    native.setAdapter({ power: 'off', safeReason: 'still off' })
    await settle(60)

    native.setAdapter({ power: 'on', safeReason: null })
    await eventually('the reconnection', () => supervisor.snapshot.state === 'connected')
    expect(native.opsInvoked('connection.connect').length).toBeGreaterThanOrEqual(3)

    // The manager follows the new generation: a later link loss on the new
    // connection is observed and the supervisor reconnects again.
    const recovered = supervisor.snapshot.connectionGeneration
    const connects = native.opsInvoked('connection.connect').length
    native.dropLink()
    await eventually(
      'the second reconnection',
      () =>
        native.opsInvoked('connection.connect').length > connects &&
        supervisor.snapshot.state === 'connected' &&
        supervisor.snapshot.connectionGeneration !== recovered
    )
    expect((await supervisor.stop()).state).toBe('released')
    expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
  })

  test('a subscription the loss ended ends source-failed, and destroy after the loss reports released', async () => {
    const { native, manager, publicManager, peerId } = await open(platform)
    const connection = await publicManager.connect(peerId)
    const database = await connection.discover()
    const characteristic = database.services[0].characteristics[0]
    const subscription = await characteristic.subscribe()
    loseAdapter(native, '2')
    await settle(60)
    const iterator = subscription.values[Symbol.asyncIterator]()
    let last
    for (;;) {
      const item = await iterator.next()
      if (item.done) break
      last = item.value
      if (item.value.kind === 'terminal') break
    }
    // One word on every host for a stream an adapter loss ended: `source-failed`
    // (legacy React Native, the desktop backends and Tauri all said it).
    expect(last).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
  })
})
