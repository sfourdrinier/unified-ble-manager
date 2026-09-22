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

const { createExpoBleManagerWithEnvironment, mapExpoReadiness } = require('../src/expo')
const { createReactNativeManagerHost } = require('../src/react-native-manager')

/**
 * Finding 179: every readiness action a backend returns must have a working
 * API that performs it. The per-platform doubles below mirror the NATIVE
 * contract, not the TS wiring:
 *
 * - Android `UnifiedBleExpoRuntimeModule.requestPermissions` shows the system
 *   runtime prompt and resolves the granted/denied shape.
 * - Apple `UnifiedBleExpoRuntime.requestPermissions` (fixed) presents the
 *   CoreBluetooth prompt through the process-owned radio and resolves the
 *   same shape; before the fix it rejected `unsupportedPermissionPrompt`,
 *   which made the `request-permission` action unactionable and failed this
 *   suite on the Apple legs.
 */
function grantedResult() {
  return {
    requested: ['bluetooth'],
    granted: ['bluetooth'],
    denied: [],
    recommendedSettingsTarget: null
  }
}

function bridgesFor(platform) {
  if (platform === 'android') {
    return {
      permissionBridge: jest.fn().mockResolvedValue(grantedResult()),
      settingsBridge: jest.fn().mockResolvedValue(undefined)
    }
  }
  return {
    permissionBridge: jest.fn().mockResolvedValue(grantedResult()),
    settingsBridge: jest.fn().mockImplementation(target => {
      if (target === 'app') return Promise.resolve(undefined)
      return Promise.reject({
        code: 'settingsUnsupported',
        message: `iOS exposes only the application settings URL; ${target} cannot be targeted.`
      })
    })
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

function environment(expo, platform) {
  return {
    platform,
    now: () => 1,
    clientId: 'client',
    managerId: 'manager',
    hostSessionScope: 'scope',
    expo
  }
}

/** Adapter states covering every action `mapExpoReadiness` can emit. */
function readinessCases(platform) {
  const base =
    platform === 'android'
      ? { executionEnvironment: 'development-build', nativeModuleAvailable: true, androidApiLevel: 35 }
      : { executionEnvironment: 'development-build', nativeModuleAvailable: true }
  return [
    { name: 'pending permission', adapter: adapterState({ authorization: 'not-determined', power: 'unknown' }) },
    { name: 'denied permission', adapter: adapterState({ authorization: 'denied' }) },
    { name: 'powered off', adapter: adapterState({ power: 'off' }) },
    { name: 'ready', adapter: adapterState() },
    { name: 'restricted', adapter: adapterState({ authorization: 'restricted' }) },
    { name: 'transient power', adapter: adapterState({ power: 'unknown' }) },
    ...(platform === 'android'
      ? [{ name: 'legacy location rebuild', adapter: adapterState(), androidApiLevel: 30, legacyLocation: 'none' }]
      : []),
    { name: `${platform} base`, adapter: adapterState(), generally: true }
  ].map(entry => ({
    ...entry,
    configuration: {
      ...base,
      ...(entry.androidApiLevel === undefined ? {} : { androidApiLevel: entry.androidApiLevel }),
      ...(entry.legacyLocation === undefined
        ? {}
        : { permissions: { android: { legacyLocation: entry.legacyLocation } } })
    }
  }))
}

/**
 * Performs one readiness action through the manager's public API. Throws on
 * an unknown action kind so a new action without a performer fails closed.
 * Returns a string describing what performed it.
 */
async function performAction(manager, action, platform) {
  switch (action.kind) {
    case 'request-permission': {
      const result = await manager.permissions.request({ purpose: 'scan-and-connect' })
      if (!Array.isArray(result.granted) || !Array.isArray(result.denied)) {
        throw new Error('request-permission did not report granted/denied')
      }
      return `permissions.request granted=${result.granted.join(',')} denied=${result.denied.join(',')}`
    }
    case 'open-settings': {
      await manager.openSettings(action.target)
      return `openSettings ${action.target}`
    }
    case 'enable-bluetooth': {
      // No application API toggles Bluetooth: the OS power alert performs
      // this action (the plugin's `showPowerAlert` central option). The
      // library must mark it system-UI-only rather than point at an API.
      if (action.systemUiOnly !== true) throw new Error('enable-bluetooth must be systemUiOnly')
      if (platform === 'android') {
        await manager.openSettings('bluetooth')
        return 'os power alert with bluetooth settings fallback'
      }
      await expect(manager.openSettings('bluetooth')).rejects.toMatchObject({ code: 'capability.unsupported' })
      return 'os power alert (no programmable settings target on Apple)'
    }
    case 'rebuild-native-app': {
      if (typeof action.reason !== 'string' || action.reason.length === 0) {
        throw new Error('rebuild-native-app must carry a reason')
      }
      return `rebuild: ${action.reason}`
    }
    case 'create-development-build': {
      return 'development build'
    }
    default: {
      throw new Error(`readiness action has no performing API: ${JSON.stringify(action)}`)
    }
  }
}

describe.each(['android', 'apple'])('readiness actions are actionable on %s (finding 179)', platform => {
  beforeEach(() => {
    jest.clearAllMocks()
  })

  test.each(readinessCases(platform).map(entry => [entry.name, entry]))('%s', async (_name, entry) => {
    const bridges = bridgesFor(platform)
    const manager = { adapter: { state: jest.fn().mockResolvedValue(entry.adapter) } }
    createReactNativeManagerHost.mockResolvedValue({
      manager,
      services: {},
      claimRestoration: jest.fn()
    })

    const expoManager = await createExpoBleManagerWithEnvironment(environment({ ...entry.configuration, ...bridges }, platform))
    const readiness = await expoManager.readiness()
    expect(readiness.adapter).toBe(entry.adapter)

    const performed = []
    for (const action of readiness.actions) {
      performed.push(await performAction(expoManager, action, platform))
    }

    if (readiness.state === 'ready') {
      expect(readiness.actions).toEqual([])
    } else if (readiness.state === 'action-required') {
      // An empty action list is only honest for a transient snapshot the
      // next poll resolves; anything settled must name its performer.
      if (readiness.actions.length === 0) {
        expect(['unknown', 'resetting']).toContain(entry.adapter.power)
      } else {
        expect(performed.length).toBe(readiness.actions.length)
      }
    } else {
      expect(readiness.state).toBe('unavailable')
    }
  })

  test('a denied prompt decision reports denied with its settings target', async () => {
    const bridges = bridgesFor(platform)
    bridges.permissionBridge.mockResolvedValue({
      requested: ['bluetooth'],
      granted: [],
      denied: ['bluetooth'],
      recommendedSettingsTarget: 'app'
    })
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState({ authorization: 'not-determined' })) }
    }
    createReactNativeManagerHost.mockResolvedValue({
      manager,
      services: {},
      claimRestoration: jest.fn()
    })

    const expoManager = await createExpoBleManagerWithEnvironment(
      environment(
        {
          executionEnvironment: 'development-build',
          nativeModuleAvailable: true,
          ...(platform === 'android' ? { androidApiLevel: 35 } : {}),
          ...bridges
        },
        platform
      )
    )
    const readiness = await expoManager.readiness()
    expect(readiness.actions).toEqual([{ kind: 'request-permission', permission: 'bluetooth' }])

    const result = await expoManager.permissions.request({ purpose: 'scan-and-connect' })
    expect(result.denied).toEqual(['bluetooth'])
    expect(result.recommendedSettingsTarget).toBe('app')
  })
})
