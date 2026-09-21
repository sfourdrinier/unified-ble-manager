'use strict'

// FXE: the React Native adapter watch is atomic with its initial snapshot. A
// same-state owner re-announce must not surface as a transition: the watch
// would otherwise deliver the initial state twice (initial snapshot plus a
// duplicate first transition), and a scenario staging a later change reads the
// stale duplicate first. Same defect class FXD fixed on desktop.

const {
  rustCoreHarness,
  environment,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

jest.setTimeout(30000)

describe.each(['android', 'apple'])('%s: adapter watch is atomic with its initial snapshot', platform => {
  test('a same-state re-announce is not a transition', async () => {
    const harness = rustCoreHarness({ platform })
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const backend = manager.attachedBackend.backend
    try {
      const watch = await backend.adapter.watchState()
      expect(watch.initial.power).toBe('on')
      const pending = watch.transitions[Symbol.asyncIterator]().next()
      // Stale re-announce: the owner repeats the exact observable state.
      harness.native.setAdapter({})
      // The real change staged afterwards.
      harness.native.setAdapter({ power: 'off', safeReason: 'fxe real change' })
      await settle(60)
      const transition = await pending
      expect(transition.done).toBe(false)
      expect(transition.value.value.power).toBe('off')
      await watch.transitions.close()
    } finally {
      await manager.destroy()
    }
  })
})
