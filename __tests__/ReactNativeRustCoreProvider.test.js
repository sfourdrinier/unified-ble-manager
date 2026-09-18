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
      if (typeof script.onInvoke === 'function') {
        const override = await script.onInvoke(op, args)
        if (override !== undefined) return override
      }
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
          if (script.disposeError) throw script.disposeError
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
        timeoutMs: '500',
        duplicatePolicy: 'all'
      })
      const observation = await takeStreamValue(lease.observations)
      expect(observation.device.id).toBeDefined()
      expect(observation.rssi).toMatchObject({ state: 'present', value: -60 })
      expect(observation.localName).toMatchObject({ state: 'present', value: 'Movesense' })
      await lease.stop()
      expect(ops(fake.calls, 'scan.stop')).toEqual([{ opId: 'scan-op-1', nowMs: '1000' }])
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

describe('React Native Rust core provider lifecycle (R07-R13 handoff)', () => {
  const tick = () => new Promise(resolve => setTimeout(resolve, 0))
  const settle = (ms = 50) => new Promise(resolve => setTimeout(resolve, ms))

  async function makeBackend(script) {
    const fake = createFakeCore(script)
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    return { fake, provider, backend }
  }

  async function connectedPath(backend) {
    const lease = await backend.scanner.start(scanOptions(), 'client-a')
    const observation = await takeStreamValue(lease.observations)
    const connectionLease = await backend.connections.connect(
      observation.device.id,
      'client-a',
      { signal: null, deadline: null }
    )
    const database = await backend.gatt.discover(connectionLease.connection, { signal: null, deadline: null })
    const snapshot = await database.snapshot()
    return { lease, observation, connectionLease, database, path: snapshot.characteristics[0].path }
  }

  test('R07: mid-flight abort cancels by the same operationId the op was invoked with', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { fake, backend } = await makeBackend(script)
    try {
      const { path } = await connectedPath(backend)
      let releaseRead
      script.onInvoke = op => {
        if (op === 'gatt.read') {
          return new Promise(resolve => {
            releaseRead = () => resolve({ value: new Uint8Array([0x09]) })
          })
        }
        return undefined
      }
      const controller = new AbortController()
      const dispatch = backend.gatt.read(path, {
        operation: { signal: controller.signal, deadline: null, correlation: 'corr-r07-read' }
      })
      await tick()
      controller.abort()
      releaseRead()
      const result = await dispatch.completion
      expect([...result.value]).toEqual([0x09])
      // The core must be able to link the cancel to the op: the invocation
      // carries the correlation as operationId and op.cancel references it.
      expect(ops(fake.calls, 'gatt.read')[0].operationId).toBe('corr-r07-read')
      expect(ops(fake.calls, 'op.cancel')).toEqual([{ operationId: 'corr-r07-read' }])
    } finally {
      await backend.destroy()
    }
  })

  test('R07: connect forwards the abort signal with a linkable operationId', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { fake, backend } = await makeBackend(script)
    try {
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      const observation = await takeStreamValue(lease.observations)
      let releaseConnect
      script.onInvoke = op => {
        if (op === 'connection.connect') {
          return new Promise(resolve => {
            releaseConnect = () => resolve({ peerKey: 'peerkey-1', connectionGeneration: 'conngen-1' })
          })
        }
        return undefined
      }
      const controller = new AbortController()
      const pending = backend.connections.connect(observation.device.id, 'client-a', {
        signal: controller.signal,
        deadline: null
      })
      await tick()
      controller.abort()
      releaseConnect()
      const connectionLease = await pending
      expect(connectionLease.connection.state).toBe('connected')
      const connectArgs = ops(fake.calls, 'connection.connect')[0]
      expect(typeof connectArgs.operationId).toBe('string')
      expect(connectArgs.operationId.length).toBeGreaterThan(0)
      expect(ops(fake.calls, 'op.cancel')).toEqual([{ operationId: connectArgs.operationId }])
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })

  test('R07: aborting after settle sends no spurious op.cancel (no leaked listener)', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { fake, backend } = await makeBackend(script)
    try {
      const { path } = await connectedPath(backend)
      const controller = new AbortController()
      const dispatch = backend.gatt.read(path, {
        operation: { signal: controller.signal, deadline: null, correlation: 'corr-r07-settled' }
      })
      await dispatch.completion
      const cancelsBefore = ops(fake.calls, 'op.cancel').length
      controller.abort()
      await settle()
      expect(ops(fake.calls, 'op.cancel').length).toBe(cancelsBefore)
    } finally {
      await backend.destroy()
    }
  })

  test('R08/R09: foreign revision rejects closed — the admitted session is closed, never leaked', async () => {
    const script = { revision: 'C-UBM.9.9.9-DRAFT' }
    const fake = createFakeCore(script)
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const error = await provider
      .create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
      .then(
        () => null,
        failure => failure
      )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('protocol.incompatible')
    // Byte-exact session accounting: exactly one open and one close, no
    // dispose (nothing was constructed), no adapter/counters traffic.
    expect(fake.calls).toEqual([['session.open', expect.any(String)], ['session.close', undefined]])
  })

  test('R08: listAdapters failure still releases the session (no leak on the probe path)', async () => {
    const script = { revision: 'C-UBM.9.9.9-DRAFT' }
    const fake = createFakeCore(script)
    const provider = createReactNativeRustCoreBackendProvider({
      platform: 'android',
      binding: fake.binding,
      owner: 'owner-a',
      now: () => 1000,
      control: throwingControl()
    })
    const error = await provider.listAdapters().then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('protocol.incompatible')
    expect(fake.calls).toEqual([['session.open', expect.any(String)], ['session.close', undefined]])
  })

  test('R08/R11: destroy retires every owned stream (scan, notifications, adapter watch)', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }],
      notifications: [new Uint8Array([0x06])]
    }
    const { backend } = await makeBackend(script)
    const lease = await backend.scanner.start(scanOptions(), 'client-a')
    const first = await takeStreamValue(lease.observations)
    const watch = await backend.adapter.watchState()
    const lease2 = await backend.scanner.start(scanOptions(), 'client-a')
    const connectionLease = await backend.connections.connect(
      first.device.id,
      'client-a',
      { signal: null, deadline: null }
    )
    const database = await backend.gatt.discover(connectionLease.connection, { signal: null, deadline: null })
    const snapshot = await database.snapshot()
    const subscription = await backend.gatt
      .subscribe(snapshot.characteristics[0].path, {
        operation: operationOptions({ correlation: 'corr-r08-sub' }),
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
    await backend.destroy()
    expect(lease.observations.isTerminal()).toBe(true)
    expect(lease2.observations.isTerminal()).toBe(true)
    expect(subscription.notifications.isTerminal()).toBe(true)
    expect(watch.transitions.isTerminal()).toBe(true)
    expect(backend.events().isTerminal()).toBe(true)
  })

  test('R12: one malformed observation does not kill the scan or its terminal accounting', async () => {
    const script = {
      observations: [{ rssi: -70 }, { peerId: 'peer-9', rssi: -55, serviceUuids: [HRM_SERVICE] }]
    }
    const { fake, backend } = await makeBackend(script)
    try {
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      const observation = await takeStreamValue(lease.observations)
      expect(observation.rssi).toMatchObject({ state: 'present', value: -55 })
      await settle()
      // The scan survives a single bad record: no core terminal release ran.
      expect(ops(fake.calls, 'scan.stop')).toEqual([])
      await lease.stop()
      expect(ops(fake.calls, 'scan.stop')).toEqual([{ opId: 'scan-op-1', nowMs: '1000' }])
    } finally {
      await backend.destroy()
    }
  })

  test('R13: discover without a live connection lease fails stale-handle before touching the core', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { fake, backend } = await makeBackend(script)
    try {
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      const observation = await takeStreamValue(lease.observations)
      const failure = await backend.gatt
        .discover(
          { peerId: observation.device.id, connectionId: 'stale-connection' },
          { signal: null, deadline: null }
        )
        .then(
          () => null,
          error => error
        )
      expect(failure).not.toBeNull()
      expect(failure.normalized.code).toBe('gatt.stale-handle')
      expect(ops(fake.calls, 'gatt.discover')).toEqual([])
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })

  test('R13: out-of-range core bytes fail malformed instead of wrapping silently', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { backend } = await makeBackend(script)
    try {
      const { path } = await connectedPath(backend)
      script.onInvoke = op => {
        if (op === 'gatt.read') return { value: [300, -1, 1.5] }
        return undefined
      }
      const failure = await backend.gatt
        .read(path, { operation: operationOptions({ correlation: 'corr-r13-bytes' }) })
        .completion.then(
          () => null,
          error => error
        )
      expect(failure).not.toBeNull()
      expect(failure.normalized.code).toBe('protocol.malformed')
    } finally {
      await backend.destroy()
    }
  })

  test('R13: peerFromAddress rejects malformed addresses and missing address types', async () => {
    const { fake, backend } = await makeBackend({
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    })
    try {
      expect(() =>
        backend.connections.peerFromAddress({ address: 'not-an-address', addressType: 'public' })
      ).toThrow(/argument\.invalid/)
      expect(() => backend.connections.peerFromAddress({ address: 'aa:bb:cc:dd:ee:ff' })).toThrow(
        /argument\.invalid/
      )
      const peerId = backend.connections.peerFromAddress({
        address: 'aa:bb:cc:dd:ee:ff',
        addressType: 'public'
      })
      expect(peerId).toBeDefined()
      const connectionLease = await backend.connections.connect(peerId, 'client-a', {
        signal: null,
        deadline: null
      })
      expect(ops(fake.calls, 'connection.connect')[0]).toMatchObject({ peerId: 'AA:BB:CC:DD:EE:FF' })
      await connectionLease.release()
    } finally {
      await backend.destroy()
    }
  })

  test('R08: destroy resolves only after the native dispose and session close', async () => {
    const { fake, backend } = await makeBackend({})
    await backend.destroy()
    const names = fake.calls.map(([op]) => op)
    expect(names).toContain('session.dispose')
    expect(names).toContain('session.close')
    expect(names.indexOf('session.dispose')).toBeLessThan(names.indexOf('session.close'))
  })

  test('R10: database snapshots are isolated from later core-record mutation', async () => {
    const report = {
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
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { backend } = await makeBackend(script)
    try {
      script.onInvoke = op => {
        if (op === 'gatt.discover') return report
        return undefined
      }
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      const observation = await takeStreamValue(lease.observations)
      const connectionLease = await backend.connections.connect(
        observation.device.id,
        'client-a',
        { signal: null, deadline: null }
      )
      const database = await backend.gatt.discover(connectionLease.connection, { signal: null, deadline: null })
      report.services.push({ uuid: HRM_SERVICE, occurrence: 1, characteristics: [] })
      const snapshot = await database.snapshot()
      expect(snapshot.services).toHaveLength(1)
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })

  test('R10: native shutdown failure propagates verbatim instead of resolving released', async () => {
    const { contractError } = require('../src/backend-contract/errors')
    const frozen = contractError('transport.unavailable', 'core', 'fake.session.dispose')
    const { backend } = await makeBackend({ disposeError: frozen })
    const failure = await backend.destroy().then(
      () => null,
      error => error
    )
    expect(failure).toBe(frozen)
  })

  test('R08: in-flight operations still settle their caller after destroy starts', async () => {
    const script = {
      observations: [{ peerId: 'peer-1', rssi: -60, serviceUuids: [HRM_SERVICE] }]
    }
    const { backend } = await makeBackend(script)
    const { path } = await connectedPath(backend)
    let releaseRead
    script.onInvoke = op => {
      if (op === 'gatt.read') {
        return new Promise(resolve => {
          releaseRead = () => resolve({ value: new Uint8Array([0x0a]) })
        })
      }
      return undefined
    }
    const dispatch = backend.gatt.read(path, {
      operation: { signal: null, deadline: null, correlation: 'corr-r08-late' }
    })
    await tick()
    const destroyed = backend.destroy()
    releaseRead()
    const result = await dispatch.completion
    expect([...result.value]).toEqual([0x0a])
    await destroyed
  })
})
