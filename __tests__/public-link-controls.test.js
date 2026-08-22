const { createPublicBleManager } = require('../src/public/ble-manager')
const { createReactNativeConnectionControlFeatureRegistry } = require('../src/backends/reactnative/react-native-connection-control-features')

function terminal() {
  return { correlation: 'operation-1', outcome: 'succeeded', cause: null }
}

function capability(state) {
  return { state, limitations: state === 'limited' ? [{ code: 'test', explanation: 'test', affectedGuarantee: 'test' }] : [] }
}

function fakeGattDatabase(generation) {
  const path = {
    attachment: {},
    attachmentId: 'attachment-1',
    peerId: 'peer-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'generation-1',
    databaseId: `database-${generation}`,
    databaseGeneration: generation
  }
  return {
    path,
    monotonicNow: () => 100,
    snapshot: async () => ({ path, services: [], characteristics: [], descriptors: [] }),
    read: async () => new Uint8Array(),
    write: async () => ({ terminal: terminal(), commitState: 'confirmed' }),
    writeLong: async () => ({ terminal: terminal(), planState: 'not-planned', commitState: 'not-started', totalBytes: 0, chunkSize: 0, totalChunks: 0, chunks: [], completedChunks: 0, committedBytes: 0, failedChunkIndex: null }),
    readDescriptor: async () => new Uint8Array(),
    writeDescriptor: async () => ({ terminal: terminal(), commitState: 'confirmed' }),
    subscribe: async () => ({ values: { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }) }) }, remove: async () => ({ state: 'released', failures: [] }) })
  }
}

function fakeInternalManager() {
  const descriptors = new Map([
    ['connection:direct', capability('supported')],
    ['connection:rssi', capability('limited')],
    ['connection:request-mtu', capability('limited')],
    ['connection:effective-mtu', capability('unsupported')],
    ['gatt:maximum-write-length', capability('limited')],
    ['connection:parameters', capability('unavailable')]
  ])
  let discoveryGeneration = 1
  const rediscoveryReasons = []
  const maximumWriteLengthRequests = []
  const internalConnection = {
    connectionGeneration: 'generation-1',
    events: { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }), return: async () => ({ done: true, value: undefined }) }) },
    readRssi: async () => ({ rssi: -42, observedAtMonotonicMs: 5678, terminal: terminal() }),
    requestMtu: async requestedMtu => ({
      requestedMtu,
      negotiatedMtu: 185,
      observedAtMonotonicMs: 6789,
      terminal: terminal()
    }),
    maximumWriteLength: async (mode, options) => {
      maximumWriteLengthRequests.push({ mode, options })
      return {
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        mode,
        maximumWriteLength: mode === 'with-response' ? 182 : 185,
        observedAtMonotonicMs: 4321,
        terminal: terminal()
      }
    },
    discover: async () => fakeGattDatabase(`database-${discoveryGeneration++}`),
    rediscoverGatt: async (_options, reason) => {
      rediscoveryReasons.push(reason)
      return fakeGattDatabase(`database-${discoveryGeneration++}`)
    },
    disconnect: async () => ({ state: 'released', failures: [] }),
    release: async () => ({ state: 'released', failures: [] })
  }
  return {
    supports: id => descriptors.get(id)?.state === 'supported' || descriptors.get(id)?.state === 'limited',
    capability: id => descriptors.get(id) ?? null,
    capabilities: () => [],
    attachedBackend: undefined,
    connect: async () => internalConnection,
    localResourceCounters: () => ({}),
    traceDocument: () => ({ records: [], truncated: false }),
    adapterState: async () => ({}),
    destroy: async () => ({ state: 'released', failures: [] }),
    rediscoveryReasons,
    maximumWriteLengthRequests
  }
}

describe('PR8A public link controls', () => {
  test('projects typed observations, separates requests from observations, and exposes recovery under controls', async () => {
    const internal = fakeInternalManager()
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')

    expect(connection).not.toHaveProperty('readRssi')
    expect(connection).not.toHaveProperty('requestMtu')
    expect(Object.keys(connection.controls).sort()).toEqual([
      'effectiveMtu',
      'maximumWriteLength',
      'parameterEvents',
      'parameters',
      'readPhy',
      'readRssi',
      'requestMtu',
      'requestPhy',
      'requestPriority',
      'requestSubrate',
      'writeReadiness'
    ])

    await expect(connection.controls.readRssi()).resolves.toMatchObject({
      state: 'measured',
      rssi: -42,
      connectionGeneration: 'generation-1',
      observedAtMonotonicMs: 5678,
      source: 'backend',
      authority: 'backend-operation',
      limitations: [{ code: 'test' }]
    })
    await expect(connection.controls.requestMtu(185)).resolves.toMatchObject({
      state: 'accepted',
      requestedMtu: 185,
      observedAtMonotonicMs: 6789,
      observation: {
        state: 'measured',
        attMtu: 185,
        payloadBytes: 182,
        connectionGeneration: 'generation-1',
        observedAtMonotonicMs: 6789,
        source: 'backend'
      }
    })
    await expect(connection.controls.maximumWriteLength('without-response')).resolves.toMatchObject({
      state: 'measured',
      mode: 'without-response',
      maximumWriteLength: 185,
      connectionGeneration: 'generation-1',
      observedAtMonotonicMs: 4321,
      source: 'backend',
      authority: 'backend-observation'
    })
    expect(internal.maximumWriteLengthRequests).toEqual([
      { mode: 'without-response', options: { signal: null, deadline: null } }
    ])

    await expect(connection.controls.effectiveMtu()).rejects.toMatchObject({ code: 'capability.unsupported' })
    await expect(connection.controls.requestPriority('high-throughput')).rejects.toMatchObject({ code: 'capability.unsupported' })
    await expect(connection.controls.parameters()).rejects.toMatchObject({ code: 'capability.unavailable' })

    await connection.discover()
    const rediscovered = await connection.rediscoverGatt({ reason: 'manual' })
    expect(rediscovered.generation).toBe('database-2')
    expect(internal.rediscoveryReasons).toEqual(['manual-rediscovery'])
  })

  test('registers canonical link-control capability IDs', () => {
    const registry = createReactNativeConnectionControlFeatureRegistry('android', 'test')
    expect(registry.registrations.map(registration => registration.id)).toEqual([
      'connection:rssi',
      'connection:request-mtu'
    ])
  })
})
