// __tests__/backends/reactnative/background-continuation-declaration.test.js
//
// BGS4: the declared standing order reaches the native owner while the app
// is alive. A `native` declaration without a persisting owner fails fast
// (`capability.unsupported`) — never a silent record-only.

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const { normalizeBleManagerCreateOptions } = require('../../../src/public/host-identity')
const { normalizeBackgroundContinuation } = require('../../../src/backend-contract/background-continuation')
const { validateUnifiedBleExpoPluginOptions } = require('../../../plugin/src/expoPluginSchema')
const { createReactNativeManagerHost } = require('../../../src/react-native-manager')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'

const NATIVE_DECLARATION = Object.freeze({
  onAppearance: 'native',
  peerId: 'a0:9e:1a:e9:b9:3d',
  resubscribe: Object.freeze([{ serviceUuid: HR_SERVICE, characteristicUuid: HR_MEASUREMENT }])
})

function managerHost(native, background) {
  const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
  return createReactNativeManagerHost({
    platform: 'android',
    now: () => 0,
    clientId: 'client-1',
    managerId: 'manager-1',
    hostSessionScope: 'ubm-host:test',
    androidApiLevel: 34,
    rustCore: binding,
    ...(background === undefined ? {} : { background })
  })
}

describe('background.continuation manager options', () => {
  it('keeps build-time and runtime validation aligned across peer identity and selector boundaries', () => {
    const selector = { serviceUuid: HR_SERVICE.toUpperCase(), characteristicUuid: HR_MEASUREMENT }
    const valid = [
      {},
      { onAppearance: 'native', peerId: 'aa:bb:cc:dd:ee:ff', resubscribe: [selector] },
      { onAppearance: 'native', peerId: '9828347e-45df-2eeb-e928-6e443f4065e3', resubscribe: Array(64).fill(selector) },
      { onAppearance: 'headless-task', headlessTaskName: 'Wake' }
    ]
    for (const declaration of valid) {
      const buildTime = validateUnifiedBleExpoPluginOptions({ background: { continuation: declaration } }).background
        .continuation
      expect(normalizeBackgroundContinuation(buildTime)).toEqual(normalizeBackgroundContinuation(declaration))
    }
    const invalid = [
      { peerId: '180d' },
      { peerId: 'not-a-peer' },
      { peerId: 'hci1/dev_AA_BB_CC_DD_EE_FF' },
      { resubscribe: Array(65).fill(selector) },
      { resubscribe: [{ ...selector, serviceOccurrence: 0 }] },
      { resubscribe: [{ ...selector, characteristicOccurrence: 1.5 }] },
      { resubscribe: [{ ...selector, serviceUuid: '180d' }] },
      { onAppearance: 'native', headlessTaskName: 'Wake' },
      { onAppearance: 'headless-task' },
      { onAppearance: 'foreground-service' },
      { undeclared: true }
    ]
    for (const declaration of invalid) {
      expect(() => normalizeBackgroundContinuation(declaration)).toThrow()
      expect(() => validateUnifiedBleExpoPluginOptions({ background: { continuation: declaration } })).toThrow()
    }
  })
  it('accepts canonical Apple peer UUIDs and preserves the native identifier casing', () => {
    const peerId = '9828347e-45df-2eeb-e928-6e443f4065e3'
    const options = normalizeBleManagerCreateOptions({
      background: { continuation: { ...NATIVE_DECLARATION, peerId } }
    })
    expect(options.background.continuation.peerId).toBe(peerId.toUpperCase())
    for (const invalid of ['180d', 'peer-1', `${peerId} `, 'AA:BB:CC:DD:EE']) {
      expect(() => normalizeBleManagerCreateOptions({ background: { continuation: { peerId: invalid } } })).toThrow()
    }
  })
  it('defaults to record-only and freezes the declaration', () => {
    const normalized = normalizeBleManagerCreateOptions({})
    expect(normalized.background).toBeUndefined()
  })

  it('accepts a native standing order beside restoration', () => {
    const normalized = normalizeBleManagerCreateOptions({
      restoration: { restorationId: 'r1' },
      background: { continuation: NATIVE_DECLARATION }
    })
    expect(normalized.background.continuation.onAppearance).toBe('native')
    expect(normalized.background.continuation.peerId).toBe('A0:9E:1A:E9:B9:3D')
  })

  it('refuses an undeclared strategy at the boundary, never defaulting silently', () => {
    expect(() =>
      normalizeBleManagerCreateOptions({ background: { continuation: { onAppearance: 'auto-magic' } } })
    ).toThrow()
    expect(() => normalizeBleManagerCreateOptions({ background: { continuation: { wakeUp: true } } })).toThrow()
  })
})

describe('the declared order reaches the native owner', () => {
  it('forwards the normalized declaration to the binding that persists it', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const persisted = []
    native.declareBackgroundContinuation = async json => {
      persisted.push(json)
    }
    const host = await managerHost(native, { continuation: NATIVE_DECLARATION })
    expect(host.continuation.onAppearance).toBe('native')
    expect(persisted).toHaveLength(1)
    expect(JSON.parse(persisted[0])).toMatchObject({
      onAppearance: 'native',
      peerId: 'A0:9E:1A:E9:B9:3D',
      resubscribe: [{ serviceUuid: HR_SERVICE, characteristicUuid: HR_MEASUREMENT }]
    })
    await host.manager.destroy()
  })

  it('a native declaration with no persisting owner fails fast, never a silent record-only', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    await expect(managerHost(native, { continuation: NATIVE_DECLARATION })).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
  })

  it('record-only opens with no persisting owner and persists nothing', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const host = await managerHost(native, undefined)
    expect(host.continuation.onAppearance).toBe('record-only')
    expect(native.calls.some(call => call[0] === 'declareBackgroundContinuation')).toBe(false)
    await host.manager.destroy()
  })

  it('an absent option leaves a build-time manifest declaration standing', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const declared = []
    native.declareBackgroundContinuation = async json => {
      declared.push(json)
    }
    const host = await managerHost(native, undefined)
    expect(host.continuation.onAppearance).toBe('record-only')
    expect(declared).toEqual([])
    await host.manager.destroy()
  })

  it('an explicit record-only clears a previous declaration', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const declared = []
    native.declareBackgroundContinuation = async json => {
      declared.push(json)
    }
    const host = await managerHost(native, { continuation: { onAppearance: 'record-only' } })
    expect(declared).toHaveLength(1)
    expect(JSON.parse(declared[0]).onAppearance).toBe('record-only')
    await host.manager.destroy()
  })
})
