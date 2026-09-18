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
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: { get: () => ({}) },
  NativeModules: {}
}))

const { contractError } = require('../src/backend-contract/errors')
const { BleError } = require('../src/public/errors')
const { createExpoBleManager, createExpoBleManagerWithEnvironment, mapExpoReadiness } = require('../src/expo')
const { createReactNativeManagerHost } = require('../src/react-native-manager')
const { createReactNativeApplicationHost } = require('../src/react-native-app-manager')

/** A Rust-route host around a stub manager; the services are exercised in expo-rust-host-services.test.js. */
function hostFor(manager) {
  return { manager, services: {}, claimRestoration: jest.fn() }
}
const { getNativeUnifiedBleExpoRuntime } = require('../src/expo-native-runtime')

function environment(expo, platform = 'android') {
  return {
    platform,
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

function trustedNativeExpoRuntime(overrides = {}) {
  return {
    getRuntimeConfiguration: jest.fn().mockResolvedValue({
      platform: 'android',
      configurationDigest: 'native-digest',
      legacyLocationPolicy: 'none'
    }),
    requestPermissions: jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    }),
    openSettings: jest.fn().mockResolvedValue(undefined),
    ...overrides
  }
}

describe('Expo factory', () => {
  beforeEach(() => {
    jest.clearAllMocks()
    getNativeUnifiedBleExpoRuntime.mockReturnValue(trustedNativeExpoRuntime())
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
    expect(createReactNativeManagerHost).not.toHaveBeenCalled()
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
    expect(createReactNativeManagerHost).not.toHaveBeenCalled()
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
    expect(createReactNativeManagerHost).not.toHaveBeenCalled()
  })

  test('fails closed when the expected native configuration digest has no actual digest', async () => {
    await expect(
      createExpoBleManagerWithEnvironment(
        environment({
          executionEnvironment: 'development-build',
          nativeModuleAvailable: true,
          expectedConfiguration: { digest: 'expected' }
        })
      )
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.incompatible',
      operation: 'expo.runtime.configuration'
    })
    expect(createReactNativeManagerHost).not.toHaveBeenCalled()
  })

  test('validates direct Expo runtime configuration before RN construction', async () => {
    await expect(
      createExpoBleManager(
        {},
        {
          executionEnvironment: 'development-build',
          nativeModuleAvailable: true,
          expectedConfiguration: { digest: 'expected' }
        }
      )
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.incompatible',
      operation: 'expo.runtime.configuration'
    })
    expect(createReactNativeApplicationHost).not.toHaveBeenCalled()
  })

  test('zero-argument factory resolves the trusted native Expo runtime for Android operations', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    const nativeRuntime = trustedNativeExpoRuntime({
      getRuntimeConfiguration: jest.fn().mockResolvedValue({
        platform: 'android',
        configurationDigest: 'native-digest',
        legacyLocationPolicy: 'none'
      }),
      requestPermissions: jest.fn().mockResolvedValue({
        requested: ['bluetooth'],
        granted: ['bluetooth'],
        denied: [],
        recommendedSettingsTarget: null
      }),
      openSettings: jest.fn().mockResolvedValue(undefined)
    })
    getNativeUnifiedBleExpoRuntime.mockReturnValue(nativeRuntime)
    createReactNativeApplicationHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManager()

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toMatchObject({
      granted: ['bluetooth']
    })
    await expect(result.openSettings('bluetooth')).resolves.toBeUndefined()
    await expect(result.readiness()).resolves.toMatchObject({ state: 'ready' })
    expect(nativeRuntime.getRuntimeConfiguration).toHaveBeenCalledTimes(1)
    expect(nativeRuntime.requestPermissions).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
    expect(nativeRuntime.openSettings).toHaveBeenCalledWith({ target: 'bluetooth' })
  })

  test('fails closed with an actionable normalized error when iOS cannot issue a standalone permission prompt', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    const nativeRuntime = trustedNativeExpoRuntime({
      getRuntimeConfiguration: jest.fn().mockResolvedValue({
        platform: 'apple',
        configurationDigest: 'native-digest'
      }),
      requestPermissions: jest.fn().mockRejectedValue({
        code: 'unsupportedPermissionPrompt',
        message: 'iOS has no standalone Bluetooth permission prompt; invoke a Bluetooth action first.'
      }),
      openSettings: jest.fn().mockResolvedValue(undefined)
    })
    getNativeUnifiedBleExpoRuntime.mockReturnValue(nativeRuntime)
    createReactNativeApplicationHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManager()

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.permissions.request',
      platform: {
        safeMessage: expect.stringContaining('standalone Bluetooth permission prompt')
      }
    })
  })

  test('normalizes native settings failures at the Expo boundary', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    const nativeRuntime = trustedNativeExpoRuntime({
      openSettings: jest.fn().mockRejectedValue({
        code: 'settingsUnsupported',
        message: 'This settings target is unavailable on the current host.'
      })
    })
    getNativeUnifiedBleExpoRuntime.mockReturnValue(nativeRuntime)
    createReactNativeApplicationHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManager()

    await expect(result.openSettings('bluetooth')).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.open-settings',
      platform: { code: 'settingsUnsupported' }
    })
  })

  test('fails closed before RN construction when the native Expo plugin marker is absent', async () => {
    const nativeRuntime = trustedNativeExpoRuntime({
      getRuntimeConfiguration: jest.fn().mockRejectedValue({
        code: 'nativeConfigurationMissing',
        message: 'The Unified BLE Expo plugin configuration marker is absent; rebuild the native app.'
      })
    })
    getNativeUnifiedBleExpoRuntime.mockReturnValue(nativeRuntime)

    await expect(createExpoBleManager()).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.runtime.configuration',
      platform: {
        safeMessage: expect.stringContaining('plugin configuration marker is absent')
      }
    })
    expect(createReactNativeApplicationHost).not.toHaveBeenCalled()
  })

  test('composes the existing RN manager and adds the Expo runtime surfaces', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, androidApiLevel: 35 })
    )

    expect(result).toBe(manager)
    expect(createReactNativeManagerHost).toHaveBeenCalledTimes(1)
    expect(typeof result.readiness).toBe('function')
    expect(typeof result.permissions.request).toBe('function')
    expect(typeof result.openSettings).toBe('function')
    await expect(result.readiness()).resolves.toMatchObject({ state: 'ready' })
    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.permissions.request'
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

  test('projects required Android legacy-location policy as guidance without claiming runtime permission state', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManagerWithEnvironment(
      environment({
        executionEnvironment: 'development-build',
        nativeModuleAvailable: true,
        permissions: { android: { legacyLocation: 'required' } }
      })
    )

    await expect(result.readiness()).resolves.toEqual({
      state: 'action-required',
      adapter: adapterState(),
      actions: [{ kind: 'open-settings', target: 'location-services' }]
    })
  })

  test('fails closed when no trusted permission bridge is available', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState({ authorization: 'denied' })) }
    }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true })
    )

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.permissions.request'
    })
    expect(manager.adapter.state).not.toHaveBeenCalled()
  })

  test('fails explicitly when no trusted settings bridge is available', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

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
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, settingsBridge })
    )

    await expect(result.openSettings('bluetooth')).resolves.toBeUndefined()
    expect(settingsBridge).toHaveBeenCalledWith('bluetooth')
  })

  test('uses an explicitly injected trusted permission bridge', async () => {
    const permissionBridge = jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, permissionBridge })
    )

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toMatchObject({
      granted: ['bluetooth']
    })
    expect(permissionBridge).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
  })

  test('normalizes permission bridge failures at the Expo boundary', async () => {
    const permissionBridge = jest.fn().mockRejectedValue(new Error('Native permission activity is unavailable.'))
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, permissionBridge })
    )

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'platform.failure',
      operation: 'expo.permissions.request',
      platform: { safeMessage: 'Native permission activity is unavailable.' }
    })
  })

  test('rejects malformed permission results at the Expo boundary', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeManagerHost.mockResolvedValue(hostFor(manager))
    const permissionBridge = jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['not-bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, permissionBridge })
    )

    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.malformed',
      operation: 'expo.permissions.result'
    })
  })

  test('rehydrates asynchronous React Native factory failures as public errors', async () => {
    createReactNativeApplicationHost.mockRejectedValue(
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
    createReactNativeApplicationHost.mockResolvedValue(hostFor(manager))
    const options = {
      restoration: { restorationId: 'primary', generation: '1' }
    }

    await expect(createExpoBleManager(options)).resolves.toBe(manager)
    expect(createReactNativeApplicationHost).toHaveBeenCalledWith(options)
  })

  test('passes direct Expo runtime bridges into the Expo surfaces', async () => {
    const settingsBridge = jest.fn().mockResolvedValue(undefined)
    const permissionBridge = jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeApplicationHost.mockResolvedValue(hostFor(manager))

    const result = await createExpoBleManager(
      {},
      {
        executionEnvironment: 'development-build',
        nativeModuleAvailable: true,
        settingsBridge,
        permissionBridge
      }
    )

    await result.openSettings('bluetooth')
    await result.permissions.request({ purpose: 'scan-and-connect' })

    expect(settingsBridge).toHaveBeenCalledWith('bluetooth')
    expect(permissionBridge).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
  })
})
