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
const { createExpoBleManager, createExpoBleManagerWithEnvironment, mapExpoReadiness } = require('../src/expo')
const { createReactNativeBleManager, createReactNativeBleManagerWithEnvironment } = require('../src/react-native')

function environment(expo, control = {}) {
  return {
    platform: 'android',
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

    const result = await createExpoBleManager({}, {
      executionEnvironment: 'development-build',
      nativeModuleAvailable: true,
      settingsBridge,
      permissionBridge
    })

    await result.openSettings('bluetooth')
    await result.permissions.request({ purpose: 'scan-and-connect' })

    expect(settingsBridge).toHaveBeenCalledWith('bluetooth')
    expect(permissionBridge).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
  })
})
