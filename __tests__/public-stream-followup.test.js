// __tests__/public-stream-followup.test.js

const { contractError } = require('../src/backend-contract/errors')
const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { validateTraceDocument } = require('../src/diagnostics/trace-format')
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

function emptyTrace() {
  return { format: 'unified-ble-trace-v1', truncated: false, records: [] }
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
  test.each(['invalid-limits', 'throwing-values'])(
    'compensates %s after native subscription acquisition',
    async fault => {
      const remove = jest.fn(async () => ({ state: 'released', failures: [] }))
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.subscribe = jest.fn(async () => ({
        subscriptionId: 'acquired-1',
        observedDelivery: 'notification',
        get values() {
          if (fault === 'throwing-values') throw new Error('values getter failed')
          return { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' }
        },
        remove
      }))
      const database = await createPublicGattDatabase(source)
      const characteristic = database.characteristic('180f', '2a19')
      await expect(characteristic.subscribe()).rejects.toThrow()
      expect(remove).toHaveBeenCalledTimes(1)
    }
  )

  test.each(['receipt', 'throw'])(
    'retains failed provisional removal after %s for retry without acquiring another subscription',
    async failureMode => {
      const removalError = new Error('native removal rejected')
      const remove = jest
        .fn()
        .mockImplementationOnce(async () => {
          if (failureMode === 'throw') throw removalError
          return cleanupRecord('gatt-provisional')
        })
        .mockResolvedValue({ state: 'released', failures: [] })
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      const badStream = {
        limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 },
        overflowPolicy: 'error'
      }
      source.subscribe = jest
        .fn()
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-1',
          observedDelivery: 'notification',
          values: badStream,
          remove
        })
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-2',
          observedDelivery: 'notification',
          values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
          remove: async () => ({ state: 'released', failures: [] })
        })
      const database = await createPublicGattDatabase(source)
      const characteristic = database.characteristic('180f', '2a19')
      let rejected
      try {
        await characteristic.subscribe()
      } catch (error) {
        rejected = error
      }
      expect(rejected).toBeInstanceOf(AggregateError)
      expect(rejected.errors).toHaveLength(2)
      expect(rejected.errors[0]).toMatchObject({ code: 'protocol.malformed' })
      if (failureMode === 'throw') expect(rejected.errors[1]).toBe(removalError)
      else expect(rejected.errors[1].cleanup).toMatchObject({ state: 'release-failed' })
      expect(remove).toHaveBeenCalledTimes(1)
      expect(source.subscribe).toHaveBeenCalledTimes(1)
      const next = await characteristic.subscribe()
      expect(remove).toHaveBeenCalledTimes(2)
      expect(source.subscribe).toHaveBeenCalledTimes(2)
      expect(next.effectiveDelivery).toBe('notification')
    }
  )

  test('refuses a new acquisition while provisional cleanup is still failing, then retries the same handle', async () => {
    const remove = jest
      .fn()
      .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
      .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
      .mockResolvedValueOnce({ state: 'released', failures: [] })
    const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
    source.subscribe = jest
      .fn()
      .mockResolvedValueOnce({
        subscriptionId: 'acquired-1',
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      })
      .mockResolvedValueOnce({
        subscriptionId: 'acquired-2',
        observedDelivery: 'notification',
        values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
        remove: async () => ({ state: 'released', failures: [] })
      })
    const database = await createPublicGattDatabase(source)
    const characteristic = database.characteristic('180f', '2a19')
    await expect(characteristic.subscribe()).rejects.toBeInstanceOf(AggregateError)
    await expect(characteristic.subscribe()).rejects.toMatchObject({ cleanup: { state: 'release-failed' } })
    expect(source.subscribe).toHaveBeenCalledTimes(1)
    expect(remove).toHaveBeenCalledTimes(2)
    await expect(characteristic.subscribe()).resolves.toMatchObject({ effectiveDelivery: 'notification' })
    expect(source.subscribe).toHaveBeenCalledTimes(2)
    expect(remove).toHaveBeenCalledTimes(3)
  })

  test('serializes concurrent compensation and retry for the same provisional handle', async () => {
    let resolveRemoval
    const removal = new Promise(resolve => {
      resolveRemoval = resolve
    })
    const remove = jest.fn(() => removal)
    const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
    source.subscribe = jest
      .fn()
      .mockResolvedValueOnce({
        subscriptionId: 'acquired-1',
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      })
      .mockResolvedValueOnce({
        subscriptionId: 'acquired-2',
        observedDelivery: 'notification',
        values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
        remove: async () => ({ state: 'released', failures: [] })
      })
    const database = await createPublicGattDatabase(source)
    const characteristic = database.characteristic('180f', '2a19')
    const first = expect(characteristic.subscribe()).rejects.toMatchObject({ code: 'protocol.malformed' })
    for (let turn = 0; turn < 20 && remove.mock.calls.length === 0; turn += 1) await Promise.resolve()
    expect(remove).toHaveBeenCalledTimes(1)
    const second = characteristic.subscribe()
    await Promise.resolve()
    expect(remove).toHaveBeenCalledTimes(1)
    resolveRemoval({ state: 'released', failures: [] })
    await first
    await expect(second).resolves.toMatchObject({ effectiveDelivery: 'notification' })
    expect(remove).toHaveBeenCalledTimes(1)
    expect(source.subscribe).toHaveBeenCalledTimes(2)
  })

  test('subscribe deadline is measured before deferred provisional cleanup and does not admit a late native subscribe', async () => {
    jest.useFakeTimers()
    try {
      let now = 0
      let releaseCleanup
      const deferredCleanup = new Promise(resolve => {
        releaseCleanup = resolve
      })
      const remove = jest
        .fn()
        .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
        .mockImplementationOnce(() => deferredCleanup)
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.monotonicNow = () => now
      source.subscribe = jest
        .fn()
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-1',
          observedDelivery: 'notification',
          values: {
            limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 },
            overflowPolicy: 'error'
          },
          remove
        })
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-2',
          observedDelivery: 'notification',
          values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
          remove
        })
      const database = await createPublicGattDatabase(source)
      const characteristic = database.characteristic('180f', '2a19')
      await expect(characteristic.subscribe()).rejects.toBeInstanceOf(AggregateError)
      const waiting = expect(characteristic.subscribe({ timeoutMs: 50 })).rejects.toMatchObject({
        code: 'operation.timed-out'
      })
      for (let turn = 0; turn < 20 && remove.mock.calls.length < 2; turn += 1) await Promise.resolve()
      expect(remove).toHaveBeenCalledTimes(2)
      now = 51
      jest.advanceTimersByTime(51)
      await waiting
      expect(source.subscribe).toHaveBeenCalledTimes(1)
      releaseCleanup({ state: 'released', failures: [] })
      await Promise.resolve()
      await Promise.resolve()
      expect(source.subscribe).toHaveBeenCalledTimes(1)
      await expect(characteristic.subscribe()).resolves.toMatchObject({ effectiveDelivery: 'notification' })
      expect(source.subscribe).toHaveBeenCalledTimes(2)
    } finally {
      jest.useRealTimers()
    }
  })

  test.each(['pre-aborted', 'mid-wait-aborted'])(
    'subscribe %s does not wait for or abandon provisional cleanup',
    async scenario => {
      let releaseCleanup
      const deferredCleanup = new Promise(resolve => {
        releaseCleanup = resolve
      })
      const remove = jest
        .fn()
        .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
        .mockImplementationOnce(() => deferredCleanup)
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.subscribe = jest
        .fn()
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-1',
          observedDelivery: 'notification',
          values: {
            limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 },
            overflowPolicy: 'error'
          },
          remove
        })
        .mockResolvedValueOnce({
          subscriptionId: 'acquired-2',
          observedDelivery: 'notification',
          values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
          remove
        })
      const database = await createPublicGattDatabase(source)
      const characteristic = database.characteristic('180f', '2a19')
      await expect(characteristic.subscribe()).rejects.toBeInstanceOf(AggregateError)
      const controller = new AbortController()
      if (scenario === 'pre-aborted') controller.abort()
      const waiting = expect(characteristic.subscribe({ signal: controller.signal })).rejects.toMatchObject({
        code: 'operation.aborted'
      })
      if (scenario === 'mid-wait-aborted') {
        for (let turn = 0; turn < 20 && remove.mock.calls.length < 2; turn += 1) await Promise.resolve()
        expect(remove).toHaveBeenCalledTimes(2)
        controller.abort()
      }
      await waiting
      expect(source.subscribe).toHaveBeenCalledTimes(1)
      expect(remove).toHaveBeenCalledTimes(scenario === 'pre-aborted' ? 1 : 2)
      releaseCleanup({ state: 'released', failures: [] })
      await expect(characteristic.subscribe()).resolves.toMatchObject({ effectiveDelivery: 'notification' })
      expect(source.subscribe).toHaveBeenCalledTimes(2)
    }
  )

  test('failed cleanup on device A does not block device B sharing the manager owner', async () => {
    const removeA = jest.fn(async () => cleanupRecord('device-a'))
    const sourceA = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), removeA)
    sourceA.subscribe = jest.fn(async () => ({
      subscriptionId: 'device-a',
      observedDelivery: 'notification',
      values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
      remove: removeA
    }))
    const sourceB = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), async () => ({
      state: 'released',
      failures: []
    }))
    for (const [source, peer] of [
      [sourceA, 'device-a'],
      [sourceB, 'device-b']
    ]) {
      const originalSnapshot = source.snapshot
      source.snapshot = async () => {
        const snapshot = await originalSnapshot()
        const rekey = path => ({
          ...path,
          peerId: peer,
          connectionId: `connection-${peer}`,
          databaseId: `database-${peer}`
        })
        return {
          ...snapshot,
          path: rekey(snapshot.path),
          services: snapshot.services.map(service => ({ ...service, path: rekey(service.path) })),
          characteristics: snapshot.characteristics.map(characteristic => ({
            ...characteristic,
            path: rekey(characteristic.path)
          }))
        }
      }
    }
    const subscribeB = jest.fn(async () => ({
      subscriptionId: 'device-b',
      observedDelivery: 'notification',
      values: new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'),
      remove: async () => ({ state: 'released', failures: [] })
    }))
    sourceB.subscribe = subscribeB
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: id => ({ id, state: 'supported' }),
      capabilities: () => [],
      connect: jest.fn(async peer => ({
        connectionGeneration: `generation-${peer}`,
        events: emptyEvents(),
        discover: async () => (peer === 'device-a' ? sourceA : sourceB)
      })),
      destroy: jest.fn(async () => cleanupRecord('manager-lease'))
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0, {
      peerId: id => id
    })
    const databaseA = await (await manager.connect('device-a')).discover()
    const databaseB = await (await manager.connect('device-b')).discover()
    await expect(databaseA.characteristic('180f', '2a19').subscribe()).rejects.toBeInstanceOf(AggregateError)
    await expect(databaseB.characteristic('180f', '2a19').subscribe()).resolves.toMatchObject({
      effectiveDelivery: 'notification'
    })
    expect(subscribeB).toHaveBeenCalledTimes(1)
    expect(removeA).toHaveBeenCalledTimes(1)
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    for (let turn = 0; turn < 20 && removeA.mock.calls.length < 2; turn += 1) await Promise.resolve()
    expect(removeA).toHaveBeenCalledTimes(2)
  })

  test('manager owns failed provisional removal after the caller drops its GATT objects', async () => {
    const remove = jest
      .fn()
      .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
      .mockResolvedValueOnce(cleanupRecord('gatt-provisional'))
      .mockResolvedValueOnce({ state: 'released', failures: [] })
    const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
    source.subscribe = jest.fn(async () => ({
      subscriptionId: 'acquired-1',
      observedDelivery: 'notification',
      values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
      remove
    }))
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: id => ({ id, state: 'supported' }),
      capabilities: () => [],
      connect: jest.fn(async () => ({
        connectionGeneration: 'generation-1',
        events: emptyEvents(),
        discover: async () => source,
        release: async () => ({ state: 'released', failures: [] }),
        disconnect: async () => ({ state: 'released', failures: [] })
      })),
      destroy: jest
        .fn()
        .mockResolvedValueOnce(cleanupRecord('manager-lease'))
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0, {
      peerId: id => id
    })
    {
      const connection = await manager.connect('peer-1')
      const database = await connection.discover()
      const characteristic = database.characteristic('180f', '2a19')
      await expect(characteristic.subscribe()).rejects.toBeInstanceOf(AggregateError)
    }
    expect(remove).toHaveBeenCalledTimes(1)
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(remove).toHaveBeenCalledTimes(2)
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(remove).toHaveBeenCalledTimes(3)
    expect(internal.destroy).toHaveBeenCalledTimes(2)
  })

  test.each(['released', 'release-failed'])(
    'public manager reaches parent destroy while provisional cleanup is unresolved (%s)',
    async leaseState => {
      let releaseCleanup
      const cleanupGate = new Promise(resolve => {
        releaseCleanup = resolve
      })
      const remove = jest.fn(() => cleanupGate)
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.subscribe = jest.fn(async () => ({
        subscriptionId: 'acquired-1',
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      }))
      const firstReceipt =
        leaseState === 'released' ? { state: 'released', failures: [] } : cleanupRecord('manager-lease')
      const internal = {
        identity: null,
        attachedBackend: undefined,
        traceDocument: emptyTrace,
        supports: () => true,
        capability: id => ({ id, state: 'supported' }),
        capabilities: () => [],
        connect: async () => ({
          connectionGeneration: 'generation-1',
          events: emptyEvents(),
          discover: async () => source
        }),
        destroy: jest
          .fn()
          .mockResolvedValueOnce(firstReceipt)
          .mockResolvedValueOnce({ state: 'released', failures: [] })
      }
      const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0, {
        peerId: id => id
      })
      const connection = await manager.connect('peer-1')
      const database = await connection.discover()
      const subscribeOutcome = database
        .characteristic('180f', '2a19')
        .subscribe()
        .catch(error => error)
      for (let turn = 0; turn < 20 && remove.mock.calls.length === 0; turn += 1) await Promise.resolve()
      expect(remove).toHaveBeenCalledTimes(1)
      const destroying = manager.destroy()
      for (let turn = 0; turn < 20 && internal.destroy.mock.calls.length === 0; turn += 1) await Promise.resolve()
      expect(internal.destroy).toHaveBeenCalledTimes(1)
      await expect(destroying).resolves.toMatchObject({ state: leaseState })
      if (leaseState === 'released') {
        await expect(subscribeOutcome).resolves.toMatchObject({ code: 'protocol.malformed' })
      }
      releaseCleanup(
        leaseState === 'released' ? cleanupRecord('late-provisional') : { state: 'released', failures: [] }
      )
      if (leaseState === 'release-failed') {
        await expect(subscribeOutcome).resolves.toMatchObject({ code: 'protocol.malformed' })
      }
      if (leaseState === 'release-failed') {
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(internal.destroy).toHaveBeenCalledTimes(2)
      } else {
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(internal.destroy).toHaveBeenCalledTimes(1)
      }
    }
  )

  test.each(['reject', 'failed-receipt'])(
    'public manager retains late provisional %s after a failed parent release',
    async outcome => {
      let settleCleanup
      const cleanupGate = new Promise((resolve, reject) => {
        settleCleanup = outcome === 'reject' ? reject : resolve
      })
      const lateError = new Error('late provisional remove failed')
      const remove = jest.fn().mockReturnValueOnce(cleanupGate).mockResolvedValue({ state: 'released', failures: [] })
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.subscribe = jest.fn(async () => ({
        subscriptionId: 'acquired-1',
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      }))
      const internal = {
        identity: null,
        attachedBackend: undefined,
        traceDocument: emptyTrace,
        supports: () => true,
        capability: id => ({ id, state: 'supported' }),
        capabilities: () => [],
        connect: async () => ({
          connectionGeneration: 'generation-1',
          events: emptyEvents(),
          discover: async () => source
        }),
        destroy: jest
          .fn()
          .mockResolvedValueOnce(cleanupRecord('manager-lease'))
          .mockResolvedValueOnce(cleanupRecord('manager-lease'))
          .mockResolvedValueOnce({ state: 'released', failures: [] })
      }
      const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0, {
        peerId: id => id
      })
      const connection = await manager.connect('peer-1')
      const database = await connection.discover()
      const subscribing = database
        .characteristic('180f', '2a19')
        .subscribe()
        .catch(error => error)
      for (let turn = 0; turn < 20 && remove.mock.calls.length === 0; turn += 1) await Promise.resolve()
      expect(remove).toHaveBeenCalledTimes(1)
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      settleCleanup(outcome === 'reject' ? lateError : cleanupRecord('late-provisional'))
      await subscribing
      for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
      if (outcome === 'reject') {
        await expect(manager.destroy()).rejects.toMatchObject({
          errors: expect.arrayContaining([lateError, expect.objectContaining({ name: 'BleCleanupError' })])
        })
      } else {
        await expect(manager.destroy()).resolves.toMatchObject({
          state: 'release-failed',
          failures: expect.arrayContaining([expect.objectContaining({ resourceKind: 'late-provisional' })])
        })
      }
      expect(remove).toHaveBeenCalledTimes(2)
      expect((await manager.diagnostics.startTrace().stop()).records).toEqual(
        expect.arrayContaining([expect.objectContaining({ event: 'manager.gatt-unsubscribe.late-failed' })])
      )
      await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(internal.destroy).toHaveBeenCalledTimes(3)
    }
  )

  test.each(['released', 'release-failed'])(
    'public manager reaches parent destroy while scan stop is unresolved (%s)',
    async leaseState => {
      let releaseStop
      const stopGate = new Promise(resolve => {
        releaseStop = resolve
      })
      const nativeStop = jest.fn(() => stopGate)
      const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
      const firstReceipt =
        leaseState === 'released' ? { state: 'released', failures: [] } : cleanupRecord('manager-lease')
      const internal = {
        identity: null,
        attachedBackend: undefined,
        supports: () => true,
        capability: () => null,
        capabilities: () => [],
        scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
        connect: jest.fn(),
        destroy: jest
          .fn()
          .mockResolvedValueOnce(firstReceipt)
          .mockResolvedValueOnce({ state: 'released', failures: [] })
      }
      const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
      await manager.scan()
      const destroying = manager.destroy()
      for (let turn = 0; turn < 20 && nativeStop.mock.calls.length === 0; turn += 1) await Promise.resolve()
      expect(nativeStop).toHaveBeenCalledTimes(1)
      for (let turn = 0; turn < 20 && internal.destroy.mock.calls.length === 0; turn += 1) await Promise.resolve()
      expect(internal.destroy).toHaveBeenCalledTimes(1)
      await expect(destroying).resolves.toMatchObject({ state: leaseState })
      releaseStop({ state: 'released', failures: [] })
      await Promise.resolve()
      if (leaseState === 'release-failed') {
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(internal.destroy).toHaveBeenCalledTimes(2)
      } else {
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(internal.destroy).toHaveBeenCalledTimes(1)
      }
    }
  )

  test.each(['resolve', 'reject'])(
    'released parent settles an in-flight scan stop and ignores its late %s',
    async lateOutcome => {
      let settleStop
      const stopGate = new Promise((resolve, reject) => {
        settleStop = lateOutcome === 'resolve' ? resolve : reject
      })
      const nativeStop = jest.fn(() => stopGate)
      const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
      const internal = {
        identity: null,
        attachedBackend: undefined,
        supports: () => true,
        capability: () => null,
        capabilities: () => [],
        scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
        connect: jest.fn(),
        destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
      const scan = await manager.scan()
      const states = scan.state[Symbol.asyncIterator]()
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
      const stopping = scan.stop()
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopping' } })
      await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(stopping).resolves.toEqual({ state: 'released', failures: [] })
      await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopped' } })
      await expect(states.next()).resolves.toMatchObject({ done: true })
      expect(nativeStop).toHaveBeenCalledTimes(1)
      settleStop(lateOutcome === 'resolve' ? { state: 'released', failures: [] } : new Error('late native stop'))
      await Promise.resolve()
      expect(nativeStop).toHaveBeenCalledTimes(1)
    }
  )

  test('parent release does not disguise a failed local scan iterator return', async () => {
    const localError = new Error('local iterator return failed')
    const sourceReturn = jest.fn().mockRejectedValueOnce(localError).mockResolvedValue({ done: true, value: undefined })
    const source = {
      limits: limits(2, 64, 1),
      overflowPolicy: 'drop-oldest',
      [Symbol.asyncIterator]: () => ({
        next: () => new Promise(() => undefined),
        return: sourceReturn
      })
    }
    const nativeStop = jest.fn(async () => ({ state: 'released', failures: [] }))
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
    const scan = await manager.scan()
    scan.observations[Symbol.asyncIterator]().next()
    await expect(manager.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([localError])
    })
    expect(sourceReturn).toHaveBeenCalledTimes(1)
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(sourceReturn).toHaveBeenCalledTimes(2)
    expect(nativeStop).not.toHaveBeenCalled()
  })

  test('public manager reports a late scan-stop rejection after a failed parent release', async () => {
    let rejectStop
    const stopGate = new Promise((resolve, reject) => {
      rejectStop = reject
    })
    const lateStopError = new Error('late scan stop failed')
    const nativeStop = jest.fn().mockReturnValueOnce(stopGate).mockResolvedValue({ state: 'released', failures: [] })
    const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
    const internal = {
      identity: null,
      attachedBackend: undefined,
      traceDocument: emptyTrace,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
      connect: jest.fn(),
      destroy: jest
        .fn()
        .mockResolvedValueOnce(cleanupRecord('manager-lease'))
        .mockResolvedValueOnce(cleanupRecord('manager-lease'))
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
    await manager.scan()
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    rejectStop(lateStopError)
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    await expect(manager.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([lateStopError, expect.objectContaining({ name: 'BleCleanupError' })])
    })
    expect(internal.destroy).toHaveBeenCalledTimes(2)
    expect(nativeStop).toHaveBeenCalledTimes(2)
    expect((await manager.diagnostics.startTrace().stop()).records).toEqual(
      expect.arrayContaining([expect.objectContaining({ event: 'manager.scan-stop.late-failed' })])
    )
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(internal.destroy).toHaveBeenCalledTimes(3)
  })

  test('confirmed parent release supersedes a historical late scan-stop failure', async () => {
    let rejectStop
    const stopGate = new Promise((resolve, reject) => {
      rejectStop = reject
    })
    const nativeStop = jest.fn().mockReturnValueOnce(stopGate).mockResolvedValue({ state: 'released', failures: [] })
    const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
    const internal = {
      identity: null,
      attachedBackend: undefined,
      traceDocument: emptyTrace,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
      connect: jest.fn(),
      destroy: jest
        .fn()
        .mockResolvedValueOnce(cleanupRecord('manager-lease'))
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    }
    const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
    await manager.scan()
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    rejectStop(new Error('private late details'))
    for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    const trace = await manager.diagnostics.startTrace().stop()
    expect(trace.records).toEqual(
      expect.arrayContaining([expect.objectContaining({ event: 'manager.scan-stop.late-failed' })])
    )
    expect(JSON.stringify(trace)).not.toContain('private late details')
    expect(validateTraceDocument(trace).valid).toBe(true)
  })

  test.each(['reject', 'failed-receipt'])(
    'confirmed parent release supersedes a promptly settled scan-stop %s',
    async outcome => {
      const privateError = new Error('private immediate stop details')
      const nativeStop = jest.fn(async () => {
        if (outcome === 'reject') throw privateError
        return cleanupRecord('scan-stop')
      })
      const source = new CoreBoundedStream(limits(2, 64, 1), 'drop-oldest')
      const internal = {
        identity: null,
        attachedBackend: undefined,
        traceDocument: emptyTrace,
        supports: () => true,
        capability: () => null,
        capabilities: () => [],
        scan: jest.fn(async () => ({ observations: source, stop: nativeStop })),
        connect: jest.fn(),
        destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      const manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0)
      await manager.scan()
      await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      const trace = await manager.diagnostics.startTrace().stop()
      expect(trace.records).toEqual(
        expect.arrayContaining([expect.objectContaining({ event: 'manager.scan-stop.failed' })])
      )
      expect(JSON.stringify(trace)).not.toContain('private immediate stop details')
      expect(validateTraceDocument(trace).valid).toBe(true)
    }
  )

  test.each(['successful-retry', 'simultaneous-failure'])(
    'IPC manager compensates a malformed projected subscription with %s',
    async scenario => {
      const transportError = new Error('transport unavailable')
      const remove = jest.fn().mockResolvedValueOnce(cleanupRecord('ipc-gatt-provisional'))
      if (scenario === 'simultaneous-failure') remove.mockResolvedValueOnce(cleanupRecord('ipc-gatt-provisional'))
      remove.mockResolvedValueOnce({ state: 'released', failures: [] })
      const databaseSource = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      databaseSource.subscribe = jest.fn(async () => ({
        subscriptionId: 'ipc-acquired-1',
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      }))
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
          attachment: { adapter: { adapterId: 'adapter-1' }, backendGeneration: 'backend-1' }
        },
        connect: async () => ({
          handle: 'connection-1',
          peerId: 'peer-1',
          attachmentId: 'attachment-1',
          connectionId: 'connection-1',
          ownerLeaseId: 'lease-1',
          connectionGeneration: 'generation-1',
          events: emptyEvents(),
          discover: async () => databaseSource,
          release: async () => ({ state: 'released', failures: [] })
        }),
        destroy:
          scenario === 'simultaneous-failure'
            ? jest.fn().mockRejectedValueOnce(transportError).mockResolvedValueOnce({ state: 'released', failures: [] })
            : jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      const manager = new IpcPublicManagerAdapter(ipc, { capabilities })
      {
        const connection = await manager.connect('peer-1')
        const database = await connection.discover()
        await expect(database.characteristic('180f', '2a19').subscribe()).rejects.toBeInstanceOf(AggregateError)
      }
      expect(remove).toHaveBeenCalledTimes(1)
      if (scenario === 'simultaneous-failure') {
        let rejected
        try {
          await manager.destroy()
        } catch (error) {
          rejected = error
        }
        expect(rejected).toBeInstanceOf(AggregateError)
        expect(rejected.errors).toContain(transportError)
        expect(rejected.errors).toEqual(expect.arrayContaining([expect.objectContaining({ name: 'BleCleanupError' })]))
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(remove).toHaveBeenCalledTimes(3)
        expect(ipc.destroy).toHaveBeenCalledTimes(2)
      } else {
        await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
        expect(remove).toHaveBeenCalledTimes(2)
        expect(ipc.destroy).toHaveBeenCalledTimes(1)
      }
    }
  )

  test.each(['public', 'ipc'])(
    '%s manager treats authoritative lease release as settling provisional cleanup debt',
    async kind => {
      const remove = jest.fn(async () => cleanupRecord(`${kind}-provisional`))
      const source = gattSource(new CoreBoundedStream(limits(2, 32, 1), 'drop-oldest'), remove)
      source.subscribe = jest.fn(async () => ({
        subscriptionId: `${kind}-acquired-1`,
        observedDelivery: 'notification',
        values: { limits: { itemCapacity: 0, byteCapacity: 32, reservedControlCapacity: 1 }, overflowPolicy: 'error' },
        remove
      }))
      const destroy = jest.fn(async () => ({ state: 'released', failures: [] }))
      let manager
      if (kind === 'public') {
        const internal = {
          identity: null,
          attachedBackend: undefined,
          supports: () => true,
          capability: id => ({ id, state: 'supported' }),
          capabilities: () => [],
          connect: async () => ({
            connectionGeneration: 'generation-1',
            events: emptyEvents(),
            discover: async () => source
          }),
          destroy
        }
        manager = await require('../src/public/ble-manager').createPublicBleManager(internal, () => 0, {
          peerId: id => id
        })
      } else {
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
            attachment: { adapter: { adapterId: 'adapter-1' }, backendGeneration: 'backend-1' }
          },
          connect: async () => ({
            handle: 'connection-1',
            peerId: 'peer-1',
            attachmentId: 'attachment-1',
            connectionId: 'connection-1',
            ownerLeaseId: 'lease-1',
            connectionGeneration: 'generation-1',
            events: emptyEvents(),
            discover: async () => source
          }),
          destroy
        }
        manager = new IpcPublicManagerAdapter(ipc, { capabilities })
      }
      {
        const connection = await manager.connect('peer-1')
        const database = await connection.discover()
        let rejected
        try {
          await database.characteristic('180f', '2a19').subscribe()
        } catch (error) {
          rejected = error
        }
        expect(rejected).toBeInstanceOf(AggregateError)
        expect(rejected.errors[1].cleanup).toMatchObject({ state: 'release-failed' })
      }
      expect(remove).toHaveBeenCalledTimes(1)
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
      expect(remove).toHaveBeenCalledTimes(2)
      expect(destroy).toHaveBeenCalledTimes(kind === 'public' ? 1 : 2)
    }
  )

  test('IPC manager destroy preserves a lone transport rejection and remains retryable', async () => {
    const transportError = new Error('transport unavailable')
    const capabilities = {
      supports: () => false,
      get: () => undefined,
      require: () => undefined,
      list: () => []
    }
    const ipc = {
      capabilities,
      bootstrap: { discovery: { kind: 'continuous-scan' }, attachment: { adapter: { adapterId: 'adapter-1' } } },
      destroy: jest
        .fn()
        .mockRejectedValueOnce(transportError)
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    }
    const manager = new IpcPublicManagerAdapter(ipc, { capabilities })
    await expect(manager.destroy()).rejects.toBe(transportError)
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(ipc.destroy).toHaveBeenCalledTimes(2)
  })

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

  test('collectCleanupPhases returns projected failures when no phase throws', () => {
    const cleanup = cleanupRecord('scan')
    const collected = collectCleanupPhases([{ cleanup }])
    expect(collected.state).toBe('release-failed')
    expect(collected.failures[0]).not.toBe(cleanup.failures[0])
    expect(collected.failures[0].error.platform.metadata.nested.bytes).not.toBe(
      cleanup.failures[0].error.platform.metadata.nested.bytes
    )
    cleanup.failures[0].error.platform.metadata.nested.bytes[0] = 9
    expect(collected.failures[0].error.platform.metadata.nested.bytes[0]).toBe(1)
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
    gattValues.emit({ value: new Uint8Array([1]), delivery: 'notification' }, 1)
    gattValues.emit({ value: new Uint8Array([2]), delivery: 'notification' }, 1)
    const gattItem = await subscription.values[Symbol.asyncIterator]().next()
    expect(gattItem.value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(destroyCleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([1, 2, 3]))
  })

  test('IPC adapter watch stream close stops polling and removes the abort listener', async () => {
    jest.useFakeTimers()
    try {
      const capabilities = {
        require: id => ({ id, state: 'supported', limitations: [] }),
        list: () => []
      }
      let reads = 0
      const ipc = {
        capabilities,
        bootstrap: {
          discovery: { kind: 'continuous-scan' },
          attachment: {
            adapter: { adapterId: 'adapter-1' },
            backendGeneration: 'backend-1'
          }
        },
        destroy: async () => ({ state: 'released', failures: [] }),
        adapterState: async () => {
          reads += 1
          return {
            availability: 'available',
            authorization: 'granted',
            power: 'on',
            backendGeneration: 'backend-1',
            updatedAt: reads,
            safeReason: null
          }
        }
      }
      const manager = new IpcPublicManagerAdapter(ipc, { capabilities })
      const controller = new AbortController()
      const removeListener = jest.spyOn(controller.signal, 'removeEventListener')
      const watch = await manager.adapter.watchState({ signal: controller.signal })
      const readsAfterInitial = reads
      await jest.advanceTimersByTimeAsync(500)
      expect(reads).toBe(readsAfterInitial + 1)

      await expect(watch.values.close()).resolves.toMatchObject({ state: 'released', failures: [] })
      expect(removeListener).toHaveBeenCalledWith('abort', expect.any(Function))
      const readsAfterClose = reads
      await jest.advanceTimersByTimeAsync(5_000)
      expect(reads).toBe(readsAfterClose)
      expect(jest.getTimerCount()).toBe(0)

      await expect(watch.stop()).resolves.toMatchObject({ state: 'released', failures: [] })
      expect(reads).toBe(readsAfterClose)
      const item = await watch.values[Symbol.asyncIterator]().next()
      expect(item.value).toMatchObject({ kind: 'terminal' })
    } finally {
      jest.useRealTimers()
    }
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
