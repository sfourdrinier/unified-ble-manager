// __tests__/backends/reactnative/d2v-entrypoint-ownership.test.js
//
// D2(v): the React Native and Expo entrypoints reach one native
// implementation — the production `UnifiedBleRustCore` TurboModule — with one
// session lease per manager, paired closes, no effect before admission, and
// the frozen `ubm-mobile-wire/1` argument shapes.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: name => (name === 'UnifiedBleRustCore' ? mockNativeModule : null)
  },
  NativeModules: {}
}))

const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native')
const { createExpoBleManagerWithEnvironment } = require('../../../src/expo')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { scanOptions } = require('../../../test-support/react-native/rust-core-harness')

function present() {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  mockNativeModule = native
  return native
}

function opens(native) {
  return native.calls.filter(call => call[0] === 'openSession').map(call => call[1])
}

function closes(native) {
  return native.calls.filter(call => call[0] === 'closeSession').map(call => call[1])
}

function environment(clientId, managerId, overrides = {}) {
  return {
    platform: 'android',
    now: () => 1000,
    clientId,
    managerId,
    hostSessionScope: `scope-${clientId}`,
    ...overrides
  }
}

beforeEach(() => {
  mockNativeModule = null
})

describe('D2(v) entrypoints into one native implementation', () => {
  test('expo entrypoint opens a session on the production module without a rustCore option', async () => {
    const native = present()
    const manager = await createExpoBleManagerWithEnvironment(
      environment('client-expo', 'manager-expo', {
        expo: { executionEnvironment: 'development-build', nativeModuleAvailable: true }
      })
    )
    expect(typeof manager.readiness).toBe('function')
    expect(opens(native)).toHaveLength(1)
    expect(opens(native)[0]).toContain('client-expo/manager-expo')
    expect(native.opsInvoked('adapter.state')).toHaveLength(1)
    await manager.destroy()
    expect(native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(closes(native)).toEqual(['1'])
  })

  test('both entrypoints own isolated sessions with paired closes (one lease per manager)', async () => {
    const native = present()
    const reactNative = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const expo = await createExpoBleManagerWithEnvironment(environment('client-b', 'manager-b'))
    expect(opens(native).join(' ')).toContain('client-a/manager-a')
    expect(opens(native).join(' ')).toContain('client-b/manager-b')
    await expo.destroy()
    await reactNative.destroy()
    expect([...closes(native)].sort()).toEqual(['1', '2'])
  })

  test('recreation releases the old session and opens a fresh healthy one', async () => {
    const native = present()
    const first = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const session = await first.scan(scanOptions())
    await session.stop()
    await first.destroy()
    const second = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-b'))
    const states = await second.adapterStates()
    expect(states.initial).toMatchObject({ availability: 'available' })
    await states.stop()
    await second.destroy()
    expect(opens(native)).toHaveLength(2)
    expect(closes(native)).toEqual(['1', '2'])
  })

  test('pre-aborted signal and expired deadline reject before any native effect', async () => {
    const native = present()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const before = native.calls.length
    const controller = new AbortController()
    controller.abort()
    await expect(manager.scan(scanOptions({ signal: controller.signal }))).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    await expect(manager.scan(scanOptions({ deadline: 999 }))).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(native.calls.length).toBe(before)
    await manager.destroy()
  })

  test('scan start/stop send the frozen wire shapes; a deadline travels as a relative budget', async () => {
    const native = present()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const session = await manager.scan(scanOptions())
    expect(native.opsInvoked('scan.start')[0]).toEqual({
      serviceUuids: [],
      duplicatePolicy: 'all',
      operationId: expect.any(String),
      admission: expect.any(Number)
    })
    await session.stop()
    expect(native.opsInvoked('scan.stop')[0]).toEqual({ operationId: 's1-scan-1' })
    const bounded = await manager.scan(scanOptions({ deadline: 1500 }))
    expect(native.opsInvoked('scan.start')[1].budgetMs).toBe(500)
    await bounded.stop()
    await manager.destroy()
  })

  test('a fractional clock yields an integer budget (fractions are never sent)', async () => {
    const native = present()
    const manager = await createReactNativeBleManagerWithEnvironment(
      environment('client-a', 'manager-a', { now: () => 1000.75 })
    )
    const bounded = await manager.scan(scanOptions({ deadline: 1500.25 }))
    expect(native.opsInvoked('scan.start')[0].budgetMs).toBe(499)
    await bounded.stop()
    await manager.destroy()
  })

  test('a scan.start answer in any other shape than the frozen one is protocol.malformed', async () => {
    const native = present()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    native.hold('scan.start')
    const started = manager.scan(scanOptions())
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('scan.start', { op_id: 'scan-op-1' })
    await expect(started).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    await manager.destroy()
  })

  test('destroyed manager performs no further native work', async () => {
    const native = present()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    await manager.destroy()
    const after = native.calls.length
    await expect(manager.scan(scanOptions())).rejects.toMatchObject({ normalized: { code: 'lifecycle.destroyed' } })
    expect(native.calls.length).toBe(after)
  })
})
