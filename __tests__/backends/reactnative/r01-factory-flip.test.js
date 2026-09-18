// __tests__/backends/reactnative/r01-factory-flip.test.js
//
// The ordinary factories resolve the production `UnifiedBleRustCore`
// TurboModule by default. A missing module rejects `capability.unsupported`
// before any BLE effect; a foreign native build rejects
// `protocol.incompatible` before any session opens; a foreign admission record
// still closes the session it opened. There is no legacy route to request.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: name => (name === 'UnifiedBleRustCore' ? mockNativeModule : null)
  },
  NativeModules: {}
}))

const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createReactNativeBleManager } = require('../../../src/react-native-app-manager')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')

function environment(overrides = {}) {
  return {
    platform: 'android',
    now: () => 1000,
    clientId: 'client-a',
    managerId: 'manager-a',
    hostSessionScope: 'scope-a',
    ...overrides
  }
}

function count(native, name) {
  return native.calls.filter(call => call[0] === name).length
}

async function rejection(promise) {
  return promise.then(
    () => null,
    failure => failure
  )
}

beforeEach(() => {
  mockNativeModule = null
})

describe('R01 factory flip: the Rust core is the only route', () => {
  test('no-options environment factory opens a session on the production TurboModule', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    mockNativeModule = native
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    expect(count(native, 'openSession')).toBe(1)
    expect(native.calls.find(call => call[0] === 'openSession')[2]).toBe('ubm-mobile-wire/1')
    await manager.destroy()
    expect(count(native, 'closeSession')).toBe(1)
  })

  test('missing native module rejects capability.unsupported before any BLE effect', async () => {
    const error = await rejection(createReactNativeBleManagerWithEnvironment(environment()))
    expect(error).toMatchObject({ code: 'capability.unsupported' })
  })

  test('a foreign native build rejects protocol.incompatible before any session opens', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.identity = { ...native.identity, bindingSchema: '0'.repeat(64) }
    mockNativeModule = native
    const error = await rejection(createReactNativeBleManagerWithEnvironment(environment()))
    expect(error).toMatchObject({ code: 'protocol.incompatible' })
    expect(count(native, 'openSession')).toBe(0)
  })

  test('a foreign admission revision rejects and still closes the session', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.admissionOverride = id =>
      JSON.stringify({
        sessionId: id,
        contractRevision: 'C-UBM.9.9.9-DRAFT',
        wireRevision: 'ubm-mobile-wire/1',
        buildIdentity: native.identity
      })
    mockNativeModule = native
    const error = await rejection(createReactNativeBleManagerWithEnvironment(environment()))
    expect(error).toMatchObject({ code: 'protocol.incompatible' })
    expect(count(native, 'openSession')).toBe(1)
    expect(count(native, 'closeSession')).toBe(1)
    expect(count(native, 'invoke')).toBe(0)
  })

  test('the removed legacy route cannot be requested', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    mockNativeModule = native
    const error = await rejection(
      createReactNativeBleManagerWithEnvironment(environment({ legacyTypeScriptCore: 'isolated-test-reference' }))
    )
    expect(error).toMatchObject({ code: 'argument.invalid' })
    expect(native.calls).toEqual([])
  })

  test('no-options app factory resolves the production module without a rustCore option', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    mockNativeModule = native
    const manager = await createReactNativeBleManager()
    expect(count(native, 'openSession')).toBe(1)
    await manager.destroy()
    expect(count(native, 'closeSession')).toBe(1)
  })
})
