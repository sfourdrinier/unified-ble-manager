// __tests__/backends/reactnative/r01-factory-flip.test.js
//
// R01 Phase 1: the ordinary factory resolves the production
// `UnifiedBleRustCore` TurboModule binding by default. A missing native
// module rejects `capability.unsupported` before any BLE effect; a foreign
// revision rejects `protocol.incompatible` and still closes the session.
// The TypeScript providers survive only behind the exact isolated legacy
// authorization — never as a default, never inferred.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: () => mockNativeModule
  },
  NativeModules: {}
}))

jest.mock('../../../src/NativeUnifiedBleProtocolControl', () => ({
  __esModule: true,
  default: {
    getRandomBytes: async length => new Uint8Array(length)
  }
}))

const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native-manager')
const { createReactNativeBleManager } = require('../../../src/react-native-app-manager')
const {
  RUST_CORE_CONTRACT_REVISION
} = require('../../../src/backends/reactnative/react-native-rust-core')

function record(overrides = {}) {
  return {
    ok: true,
    value: '{}',
    code: '',
    domain: '',
    operation: 'test.op',
    ...overrides
  }
}

function throwingControl() {
  return new Proxy(
    {},
    {
      get: (_target, property) => {
        throw new Error(`TypeScript control surface must not execute BLE work (touched ${String(property)})`)
      }
    }
  )
}

const ZERO_COUNTERS = {
  activeScanControllers: 0,
  scanConsumers: 0,
  chooserSessions: 0,
  connectionLeases: 0,
  physicalLinks: 0,
  databaseSnapshots: 0,
  physicalCccdEnablements: 0,
  subscriptionConsumers: 0,
  queuedOperations: 0,
  dispatchedOperations: 0,
  retainedByteBuffers: 0,
  restorationRecords: 0,
  orphanedIpcOwners: 0
}

function presentNative({ revision = RUST_CORE_CONTRACT_REVISION } = {}) {
  const calls = []
  mockNativeModule = {
    openSession: async owner => {
      calls.push(['native.openSession', owner])
      return { sessionId: 'sess-1' }
    },
    invoke: async (sessionId, op, argsJson) => {
      calls.push([op, JSON.parse(argsJson)])
      switch (op) {
        case 'adapter.state':
          return record({
            operation: op,
            value: JSON.stringify({
              availability: 'available',
              authorization: 'unknown',
              power: 'on',
              backendGeneration: 'gen-1',
              updatedAt: 123,
              safeReason: null
            })
          })
        case 'counters.describe':
          return record({ operation: op, value: JSON.stringify(ZERO_COUNTERS) })
        case 'events.take':
          return record({ operation: op, value: 'null' })
        case 'session.dispose':
          return record({ operation: op, value: JSON.stringify({ state: 'released' }) })
        default:
          throw new Error(`unexpected native op ${op}`)
      }
    },
    close: async sessionId => {
      calls.push(['native.close', sessionId])
    },
    contractRevision: async () => revision
  }
  return calls
}

function ops(calls, name) {
  return calls.filter(([op]) => op === name).map(([, args]) => args)
}

function environment(overrides = {}) {
  return {
    platform: 'android',
    control: throwingControl(),
    now: () => 1000,
    clientId: 'client-a',
    managerId: 'manager-a',
    hostSessionScope: 'scope-a',
    ...overrides
  }
}

beforeEach(() => {
  mockNativeModule = null
})

describe('R01 factory flip (production binding default)', () => {
  test('no-options environment factory opens a session on the production TurboModule', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    expect(manager).toBeDefined()
    expect(ops(calls, 'native.openSession')).toHaveLength(1)
    expect(String(ops(calls, 'native.openSession')[0])).toContain('client-a/manager-a')
    expect(ops(calls, 'adapter.state')).toHaveLength(1)
    expect(ops(calls, 'counters.describe')).toHaveLength(1)
    await manager.destroy()
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
  })

  test('missing native module rejects capability.unsupported before any BLE effect', async () => {
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.code).toBe('capability.unsupported')
    expect(String(error.message)).toContain('react-native-manager.rust-core-missing')
  })

  test('foreign revision rejects protocol.incompatible and still closes the session', async () => {
    const calls = presentNative({ revision: 'C-UBM.9.9.9-DRAFT' })
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.code).toBe('protocol.incompatible')
    expect(String(error.message)).toContain('react-native-manager.rust-core-revision')
    expect(ops(calls, 'native.openSession')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
  })

  test('legacy route requires the exact isolated authorization', async () => {
    // Wrong value: still the native route, so a missing module rejects.
    const wrong = await createReactNativeBleManagerWithEnvironment(
      environment({ legacyTypeScriptCore: 'yes-please' })
    ).then(
      () => null,
      failure => failure
    )
    expect(wrong).not.toBeNull()
    expect(wrong.code).toBe('capability.unsupported')
    // Exact value: never the native-missing path (the TS route either
    // builds or fails on its own terms, without a native module present).
    const outcome = await createReactNativeBleManagerWithEnvironment(
      environment({ legacyTypeScriptCore: 'isolated-test-reference' })
    ).then(
      manager => ({ resolved: true, manager }),
      error => ({ resolved: false, error })
    )
    if (outcome.resolved) {
      await outcome.manager.destroy()
    } else {
      expect(outcome.error.code).not.toBe('capability.unsupported')
    }
  })

  test('no-options app factory resolves the production binding without a rustCore option', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManager({
      randomBytes: length => new Uint8Array(length)
    })
    expect(manager).toBeDefined()
    expect(ops(calls, 'native.openSession')).toHaveLength(1)
    expect(ops(calls, 'adapter.state')).toHaveLength(1)
    await manager.destroy()
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
  })
})
