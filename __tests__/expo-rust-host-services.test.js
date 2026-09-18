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
})
