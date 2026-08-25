// __tests__/public-stream-followup.test.js

const { contractError } = require('../src/backend-contract/errors')
const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')
const { mapIpcConnectionEvents } = require('../src/ipc/public-manager')
const { createPublicGattDatabase } = require('../src/public/gatt')
const { BleCleanupError, collectCleanupPhases } = require('../src/public/error-bridge')
const { BleError } = require('../src/public/errors')

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
  test('rehydrates malformed IPC lifecycle values and source next failures while returning iterators', async () => {
    const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
    const lifecycle = mapIpcConnectionEvents(source, {
      attachmentId: 'attachment-1',
      peerId: 'peer-1',
      connectionId: 'connection-1',
      ownerLeaseId: 'lease-1',
      connectionGeneration: 'generation-1'
    })
    const lifecycleIterator = lifecycle[Symbol.asyncIterator]()
    source.emit(
      {
        kind: 'connection-lifecycle',
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1',
        previous: 'connected',
        current: 'connected',
        cause: 'connected'
      },
      16
    )
    await expect(lifecycleIterator.next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'ipc'
    })

    const sourceError = contractError('platform.failure', 'ipc', 'followup.lifecycle-source')
    const sourceIterator = {
      next: async () => {
        throw sourceError
      },
      return: jest.fn(async () => ({ done: true, value: undefined })),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const rejectedLifecycle = mapIpcConnectionEvents(
      { [Symbol.asyncIterator]: () => sourceIterator },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    const rejectedIterator = rejectedLifecycle[Symbol.asyncIterator]()
    await expect(rejectedIterator.next()).rejects.toMatchObject({
      code: 'platform.failure',
      domain: 'ipc',
      operation: 'followup.lifecycle-source'
    })
    expect(sourceIterator.return).toHaveBeenCalledTimes(1)
    await expect(rejectedIterator.next()).resolves.toEqual({ done: true, value: undefined })

    const malformedItemLifecycle = mapIpcConnectionEvents(
      {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: false, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        })
      },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    await expect(malformedItemLifecycle[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'ipc'
    })

    const malformedIteratorLifecycle = mapIpcConnectionEvents(
      { [Symbol.asyncIterator]: () => null },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    expect(() => malformedIteratorLifecycle[Symbol.asyncIterator]()).toThrow(BleError)

    const hostileEvent = new Proxy(
      { kind: 'value' },
      {
        get() {
          throw new Error('event getter trap')
        }
      }
    )
    const hostileLifecycle = mapIpcConnectionEvents(
      {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: false, value: hostileEvent }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        })
      },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    await expect(hostileLifecycle[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'ipc'
    })
  })

  test('retries IPC iterator return rejection and handles optional or malformed return members', async () => {
    const returnError = contractError('platform.failure', 'ipc', 'followup.lifecycle-return-retry')
    let returnAttempts = 0
    const retryLifecycle = mapIpcConnectionEvents(
      {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: true, value: undefined }),
          return: jest.fn(async () => {
            returnAttempts += 1
            if (returnAttempts === 1) throw returnError
            return { done: true, value: undefined }
          }),
          [Symbol.asyncIterator]() {
            return this
          }
        })
      },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    const retryIterator = retryLifecycle[Symbol.asyncIterator]()
    await expect(retryIterator.return()).rejects.toMatchObject({
      code: 'platform.failure',
      operation: 'followup.lifecycle-return-retry'
    })
    await expect(retryIterator.return()).resolves.toEqual({ done: true, value: undefined })
    expect(returnAttempts).toBe(2)

    const optionalLifecycle = mapIpcConnectionEvents(
      {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        })
      },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    await expect(optionalLifecycle[Symbol.asyncIterator]().return()).resolves.toEqual({
      done: true,
      value: undefined
    })

    const malformedLifecycle = mapIpcConnectionEvents(
      {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: true, value: undefined }),
          return: 1,
          [Symbol.asyncIterator]() {
            return this
          }
        })
      },
      {
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        ownerLeaseId: 'lease-1',
        connectionGeneration: 'generation-1'
      }
    )
    await expect(malformedLifecycle[Symbol.asyncIterator]().return()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'ipc'
    })
  })

  test('projects aggregate primary-plus-cleanup failures before BleCleanupError exposure', () => {
    const cleanup = cleanupRecord('aggregate')
    const primary = new Error('primary failure')
    let aggregate
    try {
      collectCleanupPhases([{ error: primary }, { cleanup }])
    } catch (error) {
      aggregate = error
    }

    expect(aggregate).toBeInstanceOf(AggregateError)
    const cleanupError = aggregate.errors.find(error => error instanceof BleCleanupError)
    expect(cleanupError).toBeDefined()
    expect(cleanupError.cleanup).not.toBe(cleanup)
    expect(cleanupError.cleanup.failures[0].error.platform.metadata.nested.bytes).not.toBe(
      cleanup.failures[0].error.platform.metadata.nested.bytes
    )
    expect(Object.isFrozen(cleanupError.cleanup)).toBe(true)
    expect(Object.isFrozen(cleanupError.cleanup.failures[0].error.platform.metadata.nested)).toBe(true)
  })

  test('projects GATT remove and withSubscription cleanup with deep owned metadata', async () => {
    const values = new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest')
    const rawCleanup = cleanupRecord('gatt')
    const changed = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
    const source = gattSource(values, async () => rawCleanup)
    source.changed = changed
    const database = await createPublicGattDatabase(source)
    const characteristic = database.characteristic('180f', '2a19')
    const emptyDatabase = await createPublicGattDatabase(
      gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), async () => rawCleanup)
    )
    const emptyChangedCleanup = await emptyDatabase.changed.close()
    const repeatedEmptyChangedCleanup = await emptyDatabase.changed.close()
    expect(emptyChangedCleanup).toEqual({ state: 'released', failures: [] })
    expect(repeatedEmptyChangedCleanup).toBe(emptyChangedCleanup)
    expect(Object.isFrozen(emptyDatabase.changed)).toBe(true)
    expect(Object.isFrozen(emptyChangedCleanup)).toBe(true)
    expect(Object.isFrozen(emptyChangedCleanup.failures)).toBe(true)

    const affectedHandleRange = { start: 1, end: 2 }
    const changedIterator = database.changed[Symbol.asyncIterator]()
    changed.emit(
      {
        previousGeneration: 'generation-1',
        reason: 'service-changed',
        affectedHandleRange
      },
      16
    )
    const changedItem = await changedIterator.next()
    expect(changedItem.value.value.affectedHandleRange).not.toBe(affectedHandleRange)
    expect(Object.isFrozen(changedItem.value.value.affectedHandleRange)).toBe(true)
    affectedHandleRange.start = 9
    expect(changedItem.value.value.affectedHandleRange.start).toBe(1)

    const subscription = await characteristic.subscribe()
    const projected = await subscription.remove()
    const repeatedProjected = await subscription.remove()
    const projectedBytes = projected.failures[0].error.platform.metadata.nested.bytes
    expect(projected).toMatchObject({ state: 'release-failed' })
    expect(repeatedProjected).toBe(projected)
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
    const adapterSnapshot = {
      availability: 'available',
      authorization: 'granted',
      power: 'on',
      backendGeneration: 'backend-1',
      updatedAt: 1,
      safeReason: null
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
      adapterState: async () => adapterSnapshot
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
    const adapterIterator = publicWatch.values[Symbol.asyncIterator]()
    adapterSnapshot.updatedAt = 2
    const adapterItem = await adapterIterator.next()
    expect(adapterItem.value.kind).toBe('value')
    expect(Object.isFrozen(adapterItem.value.value)).toBe(true)
    adapterSnapshot.backendGeneration = 'mutated-after-emission'
    expect(adapterItem.value.value.backendGeneration).toBe('backend-1')
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

  test('memoizes successful public manager destroy results and retries failures', async () => {
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      connect: async () => undefined,
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
    const first = manager.destroy()
    const second = manager.destroy()
    await Promise.resolve()
    expect(internal.destroy).toHaveBeenCalledTimes(1)
    const firstResult = await first
    const secondResult = await second
    expect(secondResult).toBe(firstResult)
    expect(Object.isFrozen(firstResult)).toBe(true)
    expect(await manager.destroy()).toBe(firstResult)
    expect(internal.destroy).toHaveBeenCalledTimes(1)

    let attempts = 0
    const retryInternal = {
      ...internal,
      destroy: jest.fn(async () => {
        attempts += 1
        if (attempts === 1) throw new Error('destroy failed')
        return { state: 'released', failures: [] }
      })
    }
    const retryManager = await require('../src/public/ble-manager').createPublicBleManager(retryInternal, () => 0)
    await expect(retryManager.destroy()).rejects.toMatchObject({
      constructor: AggregateError,
      errors: expect.arrayContaining([expect.objectContaining({ message: 'destroy failed' })])
    })
    const retryResult = await retryManager.destroy()
    expect(await retryManager.destroy()).toBe(retryResult)
    expect(retryInternal.destroy).toHaveBeenCalledTimes(2)
  })
})
