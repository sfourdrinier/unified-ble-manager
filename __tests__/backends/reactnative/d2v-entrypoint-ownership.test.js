// __tests__/backends/reactnative/d2v-entrypoint-ownership.test.js
//
// D2(v): `unified-ble-manager/react-native` and `unified-ble-manager/expo`
// are two entrypoints into ONE native implementation. Both factories open
// sessions on the production binding with no `rustCore` option; every
// manager owns exactly its session (isolated opens, paired closes);
// recreation and post-destroy calls perform no stray native work.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: () => mockNativeModule
  },
  NativeModules: {}
}))

const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native')
const { createExpoBleManagerWithEnvironment } = require('../../../src/expo')
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

function presentNative({ revision = RUST_CORE_CONTRACT_REVISION, startKey = 'operationId' } = {}) {
  const calls = []
  let nextSession = 0
  mockNativeModule = {
    openSession: async owner => {
      nextSession += 1
      const sessionId = `sess-${nextSession}`
      calls.push(['native.openSession', { owner, sessionId }])
      return { sessionId }
    },
    invoke: async (sessionId, op, argsJson) => {
      calls.push([op, { sessionId, args: JSON.parse(argsJson) }])
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
        case 'scan.start':
          return record({ operation: op, value: JSON.stringify({ [startKey]: 'scan-op-1' }) })
        case 'scan.take':
          return record({ operation: op, value: 'null' })
        case 'scan.stop':
          return record({ operation: op, value: JSON.stringify({ state: 'released' }) })
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

function environment(clientId, managerId, overrides = {}) {
  return {
    platform: 'android',
    control: {},
    now: () => 1000,
    clientId,
    managerId,
    hostSessionScope: `scope-${clientId}`,
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

beforeEach(() => {
  mockNativeModule = null
})

describe('D2(v) entrypoints into one native implementation', () => {
  test('expo entrypoint opens a session on the production binding without a rustCore option', async () => {
    const calls = presentNative()
    const manager = await createExpoBleManagerWithEnvironment(
      environment('client-expo', 'manager-expo', {
        expo: { executionEnvironment: 'development-build', nativeModuleAvailable: true }
      })
    )
    expect(manager).toBeDefined()
    expect(typeof manager.readiness).toBe('function')
    const opens = ops(calls, 'native.openSession')
    expect(opens).toHaveLength(1)
    expect(opens[0].owner).toContain('client-expo/manager-expo')
    expect(ops(calls, 'adapter.state')).toHaveLength(1)
    await manager.destroy()
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toEqual([opens[0].sessionId])
  })

  test('both entrypoints own isolated sessions with paired closes (one owner per manager)', async () => {
    const calls = presentNative()
    const native = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const expo = await createExpoBleManagerWithEnvironment(environment('client-b', 'manager-b'))
    const opens = ops(calls, 'native.openSession')
    expect(opens).toHaveLength(2)
    expect(opens[0].sessionId).not.toBe(opens[1].sessionId)
    expect(opens.map(entry => entry.owner).join(' ')).toContain('client-a/manager-a')
    expect(opens.map(entry => entry.owner).join(' ')).toContain('client-b/manager-b')
    await expo.destroy()
    await native.destroy()
    const closes = ops(calls, 'native.close')
    expect(closes).toHaveLength(2)
    expect([...closes].sort()).toEqual(opens.map(entry => entry.sessionId).sort())
  })

  test('recreation releases the old session and opens a fresh healthy one', async () => {
    const calls = presentNative()
    const first = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    const session = await first.scan(scanOptions())
    await session.stop()
    await first.destroy()
    const second = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-b'))
    const states = await second.adapterStates()
    expect(states.initial).toMatchObject({ availability: 'available' })
    await states.stop()
    await second.destroy()
    const opens = ops(calls, 'native.openSession')
    expect(opens).toHaveLength(2)
    expect(opens[0].sessionId).not.toBe(opens[1].sessionId)
    expect(ops(calls, 'native.close')).toHaveLength(2)
  })

  test('pre-aborted signal and expired deadline reject before any native effect', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    try {
      const opsAfterCreate = calls.length
      const controller = new AbortController()
      controller.abort()
      const aborted = await manager
        .scan({ ...scanOptions(), signal: controller.signal })
        .then(
          () => null,
          failure => failure
        )
      expect(aborted).not.toBeNull()
      expect(aborted.normalized.code).toBe('operation.aborted')
      const expired = await manager.scan({ ...scanOptions(), deadline: 999 }).then(
        () => null,
        failure => failure
      )
      expect(expired).not.toBeNull()
      expect(expired.normalized.code).toBe('operation.timed-out')
      expect(calls.length).toBe(opsAfterCreate)
    } finally {
      await manager.destroy()
    }
  })

  test('scan start/stop send the frozen core arg shapes (DATA-02 decimals)', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    try {
      const session = await manager.scan(scanOptions())
      const starts = ops(calls, 'scan.start')
      expect(starts).toHaveLength(1)
      expect(starts[0].args.owner).toBe('client-a')
      expect(starts[0].args.timeoutMs).toBe('2147483647')
      expect(starts[0].args.nowMs).toBe('1000')
      await session.stop()
      const stops = ops(calls, 'scan.stop')
      expect(stops).toHaveLength(1)
      expect(stops[0].args.opId).toBe('scan-op-1')
      expect(stops[0].args.nowMs).toBe('1000')
      const bounded = await manager.scan({ ...scanOptions(), deadline: 1500 })
      expect(ops(calls, 'scan.start')[1].args.timeoutMs).toBe('500')
      await bounded.stop()
    } finally {
      await manager.destroy()
    }
  })

  test('fractional clocks quantize to integer millis on the wire (DATA-02)', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(
      environment('client-a', 'manager-a', { now: () => 1000.75 })
    )
    try {
      const session = await manager.scan(scanOptions())
      const args = ops(calls, 'scan.start')[0].args
      expect(args.nowMs).toBe('1000')
      expect(args.timeoutMs).toBe('2147483647')
      await session.stop()
      expect(ops(calls, 'scan.stop')[0].args.nowMs).toBe('1000')
      const bounded = await manager.scan({ ...scanOptions(), deadline: 1500.25 })
      expect(ops(calls, 'scan.start')[1].args.timeoutMs).toBe('499')
      await bounded.stop()
    } finally {
      await manager.destroy()
    }
  })

  test('scan start accepts the staged-core op_id wire form', async () => {
    const calls = presentNative({ startKey: 'op_id' })
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    try {
      const session = await manager.scan(scanOptions())
      await session.stop()
      expect(ops(calls, 'scan.stop')[0].args.opId).toBe('scan-op-1')
    } finally {
      await manager.destroy()
    }
  })

  test('destroyed manager performs no further native work', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment('client-a', 'manager-a'))
    await manager.destroy()
    const nativeOpsAfterDestroy = calls.length
    const error = await manager.scan(scanOptions()).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('lifecycle.destroyed')
    expect(calls.length).toBe(nativeOpsAfterDestroy)
  })
})
