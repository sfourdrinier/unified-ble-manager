const { IpcPublicManagerAdapter } = require('../../src/ipc/public-manager')

function descriptor(id, state, limitations = []) {
  return { id, state, limitations }
}

function capabilities(readinessState = 'unsupported') {
  const descriptors = new Map([
    ['connection:direct', descriptor('connection:direct', 'supported')],
    [
      'connection:rssi',
      descriptor('connection:rssi', 'limited', [
        {
          code: 'ipc-observation',
          explanation: 'The timestamp is assigned by the renderer after the host operation returns.',
          affectedGuarantee: 'observation timestamp authority'
        }
      ])
    ],
    ['gatt:maximum-write-length', descriptor('gatt:maximum-write-length', 'limited')],
    ['connection:effective-mtu', descriptor('connection:effective-mtu', 'unsupported')],
    ['connection:request-mtu', descriptor('connection:request-mtu', 'unsupported')],
    ['connection:priority', descriptor('connection:priority', 'unsupported')],
    ['connection:phy', descriptor('connection:phy', 'unsupported')],
    ['connection:parameters', descriptor('connection:parameters', 'unsupported')],
    ['connection:subrate', descriptor('connection:subrate', 'unsupported')],
    ['gatt:write-without-response-readiness', descriptor('gatt:write-without-response-readiness', readinessState)]
  ])
  return {
    supports: id => descriptors.get(id)?.state === 'supported',
    get: id => descriptors.get(id),
    require: id => descriptors.get(id),
    list: () => [...descriptors.values()]
  }
}

function emptyEvents() {
  return {
    [Symbol.asyncIterator]: () => ({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined })
    })
  }
}

function database(generation) {
  const path = {
    attachment: {},
    attachmentId: 'attachment-1',
    peerId: 'peer-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'connection-generation-1',
    databaseId: `database-${generation}`,
    databaseGeneration: generation
  }
  return {
    path,
    changed: emptyEvents(),
    assertCurrent: () => undefined,
    monotonicNow: () => 1234,
    scheduleDeadline: (_deadline, action) => action(),
    snapshot: async () => ({ path, services: [], characteristics: [], descriptors: [] }),
    read: async () => new Uint8Array(),
    write: async () => ({
      terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
    }),
    writeLong: async () => ({
      terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
      planState: 'not-planned',
      commitState: 'not-started',
      totalBytes: 0,
      chunkSize: 0,
      totalChunks: 0,
      chunks: [],
      completedChunks: 0,
      failedChunkIndex: null,
      committedBytes: 0
    }),
    readDescriptor: async () => new Uint8Array(),
    writeDescriptor: async () => ({
      terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
    }),
    subscribe: async () => ({ values: emptyEvents(), remove: async () => ({ state: 'released', failures: [] }) })
  }
}

function setup(readinessState) {
  const capabilitySnapshot = capabilities(readinessState)
  let discoveryCount = 0
  const calls = []
  const base = {
    handle: 'connection-handle-1',
    peerId: 'peer-1',
    attachmentId: 'attachment-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'connection-generation-1',
    events: emptyEvents(),
    readRssi: async options => {
      calls.push({ kind: 'readRssi', options })
      return -42
    },
    maximumWriteLength: async mode => {
      calls.push({ kind: 'maximumWriteLength', mode })
      return mode === 'without-response' ? 247 : 244
    },
    discover: async options => {
      calls.push({ kind: 'discover', options })
      discoveryCount += 1
      return database(`generation-${discoveryCount}`)
    },
    rediscoverGatt: async (options, reason) => {
      calls.push({ kind: 'rediscover', options, reason })
      discoveryCount += 1
      return database(`generation-${discoveryCount}`)
    },
    disconnect: async () => ({ state: 'released', failures: [] }),
    release: async () => ({ state: 'released', failures: [] })
  }
  const ipc = {
    capabilities: capabilitySnapshot,
    bootstrap: { discovery: { kind: 'continuous-scan' } },
    connect: async () => base,
    adapterState: async () => ({})
  }
  const manager = new IpcPublicManagerAdapter(ipc, {
    capabilities: capabilitySnapshot,
    adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) }
  })
  return { manager, calls }
}

describe('IPC public connection controls', () => {
  test('projects typed observations and removes naked numeric control methods', async () => {
    const { manager, calls } = setup()
    const connection = await manager.connect('peer-1')

    expect(connection.controls).toBeDefined()
    expect(connection).not.toHaveProperty('readRssi')
    expect(connection).not.toHaveProperty('maximumWriteLength')
    expect(connection).not.toHaveProperty('requestMtu')

    const rssi = await connection.controls.readRssi()
    expect(rssi).toMatchObject({
      state: 'measured',
      rssi: -42,
      connectionGeneration: 'connection-generation-1',
      source: 'backend',
      authority: 'ipc-backend-operation'
    })
    expect(rssi.limitations).toEqual(expect.arrayContaining([expect.objectContaining({ code: 'ipc-observation' })]))
    await expect(connection.controls.maximumWriteLength('without-response')).resolves.toMatchObject({
      state: 'measured',
      mode: 'without-response',
      maximumWriteLength: 247,
      connectionGeneration: 'connection-generation-1',
      source: 'backend',
      authority: 'ipc-backend-operation'
    })
    expect(calls.map(call => call.kind)).toEqual(['readRssi', 'maximumWriteLength'])
  })

  test('fails closed for unsupported controls and rejects unsupported streams on first next', async () => {
    const { manager } = setup()
    const connection = await manager.connect('peer-1')
    const unsupportedPromises = [
      connection.controls.effectiveMtu(),
      connection.controls.requestMtu(185),
      connection.controls.requestPriority('balanced'),
      connection.controls.readPhy(),
      connection.controls.requestPhy({ tx: 'le-1m' }),
      connection.controls.parameters(),
      connection.controls.requestSubrate('default')
    ]

    for (const operation of unsupportedPromises) {
      await expect(operation).rejects.toMatchObject({ code: 'capability.unsupported' })
    }

    for (const stream of [
      connection.controls.parameterEvents(),
      connection.controls.writeReadiness('without-response')
    ]) {
      await expect(stream[Symbol.asyncIterator]().next()).rejects.toMatchObject({ code: 'capability.unsupported' })
    }
  })

  test('preserves unavailable readiness state in the renderer stream error', async () => {
    const { manager } = setup('unavailable')
    const connection = await manager.connect('peer-1')

    await expect(connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'capability.unavailable'
    })
  })

  test('validates rediscovery reason and forwards mapped reasons over IPC without replaying writes', async () => {
    const { manager, calls } = setup()
    const connection = await manager.connect('peer-1')

    await expect(connection.rediscoverGatt({ reason: 'invalid' })).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(calls).toEqual([])

    await expect(connection.rediscoverGatt({ reason: 'manual' })).resolves.toMatchObject({
      generation: 'generation-1'
    })
    await expect(connection.rediscoverGatt({ reason: 'service-changed' })).resolves.toMatchObject({
      generation: 'generation-2'
    })
    expect(calls).toEqual([
      expect.objectContaining({ kind: 'rediscover', reason: 'manual-rediscovery' }),
      expect.objectContaining({ kind: 'rediscover', reason: 'service-changed' })
    ])
  })
})
