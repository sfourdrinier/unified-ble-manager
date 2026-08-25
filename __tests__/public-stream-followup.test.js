// __tests__/public-stream-followup.test.js

const { contractError } = require('../src/backend-contract/errors')
const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')
const { createPublicGattDatabase } = require('../src/public/gatt')

function limits(itemCapacity, byteCapacity, reservedControlCapacity) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(byteCapacity),
    reservedControlCapacity: capacity(reservedControlCapacity)
  }
}

function cleanupRecord(resourceKind) {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind,
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation: `followup.${resourceKind}.cleanup`,
          platform: {
            domain: 'native',
            code: 'E_CLEANUP',
            safeMessage: 'cleanup failed',
            metadata: {
              nested: {
                bytes: new Uint8Array([1, 2, 3])
              }
            }
          },
          retryability: 'caller-decides'
        }
      }
    ]
  }
}

function gattSource(values, remove) {
  const databasePath = {
    attachment: {},
    attachmentId: 'attachment-1',
    peerId: 'peer-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'connection-generation-1',
    databaseId: 'database-1',
    databaseGeneration: 'database-generation-1'
  }
  const servicePath = { ...databasePath, serviceUuid: '180f', serviceOccurrence: '0' }
  const characteristicPath = {
    ...servicePath,
    characteristicUuid: '2a19',
    characteristicOccurrence: '0',
    validity: 'current'
  }
  return {
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
            notify: true,
            indicate: false
          }
        }
      ],
      descriptors: []
    }),
    read: async () => new Uint8Array(),
    write: async () => ({
      terminal: { correlation: 'write', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
    }),
    writeLong: async () => ({
      terminal: { correlation: 'write', outcome: 'succeeded', cause: null },
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
    writeDescriptor: async () => ({
      terminal: { correlation: 'write', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
    }),
    subscribe: async () => ({ values, remove })
  }
}

function emptyEvents() {
  return {
    [Symbol.asyncIterator]: () => ({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
  }
}

describe('public stream follow-up boundaries', () => {
  test('projects GATT remove and withSubscription cleanup with deep owned metadata', async () => {
    const values = new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest')
    const rawCleanup = cleanupRecord('gatt')
    const database = await createPublicGattDatabase(gattSource(values, async () => rawCleanup))
    const characteristic = database.characteristic('180f', '2a19')

    const subscription = await characteristic.subscribe()
    const projected = await subscription.remove()
    const projectedBytes = projected.failures[0].error.platform.metadata.nested.bytes
    expect(projected).toMatchObject({ state: 'release-failed' })
    expect(projectedBytes).toEqual(new Uint8Array([1, 2, 3]))
    expect(projectedBytes).not.toBe(rawCleanup.failures[0].error.platform.metadata.nested.bytes)
    expect(Object.isFrozen(projected)).toBe(true)
    expect(Object.isFrozen(projected.failures)).toBe(true)
    expect(Object.isFrozen(projected.failures[0])).toBe(true)
    expect(Object.isFrozen(projected.failures[0].error.platform)).toBe(true)
    expect(Object.isFrozen(projected.failures[0].error.platform.metadata)).toBe(true)
    expect(Object.isFrozen(projected.failures[0].error.platform.metadata.nested)).toBe(true)
    rawCleanup.failures[0].error.platform.metadata.nested.bytes[0] = 9
    expect([...projectedBytes]).toEqual([1, 2, 3])

    const secondValues = new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest')
    const secondRawCleanup = cleanupRecord('gatt-with-subscription')
    const secondDatabase = await createPublicGattDatabase(gattSource(secondValues, async () => secondRawCleanup))
    const secondCharacteristic = secondDatabase.characteristic('180f', '2a19')
    let caught
    try {
      await secondCharacteristic.withSubscription({}, async () => 'value')
    } catch (error) {
      caught = error
    }
    expect(caught).toMatchObject({ name: 'BleCleanupError' })
    expect(caught.cleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([1, 2, 3]))
    expect(Object.isFrozen(caught.cleanup.failures[0].error.platform.metadata.nested)).toBe(true)
  })

  test('IPC scan and adapter streams expose only public wrappers and project cleanup', async () => {
    const scanValues = new CoreBoundedStream(limits(1, 64, 1), 'error')
    const scanCleanup = cleanupRecord('ipc-scan')
    const gattValues = new CoreBoundedStream(limits(1, 64, 1), 'error')
    const gattCleanup = cleanupRecord('ipc-gatt')
    const disconnectCleanup = cleanupRecord('ipc-disconnect')
    const destroyCleanup = cleanupRecord('ipc-destroy')
    const capabilities = {
      supports: id => id === 'connection:direct',
      get: id => (id === 'connection:direct' ? { id, state: 'supported', limitations: [] } : undefined),
      require: id => ({ id, state: 'supported', limitations: [] }),
      list: () => []
    }
    const ipc = {
      capabilities,
      bootstrap: {
        discovery: { kind: 'continuous-scan' },
        attachment: {
          adapter: { adapterId: 'adapter-1' },
          backendGeneration: 'backend-1'
        }
      },
      scan: async () => ({ plan: null, observations: scanValues, stop: async () => scanCleanup }),
      connect: async () => {
        const database = gattSource(gattValues, async () => gattCleanup)
        return {
          handle: 'connection-1',
          peerId: 'peer-1',
          attachmentId: 'attachment-1',
          connectionId: 'connection-1',
          ownerLeaseId: 'lease-1',
          connectionGeneration: 'connection-generation-1',
          events: emptyEvents(),
          discover: async () => database,
          rediscoverGatt: async () => database,
          disconnect: async () => disconnectCleanup,
          release: async () => ({ state: 'released', failures: [] })
        }
      },
      destroy: async () => destroyCleanup,
      adapterState: async () => ({
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        backendGeneration: 'backend-1',
        updatedAt: 1,
        safeReason: null
      })
    }
    const manager = new IpcPublicManagerAdapter(ipc, { capabilities })
    const scan = await manager.scan()
    expect(scan.observations.emit).toBeUndefined()
    expect(scan.observations.finishWithReason).toBeUndefined()
    expect(scan.observations.closeWithReason).toBeUndefined()
    expect(typeof scan.observations.limits.itemCapacity).toBe('number')

    scanValues.emit(
      {
        peerId: 'peer-1',
        localName: 'sensor',
        rssi: -40,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      20
    )
    scanValues.emit(
      {
        peerId: 'peer-2',
        localName: 'sensor-2',
        rssi: -41,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      20
    )
    const item = await scan.observations[Symbol.asyncIterator]().next()
    expect(item.value.kind).toBe('terminal')
    expect(item.value.reason).toBe('overflow')
    await expect(scan.stop()).resolves.toMatchObject({ state: 'release-failed' })
    expect(scanCleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([1, 2, 3]))

    const publicWatch = await manager.adapter.watchState()
    expect(publicWatch.values.emit).toBeUndefined()
    expect(publicWatch.values.finishWithReason).toBeUndefined()
    expect(publicWatch.values.closeWithReason).toBeUndefined()
    await expect(publicWatch.stop()).resolves.toMatchObject({ state: 'released' })

    const connection = await manager.connect('peer-1')
    const database = await connection.discover()
    const characteristic = database.characteristic('180f', '2a19')
    const subscription = await characteristic.subscribe()
    expect(subscription.values.emit).toBeUndefined()
    expect(subscription.values.finishWithReason).toBeUndefined()
    expect(subscription.values.closeWithReason).toBeUndefined()
    gattValues.emit({ value: new Uint8Array([1]), delivery: 'notification', indication: false }, 1)
    gattValues.emit({ value: new Uint8Array([2]), delivery: 'notification', indication: false }, 1)
    const gattItem = await subscription.values[Symbol.asyncIterator]().next()
    expect(gattItem.value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(destroyCleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([1, 2, 3]))
  })
})
