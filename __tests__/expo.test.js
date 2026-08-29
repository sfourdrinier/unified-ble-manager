jest.mock('../src/react-native', () => ({
  createReactNativeBleManager: jest.fn(),
  createReactNativeBleManagerWithEnvironment: jest.fn()
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
const { createReactNativeBleManager, createReactNativeBleManagerWithEnvironment } = require('../src/react-native')
const { getNativeUnifiedBleExpoRuntime } = require('../src/expo-native-runtime')

function environment(expo, control = {}, platform = 'android') {
  return {
    platform,
    control,
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
    expect(createReactNativeBleManagerWithEnvironment).not.toHaveBeenCalled()
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
    expect(createReactNativeBleManager).not.toHaveBeenCalled()
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
    createReactNativeBleManager.mockResolvedValue(manager)

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
    createReactNativeBleManager.mockResolvedValue(manager)

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
    createReactNativeBleManager.mockResolvedValue(manager)

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
    expect(createReactNativeBleManager).not.toHaveBeenCalled()
  })

  test('composes the existing RN manager and adds the Expo runtime surfaces', async () => {
    const manager = {
      adapter: { state: jest.fn().mockResolvedValue(adapterState()) }
    }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true, androidApiLevel: 35 })
    )

    expect(result).toBe(manager)
    expect(createReactNativeBleManagerWithEnvironment).toHaveBeenCalledTimes(1)
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
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

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
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

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

  test('uses an explicitly injected trusted permission bridge', async () => {
    const permissionBridge = jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

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
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
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

  test('acquires and releases one explicit connected-device background lease', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'background-1' }),
      releaseBackground: jest.fn().mockResolvedValue(undefined)
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    const lease = await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await lease.release()
    await lease.release()

    expect(control.acquireBackground).toHaveBeenCalledWith({ kind: 'connected-device', reason: 'active workout' })
    expect(control.releaseBackground).toHaveBeenCalledTimes(1)
    expect(control.releaseBackground).toHaveBeenCalledWith({ leaseId: 'background-1' })
  })

  test('coalesces concurrent connected-device background lease releases', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'background-1' }),
      releaseBackground: jest.fn().mockResolvedValue(undefined)
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    const lease = await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })

    await Promise.all([lease.release(), lease.release()])

    expect(control.releaseBackground).toHaveBeenCalledTimes(1)
    expect(control.releaseBackground).toHaveBeenCalledWith({ leaseId: 'background-1' })
  })

  test('updates the active connected-device notification without acquiring another lease', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'background-1' }),
      releaseBackground: jest.fn().mockResolvedValue(undefined),
      updateBackgroundNotification: jest.fn().mockResolvedValue(undefined)
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    await expect(result.background.updateNotification({ title: 'Glucose 108', body: 'Private' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.background.update-notification'
    })
    const lease = await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    await result.background.updateNotification({ title: 'Glucose 108', body: 'Private' })

    expect(control.acquireBackground).toHaveBeenCalledTimes(1)
    expect(control.updateBackgroundNotification).toHaveBeenCalledWith({
      leaseId: 'background-1',
      title: 'Glucose 108',
      body: 'Private'
    })
    await lease.release()
  })

  test('rejects unbounded notification text at the public boundary', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'background-1' }),
      updateBackgroundNotification: jest.fn()
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })

    await expect(result.background.updateNotification({ title: 'x'.repeat(257) })).rejects.toMatchObject({
      constructor: BleError,
      code: 'argument.invalid',
      operation: 'expo.background.update-notification'
    })
    expect(control.updateBackgroundNotification).not.toHaveBeenCalled()
  })

  test('keeps the Android notification update unsupported on Apple', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'apple-lease' }),
      updateBackgroundNotification: jest.fn().mockRejectedValue(
        Object.assign(new Error('Connected-device foreground service is Android-only'), {
          code: 'unsupportedBackground'
        })
      )
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control, 'apple')
    )
    await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })

    await expect(result.background.updateNotification({ title: 'Glucose 108' })).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unsupported',
      operation: 'expo.background.update-notification'
    })
  })

  test('returns an associated peer-directory record from explicit Android system UI', async () => {
    const control = {
      associateCompanionDevice: jest.fn().mockResolvedValue({
        source: 'associated',
        associationId: 7,
        peerId: 'AA:BB:CC:DD:EE:FF',
        displayName: 'Sensor'
      })
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    await expect(result.association.associate({ name: 'Sensor' })).resolves.toEqual({
      source: 'associated',
      associationId: 7,
      peerId: 'AA:BB:CC:DD:EE:FF',
      displayName: 'Sensor'
    })
    expect(control.associateCompanionDevice).toHaveBeenCalledWith({ name: 'Sensor' })
  })

  test('claims native restoration through the app-facing no-argument surface', async () => {
    const control = {
      claimRestoration: jest.fn().mockResolvedValue({
        receiptId: 'receipt-1',
        outcome: 'adopted',
        boundClientId: 'opaque-client',
        adoptionEpoch: 'epoch-1',
        replayRecordCount: 1,
        records: [
          {
            recordVersion: 1,
            namespaceValue: 'opaque-namespace',
            attachmentId: 'attachment',
            backendInstanceId: 'backend',
            backendGeneration: 'generation',
            adapterId: 'adapter',
            adapterGeneration: 'adapter-generation',
            ordinal: 1,
            adoptionEpoch: 'epoch-1',
            kind: 'connection',
            peerId: 'peer',
            connectionId: 'connection',
            ownerLeaseId: 'owner',
            connectionGeneration: 'connection-generation'
          }
        ]
      })
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    await expect(result.restoration.claim()).resolves.toEqual({
      outcome: 'adopted',
      replayRecordCount: 1,
      records: [{ kind: 'connection', ordinal: 1, peerId: 'peer' }]
    })
    expect(control.claimRestoration).toHaveBeenCalledTimes(1)
  })

  test('fails closed with a normalized protocol error for an unknown native restoration outcome', async () => {
    const control = {
      claimRestoration: jest.fn().mockResolvedValue({
        outcome: 'futureNativeOutcome',
        replayRecordCount: 0,
        records: []
      })
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)

    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )

    await expect(result.restoration.claim()).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.malformed',
      domain: 'restoration',
      operation: 'expo.restoration.native-outcome'
    })
  })

  test('normalizes actionable native foreground-service failures', async () => {
    const nativeFailure = Object.assign(new Error('Rebuild with configured notification metadata.'), {
      code: 'foregroundServiceNotConfigured'
    })
    const control = {
      acquireBackground: jest.fn().mockRejectedValue(nativeFailure),
      releaseBackground: jest.fn()
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )

    await expect(
      result.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'capability.unavailable',
      operation: 'expo.background.acquire',
      platform: { code: 'foregroundServiceNotConfigured' }
    })
  })

  test('preserves permission denial semantics from the native foreground-service boundary', async () => {
    const nativeFailure = Object.assign(new Error('Grant Bluetooth and notification permissions, then retry.'), {
      code: 'foregroundServicePermissionDenied'
    })
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment(
        { executionEnvironment: 'development-build', nativeModuleAvailable: true },
        { acquireBackground: jest.fn().mockRejectedValue(nativeFailure), releaseBackground: jest.fn() }
      )
    )

    await expect(
      result.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'permission.denied',
      operation: 'expo.background.acquire',
      platform: { code: 'foregroundServicePermissionDenied' }
    })
  })

  test('rejects malformed permission results at the Expo boundary', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
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

  test('rejects malformed background lease results at the Expo boundary', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment(
        { executionEnvironment: 'development-build', nativeModuleAvailable: true },
        { acquireBackground: jest.fn().mockResolvedValue({ leaseId: '' }) }
      )
    )

    await expect(
      result.background.acquire({ kind: 'connected-device', reason: 'active workout' })
    ).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.malformed',
      operation: 'expo.background.acquire.result'
    })
  })

  test('rejects malformed association and restoration results at the Expo boundary', async () => {
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment(
        { executionEnvironment: 'development-build', nativeModuleAvailable: true },
        {
          associateCompanionDevice: jest.fn().mockResolvedValue({
            source: 'associated',
            associationId: 0,
            peerId: null,
            displayName: null
          }),
          claimRestoration: jest.fn().mockResolvedValue({
            outcome: 'adopted',
            replayRecordCount: 1,
            records: []
          })
        }
      )
    )

    await expect(result.association.associate()).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.malformed',
      operation: 'expo.association.result'
    })
    await expect(result.restoration.claim()).rejects.toMatchObject({
      constructor: BleError,
      code: 'protocol.malformed',
      operation: 'expo.restoration.result'
    })
  })

  test('allows a failed native release to be retried without double-releasing a successful lease', async () => {
    const control = {
      acquireBackground: jest.fn().mockResolvedValue({ leaseId: 'background-1' }),
      releaseBackground: jest
        .fn()
        .mockRejectedValueOnce(
          Object.assign(new Error('Temporary native stop failure.'), { code: 'nativeBackgroundRelease' })
        )
        .mockResolvedValue(undefined)
    }
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManagerWithEnvironment.mockResolvedValue(manager)
    const result = await createExpoBleManagerWithEnvironment(
      environment({ executionEnvironment: 'development-build', nativeModuleAvailable: true }, control)
    )
    const lease = await result.background.acquire({ kind: 'connected-device', reason: 'active workout' })

    await expect(lease.release()).rejects.toMatchObject({
      constructor: BleError,
      operation: 'expo.background.release'
    })
    await expect(lease.release()).resolves.toBeUndefined()
    await expect(lease.release()).resolves.toBeUndefined()
    expect(control.releaseBackground).toHaveBeenCalledTimes(2)
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

  test('passes direct Expo runtime bridges into the Expo surfaces', async () => {
    const settingsBridge = jest.fn().mockResolvedValue(undefined)
    const permissionBridge = jest.fn().mockResolvedValue({
      requested: ['bluetooth'],
      granted: ['bluetooth'],
      denied: [],
      recommendedSettingsTarget: null
    })
    const manager = { adapter: { state: jest.fn().mockResolvedValue(adapterState()) } }
    createReactNativeBleManager.mockResolvedValue(manager)

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
