const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const { ElectronRendererBleClient } = require('../../src/electron/renderer')
const { bootstrap } = require('./helpers/public-bootstrap')

function fixture({ acknowledgementFailure = false } = {}) {
  const current = bootstrap()
  let listener
  let releaseAttempts = 0
  let acknowledged = 0
  const commands = []
  const invoke = jest.fn(async request => {
    if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
    if (request.kind === 'release') {
      releaseAttempts += 1
      return {
        kind: 'release',
        cleanup: {
          state: releaseAttempts === 1 ? 'release-failed' : 'released',
          failures:
            releaseAttempts === 1
              ? [
                  {
                    resourceKind: 'scan',
                    error: {
                      code: 'scan.stop-failed',
                      domain: 'scan',
                      operation: 'fixture.release',
                      platform: null,
                      retryability: 'never'
                    }
                  }
                ]
              : []
        }
      }
    }
    const command = request.envelope.command
    commands.push(command)
    if (command === 'connection.connect') {
      const peerId = request.envelope.payload.peerId
      return {
        kind: 'route',
        payload: {
          handle: `connection-${peerId}`,
          peerId,
          connectionId: `id-${peerId}`,
          ownerLeaseId: current.rendererLease.leaseId,
          connectionGeneration: `generation-${peerId}`
        }
      }
    }
    if (command === 'connection.events.subscribe') {
      return {
        kind: 'route',
        payload: {
          handle: request.envelope.payload.connectionEventsHandle,
          connectionId: request.envelope.payload.connectionId,
          connectionGeneration: request.envelope.payload.connectionGeneration,
          eventSchemaVersion: 2
        }
      }
    }
    if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
    if (command === 'connection.events.unsubscribe' || command === 'connection.disconnect') {
      return { kind: 'route', payload: { state: 'released', failures: [] } }
    }
    throw new Error(`unexpected ${command}`)
  })
  return {
    transport: {
      invoke,
      subscribe: callback => {
        listener = callback
        return () => {
          listener = undefined
        }
      },
      acknowledge: async () => {
        acknowledged += 1
        return acknowledgementFailure
          ? {
              kind: 'failure',
              error: {
                code: 'protocol.violation',
                domain: 'ipc',
                operation: 'fixture.ack',
                platform: null,
                retryability: 'never'
              }
            }
          : { kind: 'event.ack' }
      }
    },
    emit: (streamId, item, eventId) => listener({ rendererLease: current.rendererLease, streamId, item, eventId }),
    commands,
    get acknowledged() {
      return acknowledged
    },
    get releaseAttempts() {
      return releaseAttempts
    }
  }
}

async function connectTwo(manager, harness) {
  const first = await manager.connect('first')
  const second = await manager.connect('second')
  const iterators = [first.lifecycleEvents[Symbol.asyncIterator](), second.lifecycleEvents[Symbol.asyncIterator]()]
  for (
    let attempt = 0;
    attempt < 10 && harness.commands.filter(command => command === 'connection.events.ready').length < 2;
    attempt += 1
  ) {
    await new Promise(resolve => setImmediate(resolve))
  }
  expect(harness.commands.filter(command => command === 'connection.events.ready')).toHaveLength(2)
  return iterators
}

describe('Electron public aggregate transport health', () => {
  test('inner loss is visible through the public adapter while the outer queue remains below capacity', async () => {
    const harness = fixture()
    const descriptor = Object.getOwnPropertyDescriptor(ElectronRendererBleClient.prototype, 'events')
    const originalGet = descriptor.get
    let getterAccess = 0
    let resumeInner
    const innerGate = new Promise(resolve => {
      resumeInner = resolve
    })
    const getter = jest.spyOn(ElectronRendererBleClient.prototype, 'events', 'get').mockImplementation(function () {
      getterAccess += 1
      const source = originalGet.call(this)
      if (getterAccess !== 2) return source
      return {
        async *[Symbol.asyncIterator]() {
          await innerGate
          for await (const item of source) yield item
        }
      }
    })
    let manager
    try {
      manager = await createElectronRendererBleManager({ transport: harness.transport })
      const iterators = await connectTwo(manager, harness)
      const pending = iterators.map(iterator => iterator.next())
      for (let batch = 0; batch < 3; batch += 1) {
        for (let index = 0; index < 80; index += 1) {
          const ordinal = batch * 80 + index
          harness.emit('unassigned-inner-event', { kind: 'value', value: { ordinal } }, `inner-event-${ordinal}`)
        }
        for (let turn = 0; turn < 8; turn += 1) await new Promise(resolve => setImmediate(resolve))
      }
      resumeInner()
      await expect(pending[0]).rejects.toMatchObject({
        code: 'stream.overflow',
        operation: 'ipc-manager.aggregate-event-loss',
        platform: { metadata: { attribution: 'unknown' } }
      })
      await expect(pending[1]).rejects.toMatchObject({ code: 'stream.overflow' })
      // The dead event route prevents child unsubscription, so the failed
      // renderer-lease receipt must report those cleanup obligations too.
      await expect(manager.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    } finally {
      resumeInner()
      getter.mockRestore()
    }
  })

  test('outer loss with multiple child IDs fails both consumers without fabricated child counts', async () => {
    const harness = fixture()
    const manager = await createElectronRendererBleManager({ transport: harness.transport })
    const iterators = await connectTwo(manager, harness)
    const pending = iterators.map(iterator => iterator.next())
    for (let index = 0; index < 130; index += 1) {
      harness.emit(
        index % 2 ? 'outer-first' : 'outer-second',
        { kind: 'value', value: { bogus: index } },
        `event-${index}`
      )
    }
    await expect(pending[0]).rejects.toMatchObject({
      code: 'stream.overflow',
      platform: { metadata: { attribution: 'unknown', droppedItems: expect.any(Number) } }
    })
    await expect(pending[1]).rejects.toMatchObject({ code: 'stream.overflow' })
    await expect(manager.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(harness.releaseAttempts).toBe(2)
  })

  test('permanent acknowledgement failure reaches public consumers', async () => {
    const harness = fixture({ acknowledgementFailure: true })
    const manager = await createElectronRendererBleManager({ transport: harness.transport })
    const iterators = await connectTwo(manager, harness)
    const pending = iterators.map(iterator => iterator.next())
    harness.emit('unused-stream', { kind: 'value', value: { value: 1 } }, 'ack-event')
    await expect(pending[0]).rejects.toMatchObject({ code: 'protocol.violation', operation: 'fixture.ack' })
    await expect(pending[1]).rejects.toMatchObject({ code: 'protocol.violation', operation: 'fixture.ack' })
    expectConsoleErrorMatching(
      '[ElectronRendererBleClient] Event acknowledgement failed permanently; terminating event delivery:',
      expect.objectContaining({ error: expect.objectContaining({ code: 'protocol.violation' }) })
    )
    expect(harness.acknowledged).toBe(1)
    await expect(manager.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('outer iterator rejection reaches public consumers with its cause', async () => {
    let rejectIterator
    const iteratorFailure = new Promise((_, reject) => {
      rejectIterator = reject
    })
    const descriptor = Object.getOwnPropertyDescriptor(ElectronRendererBleClient.prototype, 'events')
    const originalGet = descriptor.get
    let firstAccess = true
    const eventsGetter = jest
      .spyOn(ElectronRendererBleClient.prototype, 'events', 'get')
      .mockImplementation(function () {
        if (firstAccess) {
          firstAccess = false
          return {
            async *[Symbol.asyncIterator]() {
              await iteratorFailure
            }
          }
        }
        return originalGet.call(this)
      })
    try {
      const harness = fixture()
      const manager = await createElectronRendererBleManager({ transport: harness.transport })
      const iterators = await connectTwo(manager, harness)
      const pending = iterators.map(iterator => iterator.next())
      rejectIterator(new Error('outer iterator exploded'))
      await expect(pending[0]).rejects.toMatchObject({
        code: 'platform.transport',
        platform: { safeMessage: 'outer iterator exploded' }
      })
      await expect(pending[1]).rejects.toMatchObject({ code: 'platform.transport' })
      await expect(manager.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    } finally {
      eventsGetter.mockRestore()
    }
  })
})
