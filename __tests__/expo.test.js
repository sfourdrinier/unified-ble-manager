jest.mock('../src/react-native', () => ({
  createReactNativeBleManager: jest.fn(),
  createReactNativeBleManagerWithEnvironment: jest.fn()
}))

jest.mock('../src/public/ble-manager', () => ({
  createPublicBleManager: jest.fn(internal => internal)
}))

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: { get: () => ({}) },
  NativeModules: {}
}))

const { contractError } = require('../src/backend-contract/errors')
const { BleError } = require('../src/public/errors')
const {
  createExpoBleManager,
  createExpoBleManagerWithEnvironment,
  mapExpoReadiness
} = require('../src/expo')
const { createReactNativeBleManager, createReactNativeBleManagerWithEnvironment } = require('../src/react-native')

function environment(expo) {
  return {
    platform: 'android',
    control: {},
    now: () => 1,
    clientId: 'client',
    managerId: 'manager',
    hostSessionScope: 'scope',
    expo
  }
}

function adapterState(overrides = {}) {
  return {
    availability: 'available',
    authorization: 'granted',
    power: 'on',
    backendGeneration: 'generation',
    updatedAt: 1,
    safeReason: null,
    ...overrides
  }
}

describe('Expo factory', () => {
  beforeEach(() => {
    jest.clearAllMocks()
  })

  test('fails in Expo Go with an actionable development-build error before RN construction', async () => {
    await expect(
      createExpoBleManagerWithEnvironment(
        environment({ executionEnvironment: 'expo-go', nativeModuleAvailable: false })
      )
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.runtime.development-build',
      platform: {
        safeMessage: expect.stringContaining('development build')
      }
    })
    expect(createReactNativeBleManagerWithEnvironment).not.toHaveBeenCalled()
  })

  test('fails when the native module is absent before RN construction', async () => {
    await expect(
      createExpoBleManagerWithEnvironment(
        environment({ executionEnvironment: 'development-build', nativeModuleAvailable: false })
      )
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.runtime.native-module'
    })
    expect(createReactNativeBleManagerWithEnvironment).not.toHaveBeenCalled()
  })

  test('fails closed on an authoritative native configuration mismatch before RN construction', async () => {
    await expect(
      createExpoBleManagerWithEnvironment(
        environment({
          executionEnvironment: 'development-build',
          nativeModuleAvailable: true,
          nativeConfiguration: { digest: 'actual' },
          expectedConfiguration: { digest: 'expected' }
        })
      )
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.incompatible',
      operation: 'expo.runtime.configuration'
    })
    expect(createReactNativeBleManagerWithEnvironment).not.toHaveBeenCalled()
  })

  test('composes the existing RN manager and adds the Expo runtime surfaces', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true })
    )

    expect(result).toBe(manager)
    expect(createReactNativeBleManagerWithEnvironment).toHaveBeenCalledTimes(1)
    expect(typeof result.readiness).toBe('function')
    expect(typeof result.permissions.request).toBe('function')
    expect(typeof result.openSettings).toBe('function')
    await expect(result.readiness()).resolves.toMatchObject({ state: 'ready' })
    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toEqual({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
  })

  test('maps pending permission to one explicit request action without prompting', () => {
    const readiness = mapExpoReadiness(adapterState({ authorization: 'not-determined' }))

    expect(readiness).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'request-permission', permission: 'bluetooth' }]
    })
  })

  test('maps powered-off, denied, and unsupported states to distinct actions', () => {
    expect(mapExpoReadiness(adapterState({ power: 'off' })).actions).toEqual([
      { kind: 'enable-bluetooth', systemUiOnly: true }
    ])
    expect(mapExpoReadiness(adapterState({ authorization: 'denied' })).actions).toEqual([
      { kind: 'open-settings', target: 'app' }
    ])
    expect(mapExpoReadiness(adapterState({ authorization: 'restricted' })).state).toBe('unavailable')
    expect(mapExpoReadiness(adapterState({ availability: 'unsupported' })).state).toBe('unavailable')
  })

  test('returns permission state without prompting and recommends app settings after denial', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState({ authorization: 'denied' })) }
    }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true })
    )

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toEqual({
      requested: ['bluetooth'],
      granted: [],
      denied: ['bluetooth'],
      recommendedSettingsTarget: 'app'
    })
    expect(manager.adapter.state).toHaveBeenCalledTimes(1)
  })

  test('fails explicitly when no trusted settings bridge is available', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true })
    )

    await expect(result.openSettings('app')).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.open-settings',
      platform: { safeMessage: expect.stringContaining('settings bridge') }
    })
  })

  test('uses an explicitly injected trusted settings bridge', async () => {
    const settingsBridge = jest.fn().mockResolvedValue(undefined)
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, settingsBridge })
    )

    await expect(result.openSettings('bluetooth')).resolves.toBeUndefined()
    expect(settingsBridge).toHaveBeenCalledWith('bluetooth')
  })

  test('rehydrates asynchronous React Native factory failures as public errors', async () => {
    createReactNativeBleManager.mockRejectedValue(
      contractError('adapter.unavailable', 'adapter', 'react-native-manager.adapter')
    )

    await expect(createExpoBleManager()).rejects.toMatchObject({
      constructor: BleError,
      code: 'adapter.unavailable',
      domain: 'adapter',
      operation: 'react-native-manager.adapter'
    })
  })

  test('passes optional restoration options unchanged to the React Native factory', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManager.mockResolvedValue(manager)
    const options = {
      restoration: { restorationId: 'primary', generation: '1' }
    }

    await expect(createExpoBleManager(options)).resolves.toBe(manager)
    expect(createReactNativeBleManager).toHaveBeenCalledWith(options)
  })
})
