'use strict'

// FXD: the desktop adapter watch is atomic with its initial snapshot. A
// same-state OS re-announce that reaches the backend after subscribe must not
// surface as a transition: the watch would otherwise deliver the initial state
// twice (initial snapshot plus a duplicate first transition), and a scenario
// staging a later change reads the stale duplicate first. Slower CI machines
// deliver the queued announce after subscribe instead of before it, which is
// why the TCK fact held on macOS and failed on Linux/Windows.

const { openStagedBackend } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const PLATFORMS = ['corebluetooth', 'bluez', 'winrt']

describe('desktop adapter watch is atomic with its initial snapshot', () => {
  test.each(PLATFORMS)('%s: a same-state re-announce is not a transition', async platform => {
    const opened = await openStagedBackend(platform, central => central.stageAdapterState('powered-on'))
    try {
      const { backend, stage } = opened
      // Stale announce queued after setup settled: the CI schedule delivers it
      // after the watch below subscribes instead of draining it beforehand.
      await stage.stageAdapterState('powered-on', true)
      const watch = await backend.adapter.watchState()
      expect(watch.initial.power).toBe('on')
      const pending = watch.transitions[Symbol.asyncIterator]().next()
      await stage.stageAdapterState('powered-off', true)
      await backend.settleCoreEvents()
      const transition = await pending
      expect(transition.done).toBe(false)
      expect(transition.value.value.power).toBe('off')
      await watch.transitions.close()
    } finally {
      await opened.backend.destroy()
    }
  })
})
