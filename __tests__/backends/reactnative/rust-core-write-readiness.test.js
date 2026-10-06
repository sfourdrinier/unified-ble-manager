// Apple mobile write-without-response readiness and writeWhenReady.
// Android has no queue signal, so both stay capability.unsupported.

const {
  rustCoreHarness,
  environment,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })
const WITHOUT_RESPONSE = Object.freeze({ ...NO_OPTIONS, mode: 'without-response' })

async function openManager(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  return { native: harness.native, manager, backend: manager.attachedBackend.backend }
}

async function connect(platform, manager, backend) {
  const peerId =
    platform === 'apple'
      ? backend.peerIdForNativeId(DEFAULT_PEER)
      : backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  return manager.connect(peerId, NO_OPTIONS)
}

async function discover(connection) {
  const database = await connection.discover(NO_OPTIONS)
  const snapshot = await database.snapshot()
  return { database, path: snapshot.characteristics[0].path }
}

function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected a rejection')
    },
    error => error.normalized ?? error
  )
}

describe('F13 Apple write-without-response readiness', () => {
  test('the probe returns the current queue flag and records the owner op', async () => {
    const { native, manager, backend } = await openManager('apple')
    const connection = await connect('apple', manager, backend)
    const watch = await connection.writeWithoutResponseReadiness(NO_OPTIONS)
    const first = await watch.events[Symbol.asyncIterator]().next()
    expect(first.value.kind).toBe('value')
    expect(first.value.value.ready).toBe(true)
    expect(native.opsInvoked('connection.write-readiness')).toHaveLength(1)
    expect((await watch.close()).state).toBe('released')
    await manager.destroy()
  })

  test('a later peripheralIsReady edge reaches the open watch', async () => {
    const { native, manager, backend } = await openManager('apple')
    native.setWriteReady(DEFAULT_PEER, false)
    const connection = await connect('apple', manager, backend)
    const watch = await connection.writeWithoutResponseReadiness(NO_OPTIONS)
    const iterator = watch.events[Symbol.asyncIterator]()
    expect((await iterator.next()).value.value.ready).toBe(false)
    const pending = iterator.next()
    native.setWriteReady(DEFAULT_PEER, true)
    await settle()
    expect((await pending).value.value.ready).toBe(true)
    expect((await watch.close()).state).toBe('released')
    await manager.destroy()
  })

  test('writeWhenReady waits until the queue is ready, then writes without response', async () => {
    const { native, manager, backend } = await openManager('apple')
    native.setWriteReady(DEFAULT_PEER, false)
    const connection = await connect('apple', manager, backend)
    const { database, path } = await discover(connection)
    const pending = database.writeWhenReady(path, new Uint8Array([0x2a]), WITHOUT_RESPONSE)
    await settle()
    expect(native.opsInvoked('gatt.write')).toHaveLength(0)
    native.setWriteReady(DEFAULT_PEER, true)
    const receipt = await pending
    expect(receipt.commitState).toBe('unknown')
    const writes = native.opsInvoked('gatt.write')
    expect(writes).toHaveLength(1)
    expect(writes[0].mode).toBe('without-response')
    await manager.destroy()
  })

  test('disconnect closes the watch', async () => {
    const { manager, backend } = await openManager('apple')
    const connection = await connect('apple', manager, backend)
    const watch = await connection.writeWithoutResponseReadiness(NO_OPTIONS)
    const iterator = watch.events[Symbol.asyncIterator]()
    await iterator.next()
    const pending = iterator.next()
    expect((await connection.disconnect()).state).toBe('released')
    const ended = await pending
    expect(ended.value.kind).toBe('terminal')
    expect(ended.value.reason).toBe('connection-lost')
    await manager.destroy()
  })
})

describe('F13 Android has no write-readiness signal', () => {
  test('the connection method and writeWhenReady refuse before any owner op', async () => {
    const { native, manager, backend } = await openManager('android')
    const connection = await connect('android', manager, backend)
    const { database, path } = await discover(connection)
    expect((await failure(connection.writeWithoutResponseReadiness(NO_OPTIONS))).code).toBe(
      'capability.unsupported'
    )
    expect(
      (await failure(database.writeWhenReady(path, new Uint8Array([1]), WITHOUT_RESPONSE))).code
    ).toBe('capability.unsupported')
    expect(native.opsInvoked('connection.write-readiness')).toHaveLength(0)
    expect(native.opsInvoked('gatt.write')).toHaveLength(0)
    await manager.destroy()
  })
})
