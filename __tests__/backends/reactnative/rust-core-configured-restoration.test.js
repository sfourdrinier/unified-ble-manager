// __tests__/backends/reactnative/rust-core-configured-restoration.test.js
//
// PR210-72: an app that configures restoration only in its Info.plist
// (UnifiedBleProtocolRestorationId / UnifiedBleProtocolRestorationGeneration)
// claims restoration without any JS option, as the legacy native module did
// (it read the authority at module init: ios/UnifiedBleProtocolControl.mm on
// main). The configured identity comes from `restorationIdentity('{}')`.

jest.mock('react-native', () => ({
  Platform: { OS: 'ios', Version: '18.0' },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const { createReactNativeApplicationHost } = require('../../../src/react-native-app-manager')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')

const CONFIGURED = Object.freeze({
  applicationId: 'com.example.app',
  restorationId: 'restore-1',
  generation: 'g1',
  restoreIdentifier: 'com.example.app.ubm.abcdefghijklmnopqrstuv',
  namespaceValue: 'ubm-ns:configured',
  clientId: 'ubm-client:configured',
  hostSessionScope: 'ubm-host:configured'
})

function applicationHost(native) {
  const binding = createReactNativeRustCoreBinding({ platform: 'apple', native })
  return createReactNativeApplicationHost({ rustCore: binding, randomBytes: length => new Uint8Array(length).fill(9) })
}

test('Info.plist-only restoration is claimed without a JS option', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'apple' })
  native.restorationAnswer = CONFIGURED
  native.seedRestored([{ peerId: 'C0FFEE00-0000-4000-8000-000000000001', connected: true }])
  const host = await applicationHost(native)
  expect(native.calls.find(call => call[0] === 'restorationIdentity')[1]).toBe('{}')
  const claimed = await host.claimRestoration()
  expect(claimed.outcome).toBe('adopted')
  expect(claimed.replayedRecords.map(record => record.kind)).toEqual(['adapter', 'connection'])
  await host.manager.destroy()
})

test('an app without a configured identity is told restoration is unavailable', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'apple' })
  const host = await applicationHost(native)
  expect(() => host.claimRestoration()).toThrow('capability.unavailable')
  await host.manager.destroy()
})

test('the binding reads the configured identity with an empty request and parses null or the exact key set', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'apple' })
  const binding = createReactNativeRustCoreBinding({ platform: 'apple', native })
  expect(await binding.configuredRestorationIdentity()).toBeNull()
  native.restorationAnswer = CONFIGURED
  expect(await binding.configuredRestorationIdentity()).toEqual(CONFIGURED)
  native.restorationAnswer = { ...CONFIGURED, extra: 'x' }
  await expect(binding.configuredRestorationIdentity()).rejects.toMatchObject({
    normalized: { code: 'protocol.malformed' }
  })
})
