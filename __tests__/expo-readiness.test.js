jest.mock('../src/react-native', () => ({
  createReactNativeBleManager: jest.fn(),
  createReactNativeBleManagerWithEnvironment: jest.fn()
}))

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 }
}))

const { createExpoBleManager, mapExpoReadiness } = require('../src/expo')
const { createReactNativeBleManager } = require('../src/react-native')
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

describe('Expo readiness surface', () => {
  beforeEach(() => {
    jest.clearAllMocks()
  })

  test('returns the delegated React Native manager with additive readiness', async () => {
    const manager = managerFor(adapterState())
    createReactNativeBleManager.mockResolvedValue(manager)

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
    [
      'not-determined permission',
      adapterState({ authorization: 'not-determined' }),
      'action-required',
      [{ kind: 'request-permission', permission: 'bluetooth' }]
    ],
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
    createReactNativeBleManager.mockResolvedValue(manager)

    const result = await createExpoBleManager()

    await expect(result.readiness()).resolves.toEqual({
      state: expectedState,
      adapter: state,
      actions: expectedActions
    })
  })

  test('fails closed when no trusted permission bridge is available', async () => {
    const manager = managerFor(adapterState({ authorization: 'not-determined' }))
    createReactNativeBleManager.mockResolvedValue(manager)

    const result = await createExpoBleManager()
    const readiness = await result.readiness()

    expect(readiness.actions).toEqual([{ kind: 'request-permission', permission: 'bluetooth' }])
    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.permissions.request'
    })
    expect(manager.adapter.state).toHaveBeenCalledTimes(1)
  })

  test('does not claim Android readiness when the trusted API level is unavailable', () => {
    expect(mapExpoReadiness(adapterState(), { platform: 'android' })).toMatchObject({
      state: 'action-required',
      actions: [{ kind: 'open-settings', target: 'location-services' }]
    })
  })

  test('direct Android factory does not report API 24-30 ready when runtime config omits legacy location policy', async () => {
    const manager = managerFor(adapterState())
    createReactNativeBleManager.mockResolvedValue(manager)
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

  test.each(['auto', 'required'])(
    'does not report Android API 24-30 ready when legacyLocation is %s',
    policy => {
      expect(
        mapExpoReadiness(adapterState(), {
          androidApiLevel: 30,
          permissions: { android: { legacyLocation: policy } }
        })
      ).toMatchObject({
        state: 'action-required',
        actions: [{ kind: 'open-settings', target: 'location-services' }]
      })
    }
  )

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

  test('openSettings is explicit and fails closed without a trusted native bridge', async () => {
    const manager = managerFor(adapterState())
    createReactNativeBleManager.mockResolvedValue(manager)

    const result = await createExpoBleManager()

    await expect(result.openSettings('bluetooth')).rejects.toMatchObject({
      code: 'capability.unavailable',
      operation: 'expo.open-settings',
      platform: { safeMessage: expect.stringContaining('settings bridge') }
    })
  })
})
