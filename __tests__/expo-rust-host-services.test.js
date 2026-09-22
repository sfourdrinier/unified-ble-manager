// __tests__/expo-rust-host-services.test.js
//
// Expo's host surfaces — the connected-device background lease, its
// notification, companion-device association and restoration claim — run on
// the manager's Rust session (`background.*`, `companion.associate`,
// `peers.restored`), never on the legacy protocol control. Driven end to end
// through the production binding and serializer over the deterministic
// `UnifiedBleRustCore` module. Ports every assertion of the former
// control-backed Expo tests.

jest.mock('../src/NativeUnifiedBleProtocolControl', () => {
  throw new Error('POISON: Expo must not load the legacy protocol control module')
})

const { BleError } = require('../src/public/errors')
const { createExpoBleManagerWithEnvironment } = require('../src/expo')
const { rustCoreHarness, environment } = require('../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../test-support/react-native/deterministic-rust-core-native')

const EXPO = Object.freeze({ executionEnvironment: 'development-build', nativeModuleAvailable: true })

async function expoManager(platform = 'android', overrides = {}) {
  const harness = rustCoreHarness({ platform })
  const manager = await createExpoBleManagerWithEnvironment({ ...environment(harness, overrides), expo: EXPO })
  return { native: harness.native, manager }
}

describe('Expo host services on the Rust session', () => {
  test('acquires and releases one explicit connected-device background lease; a second release is a no-op', async () => {
    const { native, manager } = await expoManager()
    const lease = await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await lease.release()
    await lease.release()
    expect(native.opsInvoked('background.acquire')).toEqual([
      {
        kind: 'connected-device',
        reason: 'active workout',
        operationId: expect.any(String),
        admission: expect.any(Number)
      }
    ])
    expect(native.opsInvoked('background.release')).toEqual([{ leaseId: 'fgs-lease-1' }])
    await manager.destroy()
  })

  test('the background lease belongs to the module: a manager destroy keeps it, its handle still releases it, and module invalidation ends it (87/N8)', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    const native = harness.native
    const first = await createExpoBleManagerWithEnvironment({ ...environment(harness), expo: EXPO })
    const lease = await first.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await first.destroy()
    expect(native.opsInvoked('background.release')).toEqual([])
    expect(native.heldBackgroundLeases()).toEqual(['fgs-lease-1'])

    const second = await createExpoBleManagerWithEnvironment({ ...environment(harness), expo: EXPO })
    await lease.release()
    expect(native.opsInvoked('background.release')).toEqual([{ leaseId: 'fgs-lease-1' }])
    expect(native.heldBackgroundLeases()).toEqual([])

    await second.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await second.destroy()
    expect(native.heldBackgroundLeases()).toEqual(['fgs-lease-2'])
    native.invalidate()
    expect(native.heldBackgroundLeases()).toEqual([])
  })

  test('coalesces concurrent background lease releases', async () => {
    const { native, manager } = await expoManager()
    const lease = await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await Promise.all([lease.release(), lease.release()])
    expect(native.opsInvoked('background.release')).toHaveLength(1)
    await manager.destroy()
  })

  test('updates the active notification without acquiring another lease; none without a lease', async () => {
    const { native, manager } = await expoManager()
    await expect(
      manager.background.updateNotification({ title: 'Glucose 108', body: 'Private' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.background.update-notification'
    })
    const lease = await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await manager.background.updateNotification({ title: 'Glucose 108', body: 'Private' })
    expect(native.opsInvoked('background.acquire')).toHaveLength(1)
    expect(native.opsInvoked('background.update-notification')).toEqual([
      { leaseId: 'fgs-lease-1', title: 'Glucose 108', body: 'Private' }
    ])
    await lease.release()
    await manager.destroy()
  })

  test('rejects unbounded notification text before the owner sees it', async () => {
    const { native, manager } = await expoManager()
    await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await expect(manager.background.updateNotification({ title: 'x'.repeat(257) })).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.background.update-notification'
    })
    expect(native.opsInvoked('background.update-notification')).toHaveLength(0)
    await manager.destroy()
  })

  test('the Android foreground service is unsupported on Apple, as the owner reports', async () => {
    const { manager } = await expoManager('apple')
    await expect(
      manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.background.acquire'
    })
    // Legacy Expo reported every association failure as capability.unavailable (133).
    await expect(manager.association.associate({ name: 'Sensor' })).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.association.associate'
    })
    await manager.destroy()
  })

  test('returns the associated companion device from the owner', async () => {
    const { native, manager } = await expoManager()
    await expect(manager.association.associate({ name: 'Sensor', serviceUuid: '180D' })).resolves.toEqual({
      source: 'associated',
      associationId: 7,
      peerId: DEFAULT_PEER,
      displayName: 'Sensor'
    })
    expect(native.opsInvoked('companion.associate')[0]).toMatchObject({
      name: 'Sensor',
      serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb'
    })
    await manager.destroy()
  })

  test('an owner failure keeps its code with the owner’s detail (e.g. foreground service not configured)', async () => {
    const { native, manager } = await expoManager()
    native.failNext(
      'background.acquire',
      'capability.unavailable',
      'capability',
      'ubm-mobile.background.acquire',
      'Rebuild with configured notification metadata.'
    )
    await expect(
      manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.background.acquire',
      platform: { safeMessage: 'Rebuild with configured notification metadata.' }
    })
    native.failNext('background.acquire', 'permission.denied', 'adapter', 'ubm-mobile.background.acquire')
    await expect(
      manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({ code: 'permission.denied', operation: 'expo.background.acquire' })
    await manager.destroy()
  })

  test('observes and unobserves one known peer through device presence (issue #212)', async () => {
    const { native, manager } = await expoManager()
    await expect(manager.presence.observe({ peerId: DEFAULT_PEER })).resolves.toEqual({ state: 'observing' })
    await expect(manager.presence.unobserve({ peerId: DEFAULT_PEER })).resolves.toEqual({ state: 'idle' })
    expect(native.opsInvoked('presence.observe')[0]).toMatchObject({ peerId: DEFAULT_PEER })
    expect(native.opsInvoked('presence.unobserve')[0]).toMatchObject({ peerId: DEFAULT_PEER })
    await manager.destroy()
  })

  test('presence observation is unsupported on Apple with the owner reason, and needs a known peer', async () => {
    const { manager } = await expoManager('apple')
    await expect(manager.presence.observe({ peerId: 'C0FFEE00-0000-4000-8000-000000000001' })).rejects.toMatchObject(
      {
        constructor: BleError,
        code: 'capability.unsupported',
        operation: 'expo.presence.observe'
      }
    )
    await expect(manager.presence.unobserve({ peerId: 'C0FFEE00-0000-4000-8000-000000000001' })).rejects.toMatchObject(
      {
        constructor: BleError,
        code: 'capability.unsupported',
        operation: 'expo.presence.unobserve'
      }
    )
    await manager.destroy()

    const android = await expoManager()
    await expect(android.manager.presence.observe({})).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.presence.observe'
    })
    expect(android.native.opsInvoked('presence.observe')).toHaveLength(0)
    await android.manager.destroy()
  })

  test('a malformed presence answer is protocol.malformed at the Expo boundary', async () => {
    const { native, manager } = await expoManager()
    native.hold('presence.observe')
    const pending = manager.presence.observe({ peerId: DEFAULT_PEER })
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('presence.observe', { state: 'idle' })
    await expect(pending).rejects.toMatchObject({ code: 'protocol.malformed', operation: 'expo.presence.observe' })
    await manager.destroy()
  })

  test('a malformed owner answer is protocol.malformed at the Expo boundary', async () => {
    const { native, manager } = await expoManager()
    native.hold('background.acquire')
    const pending = manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('background.acquire', { leaseId: '' })
    await expect(pending).rejects.toMatchObject({ code: 'protocol.malformed', operation: 'expo.background.acquire' })
    native.hold('companion.associate')
    const association = manager.association.associate()
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('companion.associate', { source: 'associated', associationId: 0, peerId: null, displayName: null })
    await expect(association).rejects.toMatchObject({
      code: 'protocol.malformed',
      operation: 'expo.association.result'
    })
    await manager.destroy()
  })

  test('a failed release can be retried without double-releasing', async () => {
    const { native, manager } = await expoManager()
    const lease = await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    native.failNext(
      'background.release',
      'platform.failure',
      'platform',
      'ubm-mobile.background.release',
      'stop failed'
    )
    await expect(lease.release()).rejects.toMatchObject({ constructor: BleError, operation: 'expo.background.release' })
    await expect(lease.release()).resolves.toBeUndefined()
    await expect(lease.release()).resolves.toBeUndefined()
    expect(native.opsInvoked('background.release')).toHaveLength(2)
    await manager.destroy()
  })

  test('a second associate for an associated device reports already-associated, not a duplicate', async () => {
    const { native, manager } = await expoManager()
    const first = await manager.association.associate({ name: 'Sensor', serviceUuid: '180D' })
    expect(first).toEqual({
      source: 'associated',
      associationId: 7,
      peerId: DEFAULT_PEER,
      displayName: 'Sensor'
    })
    const second = await manager.association.associate({ name: 'Sensor', serviceUuid: '180D' })
    expect(second).toEqual({
      source: 'already-associated',
      associationId: 7,
      peerId: DEFAULT_PEER,
      displayName: 'Sensor'
    })
    expect(await manager.association.list()).toEqual([
      { associationId: 7, peerId: DEFAULT_PEER, displayName: 'Sensor' }
    ])
    await manager.destroy()
  })

  test('association listing and removal round-trip; unknown ids are reported', async () => {
    const { native, manager } = await expoManager()
    expect(await manager.association.list()).toEqual([])
    const first = await manager.association.associate({ name: 'Sensor' })
    await expect(manager.association.disassociate({ associationId: 0 })).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.association.disassociate'
    })
    await expect(manager.association.disassociate({ associationId: 999 })).rejects.toMatchObject({
      constructor: BleError,
      code: 'peer.not-found',
      operation: 'expo.association.disassociate'
    })
    await expect(manager.association.disassociate({ associationId: first.associationId })).resolves.toEqual({
      state: 'disassociated',
      associationId: first.associationId
    })
    expect(await manager.association.list()).toEqual([])
    const removals = native.opsInvoked('companion.disassociate')
    expect(removals[removals.length - 1]).toMatchObject({
      associationId: first.associationId
    })
    await manager.destroy()
  })

  test('association administration is unsupported where the platform has no companion-device concept', async () => {
    const { manager } = await expoManager('apple')
    await expect(manager.association.list()).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.association.list'
    })
    await expect(manager.association.disassociate({ associationId: 4 })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.association.disassociate'
    })
    await manager.destroy()
  })

  test('a malformed association administration answer is protocol.malformed at the Expo boundary', async () => {
    const { native, manager } = await expoManager()
    native.hold('companion.list')
    const pending = manager.association.list()
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('companion.list', { associations: [{ associationId: 0, peerId: null, displayName: null }] })
    await expect(pending).rejects.toMatchObject({ code: 'protocol.malformed', operation: 'expo.association.result' })
    native.hold('companion.disassociate')
    const removal = manager.association.disassociate({ associationId: 4 })
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    native.release('companion.disassociate', { state: 'disassociated', associationId: 0 })
    await expect(removal).rejects.toMatchObject({ code: 'protocol.malformed', operation: 'expo.association.result' })
    await manager.destroy()
  })

  test('claims Apple restoration with the configured authority; unconfigured apps are told so', async () => {
    const authority = {
      namespaceValue: 'ubm-ns:expo',
      adoptionEpoch: 'epoch-1',
      clientId: 'ubm-client:expo',
      hostSessionScope: 'ubm-host:expo'
    }
    const { native, manager } = await expoManager('apple', {
      clientId: authority.clientId,
      hostSessionScope: authority.hostSessionScope,
      restorationAuthority: authority
    })
    native.seedRestored([{ peerId: 'C0FFEE00-0000-4000-8000-000000000001', connected: true }])
    const claimed = await manager.restoration.claim()
    expect(claimed.outcome).toBe('adopted')
    expect(claimed.replayRecordCount).toBe(2)
    expect(claimed.records.map(record => record.kind)).toEqual(['adapter', 'connection'])
    expect((await manager.restoration.claim()).outcome).toBe('already-consumed')
    await manager.destroy()

    const unconfigured = await expoManager('apple')
    await expect(unconfigured.manager.restoration.claim()).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.restoration.claim'
    })
    await unconfigured.manager.destroy()
  })

  describe('background continuation (BGS4)', () => {
    const NATIVE_ORDER = {
      onAppearance: 'native',
      resubscribe: [
        {
          serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
          characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
        }
      ]
    }
    const CLAIM = JSON.stringify({
      consumerCount: 1,
      batches: [
        JSON.stringify({
          more: false,
          controlLost: 0,
          records: [
            { t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: 'AEg=', delivery: 'notification' },
            {
              t: 'stream-end',
              ordinal: 2,
              consumer: 'ubm-continuation-0',
              reason: 'overflow',
              droppedItems: 5,
              droppedBytes: 100
            }
          ]
        })
      ],
      disposed: true
    })
    const STATUS = JSON.stringify({
      strategy: 'native',
      peerId: null,
      resubscribe: 1,
      malformedDeclarations: 0,
      lastWake: {
        observedAtMs: 12345,
        event: 'continuation.completed',
        strategy: 'native',
        peerAddress: 'A0:9E:1A:E9:B9:3D',
        code: null,
        reason: null
      }
    })

    async function continuationManager() {
      const harness = rustCoreHarness({ platform: 'android' })
      harness.native.declareBackgroundContinuation = async () => {}
      harness.native.claimContinuation = async () => CLAIM
      harness.native.continuationStatus = async () => STATUS
      // The harness binds eagerly; rebuild after adding the native methods.
      const { createReactNativeRustCoreBinding } = require('../src/backends/reactnative/react-native-rust-core-binding')
      const binding = createReactNativeRustCoreBinding({ platform: 'android', native: harness.native })
      const manager = await createExpoBleManagerWithEnvironment({
        ...environment(harness, { background: { continuation: NATIVE_ORDER } }),
        rustCore: binding,
        expo: EXPO
      })
      return { native: harness.native, manager }
    }

    test('declares the standing order at open and reports status and backlog with loss accounting', async () => {
      const { manager } = await continuationManager()
      const status = await manager.continuation.status()
      expect(status.strategy).toBe('native')
      expect(status.lastWake.event).toBe('continuation.completed')
      const backlog = await manager.continuation.claim()
      expect(backlog.values).toHaveLength(1)
      expect([...backlog.values[0].value]).toEqual([0, 72])
      expect(backlog.streamEnds).toEqual([
        { consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }
      ])
      expect(backlog.disposed).toBe(true)
      await manager.destroy()
    })

    test('a native module without the claim answers unsupported, never an invented backlog', async () => {
      const harness = rustCoreHarness({ platform: 'android' })
      harness.native.declareBackgroundContinuation = async () => {}
      const { createReactNativeRustCoreBinding } = require('../src/backends/reactnative/react-native-rust-core-binding')
      const binding = createReactNativeRustCoreBinding({ platform: 'android', native: harness.native })
      const manager = await createExpoBleManagerWithEnvironment({
        ...environment(harness, { background: { continuation: NATIVE_ORDER } }),
        rustCore: binding,
        expo: EXPO
      })
      await expect(manager.continuation.claim()).rejects.toMatchObject({
        code: 'capability.unsupported',
        operation: 'expo.continuation.claim'
      })
      await manager.destroy()
    })
  })
})
