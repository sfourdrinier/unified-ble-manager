// __tests__/backends/reactnative/r01-poison.test.js
//
// R01 Phase 3: poison acceptance. `UnifiedBleCore.attach` and the legacy
// TypeScript provider entry points throw when touched; the no-options
// factory must still create/scan/cancel/destroy through the (fake) native
// binding, and the rejection paths must close admitted sessions without
// ever reaching the poisoned surface.

const poisonHits = []

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

jest.mock('../../../src/backends/reactnative/react-native-android-provider', () => {
  const actual = jest.requireActual('../../../src/backends/reactnative/react-native-android-provider')
  return {
    ...actual,
    createReactNativeAndroidBackendProvider: () => {
      poisonHits.push('legacy-android-provider')
      throw new Error('POISON: legacy Android TypeScript provider must not be constructed')
    }
  }
})

jest.mock('../../../src/backends/reactnative/react-native-apple-provider', () => {
  const actual = jest.requireActual('../../../src/backends/reactnative/react-native-apple-provider')
  return {
    ...actual,
    createReactNativeAppleBackendProvider: () => {
      poisonHits.push('legacy-apple-provider')
      throw new Error('POISON: legacy Apple TypeScript provider must not be constructed')
    }
  }
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

const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native-manager')
const { createReactNativeBleManager } = require('../../../src/react-native-app-manager')
const { UnifiedBleCore } = require('../../../src/core/unified-ble-core')
const {
  RUST_CORE_CONTRACT_REVISION
} = require('../../../src/backends/reactnative/react-native-rust-core')

const attachSpy = jest.spyOn(UnifiedBleCore, 'attach').mockImplementation(() => {
  poisonHits.push('unified-ble-core.attach')
  throw new Error('POISON: UnifiedBleCore.attach must not run on the native route')
})

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

function adapterStateRecord(op) {
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
}

function presentNative({ revision = RUST_CORE_CONTRACT_REVISION, connectGate = null } = {}) {
  const calls = []
  mockNativeModule = {
    openSession: async owner => {
      calls.push(['native.openSession', owner])
      return { sessionId: 'sess-1' }
    },
    invoke: async (sessionId, op, argsJson) => {
      const args = JSON.parse(argsJson)
      calls.push([op, args])
      switch (op) {
        case 'adapter.state':
          return adapterStateRecord(op)
        case 'counters.describe':
          return record({ operation: op, value: JSON.stringify(ZERO_COUNTERS) })
        case 'events.take':
          return record({ operation: op, value: 'null' })
        case 'scan.start':
          return record({ operation: op, value: JSON.stringify({ operationId: 'scan-op-1' }) })
        case 'scan.take':
          return record({ operation: op, value: 'null' })
        case 'scan.stop':
        case 'connection.disconnect':
          return record({ operation: op, value: JSON.stringify({ state: 'released' }) })
        case 'connection.connect':
          if (connectGate !== null) {
            await connectGate.promise
          }
          return record({
            operation: op,
            value: JSON.stringify({ peerKey: 'peerkey-1', connectionGeneration: 'conngen-1' })
          })
        case 'op.cancel':
          return record({ operation: op, value: JSON.stringify({ state: 'cancellation-requested' }) })
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

function scanOptions() {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: 8,
      byteCapacity: 65536,
      reservedControlCapacity: 4,
      overflowPolicy: 'drop-oldest'
    },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

function deferred() {
  let resolve = () => undefined
  const promise = new Promise(gate => {
    resolve = gate
  })
  return { promise, resolve }
}

async function waitForOp(calls, name, timeoutMs = 5000) {
  const startedAt = Date.now()
  while (Date.now() - startedAt < timeoutMs) {
    if (ops(calls, name).length > 0) return
    await new Promise(resolve => setTimeout(resolve, 5))
  }
  throw new Error(`timed out waiting for native op ${name}`)
}

beforeEach(() => {
  mockNativeModule = null
  poisonHits.length = 0
  attachSpy.mockClear()
})

describe('R01 poison acceptance (no TS core on the native route)', () => {
  test('create/scan/cancel/destroy reach the native binding without touching poison', async () => {
    const connectGate = deferred()
    const calls = presentNative({ connectGate })
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    try {
      expect(ops(calls, 'native.openSession')).toHaveLength(1)
      const session = await manager.scan(scanOptions())
      expect(ops(calls, 'scan.start')).toHaveLength(1)
      await session.stop()
      expect(ops(calls, 'scan.stop')).toHaveLength(1)
      const peerId = manager.attachedBackend.backend.connections.peerFromAddress({
        address: 'AA:BB:CC:DD:EE:FF',
        addressType: 'public'
      })
      const controller = new AbortController()
      const connected = manager.connect(peerId, { signal: controller.signal, deadline: null })
      await waitForOp(calls, 'connection.connect')
      controller.abort()
      await waitForOp(calls, 'op.cancel')
      connectGate.resolve()
      const connection = await connected
      expect(connection.peerId).toBe(peerId)
      const connectArgs = ops(calls, 'connection.connect')[0]
      const cancelArgs = ops(calls, 'op.cancel')[0]
      expect(cancelArgs.operationId).toBe(connectArgs.operationId)
      const release = await connection.release()
      expect(release.state).toBe('released')
      expect(ops(calls, 'connection.disconnect')).toHaveLength(1)
    } finally {
      await manager.destroy()
    }
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
    expect(attachSpy).not.toHaveBeenCalled()
    expect(poisonHits).toEqual([])
  })

  test('legacy route trips the poison (the mocks guard the real entries)', async () => {
    const error = await createReactNativeBleManagerWithEnvironment(
      environment({ legacyTypeScriptCore: 'isolated-test-reference' })
    ).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(String(error.message)).toContain('POISON')
    expect(poisonHits.length).toBeGreaterThan(0)
  })

  test('missing module rejects before any BLE effect under poison', async () => {
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.code).toBe('capability.unsupported')
    expect(attachSpy).not.toHaveBeenCalled()
    expect(poisonHits).toEqual([])
  })

  test('foreign revision rejects and releases the session under poison', async () => {
    const calls = presentNative({ revision: 'C-UBM.9.9.9-DRAFT' })
    const error = await createReactNativeBleManagerWithEnvironment(environment()).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.code).toBe('protocol.incompatible')
    expect(ops(calls, 'native.openSession')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
    expect(attachSpy).not.toHaveBeenCalled()
    expect(poisonHits).toEqual([])
  })

  test('no-options app factory creates and destroys under poison', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManager({
      randomBytes: length => new Uint8Array(length)
    })
    expect(manager).toBeDefined()
    expect(ops(calls, 'native.openSession')).toHaveLength(1)
    await manager.destroy()
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
    expect(attachSpy).not.toHaveBeenCalled()
    expect(poisonHits).toEqual([])
  })
})
