const { createPublicGattDatabase } = require('../src/public/gatt')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { resolveStreamPolicy } = require('../src/public/stream-presets')

function terminal() {
  return { correlation: 'operation-1', outcome: 'succeeded', cause: null }
}

function createSource(options = {}) {
  const writeWhenReady = Object.prototype.hasOwnProperty.call(options, 'writeWhenReady')
    ? options.writeWhenReady
    : jest.fn(async () => ({ terminal: terminal(), commitState: 'confirmed' }))
  const databasePath = {
    attachment: {},
    attachmentId: 'attachment-1',
    peerId: 'peer-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'generation-1',
    databaseId: 'database-1',
    databaseGeneration: 'database-generation-1'
  }
  const servicePath = {
    ...databasePath,
    serviceUuid: '180f',
    serviceOccurrence: '0'
  }
  const characteristicPath = {
    ...servicePath,
    characteristicUuid: '2a19',
    characteristicOccurrence: '0',
    validity: 'current'
  }
  return {
    source: {
      path: databasePath,
      monotonicNow: () => 100,
      scheduleDeadline: () => ({ cancel: () => undefined }),
      assertCurrent: () => undefined,
      snapshot: async () => ({
        path: databasePath,
        services: [{ path: servicePath }],
        characteristics: [
          {
            path: characteristicPath,
            properties: {
              read: true,
              writeWithResponse: true,
              writeWithoutResponse: true,
              notify: false,
              indicate: false
            }
          }
        ],
        descriptors: []
      }),
      read: async () => new Uint8Array(),
      write: async () => ({ terminal: terminal(), commitState: 'confirmed' }),
      writeWhenReady,
      maximumWriteLength: async () => ({ maximumWriteLength: 20 }),
      writeLong: async () => ({
        terminal: terminal(),
        planState: 'not-planned',
        commitState: 'not-started',
        totalBytes: 0,
        chunkSize: 0,
        totalChunks: 0,
        chunks: [],
        completedChunks: 0,
        committedBytes: 0,
        failedChunkIndex: null
      }),
      readDescriptor: async () => new Uint8Array(),
      writeDescriptor: async () => ({ terminal: terminal(), commitState: 'confirmed' }),
      subscribe: async () => ({
        values: { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }) }) },
        remove: async () => ({ state: 'released', failures: [] })
      })
    },
    writeWhenReady
  }
}

describe('GattCharacteristic.writeWhenReady', () => {
  test('forces write without response and forwards only operation controls', async () => {
    const fixture = createSource()
    const database = await createPublicGattDatabase(fixture.source)
    const characteristic = database.characteristic('180f', '2a19')
    const value = new Uint8Array([1, 2, 3])

    await expect(characteristic.writeWhenReady(value)).resolves.toMatchObject({ commitState: 'confirmed' })
    expect(fixture.writeWhenReady).toHaveBeenCalledWith(
      expect.objectContaining({ characteristicUuid: '2a19' }),
      value,
      { signal: null, deadline: null, mode: 'without-response' }
    )
  })

  test('forwards only signal and timeout controls without a response selector', async () => {
    const fixture = createSource()
    const database = await createPublicGattDatabase(fixture.source)
    const characteristic = database.characteristic('180f', '2a19')
    const abortController = new AbortController()

    await expect(
      characteristic.writeWhenReady(new Uint8Array([1]), { signal: abortController.signal, timeoutMs: 25 })
    ).resolves.toMatchObject({ commitState: 'confirmed' })
    expect(fixture.writeWhenReady).toHaveBeenCalledWith(
      expect.objectContaining({ characteristicUuid: '2a19' }),
      expect.any(Uint8Array),
      { signal: abortController.signal, deadline: 125, mode: 'without-response' }
    )
  })

  test('preserves the source method receiver', async () => {
    const fixture = createSource({ writeWhenReady: undefined })
    class ReceiverDependentSource {
      constructor(source) {
        Object.assign(this, source)
        delete this.writeWhenReady
        this.database = 'receiver-owned-database'
        this.calls = []
      }

      async writeWhenReady(path, value, options) {
        this.calls.push({ database: this.database, path, value, options })
        return { terminal: terminal(), commitState: 'confirmed' }
      }
    }
    const source = new ReceiverDependentSource(fixture.source)
    const database = await createPublicGattDatabase(source)
    const characteristic = database.characteristic('180f', '2a19')

    await expect(characteristic.writeWhenReady(new Uint8Array([7]))).resolves.toMatchObject({
      commitState: 'confirmed'
    })
    expect(source.calls).toEqual([
      expect.objectContaining({
        database: 'receiver-owned-database',
        path: expect.objectContaining({ characteristicUuid: '2a19' })
      })
    ])
  })

  test('rejects unsupported when the source has no authoritative readiness path', async () => {
    const fixture = createSource({ writeWhenReady: undefined })
    const database = await createPublicGattDatabase(fixture.source)
    const characteristic = database.characteristic('180f', '2a19')

    await expect(characteristic.writeWhenReady(new Uint8Array([1]))).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
  })
})

describe('GattCharacteristic.subscribeRecorded', () => {
  test('forwards the existing characteristic path and returns no value stream', async () => {
    const { source } = createSource()
    const snapshot = source.snapshot
    source.snapshot = async () => {
      const graph = await snapshot()
      graph.characteristics[0].properties.notify = true
      return graph
    }
    const remove = jest.fn(async () => ({ state: 'released', failures: [] }))
    const changed = new CoreBoundedStream(resolveStreamPolicy('latest'), 'latest')
    changed.emit({ recordingId: 'recording-1' }, 0)
    source.subscribeRecorded = jest.fn(async path => ({
      subscriptionId: 'subscription-1',
      path,
      recordingId: 'recording-1',
      changed,
      observedDelivery: 'notification',
      remove
    }))
    const database = await createPublicGattDatabase(source)
    const subscription = await database.characteristic('180f', '2a19').subscribeRecorded({
      recording: { id: 'recording-1', maxBytes: 1048576, maxRecords: 100 },
      delivery: 'require-notification'
    })
    expect(source.subscribeRecorded).toHaveBeenCalledWith(
      expect.objectContaining({ characteristicUuid: '2a19' }),
      expect.objectContaining({
        recording: { id: 'recording-1', maxBytes: 1048576, maxRecords: 100 },
        deliveryMode: 'require-notification'
      })
    )
    expect(subscription).toMatchObject({
      subscriptionId: 'subscription-1',
      recordingId: 'recording-1',
      effectiveDelivery: 'notification'
    })
    expect(subscription).not.toHaveProperty('values')
    expect((await subscription.changed[Symbol.asyncIterator]().next()).value).toEqual({
      kind: 'value',
      value: { recordingId: 'recording-1' }
    })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'released' })
    expect(remove).toHaveBeenCalledTimes(1)
  })

  test('fails unsupported without falling back to an ordinary subscription', async () => {
    const { source } = createSource()
    const snapshot = source.snapshot
    source.snapshot = async () => {
      const graph = await snapshot()
      graph.characteristics[0].properties.notify = true
      return graph
    }
    source.subscribe = jest.fn(source.subscribe)
    const database = await createPublicGattDatabase(source)
    await expect(
      database.characteristic('180f', '2a19').subscribeRecorded({
        recording: { id: 'recording-1', maxBytes: 1048576, maxRecords: 100 }
      })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(source.subscribe).not.toHaveBeenCalled()
  })
})
