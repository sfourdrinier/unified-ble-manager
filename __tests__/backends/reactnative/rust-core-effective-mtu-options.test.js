// __tests__/backends/reactnative/rust-core-effective-mtu-options.test.js
//
// FX9: `effectiveMtu` honours caller options on the React Native Rust route,
// like every sibling radio read. A pre-aborted signal fails before any native
// call, an in-flight abort cancels the owner operation by admission, and a
// deadline travels as `budgetMs` (an expired one never reaches the owner).

const {
  rustCoreHarness,
  environment,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(harnessOptions = {}, overrides = {}) {
  const harness = rustCoreHarness({ platform: 'android', ...harnessOptions })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, overrides))
  return { harness, native: harness.native, manager, backend: manager.attachedBackend.backend }
}

async function connectAndroid(manager, backend) {
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  return manager.connect(peerId, NO_OPTIONS)
}

function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected a rejection')
    },
    error => error.normalized ?? error
  )
}

describe('FX9: effectiveMtu honours caller options on the Rust route', () => {
  test('a pre-aborted signal fails before any native call', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectAndroid(manager, backend)
    const controller = new AbortController()
    controller.abort()
    expect((await failure(connection.effectiveMtu({ signal: controller.signal, deadline: null }))).code).toBe(
      'operation.aborted'
    )
    expect(native.opsInvoked('connection.effective-mtu')).toHaveLength(0)
    await manager.destroy()
  })

  test('an in-flight abort cancels the owner operation by admission', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectAndroid(manager, backend)
    native.hold('connection.effective-mtu')
    const controller = new AbortController()
    const pending = connection.effectiveMtu({ signal: controller.signal, deadline: null })
    await settle()
    const [invoked] = native.opsInvoked('connection.effective-mtu')
    expect(typeof invoked.operationId).toBe('string')
    controller.abort()
    expect((await failure(pending)).code).toBe('operation.aborted')
    expect(native.opsInvoked('op.cancel')).toEqual([{ operationId: invoked.operationId, admission: invoked.admission }])
    await manager.destroy()
  })

  test('an expired deadline never reaches the owner; a live one travels as budgetMs', async () => {
    let now = 1000
    const { native, manager, backend } = await openManager({}, { now: () => now })
    const connection = await connectAndroid(manager, backend)
    await connection.effectiveMtu({ signal: null, deadline: 1500 })
    expect(native.opsInvoked('connection.effective-mtu')[0].budgetMs).toBe(500)
    now = 2000
    const expired = await failure(connection.effectiveMtu({ signal: null, deadline: 1999 }))
    expect(expired.code).toBe('operation.timed-out')
    expect(native.opsInvoked('connection.effective-mtu')).toHaveLength(1)
    await manager.destroy()
  })
})
