// __tests__/backends/reactnative/rust-core-manager-f2b-fixes.test.js
//
// F2B fix wave for the React Native review findings (R2B + R2C):
// 1. adapter watch close() rejecting must fail destroy, never vanish;
// 2. a host bug below connections.connect surfaces as itself, never
//    connection.failed;
// 4. a backend-failure pump release that fails is routed to the manager
//    trace, never discarded;
// 5. destroying N debted managers leaves the process host scannable
//    (finding-185 sequence pin).
// Driven through the ordinary factory, the REAL binding and codec, and the
// deterministic owner.

const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(harnessOptions = {}, overrides = {}) {
  const harness = rustCoreHarness(harnessOptions)
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, overrides))
  return { harness, native: harness.native, manager, backend: manager.attachedBackend.backend }
}

async function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected the operation to fail')
    },
    error => error
  )
}

describe('F2B: react-native rust-core manager review fixes', () => {
  test('finding 1: destroy reports release-failed when an adapter watch close rejects', async () => {
    const { manager } = await openManager()
    const watch = await manager.adapterStates()
    watch.values.close = () => Promise.reject(new Error('native watch close boom'))
    const destroyed = await manager.destroy()
    expect(destroyed.state).toBe('release-failed')
    expect(destroyed.failures.length).toBeGreaterThan(0)
    expect(destroyed.failures[0].resourceKind).toBe('adapter')
  })

  test('finding 1: a failed adapter watch stop stays tracked until close succeeds', async () => {
    const { manager } = await openManager()
    const watch = await manager.adapterStates()
    let attempts = 0
    watch.values.close = () => {
      attempts += 1
      return attempts === 1
        ? Promise.reject(new Error('native watch close boom'))
        : Promise.resolve({ state: 'released', failures: [] })
    }
    const stopped = await watch.stop()
    expect(stopped.state).toBe('release-failed')
    expect(attempts).toBe(1)
    const destroyed = await manager.destroy()
    expect(destroyed.state).toBe('released')
    expect(attempts).toBe(2)
  })

  test('finding 2: a host bug below connect surfaces as itself, never connection.failed', async () => {
    const { manager, backend } = await openManager()
    const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const bug = new TypeError('host bug: undefined is not an object')
    const originalInvoke = backend.invoke.bind(backend)
    backend.invoke = async (op, args) => {
      if (op === 'connection.connect') throw bug
      return originalInvoke(op, args)
    }
    const error = await failure(manager.connect(peerId, NO_OPTIONS))
    expect(error).toBe(bug)
    backend.invoke = originalInvoke
    await manager.destroy()
  })

  test('finding 4: a failed pump release reaches the manager trace', async () => {
    const { native, manager, backend } = await openManager()
    const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const connection = await manager.connect(peerId, NO_OPTIONS)
    void connection
    native.failNext('connection.disconnect', 'platform.failure', 'platform', 'ubm-mobile.connection.disconnect')
    native.failNext('connection.disconnect', 'platform.failure', 'platform', 'ubm-mobile.connection.disconnect')
    const originalDrain = native.drain.bind(native)
    let drainFailedOnce = false
    native.drain = async (...args) => {
      if (!drainFailedOnce) {
        drainFailedOnce = true
        throw new TypeError('drain boom')
      }
      return originalDrain(...args)
    }
    native.setAdapter({ power: 'on' })
    await settle(60)
    native.drain = originalDrain
    const routed = manager
      .traces()
      .filter(record => record.transition === 'backend-failure-release-failed')
    expect(routed.length).toBeGreaterThan(0)
    expect(routed[0].cause).toBe('platform.failure')
    const destroyed = await manager.destroy()
    expect(destroyed.state).toBe('release-failed')
  })

  test('finding 5: destroying N debted managers leaves the process host scannable', async () => {
    const first = await openManager()
    const sharedNative = first.native
    const debted = await first.manager.scan(scanOptions())
    void debted
    first.native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    expect((await first.manager.destroy()).state).toBe('release-failed')
    for (let index = 0; index < 3; index += 1) {
      const opened = await openManager({ native: sharedNative }, { managerId: `manager-debt-${index}` })
      const scan = await opened.manager.scan(scanOptions())
      void scan
      opened.native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
      expect((await opened.manager.destroy()).state).toBe('release-failed')
    }
    const next = await openManager({ native: sharedNative }, { managerId: 'manager-after-debt' })
    const rescan = await next.manager.scan(scanOptions())
    expect((await rescan.stop()).state).toBe('released')
    await next.manager.destroy()
  })
})
