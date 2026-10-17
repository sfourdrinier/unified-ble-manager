jest.mock('../src/react-native-app-manager', () => ({
  createReactNativeApplicationHost: jest.fn()
}))

jest.mock('../src/public/ble-manager', () => ({
  createPublicBleManager: jest.fn(async internal => internal)
}))

jest.mock('../src/expo-native-runtime', () => ({
  getNativeUnifiedBleExpoRuntime: jest.fn()
}))

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: { get: name => (name === 'UnifiedBleRustCore' ? {} : null) }
}))

const { createExpoBleManager, mapExpoReadiness } = require('../src/expo')
const { createReactNativeApplicationHost } = require('../src/react-native-app-manager')
const { getNativeUnifiedBleExpoRuntime } = require('../src/expo-native-runtime')
const { Platform } = require('react-native')

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

function managerFor(state) {
  return {
    adapter: {
      state: jest.fn().mockResolvedValue(state)
    }
  }
}

function trustedNativeExpoRuntime() {
  return {
    getRuntimeConfiguration: jest.fn().mockResolvedValue({
      platform: 'android',
      configurationDigest: 'native-digest',
      legacyLocationPolicy: 'none',
      androidLocationServicesEnabled: true,
      androidLocationPermissionGranted: true
    }),
    requestPermissions: jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    }),
    openSettings: jest.fn().mockResolvedValue(undefined)
  }
}

describe('Expo readiness surface', () => {
  beforeEach(() => {
    jest.clearAllMocks()
    getNativeUnifiedBleExpoRuntime.mockReturnValue(trustedNativeExpoRuntime())
  })

  test('returns the delegated React Native manager with additive readiness', async () => {
    const manager = managerFor(adapterState())
    createReactNativeApplicationHost.mockResolvedValue({ manager: manager, services: {}, claimRestoration: jest.fn() })

    const result = await createExpoBleManager()

    expect(result).toBe(manager)
    expect(typeof result.readiness).toBe('function')
    await expect(result.readiness()).resolves.toEqual({
      state: 'ready',
      adapter: adapterState(),
      actions: []
    })
    expect(manager.adapter.state).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['not-determined permission', adapterState({ authorization: 'not-determined' }), 'ready', []],
    [
      'powered off adapter',
      adapterState({ power: 'off' }),
      'action-required',
      [{ kind: 'enable-bluetooth', systemUiOnly: true }]
    ],
    [
      'denied permission',
      adapterState({ authorization: 'denied' }),
      'action-required',
      [{ kind: 'open-settings', target: 'app' }]
    ],
    ['restricted authorization', adapterState({ authorization: 'restricted' }), 'unavailable', []],
    ['unavailable adapter', adapterState({ availability: 'unavailable' }), 'unavailable', []],
    ['unsupported adapter', adapterState({ availability: 'unsupported' }), 'unavailable', []]
  ])('maps %s from trusted adapter state', async (_name, state, expectedState, expectedActions) => {
    const manager = managerFor(state)
    createReactNativeApplicationHost.mockResolvedValue({ manager: manager, services: {}, claimRestoration: jest.fn() })

    const result = await createExpoBleManager()

    await expect(result.readiness()).resolves.toEqual({
      state: expectedState,
      adapter: state,
      actions: expectedActions
    })
  })

  test('uses the trusted native permission bridge for a pending adapter permission action', async () => {
    const manager = managerFor(adapterState({ authorization: 'not-determined', power: 'unknown' }))
    createReactNativeApplicationHost.mockResolvedValue({ manager: manager, services: {}, claimRestoration: jest.fn() })

    const result = await createExpoBleManager()
    const readiness = await result.readiness()

    expect(readiness.actions).toEqual([{ kind: 'request-permission', permission: 'bluetooth' }])
    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toMatchObject({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: []
    })
    expect(manager.adapter.state).toHaveBeenCalledTimes(1)
  })

  test('does not claim Android readiness when the trusted API level is unavailable', () => {
    expect(mapExpoReadiness(adapterState(), { platform: 'android' })).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'open-settings', target: 'location-services' }]
    })
  })

  test.each([24, 30])('reports Android API %s ready when native location prerequisites are measured true', apiLevel => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: apiLevel,
        androidLocationServicesEnabled: true,
        androidLocationPermissionGranted: true,
        permissions: { android: { legacyLocation: 'auto' } }
      })
    ).toMatchObject({ state: 'ready', actions: [] })
  })

  test('opens location settings when measured Android location services are disabled', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 30,
        androidLocationServicesEnabled: false,
        androidLocationPermissionGranted: true,
        permissions: { android: { legacyLocation: 'auto' } }
      })
    ).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'open-settings', target: 'location-services' }]
    })
  })

  test('requests the normal Bluetooth permission bridge when measured Android location permission is missing', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 30,
        androidLocationServicesEnabled: true,
        androidLocationPermissionGranted: false,
        permissions: { android: { legacyLocation: 'auto' } }
      })
    ).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'request-permission', permission: 'bluetooth' }]
    })
  })

  test('fails closed with an action when native Android location measurements are unknown', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 30,
        permissions: { android: { legacyLocation: 'auto' } }
      })
    ).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'request-permission', permission: 'bluetooth' }]
    })
  })

  test('direct Android factory does not report API 24-30 ready when runtime config omits legacy location policy', async () => {
    const manager = managerFor(adapterState())
    createReactNativeApplicationHost.mockResolvedValue({ manager: manager, services: {}, claimRestoration: jest.fn() })
    const originalVersion = Platform.Version
    Platform.Version = 30

    try {
      const result = await createExpoBleManager()

      await expect(result.readiness()).resolves.toMatchObject({
        state: 'unavailable',
        actions: [{ kind: 'rebuild-native-app' }]
      })
    } finally {
      Platform.Version = originalVersion
    }
  })

  test.each(['auto', 'required'])('does not report Android API 24-30 ready when legacyLocation is %s', policy => {
    expect(
      mapExpoReadiness(adapterState(), {
        androidApiLevel: 30,
        permissions: { android: { legacyLocation: policy } }
      })
    ).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'request-permission', permission: 'bluetooth' }]
    })
  })

  test('does not report Android API 24-30 ready when the plugin default is explicit legacyLocation none', () => {
    expect(mapExpoReadiness(adapterState(), { androidApiLevel: 30 })).toMatchObject({
      state: 'unavailable',
      actions: [{ kind: 'rebuild-native-app' }]
    })
  })

  test('preserves the explicit legacyLocation none policy on Android 12 and later', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        androidApiLevel: 31,
        permissions: { android: { legacyLocation: 'none' } }
      })
    ).toMatchObject({ state: 'ready', actions: [] })
  })

  test('keeps Android API 24-30 legacyLocation none as a rebuild requirement even with measured state', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 30,
        androidLocationServicesEnabled: true,
        androidLocationPermissionGranted: true,
        permissions: { android: { legacyLocation: 'none' } }
      })
    ).toMatchObject({ state: 'unavailable', actions: [{ kind: 'rebuild-native-app' }] })
  })

  test('does not require legacy location for Android API 31 auto or none policies', () => {
    for (const legacyLocation of ['auto', 'none']) {
      expect(
        mapExpoReadiness(adapterState(), {
          platform: 'android',
          androidApiLevel: 31,
          permissions: { android: { legacyLocation } }
        })
      ).toMatchObject({ state: 'ready', actions: [] })
    }
  })

  test('uses measured location prerequisites for required policy on Android API 31', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 31,
        androidLocationServicesEnabled: true,
        androidLocationPermissionGranted: false,
        permissions: { android: { legacyLocation: 'required' } }
      })
    ).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'request-permission', permission: 'bluetooth' }]
    })
  })

  test('does not infer a granted location permission from an enabled location service', () => {
    expect(
      mapExpoReadiness(adapterState(), {
        platform: 'android',
        androidApiLevel: 30,
        permissions: { android: { legacyLocation: 'auto' } },
        androidLocationServicesEnabled: true
      })
    ).toMatchObject({ state: 'action-required' })
  })

  test('refreshes native measured location values for every production readiness call', async () => {
    const manager = managerFor(adapterState())
    createReactNativeApplicationHost.mockResolvedValue({ manager, services: {}, claimRestoration: jest.fn() })
    const runtime = trustedNativeExpoRuntime()
    runtime.getRuntimeConfiguration
      .mockResolvedValueOnce({
        platform: 'android',
        configurationDigest: 'native-digest',
        legacyLocationPolicy: 'auto',
        androidLocationServicesEnabled: true,
        androidLocationPermissionGranted: true
      })
      .mockResolvedValueOnce({
        platform: 'android',
        configurationDigest: 'native-digest',
        legacyLocationPolicy: 'auto',
        androidLocationServicesEnabled: false,
        androidLocationPermissionGranted: true
      })
      .mockResolvedValueOnce({
        platform: 'android',
        configurationDigest: 'native-digest',
        legacyLocationPolicy: 'auto',
        androidLocationServicesEnabled: false,
        androidLocationPermissionGranted: true
      })
    getNativeUnifiedBleExpoRuntime.mockReturnValue(runtime)
    const originalVersion = Platform.Version
    Platform.Version = 30

    try {
      const result = await createExpoBleManager()
      await expect(result.readiness()).resolves.toMatchObject({ state: 'action-required' })
      await expect(result.readiness()).resolves.toMatchObject({
        actions: [{ kind: 'open-settings', target: 'location-services' }]
      })
      expect(runtime.getRuntimeConfiguration).toHaveBeenCalledTimes(3)
    } finally {
      Platform.Version = originalVersion
    }
  })

  test('openSettings uses the trusted native settings bridge explicitly', async () => {
    const manager = managerFor(adapterState())
    createReactNativeApplicationHost.mockResolvedValue({ manager: manager, services: {}, claimRestoration: jest.fn() })

    const result = await createExpoBleManager()

    await expect(result.openSettings('bluetooth')).resolves.toBeUndefined()
  })
})
