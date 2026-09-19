jest.mock('../src/react-native-manager', () => ({
  createReactNativeManagerHost: jest.fn()
}))

jest.mock('../src/react-native-app-manager', () => ({
  createReactNativeApplicationHost: jest.fn()
}))

jest.mock('../src/expo-native-runtime', () => ({
  getNativeUnifiedBleExpoRuntime: jest.fn()
}))

jest.mock('../src/public/ble-manager', () => ({
  createPublicBleManager: jest.fn(internal => internal)
}))

jest.mock('react-native', () => ({
  Platform: { OS: 'ios', Version: '18.0' },
  TurboModuleRegistry: { get: () => ({}) },
  NativeModules: {}
}))

const { BleError } = require('../src/public/errors')
const { createExpoBleManagerWithEnvironment } = require('../src/expo')
const { createReactNativeManagerHost } = require('../src/react-native-manager')

function adapterState(overrides = {}) {
  return {
    availability: 'available',
    authorization: 'not-determined',
    power: 'unknown',
    backendGeneration: 'generation',
    updatedAt: 1,
    safeReason: null,
    ...overrides
  }
}

function environment(expo, platform = 'apple') {
  return {
    platform,
    now: () => 1,
    clientId: 'client',
    managerId: 'manager',
    hostSessionScope: 'scope',
    expo
  }
}

/** A Rust-route host around a stub manager. */
function hostFor(manager) {
  return { manager, services: {}, claimRestoration: jest.fn() }
}

function appleGrantedResult() {
  return {
    requested: ['bluetooth'],
    granted: ['bluetooth'],
    denied: [],
    recommendedSettingsTarget: null
  }
}

async function managerWithBridge(permissionBridge) {
  const manager = {
    adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
  }
  createReactNativeManagerHost.mockResolvedValue(hostFor(manager))
  return createExpoBleManagerWithEnvironment(
    environment(
      {
        executionEnvironment: 'development-build',
        nativeModuleAvailable: true,
        permissionBridge
      },
      'apple'
    )
  )
}

describe('Expo permissions.request on Apple (finding 179)', () => {
  beforeEach(() => {
    jest.clearAllMocks()
    jest.useRealTimers()
  })

  test('resolves the system prompt decision in the Android result shape', async () => {
    const permissionBridge = jest.fn().mockResolvedValue(appleGrantedResult())

    const result = await managerWithBridge(permissionBridge)

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toMatchObject({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    expect(permissionBridge).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
  })

  test('a restricted platform answer is capability.unsupported with the reason, not a denial', async () => {
    const permissionBridge = jest.fn().mockRejectedValue({
      code: 'permissionRestricted',
      message: 'iOS restrictions prevent Bluetooth use; the user cannot change this.'
    })

    const result = await managerWithBridge(permissionBridge)

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.permissions.request',
      platform: {
        code: 'permissionRestricted',
        safeMessage: expect.stringContaining('restrictions prevent Bluetooth use')
      }
    })
  })

  test('a platform timeout is operation.timed-out and reports what happened', async () => {
    const permissionBridge = jest.fn().mockRejectedValue({
      code: 'permissionTimeout',
      message: 'The Bluetooth permission prompt was not answered within 300000ms.'
    })

    const result = await managerWithBridge(permissionBridge)

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'operation.timed-out',
      operation: 'expo.permissions.request'
    })
  })

  test('an already-aborted signal fails without touching the bridge', async () => {
    const permissionBridge = jest.fn().mockResolvedValue(appleGrantedResult())
    const controller = new AbortController()
    controller.abort()

    const result = await managerWithBridge(permissionBridge)

    await expect(
      result.permissions.request({ purpose: 'scan-and-connect', signal: controller.signal })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'operation.aborted',
      operation: 'expo.permissions.request'
    })
    expect(permissionBridge).not.toHaveBeenCalled()
  })

  test('aborting a pending request fails it and ignores the late bridge answer', async () => {
    let resolveBridge = null
    const permissionBridge = jest.fn().mockImplementation(
      () =>
        new Promise(resolve => {
          resolveBridge = resolve
        })
    )
    const controller = new AbortController()

    const result = await managerWithBridge(permissionBridge)
    const pending = result.permissions.request({ purpose: 'scan-and-connect', signal: controller.signal })
    const assertion = expect(pending).rejects.toMatchObject({
      constructor: BleError,
      code: 'operation.aborted',
      operation: 'expo.permissions.request'
    })
    controller.abort()
    await assertion
    resolveBridge(appleGrantedResult())
    await Promise.resolve()
    expect(permissionBridge).toHaveBeenCalledTimes(1)
  })

  test('a caller timeout fails a pending request and ignores the late bridge answer', async () => {
    jest.useFakeTimers()
    try {
      let resolveBridge = null
      const permissionBridge = jest.fn().mockImplementation(
        () =>
          new Promise(resolve => {
            resolveBridge = resolve
          })
      )

      const result = await managerWithBridge(permissionBridge)
      const pending = result.permissions.request({ purpose: 'scan-and-connect', timeoutMs: 50 })
      const assertion = expect(pending).rejects.toMatchObject({
        constructor: BleError,
        code: 'operation.timed-out',
        operation: 'expo.permissions.request'
      })
      await jest.advanceTimersByTimeAsync(60)
      await assertion
      resolveBridge(appleGrantedResult())
      await Promise.resolve()
      expect(permissionBridge).toHaveBeenCalledTimes(1)
    } finally {
      jest.useRealTimers()
    }
  })

  test('a non-positive timeoutMs is argument.invalid before the bridge runs', async () => {
    const permissionBridge = jest.fn().mockResolvedValue(appleGrantedResult())

    const result = await managerWithBridge(permissionBridge)

    await expect(result.permissions.request({ purpose: 'scan-and-connect', timeoutMs: 0 })).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.permissions.timeout'
    })
    expect(permissionBridge).not.toHaveBeenCalled()
  })

  test('a non-AbortSignal signal is argument.invalid before the bridge runs', async () => {
    const permissionBridge = jest.fn().mockResolvedValue(appleGrantedResult())

    const result = await managerWithBridge(permissionBridge)

    await expect(
      result.permissions.request({ purpose: 'scan-and-connect', signal: 'not-a-signal' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.permissions.signal'
    })
    expect(permissionBridge).not.toHaveBeenCalled()
  })
})
