// __tests__/node-host-adapter-selection.test.js
//
// The Node convenience factory (createBluezBleManager, via
// createNodeBleManagerFromProvider) must "just work" on the common
// single-adapter machine and stay predictable on a multi-adapter host: when the
// caller names no adapter it selects the first one, ordered deterministically by
// id, and it honours an explicit adapterId (including a non-first controller
// such as a USB dongle) rather than falling back.

const { createNodeBleManagerFromProvider } = require('../src/node-host-manager')
const { version, versionRange } = require('../src/backend-contract/primitives')
const { createBluezBackendProvider } = require('../src/backends/bluez/bluez-backend-provider')
const {
  BLUEZ_ADAPTER_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../test-support/bluez/in-memory-bluez-object-manager')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function adapter(path, address, alias) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_ADAPTER_INTERFACE,
        properties: {
          Address: { signature: 's', value: address },
          Alias: { signature: 's', value: alias },
          Powered: { signature: 'b', value: true }
        }
      }
    ]
  }
}

// Deliberately list hci1 before hci0 so the test proves ordering, not input
// order. Each open() consumes one boundary, so hand out enough identical ones
// for listAdapters + create.
function twoAdapterProvider(opens = 4) {
  const boundaries = Array.from(
    { length: opens },
    () =>
      new InMemoryBluezBoundary({
        objects: [
          adapter('/org/bluez/hci1', '00:00:00:00:00:02', 'usb-dongle'),
          adapter('/org/bluez/hci0', '00:00:00:00:00:01', 'built-in')
        ]
      })
  )
  return createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory(boundaries),
    now: () => 10
  })
}

// Record the selection the host layer passes down to the provider, so the test
// asserts the actual adapter chosen rather than reaching into manager internals.
function recordingProvider(real) {
  const selections = []
  const provider = new Proxy(real, {
    get(target, property, receiver) {
      if (property === 'create') {
        return selection => {
          selections.push(selection)
          return target.create(selection)
        }
      }
      const value = Reflect.get(target, property, receiver)
      return typeof value === 'function' ? value.bind(target) : value
    }
  })
  return { provider, selections }
}

describe('Node host adapter selection', () => {
  test('defaults to the first adapter, ordered by id, when the caller names none', async () => {
    const { provider, selections } = recordingProvider(twoAdapterProvider())
    const manager = await createNodeBleManagerFromProvider(provider, compatibility(), {})
    expect(selections.length).toBeGreaterThanOrEqual(1)
    expect(String(selections[0].selectedAdapterId)).toBe('/org/bluez/hci0')
    await manager.destroy()
  })

  test('honours an explicit adapterId, including a non-first controller', async () => {
    const { provider, selections } = recordingProvider(twoAdapterProvider())
    const manager = await createNodeBleManagerFromProvider(provider, compatibility(), {
      adapterId: '/org/bluez/hci1'
    })
    expect(String(selections[0].selectedAdapterId)).toBe('/org/bluez/hci1')
    await manager.destroy()
  })

  test('rejects an adapterId that does not exist instead of falling back to a default', async () => {
    const { provider, selections } = recordingProvider(twoAdapterProvider())
    await expect(
      createNodeBleManagerFromProvider(provider, compatibility(), { adapterId: '/org/bluez/hci9' })
    ).rejects.toMatchObject({ code: 'adapter.unavailable' })
    // Fail closed: a bad selection never silently attaches a different adapter.
    expect(selections).toEqual([])
  })
})
