// __tests__/backends/reactnative/rust-core-diagnostic-surfacing.test.js
//
// Finding 102: every `diagnostic-warning` the React Native Rust provider
// emits reaches the public API. A fact with a typed home (data lost on a
// stream, a lifecycle record the owner could not queue) surfaces on that
// typed channel; every warning also enters the public diagnostic trace
// (`manager.diagnostics.snapshot().trace`) as `diagnostic-warning:<code>`.
// Driven through the ordinary factory, the REAL binding and codec, and the
// deterministic owner.

const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(platform = 'android') {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  return { native: harness.native, manager, publicManager, backend: manager.attachedBackend.backend }
}

function warningEvents(publicManager) {
  return publicManager.diagnostics
    .snapshot()
    .trace.records.filter(record => record.event.startsWith('diagnostic-warning:'))
    .map(record => record.event)
}

async function connectAndSubscribe(manager, backend) {
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await manager.connect(peerId, NO_OPTIONS)
  const database = await connection.discover(NO_OPTIONS)
  const path = (await database.snapshot()).characteristics[0].path
  const subscription = await database.subscribe(path, subscribeOptions())
  return { connection, database, path, subscription }
}

async function take(stream) {
  return stream[Symbol.asyncIterator]().next()
}

function onlySession(native) {
  const [session] = native.liveSessions()
  return session
}

describe('every diagnostic warning reaches the public diagnostic trace', () => {
  test('owner records this backend does not hold', async () => {
    const { native, manager, publicManager } = await openManager()
    const session = onlySession(native)
    native.push(session, { t: 'value', consumer: 'nobody', valueB64: 'AQ==', delivery: 'notification' })
    native.push(session, {
      t: 'link',
      peerId: DEFAULT_PEER,
      connectionGeneration: 'unknown-generation',
      databaseGeneration: null,
      reason: 'peer'
    })
    native.push(session, {
      t: 'db-changed',
      peerId: DEFAULT_PEER,
      connectionGeneration: 'unknown-generation',
      databaseGeneration: 'db-x'
    })
    native.push(session, { t: 'scan-end', operationId: 's9-scan-9', reason: 'source-failed' })
    await settle(60)
    expect(warningEvents(publicManager)).toEqual([
      'diagnostic-warning:unmatched-notification',
      'diagnostic-warning:unmatched-link',
      'diagnostic-warning:unmatched-database-change',
      'diagnostic-warning:unmatched-scan-end'
    ])
    await manager.destroy()
  })

  test('a native ingress drop of every class', async () => {
    const { native, manager, publicManager } = await openManager()
    native.ingressDrop('advertisement', 2)
    native.ingressDrop('notification', 1)
    native.ingressDrop('control', 1)
    await settle(80)
    const warnings = warningEvents(publicManager)
    expect(warnings.filter(event => event === 'diagnostic-warning:native-ingress-drop')).toHaveLength(3)
    await manager.destroy()
  })

  test('a refused op.cancel', async () => {
    const { native, manager, publicManager, backend } = await openManager()
    const { database, path } = await connectAndSubscribe(manager, backend)
    native.hold('gatt.read')
    native.failNext('op.cancel', 'platform.failure')
    const controller = new AbortController()
    const read = database.read(path, { signal: controller.signal, deadline: null }).catch(error => error)
    await settle()
    controller.abort()
    await settle(60)
    native.release('gatt.read', { valueB64: 'AA==' })
    await read
    const records = publicManager.diagnostics.snapshot().trace.records
    expect(records).toContainEqual(
      expect.objectContaining({
        kind: 'attachment',
        event: 'diagnostic-warning:cancel-failed',
        cause: 'platform.failure'
      })
    )
    await manager.destroy()
  })

  test('a scan its signal ended that the owner could not release', async () => {
    const { native, manager, publicManager } = await openManager()
    const controller = new AbortController()
    const scan = await manager.scan(scanOptions({ signal: controller.signal }))
    native.failNext('scan.stop', 'platform.failure')
    controller.abort()
    await settle(60)
    expect(warningEvents(publicManager)).toContain('diagnostic-warning:scan-cleanup-requires-retry')
    expect((await scan.stop()).state).toBe('released')
    await manager.destroy()
  })

  test('a counters refresh the owner did not answer', async () => {
    const { native, manager, publicManager } = await openManager()
    native.failNext('counters.describe', 'platform.failure')
    const scan = await manager.scan(scanOptions())
    await settle(20)
    const records = publicManager.diagnostics.snapshot().trace.records
    expect(records).toContainEqual(
      expect.objectContaining({ event: 'diagnostic-warning:counters-refresh-failed', cause: 'platform.failure' })
    )
    await scan.stop()
    await manager.destroy()
  })
})

describe('a data-loss fact is typed on the stream that lost it, never diagnostic-only', () => {
  test('advertisement ingress drops accumulate on the scan stream’s drop accounting', async () => {
    const { native, manager } = await openManager()
    const scan = await manager.scan(scanOptions())
    native.ingressDrop('advertisement', 2)
    await settle(60)
    expect((await take(scan.observations)).value).toMatchObject({ kind: 'overflow', droppedItems: 2 })
    native.ingressDrop('advertisement', 3)
    await settle(60)
    expect((await take(scan.observations)).value).toMatchObject({ kind: 'overflow', droppedItems: 5 })
    await scan.stop()
    await manager.destroy()
  })

  test('notification ingress drops reach every live subscription’s drop accounting', async () => {
    const { native, manager, backend } = await openManager()
    const { subscription } = await connectAndSubscribe(manager, backend)
    native.ingressDrop('notification', 1)
    await settle(60)
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'overflow', droppedItems: 1 })
    native.ingressDrop('notification', 4)
    await settle(60)
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'overflow', droppedItems: 5 })
    native.emitNotification(new Uint8Array([9]))
    await settle(60)
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'value' })
    await manager.destroy()
  })
})

describe('a lost control record is reconciled from the owner into typed transitions', () => {
  test('a link whose `link` record was lost ends connection-lost, with its subscription', async () => {
    const { native, manager, backend } = await openManager()
    const { connection, subscription } = await connectAndSubscribe(manager, backend)
    const lifecycle = []
    const reader = (async () => {
      for await (const item of connection.events) {
        if (item.kind !== 'value') break
        lifecycle.push(item.value.cause)
      }
    })()
    const session = onlySession(native)
    for (const lease of session.leases.values()) lease.connected = false
    session.consumers.clear()
    native.ingressDrop('control', 2)
    await settle(120)
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
    await reader
    expect(lifecycle).toEqual(['connected', 'peer-link-loss'])
    expect(native.opsInvoked('session.reconcile').length).toBeGreaterThan(0)
    await manager.destroy()
  })

  test('a re-read the owner does not answer is itself reported', async () => {
    const { native, manager, publicManager } = await openManager()
    native.failNext('session.reconcile', 'platform.failure')
    native.ingressDrop('control', 1)
    await settle(120)
    expect(publicManager.diagnostics.snapshot().trace.records).toContainEqual(
      expect.objectContaining({ event: 'diagnostic-warning:control-reconcile-failed', cause: 'platform.failure' })
    )
    await manager.destroy()
  })

  test('a live link the owner still reports connected is left alone', async () => {
    const { native, manager, backend } = await openManager()
    const { subscription } = await connectAndSubscribe(manager, backend)
    native.ingressDrop('control', 1)
    await settle(120)
    native.emitNotification(new Uint8Array([7]))
    await settle(60)
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'value' })
    await manager.destroy()
  })

  test('a scan whose `scan-end` record was lost ends source-failed', async () => {
    const { native, manager } = await openManager()
    const scan = await manager.scan(scanOptions())
    onlySession(native).scans.clear()
    native.ingressDrop('control', 1)
    await settle(120)
    expect((await take(scan.observations)).value).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    expect((await scan.stop()).state).toBe('released')
    expect(native.opsInvoked('scan.stop')).toHaveLength(0)
    await manager.destroy()
  })

  test('an adapter change whose `adapter` record was lost reaches adapter watches', async () => {
    const { native, manager } = await openManager()
    const watch = await manager.adapterStates()
    native.adapter = { ...native.adapter, power: 'off', safeReason: 'bluetooth off' }
    native.ingressDrop('control', 1)
    await settle(120)
    expect((await take(watch.values)).value.value).toMatchObject({ power: 'off', safeReason: 'bluetooth off' })
    await watch.stop()
    await manager.destroy()
  })
})
