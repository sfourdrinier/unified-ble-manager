const { createPublicBleManager } = require('../src/public/ble-manager')
const { createReactNativeConnectionControlFeatureRegistry } = require('../src/backends/reactnative/react-native-connection-control-features')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')

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

function fakeInternalManager({
  readinessEnabled = false,
  readinessObservation = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    ready: true,
    observedAtMonotonicMs: 8000,
    ordinal: 1
  },
  readinessWatchOverride,
  effectiveMtuObservation,
  requestMtuResult,
  maximumWriteLengthResult,
  readPhyResult,
  requestPhyResult
} = {}) {
  const descriptors = new Map([
    ['connection:direct', capability('supported')],
    ['connection:rssi', capability('limited')],
    ['connection:request-mtu', capability('limited')],
    ['connection:priority', capability('limited')],
    ['connection:phy', capability('limited')],
    ['connection:effective-mtu', capability(effectiveMtuObservation === undefined ? 'unsupported' : 'limited')],
    ['gatt:maximum-write-length', capability('limited')],
    ['connection:parameters', capability('unavailable')],
    ...(readinessEnabled ? [['gatt:write-without-response-readiness', capability('limited')]] : [])
  ])
  let discoveryGeneration = 1
  const rediscoveryReasons = []
  const maximumWriteLengthRequests = []
  const priorityRequests = []
  const readiness = new CoreBoundedStream(
    { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const readinessClose = jest.fn(async () => {
    readiness.closeWithReason('owner-released')
    return { state: 'released', failures: [] }
  })
  const readinessWatch =
    readinessWatchOverride ?? {
      events: readiness,
      close: readinessClose
    }
  const internalConnection = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    events: { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }), return: async () => ({ done: true, value: undefined }) }) },
    readRssi: async () => ({ rssi: -42, observedAtMonotonicMs: 5678, terminal: terminal() }),
    requestMtu: async requestedMtu =>
      requestMtuResult === undefined
        ? {
            requestedMtu,
            negotiatedMtu: 185,
            observedAtMonotonicMs: 6789,
            terminal: terminal()
          }
        : requestMtuResult(requestedMtu),
    maximumWriteLength: async (mode, options) => {
      maximumWriteLengthRequests.push({ mode, options })
      if (maximumWriteLengthResult !== undefined) return maximumWriteLengthResult(mode)
      return {
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        mode,
        maximumWriteLength: mode === 'with-response' ? 182 : 185,
        observedAtMonotonicMs: 4321,
        terminal: terminal()
      }
    },
    ...(effectiveMtuObservation === undefined
      ? {}
      : {
          effectiveMtu: async () => ({
            connectionId: 'connection-1',
            connectionGeneration: 'generation-1',
            ...effectiveMtuObservation,
            terminal: terminal()
          })
        }),
    requestPriority: async (requested, options) => {
      priorityRequests.push({ requested, options })
      return {
        requested,
        accepted: true,
        observedAtMonotonicMs: 7890,
        terminal: terminal()
      }
    },
    readPhy: async () =>
      readPhyResult === undefined
        ? {
            txPhy: 'le-2m',
            rxPhy: 'le-coded',
            observedAtMonotonicMs: 8123,
            terminal: terminal()
          }
        : readPhyResult(),
    requestPhy: async (requested, options) =>
      requestPhyResult === undefined
        ? {
            requested,
            accepted: true,
            observation: {
              txPhy: 'le-2m',
              rxPhy: 'le-coded',
              observedAtMonotonicMs: 8124,
              terminal: terminal()
            },
            observedAtMonotonicMs: 8124,
            terminal: terminal()
          }
        : requestPhyResult(requested, options),
    ...(readinessEnabled
      ? {
          writeWithoutResponseReadiness: async () => {
            if (readinessWatchOverride === undefined) readiness.emit(readinessObservation, 128)
            return readinessWatch
          }
        }
      : {}),
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
    maximumWriteLengthRequests,
    priorityRequests,
    readiness,
    readinessClose,
    readinessWatch
  }
}

function readinessEvents(next) {
  const iterator = {
    next: jest.fn(next),
    return: jest.fn(async () => ({ done: true, value: undefined })),
    [Symbol.asyncIterator]() {
      return this
    }
  }
  return {
    [Symbol.asyncIterator]: () => iterator
  }
}

function failedReadinessCleanup() {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind: 'gatt.write-readiness',
        error: { code: 'platform.failure', domain: 'cleanup', operation: 'test-readiness-close' }
      }
    ]
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
    await expect(connection.controls.requestPriority('high-throughput')).resolves.toMatchObject({
      state: 'accepted',
      requested: 'high-throughput',
      connectionGeneration: 'generation-1',
      observedAtMonotonicMs: 7890,
      source: 'backend',
      authority: 'backend-operation'
    })
    await expect(connection.controls.readPhy()).resolves.toMatchObject({
      state: 'measured',
      tx: 'le-2m',
      rx: 'le-coded',
      observedAtMonotonicMs: 8123,
      source: 'backend'
    })
    await expect(connection.controls.requestPhy({ tx: 'le-1m', rx: 'le-coded' })).resolves.toMatchObject({
      state: 'accepted',
      requested: { tx: 'le-1m', rx: 'le-coded' },
      observation: { state: 'measured', tx: 'le-2m', rx: 'le-coded', observedAtMonotonicMs: 8124 },
      source: 'backend'
    })
    const priorityResult = await connection.controls.requestPriority('balanced')
    expect(priorityResult).not.toHaveProperty('observation')
    expect(internal.priorityRequests).toEqual([
      { requested: 'high-throughput', options: { signal: null, deadline: null } },
      { requested: 'balanced', options: { signal: null, deadline: null } }
    ])
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
    await expect(connection.controls.parameters()).rejects.toMatchObject({ code: 'capability.unavailable' })

    await connection.discover()
    const rediscovered = await connection.rediscoverGatt({ reason: 'manual' })
    expect(rediscovered.generation).toBe('database-2')
    expect(internal.rediscoveryReasons).toEqual(['manual-rediscovery'])
  })

  test.each([
    ['connectionId', { connectionId: 'stale-connection' }],
    ['connectionGeneration', { connectionGeneration: 'stale-generation' }]
  ])('rejects readiness observations with a mismatched %s before projection', async (_identity, mismatch) => {
    const internal = fakeInternalManager({
      readinessEnabled: true,
      readinessObservation: {
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        ready: true,
        observedAtMonotonicMs: 8000,
        ordinal: 1,
        ...mismatch
      }
    })
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')
    const iterator = connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]()

    await expect(iterator.next()).rejects.toMatchObject({ code: 'protocol.violation' })
    expect(internal.readinessClose).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['connectionId', { connectionId: 'stale-connection' }],
    ['connectionGeneration', { connectionGeneration: 'stale-generation' }]
  ])('rejects effective MTU observations with a mismatched %s before projection', async (_identity, mismatch) => {
    const internal = fakeInternalManager({
      effectiveMtuObservation: {
        attMtu: 185,
        payloadBytes: 182,
        platformPduBytes: null,
        observedAtMonotonicMs: 8000,
        ...mismatch
      }
    })
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')

    await expect(connection.controls.effectiveMtu()).rejects.toMatchObject({ code: 'protocol.violation' })
  })

  test('P1-04 rejects non-integer MTU, mismatched write-length identity, and invalid PHY enums', async () => {
    const nanMtu = fakeInternalManager({
      requestMtuResult: () => ({
        requestedMtu: 185,
        negotiatedMtu: Number.NaN,
        observedAtMonotonicMs: 1,
        terminal: terminal()
      })
    })
    const nanManager = await createPublicBleManager(nanMtu, () => 1234)
    const nanConnection = await nanManager.connect('peer-1')
    await expect(nanConnection.controls.requestMtu(185)).rejects.toMatchObject({ code: 'protocol.violation' })

    const staleWrite = fakeInternalManager({
      maximumWriteLengthResult: mode => ({
        connectionId: 'stale-connection',
        connectionGeneration: 'generation-1',
        mode,
        maximumWriteLength: 20,
        observedAtMonotonicMs: 1,
        terminal: terminal()
      })
    })
    const staleManager = await createPublicBleManager(staleWrite, () => 1234)
    const staleConnection = await staleManager.connect('peer-1')
    await expect(staleConnection.controls.maximumWriteLength('with-response')).rejects.toMatchObject({
      code: 'protocol.violation'
    })

    const badPhy = fakeInternalManager({
      readPhyResult: () => ({
        txPhy: 'le-coded-s2',
        rxPhy: 'le-2m',
        observedAtMonotonicMs: 1,
        terminal: terminal()
      })
    })
    const phyManager = await createPublicBleManager(badPhy, () => 1234)
    const phyConnection = await phyManager.connect('peer-1')
    await expect(phyConnection.controls.readPhy()).rejects.toMatchObject({ code: 'protocol.violation' })

    const nanEffective = fakeInternalManager({
      effectiveMtuObservation: {
        attMtu: Number.NaN,
        payloadBytes: Number.NaN,
        platformPduBytes: null,
        observedAtMonotonicMs: 1,
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1'
      }
    })
    const nanEffectiveManager = await createPublicBleManager(nanEffective, () => 1234)
    const nanEffectiveConnection = await nanEffectiveManager.connect('peer-1')
    await expect(nanEffectiveConnection.controls.effectiveMtu()).rejects.toMatchObject({
      code: 'protocol.violation'
    })

    const mismatchedMode = fakeInternalManager({
      maximumWriteLengthResult: () => ({
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        mode: 'without-response',
        maximumWriteLength: 20,
        observedAtMonotonicMs: 1,
        terminal: terminal()
      })
    })
    const modeManager = await createPublicBleManager(mismatchedMode, () => 1234)
    const modeConnection = await modeManager.connect('peer-1')
    await expect(modeConnection.controls.maximumWriteLength('with-response')).rejects.toMatchObject({
      code: 'protocol.violation'
    })

    const badRequestPhy = fakeInternalManager({
      requestPhyResult: () => ({
        requested: { tx: 'le-1m', rx: 'le-coded' },
        accepted: true,
        observation: {
          txPhy: 'le-coded-s2',
          rxPhy: 'le-2m',
          observedAtMonotonicMs: 1,
          terminal: terminal()
        },
        observedAtMonotonicMs: 1,
        terminal: terminal()
      })
    })
    const requestPhyManager = await createPublicBleManager(badRequestPhy, () => 1234)
    const requestPhyConnection = await requestPhyManager.connect('peer-1')
    await expect(requestPhyConnection.controls.requestPhy({ tx: 'le-1m', rx: 'le-coded' })).rejects.toMatchObject({
      code: 'protocol.violation'
    })
  })

  test('rejects a runtime-invalid maximum-write mode before dispatch', async () => {
    const internal = fakeInternalManager()
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')

    await expect(connection.controls.maximumWriteLength('without-respons')).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(internal.maximumWriteLengthRequests).toEqual([])
  })

  test('leaves Android PHY registration to the opened runtime capability registry', () => {
    const registry = createReactNativeConnectionControlFeatureRegistry('android', 'test')
    expect(registry.registrations.map(registration => registration.id)).toEqual([
      'connection:rssi',
      'connection:request-mtu',
      'connection:effective-mtu'
    ])
  })

  test('delivers a current readiness snapshot, ordered edge, and owned cleanup', async () => {
    const internal = fakeInternalManager({ readinessEnabled: true })
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')
    const iterator = connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: {
        state: 'measured',
        mode: 'without-response',
        ready: true,
        connectionGeneration: 'generation-1',
        observedAtMonotonicMs: 8000
      }
    })
    internal.readiness.emit(
      {
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        ready: false,
        observedAtMonotonicMs: 8001,
        ordinal: 2
      },
      128
    )
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { ready: false, observedAtMonotonicMs: 8001 }
    })
    await iterator.return()
    expect(internal.readinessClose).toHaveBeenCalledTimes(1)
  })

  test.each([
    [
      'iterator close',
      readinessEvents(async () => ({
        done: false,
        value: {
          kind: 'value',
          value: {
            connectionId: 'connection-1',
            connectionGeneration: 'generation-1',
            ready: true,
            observedAtMonotonicMs: 8000,
            ordinal: 1
          }
        }
      })),
      async iterator => {
        await expect(iterator.next()).resolves.toMatchObject({ done: false })
        await expect(iterator.return()).rejects.toMatchObject({
          cleanup: { state: 'release-failed' }
        })
      }
    ],
    [
      'terminal delivery',
      readinessEvents(async () => ({
        done: false,
        value: { kind: 'terminal', reason: 'closed' }
      })),
      async iterator => {
        await expect(iterator.next()).rejects.toMatchObject({
          cleanup: { state: 'release-failed' }
        })
      }
    ],
    [
      'source error',
      readinessEvents(async () => {
        throw new Error('readiness source failed')
      }),
      async iterator => {
        let failure
        try {
          await iterator.next()
        } catch (error) {
          failure = error
        }
        expect(failure).toBeInstanceOf(AggregateError)
        expect(failure.errors).toEqual(
          expect.arrayContaining([
            expect.objectContaining({ message: 'readiness source failed' }),
            expect.objectContaining({ cleanup: expect.objectContaining({ state: 'release-failed' }) })
          ])
        )
      }
    ]
  ])('retains release-failed cleanup on readiness %s', async (_path, events, assertFailure) => {
    const internal = fakeInternalManager({
      readinessEnabled: true,
      readinessWatchOverride: {
        events,
        close: jest.fn(async () => failedReadinessCleanup())
      }
    })
    const manager = await createPublicBleManager(internal, () => 1234)
    const connection = await manager.connect('peer-1')
    await assertFailure(connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]())
  })
})
