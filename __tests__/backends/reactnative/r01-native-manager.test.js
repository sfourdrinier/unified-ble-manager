// __tests__/backends/reactnative/r01-native-manager.test.js
//
// R01 Phase 2: the native-owned manager implements the internal manager
// surface over the admitted session + binding-backed backend without
// constructing `UnifiedBleCore`. This suite pins (1) the module boundary
// (no runtime lifecycle imports), (2) prototype parity with the internal
// handle classes (presence + arity, so the structural casts cannot drift),
// and (3) verb routing (each verb emits the expected native ops).

const fs = require('fs')
const path = require('path')

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
} = require('../../../src/react-native-manager')
const {
  RUST_CORE_CONTRACT_REVISION
} = require('../../../src/backends/reactnative/react-native-rust-core')
const InternalShapes = require('../../../src/manager/ble-manager')

const MANAGER_SOURCE = path.join(
  __dirname,
  '../../../src/backends/reactnative/react-native-rust-core-manager.ts'
)

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
      const args = JSON.parse(argsJson)
      calls.push([op, args])
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
          return record({ operation: op, value: JSON.stringify({ operationId: 'scan-op-1' }) })
        case 'scan.take':
          return record({ operation: op, value: 'null' })
        case 'scan.stop':
        case 'connection.disconnect':
          return record({ operation: op, value: JSON.stringify({ state: 'released' }) })
        case 'connection.connect':
          return record({
            operation: op,
            value: JSON.stringify({ peerKey: 'peerkey-1', connectionGeneration: 'conngen-1' })
          })
        case 'gatt.discover':
          return record({
            operation: op,
            value: JSON.stringify({
              services: [
                {
                  uuid: '0000180d-0000-1000-8000-00805f9b34fb',
                  occurrence: 0,
                  characteristics: [
                    {
                      uuid: '00002a37-0000-1000-8000-00805f9b34fb',
                      occurrence: 0,
                      properties: 9,
                      descriptors: [{ uuid: '00002901-0000-1000-8000-00805f9b34fb', occurrence: 0 }]
                    }
                  ]
                }
              ]
            })
          })
        case 'gatt.read':
        case 'gatt.read-descriptor':
          return record({ operation: op, value: JSON.stringify({ value: [0x42] }) })
        case 'gatt.write':
        case 'gatt.write-descriptor':
        case 'gatt.subscribe':
          return record({ operation: op, value: JSON.stringify({}) })
        case 'notifications.take':
          return record({ operation: op, value: 'null' })
        case 'gatt.unsubscribe':
          return record({ operation: op, value: JSON.stringify({ disabled: true }) })
        case 'op.cancel':
          return record({ operation: op, value: JSON.stringify({ state: 'not-cancellable' }) })
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

function prototypeMembers(klass) {
  return Object.getOwnPropertyNames(klass.prototype)
    .filter(name => name !== 'constructor')
    .map(name => {
      const descriptor = Object.getOwnPropertyDescriptor(klass.prototype, name)
      const kind = descriptor.get !== undefined ? 'getter' : 'method'
      return { name, kind, arity: kind === 'method' ? descriptor.value.length : 0 }
    })
}

function expectParity(adapter, klass, label) {
  const problems = []
  for (const { name, kind, arity } of prototypeMembers(klass)) {
    if (typeof adapter[name] === 'undefined') {
      problems.push(`${label}.${name} missing`)
      continue
    }
    if (kind === 'method') {
      if (typeof adapter[name] !== 'function') problems.push(`${label}.${name} not a method`)
      else if (adapter[name].length !== arity) {
        problems.push(`${label}.${name} arity ${adapter[name].length} !== ${arity}`)
      }
    }
  }
  expect(problems).toEqual([])
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

describe('R01 native-owned manager', () => {
  test('module boundary: no runtime lifecycle imports', () => {
    const source = fs.readFileSync(MANAGER_SOURCE, 'utf8')
    const runtimeImports = [...source.matchAll(/^import (?!\s*type)(?:[^;]+?)from '([^']+)'/gm)]
      .map(match => match[1])
      .sort()
    expect(runtimeImports).toEqual(
      [
        '../../backend-contract/backend',
        '../../backend-contract/capabilities',
        '../../backend-contract/errors',
        '../../backend-contract/primitives',
        '../../backend-contract/serializable',
        '../../core/bounded-stream',
        '../../core/connection-lifecycle-rules',
        '../../core/core-capabilities',
        '../../core/gatt-path-equality',
        '../../core/unified-ble-core-helpers',
        '../../diagnostics/trace-format'
      ].sort()
    )
    for (const spec of runtimeImports) {
      expect(spec).not.toMatch(/manager\/ble-manager/)
      expect(spec).not.toMatch(/manager\/consumer-handles/)
      expect(spec).not.toMatch(/manager\/manager-ownership-authority/)
      expect(spec).not.toMatch(/core\/unified-ble-core$/)
      expect(spec).not.toMatch(/core\/core-gatt-handles/)
      expect(spec).not.toMatch(/core\/subscription-registry/)
      expect(spec).not.toMatch(/core\/operation-coordinator/)
      expect(spec).not.toMatch(/core\/core-backend-event-stream/)
    }
  })

  test('manager parity + scan routing through the native session', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    try {
      expectParity(manager, InternalShapes.BleManager, 'manager')
      expect(manager.state).toBe('ready')
      expect(manager.ownerMode).toBe('owning')
      const session = await manager.scan(scanOptions())
      expectParity(session, InternalShapes.ScanSession, 'scanSession')
      expect(ops(calls, 'scan.start')).toHaveLength(1)
      await session.stop()
      expect(ops(calls, 'scan.stop')).toHaveLength(1)
      const states = await manager.adapterStates()
      expect(states.initial).toMatchObject({ availability: 'available' })
      await states.stop()
    } finally {
      await manager.destroy()
    }
    expect(ops(calls, 'session.dispose')).toHaveLength(1)
    expect(ops(calls, 'native.close')).toHaveLength(1)
  })

  test('connection + GATT routing with handle parity', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    try {
      const peerId = manager.attachedBackend.backend.connections.peerFromAddress({
        address: 'AA:BB:CC:DD:EE:FF',
        addressType: 'public'
      })
      const connection = await manager.connect(peerId, { signal: null, deadline: null })
      expectParity(connection, InternalShapes.Connection, 'connection')
      expect(ops(calls, 'connection.connect')).toHaveLength(1)
      expect(connection.peerId).toBe(peerId)
      const database = await connection.discover({ signal: null, deadline: null })
      expectParity(database, InternalShapes.DiscoveredGattDatabase, 'database')
      expect(ops(calls, 'gatt.discover')).toHaveLength(1)
      const snapshot = await database.snapshot()
      expect(snapshot.characteristics).toHaveLength(1)
      const portable = snapshot.characteristics[0].path
      const value = await database.read(portable, { signal: null, deadline: null })
      expect([...value]).toEqual([0x42])
      expect(ops(calls, 'gatt.read')).toHaveLength(1)
      await database.write(portable, new Uint8Array([0x01]), { signal: null, deadline: null, mode: 'with-response' })
      expect(ops(calls, 'gatt.write')).toHaveLength(1)
      const subscription = await database.subscribe(portable, {
        signal: null,
        deadline: null,
        delivery: { itemCapacity: 4, byteCapacity: 4096, reservedControlCapacity: 1, overflowPolicy: 'drop-oldest' }
      })
      expectParity(subscription, InternalShapes.Subscription, 'subscription')
      expect(ops(calls, 'gatt.subscribe')).toHaveLength(1)
      await subscription.remove()
      expect(ops(calls, 'gatt.unsubscribe')).toHaveLength(1)
      const release = await connection.release()
      expect(release.state).toBe('released')
      expect(ops(calls, 'connection.disconnect')).toHaveLength(1)
      expect(connection.isReleased()).toBe(true)
    } finally {
      await manager.destroy()
    }
  })

  test('lifecycle event projection + unimplemented surface fails closed', async () => {
    const calls = presentNative()
    const manager = await createReactNativeBleManagerWithEnvironment(environment())
    try {
      const peerId = manager.attachedBackend.backend.connections.peerFromAddress({
        address: 'AA:BB:CC:DD:EE:FF',
        addressType: 'random'
      })
      const connection = await manager.connect(peerId, { signal: null, deadline: null })
      const seen = []
      const reader = (async () => {
        for await (const item of connection.events) {
          if (item.kind !== 'value') break
          seen.push(item.value)
          if (seen.length >= 2) break
        }
      })()
      await connection.disconnect()
      await reader
      expect(seen.map(event => event.cause)).toEqual(['connected', 'requested-disconnect'])
      expect(seen[0].sequence).toBe(1)
      const rssiError = await connection.readRssi({ signal: null, deadline: null }).then(
        () => null,
        failure => failure
      )
      expect(rssiError.normalized.code).toBe('capability.unsupported')
      const transferError = await manager.transferOwnership({}).then(
        () => null,
        failure => failure
      )
      expect(transferError.normalized.code).toBe('ownership.denied')
      expect(manager.acceptsOwnershipTransfer()).toBe(false)
      expect(ops(calls, 'connection.disconnect')).toHaveLength(1)
    } finally {
      await manager.destroy()
    }
  })
})
