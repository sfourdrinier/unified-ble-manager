// __tests__/backends/reactnative/rust-core-binding.test.js
//
// PR210-15 / PR210-18 (runtime): the production binding over the
// `UnifiedBleRustCore` TurboModule verifies the binary's build identity and
// revisions before any session exists, re-checks them against the admission
// record, and closes every lease it opened when admission fails. No radio op
// (`invoke`) ever runs before admission completes.

let mockTurboModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => mockTurboModule }
}))

const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const { RUST_CORE_CONTRACT_REVISION } = require('../../../src/backends/reactnative/react-native-rust-core')
const { EXPECTED_NATIVE_BUILD_IDENTITY } = require('../../../src/generated/native-build-identity')

async function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected a rejection')
    },
    error => error.normalized ?? error
  )
}

function counts(native) {
  const count = name => native.calls.filter(call => call[0] === name).length
  return { open: count('openSession'), close: count('closeSession'), invoke: count('invoke') }
}

beforeEach(() => {
  mockTurboModule = null
})

describe('module resolution', () => {
  test('a missing UnifiedBleRustCore module fails loudly as capability.unsupported', () => {
    let thrown = null
    try {
      createReactNativeRustCoreBinding({ platform: 'android' })
    } catch (error) {
      thrown = error.normalized
    }
    expect(thrown).toMatchObject({
      code: 'capability.unsupported',
      operation: 'react-native-manager.rust-core-missing'
    })
  })

  test('the production path resolves the TurboModule and admits a session', async () => {
    mockTurboModule = new DeterministicRustCoreNative({ platform: 'android' })
    const binding = createReactNativeRustCoreBinding({ platform: 'android' })
    const session = await binding.openSession('owner-a')
    expect(session.sessionId).toBe('1')
    expect(session.buildIdentity.binding).toBe('jni')
    await session.close()
    expect(counts(mockTurboModule)).toEqual({ open: 1, close: 1, invoke: 0 })
  })

  test('the pinned contract revision is the sealed identity’s', () => {
    expect(RUST_CORE_CONTRACT_REVISION).toBe(EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision)
  })
})

describe('PR210-18 runtime identity is checked before any session or radio op', () => {
  test.each([
    ['sourceDigest', { sourceDigest: '0'.repeat(64) }],
    ['bindingSchema', { bindingSchema: 'unsealed' }],
    ['binding', { binding: 'uniffi' }],
    ['target', { target: 'x86_64-unknown-linux-gnu' }],
    ['contractRevision', { contractRevision: 'C-UBM.9.9.9' }]
  ])('a foreign %s fails protocol.incompatible with zero opens', async (field, patch) => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.identity = { ...native.identity, ...patch }
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const error = await failure(binding.openSession('owner-a'))
    expect(error).toMatchObject({ code: 'protocol.incompatible', operation: 'react-native-rust-core.native-identity' })
    expect(error.platform.metadata.fields).toContain(field)
    expect(counts(native)).toEqual({ open: 0, close: 0, invoke: 0 })
  })

  test('a malformed identity record fails before any open', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'apple' })
    native.nativeBuildIdentity = async () => '{"schema":"ubm-native-build-identity/1"}'
    const binding = createReactNativeRustCoreBinding({ platform: 'apple', native })
    expect((await failure(binding.openSession('owner-a'))).code).toBe('protocol.incompatible')
    expect(counts(native).open).toBe(0)
  })

  test('a foreign wire or contract revision answer fails before any open', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.wireRevisionAnswer = 'ubm-mobile-wire/0'
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const error = await failure(binding.openSession('owner-a'))
    expect(error.platform.metadata.fields).toEqual(['wireRevision()'])
    native.wireRevisionAnswer = 'ubm-mobile-wire/1'
    native.contractRevisionAnswer = 'C-UBM.0.0.0'
    expect((await failure(binding.openSession('owner-a'))).platform.metadata.fields).toEqual(['contractRevision()'])
    expect(counts(native).open).toBe(0)
  })

  test('a debug-profile source build is admitted (explicit source mode); the digests still bind it', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.identity = { ...native.identity, profile: 'debug' }
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const session = await binding.openSession('owner-a')
    await session.close()
  })
})

describe('PR210-15 every bootstrap failure closes the lease it opened', () => {
  test('a rejected revision query happens before open: nothing to leak', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.contractRevision = async () => {
      throw new Error(JSON.stringify({ code: 'platform.failure', domain: 'platform', operation: 'x', detail: null }))
    }
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    expect((await failure(binding.openSession('owner-a'))).code).toBe('platform.failure')
    expect(counts(native)).toEqual({ open: 0, close: 0, invoke: 0 })
  })

  test('an admission record naming another identity closes exactly once', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.admissionOverride = id =>
      JSON.stringify({
        sessionId: id,
        contractRevision: EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision,
        wireRevision: 'ubm-mobile-wire/1',
        buildIdentity: { ...native.identity, rustc: 'rustc 0.0.0 (other build)' }
      })
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const error = await failure(binding.openSession('owner-a'))
    expect(error.code).toBe('protocol.incompatible')
    expect(error.platform.metadata.fields).toEqual(['buildIdentity'])
    expect(counts(native)).toEqual({ open: 1, close: 1, invoke: 0 })
  })

  test('an incompatible admission revision closes exactly once', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.admissionOverride = id =>
      JSON.stringify({
        sessionId: id,
        contractRevision: 'C-UBM.0.0.0',
        wireRevision: 'ubm-mobile-wire/1',
        buildIdentity: native.identity
      })
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    expect((await failure(binding.openSession('owner-a'))).platform.metadata.fields).toEqual(['contractRevision'])
    expect(counts(native)).toEqual({ open: 1, close: 1, invoke: 0 })
  })

  test('a close failure during bootstrap keeps the original failure and reports the cleanup debt', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.admissionOverride = id =>
      JSON.stringify({
        sessionId: id,
        contractRevision: 'C-UBM.0.0.0',
        wireRevision: 'ubm-mobile-wire/1',
        buildIdentity: native.identity
      })
    native.failNext('closeSession', 'lifecycle.invalid-state', 'cleanup', 'rust-core.close-session', 'release-failed')
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const error = await failure(binding.openSession('owner-a'))
    expect(error.code).toBe('protocol.incompatible')
    expect(error.platform).toMatchObject({
      code: 'cleanup-debt',
      metadata: { sessionId: '1', closeCode: 'lifecycle.invalid-state' }
    })
  })

  test('a malformed admission text is protocol.malformed and names the unaddressable lease', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.admissionOverride = () => '{"sessionId":1.5}'
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const error = await failure(binding.openSession('owner-a'))
    expect(error).toMatchObject({ code: 'protocol.malformed', platform: { code: 'cleanup-debt' } })
    expect(counts(native).invoke).toBe(0)
  })

  test('an open rejected by the owner carries its structured identity', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.failNext('openSession', 'lifecycle.destroyed', 'core', 'ubm-mobile.session.open')
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    expect(await failure(binding.openSession('owner-a'))).toMatchObject({
      code: 'lifecycle.destroyed',
      operation: 'ubm-mobile.session.open'
    })
  })

  test('an unstructured native rejection is protocol.malformed with its text, never a guessed identity', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.openSession = async () => {
      throw new Error('boom')
    }
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    expect(await failure(binding.openSession('owner-a'))).toMatchObject({
      code: 'protocol.malformed',
      platform: { code: 'unstructured-native-rejection', safeMessage: 'boom' }
    })
  })
})

describe('host services', () => {
  test('randomBytes is bounded before the call and decoded strictly', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    expect((await binding.randomBytes(40)).byteLength).toBe(40)
    for (const length of [0, 1025, 1.5]) {
      expect((await failure(binding.randomBytes(length))).code).toBe('argument.invalid')
    }
    expect(native.calls.filter(call => call[0] === 'randomBytes')).toHaveLength(1)
    native.randomBytes = async () => 'AAAA'
    expect((await failure(binding.randomBytes(4))).code).toBe('protocol.malformed')
  })

  test('restorationIdentity sends exactly {restorationId, generation} and parses the exact key set', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'apple' })
    const identity = {
      applicationId: 'com.example',
      restorationId: 'r1',
      generation: 'g1',
      restoreIdentifier: 'com.example.ubm.abc',
      namespaceValue: 'ubm-ns:abc',
      clientId: 'ubm-client:abc',
      hostSessionScope: 'ubm-host:abc'
    }
    native.restorationAnswer = identity
    const binding = createReactNativeRustCoreBinding({ platform: 'apple', native })
    expect(await binding.restorationIdentity({ restorationId: 'r1', generation: 'g1' })).toEqual(identity)
    expect(JSON.parse(native.calls.find(call => call[0] === 'restorationIdentity')[1])).toEqual({
      restorationId: 'r1',
      generation: 'g1'
    })
    expect((await failure(binding.restorationIdentity({ restorationId: 'r2', generation: 'g1' }))).code).toBe(
      'platform.failure'
    )
  })
})
