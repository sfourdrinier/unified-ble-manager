// __tests__/ReactNativeRustCoreProvider.test.js
//
// F01 RN factory routing: manager creation, scan, connect, subscribe,
// timeout, and dispose execute the native Rust core through the
// binding-backed provider — never the TypeScript 4.0 manager.
//
// The fake binding below implements the exact F01 op contract documented in
// `src/backends/reactnative/react-native-rust-core-provider.ts` (the same
// contract the F01 acceptance proof implements over the packed NAPI addon).
// Every test asserts the core saw the op; the control surface is a throwing
// proxy, so any TypeScript-fallback BLE work fails the test loudly.

'use strict'

const { capacity } = require('../src/backend-contract/primitives')
const { RUST_CORE_CONTRACT_REVISION } = require('../src/backends/reactnative/react-native-rust-core')
const {
  createReactNativeRustCoreBackendProvider
} = require('../src/backends/reactnative/react-native-rust-core-provider')
const { createReactNativeBleManagerWithEnvironment } = require('../src/react-native-manager')
const { reactNativeAndroidDefaultAdapterId } = require('../src/backends/reactnative/react-native-android-provider')

const HRM_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const CHAR_USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb'

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

function createFakeCore(script = {}) {
  const calls = []
  const observations = Array.isArray(script.observations) ? [...script.observations] : []
  const notifications = Array.isArray(script.notifications) ? [...script.notifications] : []
  const session = {
    contractRevision: () => script.revision || RUST_CORE_CONTRACT_REVISION,
    invoke: async (op, args) => {
      calls.push([op, args])
      switch (op) {
        case 'adapter.state':
          return {
            availability: 'available',
            authorization: 'unknown',
            power: 'on',
            backendGeneration: 'gen-1',
            updatedAt: 123,
            safeReason: null
          }
        case 'counters.describe':
          return {
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
        case 'scan.start':
          return { operationId: 'scan-op-1' }
        case 'scan.take':
          return observations.length > 0 ? observations.shift() : null
        case 'scan.stop':
          return { state: 'released' }
        case 'connection.connect':
          return { peerKey: 'peerkey-1', connectionGeneration: 'conngen-1' }
        case 'connection.disconnect':
          return {}
        case 'gatt.discover':
          return {
            services: [
              {
                uuid: HRM_SERVICE,
                occurrence: 0,
                characteristics: [
                  {
                    uuid: HRM_MEASUREMENT,
                    occurrence: 0,
                    properties: 0x09,
                    descriptors: [{ uuid: CHAR_USER_DESCRIPTION, occurrence: 0 }]
                  }
                ]
              }
            ]
          }
        case 'gatt.read':
        case 'gatt.read-descriptor':
          return { value: new Uint8Array([0x42]) }
        case 'gatt.write':
        case 'gatt.write-descriptor':
        case 'gatt.subscribe':
          return {}
        case 'notifications.take':
          return notifications.length > 0 ? { value: notifications.shift() } : null
        case 'gatt.unsubscribe':
          return { disabled: true }
        case 'peers.resolve':
          return null
        case 'peers.known':
        case 'peers.connected':
          return []
        case 'events.take':
          return null
        case 'op.cancel':
          return { state: 'not-cancellable' }
        case 'session.dispose':
          return { state: 'released' }
        default:
          throw new Error(`unexpected core op ${op}`)
      }
    },
    close: async () => {
      calls.push(['session.close', undefined])
    }
  }
  return {
    calls,
    observations,
    notifications,
    binding: {
      openSession: async owner => {
        if (typeof owner !== 'string' || owner.length === 0) throw new Error('owner must not be empty')
        calls.push(['session.open', owner])
        return session
      }
    }
  }
}

function ops(calls, name) {
  return calls.filter(([op]) => op === name).map(([, args]) => args)
}

function scanOptions(overrides = {}) {
  return {
    filter: { serviceUuids: [HRM_SERVICE], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false },
    ...overrides
  }
}

function operationOptions(overrides = {}) {
  return { signal: null, deadline: null, correlation: 'test-correlation-1', ...overrides }
}

async function takeStreamValue(stream) {
  for await (const item of stream) {
    if (item.kind === 'value') return item.value
  }
  throw new Error('stream terminated without a value')
}

describe('React Native Rust core provider (F01 factory routing)', () => {
  test('factory creation with a binding executes the core, never the TS control', async () => {
    const fake = createFakeCore()
    const manager = await createReactNativeBleManagerWithEnvironment({
      platform: 'android',
      control: throwingControl(),
      now: () => 1000,
      clientId: 'client-a',
      managerId: 'manager-a',
      hostSessionScope: 'scope-a',
      rustCore: fake.binding
    })
    expect(manager).toBeDefined()
    expect(ops(fake.calls, 'adapter.state').length).toBe(1)
    expect(ops(fake.calls, 'counters.describe').length).toBe(1)
    expect(ops(fake.calls, 'session.open').length).toBe(1)
    await manager.destroy()
    expect(ops(fake.calls, 'session.dispose').length).toBe(1)
    expect(ops(fake.calls, 'session.close').length).toBe(1)
  })

  test('foreign core revision fails manager creation closed, never TS fallback', async () => {
    const fake = createFakeCore({ revision: 'C-UBM.9.9.9-DRAFT' })
    const error = await createReactNativeBleManagerWithEnvironment({
      platform: 'android',
      control: throwingControl(),
      now: () => 1000,
      clientId: 'client-a',
      managerId: 'manager-a',
      hostSessionScope: 'scope-a',
      rustCore: fake.binding
    }).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    // The public factory rehydrates the seam rejection as a BleError carrying
    // the frozen wire identity — never a silent TS-manager substitution.
    expect(String(error.message)).toContain('protocol.incompatible')
    expect(String(error.message)).toContain('react-native-manager.rust-core-revision')
  })

  test('scan routes through the core with the deadline passed as timeoutMs', async () => {
    const fake = createFakeCore({
      observations: [{ peerId: 'peer-1', rssi: -60, localName: 'Movesense', serviceUuids: [HRM_SERVICE] }]
    })
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    try {
      const lease = await backend.scanner.start(scanOptions({ deadline: 1500 }), 'client-a')
      expect(ops(fake.calls, 'scan.start')[0]).toMatchObject({
        serviceUuids: [HRM_SERVICE],
        timeoutMs: 500,
        duplicatePolicy: 'all'
      })
      const observation = await takeStreamValue(lease.observations)
      expect(observation.device.id).toBeDefined()
      expect(observation.rssi).toMatchObject({ state: 'present', value: -60 })
      expect(observation.localName).toMatchObject({ state: 'present', value: 'Movesense' })
      await lease.stop()
      expect(ops(fake.calls, 'scan.stop')).toEqual([{ operationId: 'scan-op-1' }])
    } finally {
      await backend.destroy()
    }
  })

  test('connect/discover/read/write/subscribe/unsubscribe/dispose execute the core', async () => {
    const fake = createFakeCore({
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }],
      notifications: [new Uint8Array([0x06, 0x40])]
    })
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    try {
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      const observation = await takeStreamValue(lease.observations)

      const connectionLease = await backend.connections.connect(
        observation.device.id,
        'client-a',
        { signal: null, deadline: null }
      )
      expect(ops(fake.calls, 'connection.connect')[0]).toMatchObject({ peerId: 'peer-1' })
      expect(connectionLease.connection.state).toBe('connected')
      // F01 regression: the core matches discover/disconnect against the
      // exact lease string connect established. The provider must resend
      // that raw core lease — never the branded public leaseId — or the
      // core resolves GATT paths against a foreign lease.
      const connectLease = ops(fake.calls, 'connection.connect')[0].lease
      expect(typeof connectLease).toBe('string')
      expect(connectLease.length).toBeGreaterThan(0)

      const database = await backend.gatt.discover(connectionLease.connection, { signal: null, deadline: null })
      expect(ops(fake.calls, 'gatt.discover')[0]).toMatchObject({ peerId: 'peer-1' })
      expect(ops(fake.calls, 'gatt.discover')[0].lease).toBe(connectLease)
      const snapshot = await database.snapshot()
      expect(snapshot.services).toHaveLength(1)
      expect(snapshot.characteristics).toHaveLength(1)
      expect(snapshot.descriptors).toHaveLength(1)
      const path = snapshot.characteristics[0].path
      expect(path.serviceUuid).toBe(HRM_SERVICE)
      // F01 regression: occurrence identities are decimal strings of the
      // core numeral (the portable snapshot layer requires
      // /^(0|[1-9][0-9]*)$/); branded labels fail public-gatt.occurrence.
      expect(String(path.serviceOccurrence)).toMatch(/^(0|[1-9][0-9]*)$/)
      expect(String(path.characteristicOccurrence)).toMatch(/^(0|[1-9][0-9]*)$/)

      const read = await backend.gatt
        .read(path, { operation: operationOptions() })
        .completion.then(result => result)
      expect([...read.value]).toEqual([0x42])
      expect(ops(fake.calls, 'gatt.read')[0]).toMatchObject({
        peerId: 'peer-1',
        selector: {
          serviceUuid: HRM_SERVICE,
          serviceOccurrence: 0,
          characteristicUuid: HRM_MEASUREMENT,
          characteristicOccurrence: 0
        }
      })

      await backend.gatt
        .write(path, { operation: operationOptions(), bytes: new Uint8Array([0x01]), mode: 'without-response' })
        .completion
      expect(ops(fake.calls, 'gatt.write')[0]).toMatchObject({ peerId: 'peer-1', mode: 'without-response' })

      const subscription = await backend.gatt
        .subscribe(path, {
          operation: operationOptions(),
          options: {
            signal: null,
            deadline: null,
            delivery: {
              itemCapacity: capacity(4),
              byteCapacity: capacity(4096),
              reservedControlCapacity: capacity(1),
              overflowPolicy: 'drop-oldest'
            }
          }
        })
        .completion
      expect(ops(fake.calls, 'gatt.subscribe')[0]).toMatchObject({ peerId: 'peer-1' })
      const notification = await takeStreamValue(subscription.notifications)
      expect([...notification.value]).toEqual([0x06, 0x40])

      const terminal = await backend.gatt
        .unsubscribe(subscription, operationOptions({ correlation: 'test-correlation-2' }))
        .completion
      expect(terminal.outcome).toBe('succeeded')
      expect(ops(fake.calls, 'gatt.unsubscribe')[0]).toMatchObject({ peerId: 'peer-1' })

      await connectionLease.release()
      expect(ops(fake.calls, 'connection.disconnect')[0]).toMatchObject({ peerId: 'peer-1' })
      expect(ops(fake.calls, 'connection.disconnect')[0].lease).toBe(connectLease)
      await lease.stop()
    } finally {
      await backend.destroy()
    }
    expect(ops(fake.calls, 'session.dispose').length).toBe(1)
  })

  test('destroyed backend refuses loudly without touching the core', async () => {
    const fake = createFakeCore()
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    await backend.destroy()
    const callsAfterDestroy = fake.calls.length
    const error = await backend.scanner.start(scanOptions(), 'client-a').then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('lifecycle.destroyed')
    expect(fake.calls.length).toBe(callsAfterDestroy)
  })
})
