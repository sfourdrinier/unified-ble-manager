// __tests__/backends/reactnative/r01-poison.test.js
//
// Poison acceptance: the TypeScript core (`UnifiedBleCore.attach`,
// `createBleManagerFromProvider`), the legacy TypeScript providers and the
// legacy `UnifiedBleProtocolControl` module all throw when touched. The
// ordinary factories must still create, scan, cancel and destroy through the
// production `UnifiedBleRustCore` TurboModule resolution (here a deterministic
// module that speaks the real wire), and every rejection path must close the
// lease it opened without reaching the poisoned surface.

const poisonHits = []

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: name => (name === 'UnifiedBleRustCore' ? mockNativeModule : null)
  },
  NativeModules: {}
}))

jest.mock('../../../src/NativeUnifiedBleProtocolControl', () => {
  poisonHits.push('protocol-control-module')
  throw new Error('POISON: the legacy protocol control module must not be loaded on the production route')
})

jest.mock('../../../src/backends/reactnative/react-native-android-provider', () => {
  poisonHits.push('legacy-android-provider-module')
  throw new Error('POISON: the legacy Android TypeScript provider module must not be loaded')
})

jest.mock('../../../src/backends/reactnative/react-native-apple-provider', () => {
  poisonHits.push('legacy-apple-provider-module')
  throw new Error('POISON: the legacy Apple TypeScript provider module must not be loaded')
})

jest.mock('../../../src/manager/ble-manager', () => {
  const actual = jest.requireActual('../../../src/manager/ble-manager')
  return {
    ...actual,
    createBleManagerFromProvider: () => {
      poisonHits.push('ble-manager-from-provider')
      throw new Error('POISON: createBleManagerFromProvider must not run on the native route')
    }
  }
})

const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createReactNativeBleManager } = require('../../../src/react-native-app-manager')
const { UnifiedBleCore } = require('../../../src/core/unified-ble-core')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { scanOptions, settle } = require('../../../test-support/react-native/rust-core-harness')

const attachSpy = jest.spyOn(UnifiedBleCore, 'attach').mockImplementation(() => {
  poisonHits.push('unified-ble-core.attach')
  throw new Error('POISON: UnifiedBleCore.attach must not run on the native route')
})

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

beforeEach(() => {
  mockNativeModule = null
  attachSpy.mockClear()
})

afterEach(() => {
  expect(poisonHits).toEqual([])
})

describe('R01 poison acceptance (no TS core, no legacy provider, no protocol control)', () => {
  test('create/scan/cancel/destroy reach the native module without touching poison', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    mockNativeModule = native
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    expect(count(native, 'openSession')).toBe(1)
    const session = await manager.scan(scanOptions())
    expect(native.opsInvoked('scan.start')).toHaveLength(1)
    await session.stop()
    expect(native.opsInvoked('scan.stop')).toHaveLength(1)
    const peerId = manager.attachedBackend.backend.connections.peerFromAddress({
      address: 'A0:9E:1A:00:00:01',
      addressType: 'public'
    })
    native.hold('connection.connect')
    const controller = new AbortController()
    const connected = manager.connect(peerId, { signal: controller.signal, deadline: null })
    await settle()
    controller.abort()
    await expect(connected).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(native.opsInvoked('op.cancel')[0].operationId).toBe(native.opsInvoked('connection.connect')[0].operationId)
    await manager.destroy()
    expect(native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(count(native, 'closeSession')).toBe(1)
    expect(attachSpy).not.toHaveBeenCalled()
  })

  test.each(['legacyTypeScriptCore', 'control'])(
    'the removed legacy option %s is refused, never silently rerouted',
    async option => {
      const native = new DeterministicRustCoreNative({ platform: 'android' })
      mockNativeModule = native
      const error = await createReactNativeBleManagerWithEnvironment(
        environment({ [option]: option === 'control' ? {} : 'isolated-test-reference' })
      ).then(
        () => null,
        failure => failure
      )
      expect(error).toMatchObject({ code: 'argument.invalid', operation: `react-native-manager.${option}` })
      expect(native.calls).toEqual([])
    }
  )

  test('apple route reaches the native module without touching poison', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'apple' })
    mockNativeModule = native
    const manager = await createReactNativeBleManagerWithEnvironment(environment({ platform: 'apple' }))
    const session = await manager.scan(scanOptions())
    await session.stop()
    await manager.destroy()
    expect(native.opsInvoked('scan.start')).toHaveLength(1)
    expect(count(native, 'closeSession')).toBe(1)
  })

  test('a missing module rejects before any BLE effect', async () => {
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).toMatchObject({ code: 'capability.unsupported' })
    expect(attachSpy).not.toHaveBeenCalled()
  })

  test('a foreign native build rejects before any session opens', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.identity = { ...native.identity, sourceDigest: 'f'.repeat(64) }
    mockNativeModule = native
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).toMatchObject({ code: 'protocol.incompatible' })
    expect(count(native, 'openSession')).toBe(0)
    expect(count(native, 'invoke')).toBe(0)
  })

  test('the no-options app factory takes its entropy from the Rust module and destroys under poison', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    mockNativeModule = native
    const manager = await createReactNativeBleManager()
    expect(count(native, 'randomBytes')).toBe(1)
    expect(count(native, 'openSession')).toBe(1)
    await manager.destroy()
    expect(native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(count(native, 'closeSession')).toBe(1)
  })
})
