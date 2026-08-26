// __tests__/backends/bluez/bluez-connection-controls.test.js
//
// #149: BlueZ exposes no LE connection-parameter surface over D-Bus, so
// `connection:priority` and `connection:parameters` must be *explicitly*
// unsupported — registered with limitations that name the platform gap and the
// privilege requirement — and the fail-closed public errors must carry that
// reason instead of a bare `capability.unsupported`.

const { attachBackend } = require('../../../src/backend-contract/backend')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createFeatureRegistry } = require('../../../src/backend-contract/capabilities')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const {
  BLUEZ_CONNECTION_PRIORITY_LIMITATIONS,
  BLUEZ_CONNECTION_PARAMETERS_LIMITATIONS,
  createBluezConnectionControlRegistrations
} = require('../../../src/backends/bluez/bluez-connection-capabilities')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const {
  BLUEZ_ADAPTER_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function adapterObject() {
  return {
    path: adapterPath,
    interfaces: [
      {
        name: BLUEZ_ADAPTER_INTERFACE,
        properties: {
          Address: { signature: 's', value: '00:11:22:33:44:55' },
          Alias: { signature: 's', value: 'primary' },
          Powered: { signature: 'b', value: true }
        }
      }
    ]
  }
}

async function backendFixture() {
  const boundary = new InMemoryBluezBoundary({ objects: [adapterObject()] })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 20
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  await attachBackend(backend, compatibility())
  return backend
}

function terminal() {
  return { correlation: 'operation-1', outcome: 'succeeded', cause: null }
}

/** Public façade over an internal manager whose capability truth is the real BlueZ registry. */
function bluezCapabilityInternalManager({ withRegistrations = true } = {}) {
  const registry = createFeatureRegistry(withRegistrations ? createBluezConnectionControlRegistrations('test') : [])
  const direct = { id: 'connection:direct', state: 'supported', limitations: [] }
  const capability = id =>
    id === 'connection:direct' ? direct : (registry.descriptors.find(descriptor => descriptor.id === id) ?? null)
  const internalConnection = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    events: {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined })
      })
    },
    discover: async () => {
      throw new Error('not used by this test')
    },
    rediscoverGatt: async () => {
      throw new Error('not used by this test')
    },
    requestPriority: async requested => ({
      requested,
      accepted: true,
      observedAtMonotonicMs: 1,
      terminal: terminal()
    }),
    disconnect: async () => ({ state: 'released', failures: [] }),
    release: async () => ({ state: 'released', failures: [] })
  }
  return {
    supports: id => capability(id)?.state === 'supported' || capability(id)?.state === 'limited',
    capability,
    capabilities: () => registry.descriptors,
    attachedBackend: undefined,
    connect: async () => internalConnection,
    localResourceCounters: () => ({}),
    traceDocument: () => ({ records: [], truncated: false }),
    adapterState: async () => ({}),
    destroy: async () => ({ state: 'released', failures: [] })
  }
}

async function publicConnection(internal) {
  const manager = await createPublicBleManager(internal, () => 0, {
    peerId: value => opaqueId(value, 'peer', 'bluez-connection-controls-test')
  })
  return manager.connect('peer-1')
}

describe('BlueZ connection-control capability truth (#149)', () => {
  test('registers connection:priority and connection:parameters as explicitly unsupported with the platform reason', async () => {
    const backend = await backendFixture()
    try {
      const priority = backend.features.descriptors.find(descriptor => descriptor.id === 'connection:priority')
      const parameters = backend.features.descriptors.find(descriptor => descriptor.id === 'connection:parameters')

      expect(priority).toMatchObject({
        state: 'unsupported',
        evidence: { evidenceLevel: 'blocked' },
        limitations: [
          { code: 'bluez-dbus-exposes-no-connection-priority' },
          { code: 'bluez-connection-interval-is-peer-negotiated' }
        ]
      })
      expect(parameters).toMatchObject({
        state: 'unsupported',
        evidence: { evidenceLevel: 'blocked' },
        limitations: [{ code: 'bluez-dbus-exposes-no-connection-parameters' }]
      })

      // The limitations must teach the caller what the platform cannot do, which
      // privilege the alternative channels demand, and what the consequence is.
      const priorityText = priority.limitations.map(limitation => limitation.explanation).join(' ')
      expect(priorityText).toMatch(/D-Bus/)
      expect(priorityText).toMatch(/CAP_NET_ADMIN/)
      expect(priorityText).toMatch(/debugfs/)
      expect(priorityText).toMatch(/hundreds of milliseconds/)
      const parametersText = parameters.limitations.map(limitation => limitation.explanation).join(' ')
      expect(parametersText).toMatch(/D-Bus/)
      expect(parametersText).toMatch(/not observable/)

      // TCK truth: `unsupported` keeps `connection-parameters-truth-is-explicit`
      // and the priority else-branch behaviour identical to an absent registration.
      for (const id of ['connection:priority', 'connection:parameters']) {
        const registration = backend.features.registrations.find(candidate => candidate.id === id)
        expect(registration.state).toBe('unsupported')
      }
    } finally {
      await backend.destroy()
    }
  })

  test('requestPriority fails closed with an actionable reason instead of a bare capability.unsupported', async () => {
    const internal = bluezCapabilityInternalManager()
    const connection = await publicConnection(internal)

    const error = await connection.controls.requestPriority('high-throughput').then(
      () => {
        throw new Error('requestPriority must fail closed on BlueZ')
      },
      caught => caught
    )

    expect(error).toMatchObject({
      name: 'BleError',
      code: 'capability.unsupported',
      operation: 'public-connection.controls.request-priority',
      limitations: [
        { code: 'bluez-dbus-exposes-no-connection-priority' },
        { code: 'bluez-connection-interval-is-peer-negotiated' }
      ],
      platform: {
        domain: 'capability',
        code: 'bluez-dbus-exposes-no-connection-priority',
        metadata: {
          featureId: 'connection:priority',
          state: 'unsupported',
          limitationCodes: ['bluez-dbus-exposes-no-connection-priority', 'bluez-connection-interval-is-peer-negotiated']
        }
      }
    })
    expect(error.platform.safeMessage).toMatch(/no D-Bus API/)
    expect(error.platform.safeMessage).toMatch(/CAP_NET_ADMIN/)
    expect(error.platform.safeMessage).toMatch(/hundreds of milliseconds/)
    // Never claim link parameters the backend did not observe.
    expect(error.platform.safeMessage).not.toMatch(/\b\d+\s*ms measured/)
  })

  test('parameters() and parameterEvents() fail closed with the observability reason', async () => {
    const internal = bluezCapabilityInternalManager()
    const connection = await publicConnection(internal)

    const expected = {
      name: 'BleError',
      code: 'capability.unsupported',
      limitations: [{ code: 'bluez-dbus-exposes-no-connection-parameters' }],
      platform: {
        domain: 'capability',
        code: 'bluez-dbus-exposes-no-connection-parameters',
        metadata: { featureId: 'connection:parameters', state: 'unsupported' }
      }
    }

    await expect(connection.controls.parameters()).rejects.toMatchObject({
      ...expected,
      operation: 'public-connection.controls.parameters'
    })

    const iterator = connection.controls.parameterEvents()[Symbol.asyncIterator]()
    await expect(iterator.next()).rejects.toMatchObject({
      ...expected,
      operation: 'public-connection.controls.parameter-events'
    })

    const parametersError = await connection.controls.parameters().catch(caught => caught)
    expect(parametersError.platform.safeMessage).toMatch(/not observable/)
    expect(parametersError.platform.safeMessage).toMatch(/Get Connection Information/)
  })

  test('a backend without control registrations still fails closed with the bare contract error', async () => {
    const internal = bluezCapabilityInternalManager({ withRegistrations: false })
    const connection = await publicConnection(internal)

    await expect(connection.controls.requestPriority('high-throughput')).rejects.toMatchObject({
      code: 'capability.unsupported',
      operation: 'public-connection.controls.request-priority',
      platform: null
    })
    await expect(connection.controls.parameters()).rejects.toMatchObject({
      code: 'capability.unsupported',
      operation: 'public-connection.controls.parameters',
      platform: null
    })
  })

  test('exported limitation constants are the registered capability truth', () => {
    const registrations = createBluezConnectionControlRegistrations('test')
    const priority = registrations.find(registration => registration.id === 'connection:priority')
    const parameters = registrations.find(registration => registration.id === 'connection:parameters')
    expect(priority.limitations).toEqual(BLUEZ_CONNECTION_PRIORITY_LIMITATIONS)
    expect(parameters.limitations).toEqual(BLUEZ_CONNECTION_PARAMETERS_LIMITATIONS)
  })
})
