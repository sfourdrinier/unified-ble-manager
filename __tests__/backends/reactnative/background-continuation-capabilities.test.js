// __tests__/backends/reactnative/background-continuation-capabilities.test.js
//
// Per-strategy capabilities use the instantiated native binding and runtime
// facts. Mechanism admission is separate from OS acceptance or completed work.

const { BUILT_IN_FEATURE_IDS } = require('../../../src/backend-contract/capabilities')
const {
  createReactNativeContinuationFeatureRegistry
} = require('../../../src/backends/reactnative/react-native-continuation')

const VERSION = '5.0.0-alpha.1'

test.each([true, false])(
  'the actual provider derives continuation availability from native methods: %s',
  async available => {
    const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
    const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
    const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    if (available) {
      native.declareBackgroundContinuation = jest.fn(async () => {})
      native.continuationStatus = jest.fn()
      native.prepareContinuationClaim = jest.fn()
      native.acknowledgeContinuationClaim = jest.fn()
    }
    const manager = await createReactNativeBleManagerWithEnvironment(environment(rustCoreHarness({ native })))
    try {
      const actual = states(manager.attachedBackend.backend.features)
      for (const id of ['background:headless-task', 'background:wake-notification', 'background:native-resubscribe']) {
        expect(actual[id]).toBe(available ? 'limited' : 'unsupported')
      }
    } finally {
      expect((await manager.destroy()).state).toBe('released')
    }
  }
)

function states(registry) {
  const out = {}
  for (const registration of registry.registrations) out[registration.id] = registration.state
  return out
}

function limitationFor(registry, id) {
  const registration = registry.registrations.find(r => r.id === id)
  return registration.limitations[0]
}

describe('background.continuation capabilities are built-in', () => {
  it('catalogues one capability per strategy', () => {
    expect(BUILT_IN_FEATURE_IDS.backgroundWakeOnAppearance).toBe('background:wake-on-appearance')
    expect(BUILT_IN_FEATURE_IDS.backgroundNativeResubscribe).toBe('background:native-resubscribe')
    expect(BUILT_IN_FEATURE_IDS.backgroundHeadlessTask).toBe('background:headless-task')
    expect(BUILT_IN_FEATURE_IDS.backgroundWakeNotification).toBe('background:wake-notification')
  })
})

describe('android runtime capabilities', () => {
  it.each([null, 30, NaN, Infinity, 31.5])(
    'does not advertise Android task/service wake on invalid or old API %s',
    api => {
      const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, {
        androidApiLevel: api,
        continuationBindingAvailable: true
      })
      expect(states(registry)['background:headless-task']).toBe('unsupported')
      expect(states(registry)['background:wake-notification']).toBe('unsupported')
    }
  )
  it.each(['android', 'apple'])('requires native ownership methods for %s native resubscribe', platform => {
    const registry = createReactNativeContinuationFeatureRegistry(platform, VERSION, {
      androidApiLevel: 34,
      appleRestorationConfigured: true
    })
    expect(states(registry)['background:native-resubscribe']).toBe('unsupported')
  })
  it('reports wake + native as limited on API 31+ (deterministic evidence)', () => {
    const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, {
      androidApiLevel: 34,
      continuationBindingAvailable: true
    })
    expect(states(registry)).toMatchObject({
      'background:wake-on-appearance': 'limited',
      'background:native-resubscribe': 'limited',
      'background:headless-task': 'limited',
      'background:wake-notification': 'limited'
    })
  })

  it('fails closed when the running native continuation binding is absent', () => {
    const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, { androidApiLevel: 34 })
    for (const id of ['background:headless-task', 'background:wake-notification']) {
      const limitation = limitationFor(registry, id)
      expect(states(registry)[id]).toBe('unsupported')
      expect(limitation.code).toBe('native-continuation-binding-required')
    }
  })

  it('answers unsupported with the platform reason below API 31 (no wake exists)', () => {
    const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, { androidApiLevel: 30 })
    expect(states(registry)).toMatchObject({
      'background:wake-on-appearance': 'unsupported',
      'background:native-resubscribe': 'unsupported'
    })
    expect(limitationFor(registry, 'background:wake-on-appearance').explanation).toMatch(/API 31/)
  })
})

describe('apple runtime continuation capabilities', () => {
  it('reports native continuation only with configured restoration authority', () => {
    const registry = createReactNativeContinuationFeatureRegistry('apple', VERSION, {
      androidApiLevel: null,
      continuationBindingAvailable: true,
      appleRestorationConfigured: true
    })
    expect(states(registry)['background:native-resubscribe']).toBe('limited')
    expect(limitationFor(registry, 'background:native-resubscribe').code).toBe('live-radio-qualification-pending')
  })

  it('does not advertise native wake streaming without restoration configuration', () => {
    const registry = createReactNativeContinuationFeatureRegistry('apple', VERSION, { androidApiLevel: null })
    expect(states(registry)).toMatchObject({
      'background:wake-on-appearance': 'unsupported',
      'background:native-resubscribe': 'unsupported',
      'background:headless-task': 'unsupported',
      'background:wake-notification': 'unsupported'
    })
    expect(limitationFor(registry, 'background:wake-on-appearance').explanation).toMatch(/configured restoration/)
  })

  it('reports actual Apple platform limits for Android-only strategies', () => {
    const registry = createReactNativeContinuationFeatureRegistry('apple', VERSION, { androidApiLevel: null })
    for (const id of ['background:headless-task', 'background:wake-notification']) {
      expect(limitationFor(registry, id).code).toBe('android-only-wake-mechanism')
      expect(limitationFor(registry, id).explanation).not.toMatch(/not implemented/)
    }
  })
})
