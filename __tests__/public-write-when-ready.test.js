const { createPublicGattDatabase } = require('../src/public/gatt')

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

  test('rejects unsupported when the source has no authoritative readiness path', async () => {
    const fixture = createSource({ writeWhenReady: undefined })
    const database = await createPublicGattDatabase(fixture.source)
    const characteristic = database.characteristic('180f', '2a19')

    await expect(characteristic.writeWhenReady(new Uint8Array([1]))).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
  })
})
