// __tests__/backends/reactnative/rust-core-legacy-error-identity.test.js
//
// Finding 113: a radio failure reaches the app with the identity legacy
// React Native gave it (4.x `rn-android-boundary.ts` `nativeOperationFailure`):
// `platform.failure` with `{domain:'android', code:<native code>,
// metadata:{androidGattStatus}}` on Android, the `NSError` domain and code on
// Apple, and `connection.lost` for an Android link loss. Driven through the
// ordinary factory, the REAL binding and codec, and the deterministic owner
// answering exactly as crates/ubm-mobile does (tests/session.rs pins the owner).

const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openDatabase(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(
    environment(harness, platform === 'apple' ? { androidApiLevel: undefined } : {})
  )
  const backend = manager.attachedBackend.backend
  const peerId =
    platform === 'apple'
      ? backend.peerIdForNativeId(DEFAULT_PEER)
      : backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await manager.connect(peerId, NO_OPTIONS)
  const database = await connection.discover(NO_OPTIONS)
  const path = (await database.snapshot()).characteristics[0].path
  return { native: harness.native, manager, database, path }
}

async function rejection(promise) {
  try {
    await promise
  } catch (error) {
    return error
  }
  throw new Error('expected a rejection')
}

async function failure(promise) {
  return (await rejection(promise)).normalized
}

describe('legacy error identity on the Rust route (113)', () => {
  test('an Android GATT failure is platform.failure with the Android status', async () => {
    const { native, manager, database, path } = await openDatabase('android')
    native.failNext('gatt.write', 'platform.failure', 'platform', 'gatt.write', 'gatt-status: denied', 'uncertain', {
      domain: 'android',
      code: 'writeFailed',
      message: 'GATT_INSUFFICIENT_AUTHENTICATION',
      metadata: { androidGattStatus: 5 }
    })
    const error = await failure(database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'with-response' }))
    expect(error.code).toBe('platform.failure')
    expect(error.platform).toEqual({
      domain: 'android',
      code: 'writeFailed',
      safeMessage: 'GATT_INSUFFICIENT_AUTHENTICATION',
      metadata: { androidGattStatus: 5 }
    })
    await manager.destroy()
  })

  test('an Apple failure carries the NSError domain and code', async () => {
    const { native, manager, database, path } = await openDatabase('apple')
    native.failNext('gatt.read', 'platform.failure', 'platform', 'gatt.read', 'x', null, {
      domain: 'CBATTErrorDomain',
      code: '15',
      message: 'Encryption is insufficient.',
      metadata: {}
    })
    const error = await failure(database.read(path, NO_OPTIONS))
    expect(error.code).toBe('platform.failure')
    expect(error.platform).toMatchObject({ domain: 'CBATTErrorDomain', code: '15', metadata: {} })
    await manager.destroy()
  })

  test('an Android link loss is connection.lost with the native connectionLost code', async () => {
    const { native, manager, database, path } = await openDatabase('android')
    native.failNext('gatt.read', 'connection.lost', 'connection', 'gatt.read', 'link lost', null, {
      domain: 'android',
      code: 'connectionLost',
      message: 'link lost',
      metadata: { androidGattStatus: 19 }
    })
    const error = await failure(database.read(path, NO_OPTIONS))
    expect(error.code).toBe('connection.lost')
    expect(error.platform).toMatchObject({
      domain: 'android',
      code: 'connectionLost',
      metadata: { androidGattStatus: 19 }
    })
    await manager.destroy()
  })
})
