// __tests__/backends/reactnative/rust-core-control-reconcile.test.js
//
// Findings 104/105: when the owner's control queue is full, every lost
// control record (`db-changed`, `stream-end` on a live link, `security`,
// `restored`, `link` with its reason, a reconnect under a new connection
// generation) is re-read through `session.reconcile` and becomes the typed
// transition the record would have caused. Driven through the ordinary
// factory, the REAL binding and codec, and the deterministic owner.

const {
  rustCoreHarness,
  environment,
  settle,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(platform = 'android') {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const events = []
  const stream = backend.events()
  ;(async () => {
    for await (const item of stream) {
      if (item.kind !== 'value') break
      events.push(item.value)
    }
  })()
  return { native: harness.native, manager, backend, events }
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

function kinds(events) {
  return events.map(event => event.kind)
}

describe('a lost control record becomes the transition it would have caused (104/105)', () => {
  test('a lost db-changed invalidates the database and ends its stream service-changed', async () => {
    const { native, manager, backend, events } = await openManager()
    const { path, subscription } = await connectAndSubscribe(manager, backend)
    native.loseControl(() => native.changeDatabase(DEFAULT_PEER))
    await settle(120)
    expect(kinds(events)).toContain('database-changed')
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'terminal', reason: 'service-changed' })
    const reads = native.opsInvoked('gatt.read').length
    await expect(
      (async () => backend.gatt.read(path, { operation: { ...NO_OPTIONS, correlation: 'r-1' } }).completion)()
    ).rejects.toThrow('gatt.stale-handle')
    expect(native.opsInvoked('gatt.read')).toHaveLength(reads)
    expect(native.opsInvoked('session.reconcile').length).toBeGreaterThan(0)
    await manager.destroy()
  })

  test('a lost stream-end on a live link ends the stream with the owner’s reason and drop counts', async () => {
    const { native, manager, backend } = await openManager()
    const { subscription } = await connectAndSubscribe(manager, backend)
    native.loseControl(() => native.endConsumer(DEFAULT_PEER, 'overflow', 3, 12))
    await settle(120)
    const first = (await take(subscription.values)).value
    expect(first).toMatchObject({ kind: 'overflow', droppedItems: 3, droppedBytes: 12 })
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    await manager.destroy()
  })

  test('a lost security record surfaces as a bond/security event', async () => {
    const { native, manager, backend, events } = await openManager()
    await connectAndSubscribe(manager, backend)
    native.loseControl(() =>
      native.reportSecurity(DEFAULT_PEER, {
        bond: 'bonded',
        encryption: 'encrypted',
        authentication: 'authenticated',
        secureConnections: 'yes',
        pairingPossible: null
      })
    )
    await settle(120)
    expect(events).toContainEqual(
      expect.objectContaining({ kind: 'bond-security-changed', bond: 'bonded', security: 'authenticated' })
    )
    const count = events.filter(event => event.kind === 'bond-security-changed').length
    native.ingressDrop('control', 1)
    await settle(120)
    expect(events.filter(event => event.kind === 'bond-security-changed')).toHaveLength(count)
    await manager.destroy()
  })

  test('a lost restored record makes the restored peers available for adoption', async () => {
    const { native, manager, events } = await openManager('apple')
    native.loseControl(() => native.seedRestored([{ peerId: 'C0FFEE00-0000-4000-8000-000000000001', connected: true }]))
    await settle(120)
    const received = events.filter(event => event.kind === 'restoration-received')
    expect(received).toHaveLength(1)
    expect(received[0].record.peers).toEqual([expect.objectContaining({ connected: true })])
    native.ingressDrop('control', 1)
    await settle(120)
    expect(events.filter(event => event.kind === 'restoration-received')).toHaveLength(1)
    await manager.destroy()
  })

  // An adapter loss ends the link `connected -> lost` (reason adapter) and its
  // streams `source-failed`, as the legacy adapter-loss cleanup reported it.
  test.each([
    ['local', 'disconnected', 'connection-lost'],
    ['adapter', 'connection-state-changed', 'source-failed'],
    ['peer', 'connection-lost', 'connection-lost']
  ])('a lost link record (%s) ends the link with its own reason', async (reason, kind, terminal) => {
    const { native, manager, backend, events } = await openManager()
    const { subscription } = await connectAndSubscribe(manager, backend)
    native.loseControl(() => native.dropLink(DEFAULT_PEER, reason))
    await settle(120)
    const ended = events.filter(
      event =>
        event.kind === 'disconnected' || event.kind === 'connection-lost' || event.kind === 'connection-state-changed'
    )
    expect(ended).toHaveLength(1)
    expect(ended[0].kind).toBe(kind)
    if (kind !== 'connection-lost') expect(ended[0].reason).toBe(reason)
    if (kind === 'connection-state-changed') expect(ended[0]).toMatchObject({ previous: 'connected', current: 'lost' })
    expect((await take(subscription.values)).value).toMatchObject({ kind: 'terminal', reason: terminal })
    await manager.destroy()
  })

  test('a reconnect under a new generation ends the held link the lost record ended', async () => {
    const { native, manager, backend, events } = await openManager()
    const { connection } = await connectAndSubscribe(manager, backend)
    native.loseControl(() => {
      native.dropLink(DEFAULT_PEER, 'peer')
      native.reconnect(DEFAULT_PEER)
    })
    await settle(120)
    expect(kinds(events)).toContain('connection-lost')
    await expect(connection.discover(NO_OPTIONS)).rejects.toThrow('connection.stale')
    await manager.destroy()
  })
})
