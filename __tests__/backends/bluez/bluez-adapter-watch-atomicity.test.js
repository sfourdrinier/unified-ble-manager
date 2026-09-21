'use strict'

// FXE: the BlueZ adapter watch is atomic with its initial snapshot. ANY
// adapter-path D-Bus signal (for example another client toggling Discovering)
// rebroadcasts to watchers, even when availability, authorization, power and
// safeReason are all unchanged, so the watch delivers the initial state twice.
// Same defect class FXD fixed on desktop.

const { attachBackend } = require('../../../src/backend-contract/backend')
const { version, versionRange } = require('../../../src/backend-contract/primitives')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const {
  BLUEZ_ADAPTER_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

jest.setTimeout(30000)

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function adapter(path, address, alias, powered = true) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_ADAPTER_INTERFACE,
        properties: {
          Address: { signature: 's', value: address },
          Alias: { signature: 's', value: alias },
          Powered: { signature: 'b', value: powered }
        }
      }
    ]
  }
}

describe('BlueZ adapter watch is atomic with its initial snapshot', () => {
  test('an unrelated adapter-path signal is not a transition', async () => {
    const boundary = new InMemoryBluezBoundary({
      objects: [adapter('/org/bluez/hci0', '00:00:00:00:00:01', 'primary')]
    })
    const factory = new InMemoryBluezBoundaryFactory([boundary])
    const provider = createBluezBackendProvider({ busKind: 'system', boundaryFactory: factory, now: () => 10 })
    const backend = await provider.create({ selectedAdapterId: '/org/bluez/hci0' })
    await attachBackend(backend, compatibility())
    try {
      const watch = await backend.adapter.watchState()
      expect(watch.initial.power).toBe('on')
      const pending = watch.transitions[Symbol.asyncIterator]().next()
      // Stale announce: another D-Bus client touches Discovering, which is not
      // part of the observable adapter state.
      boundary.holdExternalDiscovery()
      // The real change staged afterwards.
      boundary.objectManager.emitPropertiesChanged('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, {
        Powered: { signature: 'b', value: false }
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
