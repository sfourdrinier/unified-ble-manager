// __tests__/expo-legacy-error-codes.test.js
//
// Finding 133: the public Expo API reports the error codes legacy Expo did
// (4.x src/expo.ts `normalizedBackgroundErrorCode`, the association and
// restoration wrappers), with the native code as `platform.code` under the
// `expo` domain. Ported from 4.x __tests__/expo.test.js and driven through
// the Rust session: the deterministic owner answers as crates/ubm-mobile does
// (a named Android code travels as the error's platform detail).

jest.mock('../src/NativeUnifiedBleProtocolControl', () => {
  throw new Error('POISON: Expo must not load the legacy protocol control module')
})

const { BleError } = require('../src/public/errors')
const { createExpoBleManagerWithEnvironment } = require('../src/expo')
const { rustCoreHarness, environment } = require('../test-support/react-native/rust-core-harness')

const EXPO = Object.freeze({ executionEnvironment: 'development-build', nativeModuleAvailable: true })

async function expoManager(platform = 'android', overrides = {}) {
  const harness = rustCoreHarness({ platform })
  const manager = await createExpoBleManagerWithEnvironment({ ...environment(harness, overrides), expo: EXPO })
  return { native: harness.native, manager }
}

function android(code, message = 'native failure') {
  return { domain: 'android', code, message, metadata: {} }
}

describe('legacy Expo error codes (133)', () => {
  test.each([
    ['foregroundServiceNotConfigured', 'capability.unsupported', 'capability.unavailable'],
    ['foregroundServiceNotRunning', 'platform.failure', 'capability.unavailable'],
    ['foregroundServicePermissionDenied', 'permission.denied', 'permission.denied'],
    ['invalidBackgroundRequest', 'platform.failure', 'argument.invalid'],
    ['unsupportedBackground', 'capability.unsupported', 'capability.unsupported'],
    ['someFutureCode', 'platform.failure', 'platform.failure']
  ])('a native %s acquire failure is %s from the owner and %s on the Expo API', async (nativeCode, ownerCode, code) => {
    const { native, manager } = await expoManager()
    native.failNext(
      'background.acquire',
      ownerCode,
      'platform',
      'background.acquire',
      'x',
      null,
      android(nativeCode, 'Rebuild with configured notification metadata.')
    )
    await expect(
      manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code,
      operation: 'expo.background.acquire',
      platform: { domain: 'expo', code: nativeCode, safeMessage: 'Rebuild with configured notification metadata.' }
    })
    await manager.destroy()
  })

  test('an invalid lease is lifecycle.invalid-state, the owner refusing it or the registry', async () => {
    const { native, manager } = await expoManager()
    const lease = await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    native.failNext(
      'background.release',
      'platform.failure',
      'platform',
      'background.release',
      'x',
      null,
      android('invalidBackgroundLease')
    )
    await expect(lease.release()).rejects.toMatchObject({
      code: 'lifecycle.invalid-state',
      operation: 'expo.background.release',
      platform: { domain: 'expo', code: 'invalidBackgroundLease' }
    })
    native.failNext('background.release', 'ownership.denied', 'core', 'background.release')
    await expect(lease.release()).rejects.toMatchObject({
      code: 'lifecycle.invalid-state',
      platform: { domain: 'expo', code: 'invalidBackgroundLease' }
    })
    await manager.destroy()
  })

  test('the notification update keeps legacy codes too', async () => {
    const { native, manager } = await expoManager()
    await manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    native.failNext(
      'background.update-notification',
      'platform.failure',
      'platform',
      'background.update-notification',
      'x',
      null,
      android('foregroundServiceNotRunning')
    )
    await expect(manager.background.updateNotification({ title: 'Glucose 108' })).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.background.update-notification',
      platform: { domain: 'expo', code: 'foregroundServiceNotRunning' }
    })
    await manager.destroy()
  })

  test('keeps the Android foreground service unsupported on Apple', async () => {
    const { manager } = await expoManager('apple')
    await expect(
      manager.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({ code: 'capability.unsupported', operation: 'expo.background.acquire' })
    await manager.destroy()
  })

  test.each([
    ['associationCancelled', 'operation.aborted'],
    ['associationBusy', 'platform.failure'],
    ['associationFailed', 'platform.failure'],
    ['unsupportedAssociation', 'capability.unsupported']
  ])('every companion failure (%s) is capability.unavailable, as legacy reported it', async (nativeCode, ownerCode) => {
    const { native, manager } = await expoManager()
    native.failNext('companion.associate', ownerCode, 'platform', 'companion.associate', 'x', null, android(nativeCode))
    await expect(manager.association.associate({ name: 'Sensor' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.association.associate',
      platform: { domain: 'expo', code: nativeCode }
    })
    await manager.destroy()
  })

  test('companion association on Apple is capability.unavailable, as legacy reported it', async () => {
    const { manager } = await expoManager('apple')
    await expect(manager.association.associate({ name: 'Sensor' })).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.association.associate'
    })
    await manager.destroy()
  })

  test('a failed restoration claim is capability.unavailable, as legacy wrapped it', async () => {
    const { native, manager } = await expoManager('apple', {
      clientId: 'ubm-client:expo',
      hostSessionScope: 'ubm-host:expo',
      restorationAuthority: {
        namespaceValue: 'ubm-ns:expo',
        adoptionEpoch: 'epoch-1',
        clientId: 'ubm-client:expo',
        hostSessionScope: 'ubm-host:expo'
      }
    })
    native.failNext('peers.claim-restored', 'platform.failure', 'platform', 'peers.claim-restored', 'journal refused')
    await expect(manager.restoration.claim()).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.restoration.claim'
    })
    expectConsoleErrorMatching(
      '[ReactNativeRestorationCoordinator.adopt] Native restoration adoption failed:',
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'platform.failure' }) })
    )
    await manager.destroy()
  })
})
