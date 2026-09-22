'use strict'

// FXE: the WinRT adapter watch is atomic with its initial snapshot. Every
// native adapter record is emitted to watchers, even when availability,
// authorization, power and safeReason are all unchanged, so the watch delivers
// the initial state twice. Same defect class FXD fixed on desktop.

const { attachBackend } = require('../../../src/backend-contract/backend')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createWinRtBackendProvider } = require('../../../src/backends/winrt/winrt-provider')

jest.setTimeout(30000)

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function completed(value) {
  return { completion: Promise.resolve(value) }
}

class DeterministicWinRtAdapterBoundary {
  constructor() {
    this.state = { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
    this.adapterListeners = new Set()
  }

  listAdapters() {
    return completed([
      {
        nativeAdapterId: 'winrt-test-adapter',
        displayName: 'WinRT test adapter',
        state: { ...this.state },
        deployment: 'unpackaged'
      }
    ])
  }

  selectAdapter() {
    return completed(undefined)
  }

  adapterSnapshot() {
    return { ...this.state }
  }

  onAdapterState(listener) {
    this.adapterListeners.add(listener)
    return () => this.adapterListeners.delete(listener)
  }

  onConnectionLost() {
    return () => undefined
  }

  onDatabaseChanged() {
    return () => undefined
  }

  onScanTerminal() {
    return () => undefined
  }

  emitAdapterState(state) {
    this.state = { ...state }
    for (const listener of [...this.adapterListeners]) listener({ ...this.state })
  }

  destroy() {
    return completed(undefined)
  }
}

function selectedAdapterId() {
  return opaqueId('winrt-test-adapter', 'adapter', 'winrt')
}

describe('WinRT adapter watch is atomic with its initial snapshot', () => {
  test('a same-state re-announce is not a transition', async () => {
    let boundary = null
    const provider = createWinRtBackendProvider({
      boundaryFactory: () => {
        boundary = new DeterministicWinRtAdapterBoundary()
        return boundary
      },
      now: () => 20,
      hostKind: 'node'
    })
    const backend = await provider.create({ selectedAdapterId: selectedAdapterId() })
    await attachBackend(backend, compatibility())
    try {
      const watch = await backend.adapter.watchState()
      expect(watch.initial.power).toBe('on')
      const pending = watch.transitions[Symbol.asyncIterator]().next()
      // Stale re-announce: the radio repeats the exact observable state.
      boundary.emitAdapterState({ availability: 'available', authorization: 'granted', power: 'on', safeReason: null })
      // The real change staged afterwards.
      boundary.emitAdapterState({
        availability: 'available',
        authorization: 'granted',
        power: 'off',
        safeReason: 'fxe real change'
      })
      const transition = await pending
      expect(transition.done).toBe(false)
      expect(transition.value.value.power).toBe('off')
      await watch.transitions.close()
    } finally {
      await backend.destroy()
    }
  })
})
