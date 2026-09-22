// __tests__/backends/reactnative/react-native-apple-rust-authority.test.js
//
// R02 Apple authority on the Rust route: the public Apple provider executes
// the Rust mobile owner through the `UnifiedBleRustCore` module or fails
// loudly. A missing or malformed binding opens zero sessions; a foreign build
// fails before any session; adapter probes release their lease; a foreign
// adapter selection is refused before the owner is touched.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'ios' },
  TurboModuleRegistry: {
    get: name => (name === 'UnifiedBleRustCore' ? mockNativeModule : null)
  },
  NativeModules: {}
}))

const {
  createReactNativeAppleBackendProvider,
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID
} = require('../../../src/react-native')
const { opaqueId } = require('../../../src/backend-contract/primitives')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { rustCoreHarness } = require('../../../test-support/react-native/rust-core-harness')

const APPLE_ADAPTER = opaqueId('apple-corebluetooth-default-adapter', 'adapter', 'react-native-apple')

function count(native, name) {
  return native.calls.filter(call => call[0] === name).length
}

beforeEach(() => {
  mockNativeModule = null
})

describe('R02 Apple Rust authority', () => {
  test('a missing UnifiedBleRustCore module fails loudly before any native session opens', () => {
    expect(() => createReactNativeAppleBackendProvider({ now: () => 20 })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'capability.unsupported' }) })
    )
  })

  test('a malformed injected binding fails loudly and opens zero native sessions', () => {
    for (const rustCore of [{}, { openSession: 'yes' }]) {
      expect(() => createReactNativeAppleBackendProvider({ now: () => 20, rustCore })).toThrow(
        expect.objectContaining({ normalized: expect.objectContaining({ code: 'capability.unsupported' }) })
      )
    }
  })

  test('a foreign native build fails closed before any session opens', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'apple' })
    native.identity = { ...native.identity, binding: 'jni' }
    mockNativeModule = native
    const provider = createReactNativeAppleBackendProvider({ now: () => 20 })
    await expect(provider.create({ selectedAdapterId: APPLE_ADAPTER })).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible' }
    })
    expect(count(native, 'openSession')).toBe(0)
  })

  test('the production module executes open/invoke/close through the owner', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'apple' })
    mockNativeModule = native
    const provider = createReactNativeAppleBackendProvider({ now: () => 20 })
    const backend = await provider.create({ selectedAdapterId: APPLE_ADAPTER })
    expect(backend.identity).toMatchObject({
      registeredBackendId: REACT_NATIVE_APPLE_BACKEND_ID,
      registeredPlatformId: REACT_NATIVE_APPLE_PLATFORM_ID,
      runtime: {
        diagnostics: { boundary: 'ubm-mobile-wire/1', transport: 'native-core-session', nativeBinding: 'uniffi' }
      }
    })
    expect(native.opsInvoked('adapter.state')).toHaveLength(1)
    expect(await backend.destroy()).toEqual({ state: 'released', failures: [] })
    expect(count(native, 'closeSession')).toBe(1)
  })

  test('listAdapters probes the owner and releases the probe session', async () => {
    const harness = rustCoreHarness({ platform: 'apple' })
    const provider = createReactNativeAppleBackendProvider({ now: () => 20, rustCore: harness.binding })
    const adapters = await provider.listAdapters()
    expect(adapters.map(adapter => String(adapter.adapterId))).toEqual(['apple-corebluetooth-default-adapter'])
    expect(harness.native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(count(harness.native, 'closeSession')).toBe(1)
  })

  test('a foreign adapter selection is refused before touching the owner', async () => {
    const harness = rustCoreHarness({ platform: 'apple' })
    const provider = createReactNativeAppleBackendProvider({ now: () => 20, rustCore: harness.binding })
    await expect(
      provider.create({ selectedAdapterId: opaqueId('other-adapter', 'adapter', 'react-native-apple') })
    ).rejects.toMatchObject({ normalized: { code: 'adapter.unavailable' } })
    expect(harness.native.calls).toEqual([])
  })
})
