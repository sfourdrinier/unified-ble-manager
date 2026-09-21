'use strict'

// FXE: the legacy CoreBluetooth adapter watch is atomic with its initial
// snapshot. Every native state callback is emitted to watchers, even when
// availability, authorization, power and safeReason are all unchanged, so the
// watch delivers the initial state twice. Same defect class FXD fixed on
// desktop.

const { attachBackend } = require('../../../src/backend-contract/backend')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const {
  createCoreBluetoothBackendProvider
} = require('../../../src/backends/corebluetooth/corebluetooth-provider')
const {
  InMemoryCoreBluetoothBoundary
} = require('../../../test-support/corebluetooth/in-memory-corebluetooth-boundary')

jest.setTimeout(30000)

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function selectedAdapterId() {
  return opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
}

describe('legacy CoreBluetooth adapter watch is atomic with its initial snapshot', () => {
  test('a same-state re-announce is not a transition', async () => {
    let boundary = null
    const provider = createCoreBluetoothBackendProvider({
      boundaryFactory: () => {
        boundary = new InMemoryCoreBluetoothBoundary({ serviceUuid, characteristicUuid })
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
      boundary.setAdapterState({ availability: 'available', authorization: 'granted', power: 'on', safeReason: null })
      // The real change staged afterwards.
      boundary.setAdapterState({
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
