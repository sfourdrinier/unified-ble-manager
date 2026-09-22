// __tests__/backends/reactnative/background-continuation-capabilities.test.js
//
// BGS4: per-strategy capabilities reported at runtime by the instantiated
// backend. Deferred strategies answer `capability.unsupported` with
// "not implemented in this release" — never "the platform cannot".

const { BUILT_IN_FEATURE_IDS } = require('../../../src/backend-contract/capabilities')
const {
  createReactNativeContinuationFeatureRegistry
} = require('../../../src/backends/reactnative/react-native-continuation')

const VERSION = '5.0.0-alpha.1'

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
  it('reports wake + native as limited on API 31+ (deterministic evidence)', () => {
    const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, { androidApiLevel: 34 })
    expect(states(registry)).toMatchObject({
      'background:wake-on-appearance': 'limited',
      'background:native-resubscribe': 'limited',
      'background:headless-task': 'unsupported',
      'background:wake-notification': 'unsupported'
    })
  })

  it('says "not implemented in this release" for deferred strategies, not "platform cannot"', () => {
    const registry = createReactNativeContinuationFeatureRegistry('android', VERSION, { androidApiLevel: 34 })
    for (const id of ['background:headless-task', 'background:wake-notification']) {
      const limitation = limitationFor(registry, id)
      expect(limitation.explanation).toMatch(/not implemented in this release/)
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

describe('apple runtime capabilities (parity deferred to rc.1)', () => {
  it('reports the wake via restoration, native as not-yet-built (not platform-cannot)', () => {
    const registry = createReactNativeContinuationFeatureRegistry('apple', VERSION, { androidApiLevel: null })
    expect(states(registry)).toMatchObject({
      'background:wake-on-appearance': 'limited',
      'background:native-resubscribe': 'unsupported',
      'background:headless-task': 'unsupported',
      'background:wake-notification': 'unsupported'
    })
    expect(limitationFor(registry, 'background:native-resubscribe').explanation).toMatch(
      /not implemented in this release/
    )
  })
})
