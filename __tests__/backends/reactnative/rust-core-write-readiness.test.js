// Apple mobile write-without-response readiness and writeWhenReady.
// Android has no queue signal, so both stay capability.unsupported.

const { rustCoreHarness, environment, settle } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })
const WITHOUT_RESPONSE = Object.freeze({ ...NO_OPTIONS, mode: 'without-response' })

// The TurboModule facade rejects with the private protocol's failure JSON.
function drainFailure(operation) {
  return new Error(JSON.stringify({ code: 'adapter.powered-off', domain: 'adapter', operation, detail: null }))
}

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
  test.each([false, true])(
    'atomic readiness write reports drain failure independently of refused cancellation: %s',
    async cancellationFails => {
      const { native, manager, backend } = await openManager('apple')
      native.setWriteReady(DEFAULT_PEER, false)
      const connection = await connect('apple', manager, backend)
      const { database, path } = await discover(connection)
      const pending = failure(database.writeWhenReady(path, new Uint8Array([42]), WITHOUT_RESPONSE))
      await settle()
      if (cancellationFails) native.failNext('op.cancel', 'platform.failure')
      const originalDrain = native.drain.bind(native)
      try {
        native.drain = async () => {
          throw drainFailure('test.atomic-readiness.source')
        }
        native.setWriteReady(DEFAULT_PEER, false)
        expect(await pending).toMatchObject({ code: 'adapter.powered-off', operation: 'test.atomic-readiness.source' })
        expect(native.opsInvoked('op.cancel')).toHaveLength(1)
        expect(native.completedWrites).toEqual([])
        const prior = native.opsInvoked('gatt.write-when-ready').length
        // Source failure retired this database. A new call on the ended handle
        // is stale; the already-admitted write above retains the source cause.
        expect(await failure(database.writeWhenReady(path, new Uint8Array([2]), WITHOUT_RESPONSE))).toMatchObject({
          code: 'gatt.stale-handle',
          operation: 'rust-core-gatt-database.current'
        })
        expect(native.opsInvoked('gatt.write-when-ready')).toHaveLength(prior)
      } finally {
        native.drain = originalDrain
        await manager.destroy()
        native.setWriteReady(DEFAULT_PEER, true)
        await settle()
        expect(native.completedWrites).toEqual([])
      }
    }
  )
  test('a failed source settles a held opening probe and refuses later acquisition with the original failure', async () => {
    const { native, manager, backend } = await openManager('apple')
    const connection = await connect('apple', manager, backend)
    native.hold('connection.write-readiness')
    const opening = failure(connection.writeWithoutResponseReadiness(NO_OPTIONS))
    await settle()
    const originalDrain = native.drain.bind(native)
    try {
      native.drain = async () => {
        throw drainFailure('test.readiness.opening')
      }
      native.setWriteReady(DEFAULT_PEER, true)
      expect(await opening).toMatchObject({ code: 'adapter.powered-off', operation: 'test.readiness.opening' })
      expect(backend.readinessWatches.size).toBe(0)
      const count = native.opsInvoked('connection.write-readiness').length
      expect(await failure(connection.writeWithoutResponseReadiness(NO_OPTIONS))).toMatchObject({
        code: 'adapter.powered-off',
        operation: 'test.readiness.opening'
      })
      expect(native.opsInvoked('connection.write-readiness')).toHaveLength(count)
      native.release('connection.write-readiness', { ready: true })
      await settle()
      expect(backend.readinessWatches.size).toBe(0)
    } finally {
      native.drain = originalDrain
      native.release('connection.write-readiness', { ready: true })
      await manager.destroy()
    }
  })

  test('source failure does not wait for a refused probe cancellation or disconnect cleanup', async () => {
    const { native, manager, backend } = await openManager('apple')
    const connection = await connect('apple', manager, backend)
    native.hold('connection.write-readiness')
    const opening = failure(connection.writeWithoutResponseReadiness(NO_OPTIONS))
    await settle()
    native.failNext('op.cancel', 'platform.failure')
    native.failNext('connection.disconnect', 'platform.failure')
    const originalDrain = native.drain.bind(native)
    try {
      native.drain = async () => {
        throw drainFailure('test.readiness.cleanup-debt')
      }
      native.setWriteReady(DEFAULT_PEER, true)
      expect(await opening).toMatchObject({ code: 'adapter.powered-off', operation: 'test.readiness.cleanup-debt' })
      expect(backend.readinessWatches.size).toBe(0)
      expect(native.opsInvoked('op.cancel')).toHaveLength(1)
      native.release('connection.write-readiness', { ready: true })
      await settle()
      expect(backend.readinessWatches.size).toBe(0)
    } finally {
      native.drain = originalDrain
      native.release('connection.write-readiness', { ready: true })
      await manager.destroy()
    }
  })

  test.each([false, true])(
    'source failure terminalizes readiness even when disconnect fails: %s',
    async cleanupFails => {
      const { native, manager, backend } = await openManager('apple')
      const publicManager = await createPublicBleManager(manager, () => 1000)
      const connection = await publicManager.connect(backend.peerIdForNativeId(DEFAULT_PEER))
      const iterator = connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]()
      await iterator.next()
      const pending = iterator.next()
      if (cleanupFails) native.failNext('connection.disconnect', 'platform.failure')
      const originalDrain = native.drain.bind(native)
      try {
        native.drain = async () => {
          throw drainFailure('test.readiness.source')
        }
        native.setWriteReady(DEFAULT_PEER, true)
        await expect(pending).rejects.toMatchObject({ code: 'adapter.powered-off', operation: 'test.readiness.source' })
        expect(backend.readinessWatches.size).toBe(0)
      } finally {
        native.drain = originalDrain
        await iterator.return()
        await publicManager.destroy()
      }
    }
  )

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
    const writes = native.opsInvoked('gatt.write-when-ready')
    expect(writes).toHaveLength(1)
    expect(writes[0].mode).toBe('without-response')
    await manager.destroy()
  })

  test('the native readiness operation owns bytes and preserves its FIFO position ahead of an ordinary write', async () => {
    const { native, manager, backend } = await openManager('apple')
    native.setWriteReady(DEFAULT_PEER, false)
    const connection = await connect('apple', manager, backend)
    const { database, path } = await discover(connection)
    const input = new Uint8Array([42])
    const first = database.writeWhenReady(path, input, WITHOUT_RESPONSE)
    input[0] = 99
    const second = database.write(path, new Uint8Array([2]), WITHOUT_RESPONSE)
    await settle()
    expect(native.completedWrites).toEqual([])
    native.setWriteReady(DEFAULT_PEER, true)
    await Promise.all([first, second])
    expect(native.completedWrites.map(write => [...write.value])).toEqual([[42], [2]])
    await manager.destroy()
  })

  test('native readiness waiting settles on database invalidation without another readiness event', async () => {
    const { native, manager, backend } = await openManager('apple')
    native.setWriteReady(DEFAULT_PEER, false)
    const connection = await connect('apple', manager, backend)
    const { database, path } = await discover(connection)
    const waiting = failure(database.writeWhenReady(path, new Uint8Array([1]), WITHOUT_RESPONSE))
    await settle()
    native.changeDatabase(DEFAULT_PEER)
    expect(await waiting).toMatchObject({ code: 'gatt.stale-handle' })
    expect(native.completedWrites).toEqual([])
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
    expect((await failure(connection.writeWithoutResponseReadiness(NO_OPTIONS))).code).toBe('capability.unsupported')
    expect((await failure(database.writeWhenReady(path, new Uint8Array([1]), WITHOUT_RESPONSE))).code).toBe(
      'capability.unsupported'
    )
    expect(native.opsInvoked('connection.write-readiness')).toHaveLength(0)
    expect(native.opsInvoked('gatt.write')).toHaveLength(0)
    await manager.destroy()
  })
})
