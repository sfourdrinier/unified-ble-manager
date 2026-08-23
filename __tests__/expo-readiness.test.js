jest.mock('../src/react-native', () => ({
  createReactNativeBleManager: jest.fn(),
  createReactNativeBleManagerWithEnvironment: jest.fn()
}))

const { createExpoBleManager } = require('../src/expo')
const { createReactNativeBleManager } = require('../src/react-native')

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

  test('returns a measured permission breakdown without prompting automatically', async () => {
    const manager = managerFor(adapterState({ authorization: 'not-determined' }))
    createReactNativeBleManager.mockResolvedValue(manager)

    const result = await createExpoBleManager()
    const readiness = await result.readiness()

    expect(readiness.actions).toEqual([{ kind: 'request-permission', permission: 'bluetooth' }])
    await expect(result.permissions.request({ purpose: 'scan-and-connect' })).resolves.toEqual({
      requested: ['bluetooth'],
      granted: [],
      denied: [],
      recommendedSettingsTarget: null
    })
    expect(manager.adapter.state).toHaveBeenCalledTimes(2)
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
