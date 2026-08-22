const { capacity, byteLimit, deadline } = require('../../src/backend-contract/primitives')
const { contractError } = require('../../src/backend-contract/errors')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { writeCoreCharacteristicWhenReady } = require('../../src/core/core-characteristic-operations')
const { CoreOperationCoordinator } = require('../../src/core/operation-coordinator')
const { ResourceLedger } = require('../../src/core/resource-ledger')
const { CoreTraceRecorder } = require('../../src/core/trace-recorder')

function deferred() {
  let resolve
  let reject
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 8; turn += 1) await Promise.resolve()
}

function createFixture({
  readiness = true,
  readinessCapability = true,
  readinessCapabilityState = 'limited',
  writeCompletion,
  readinessOpenPromise,
  readinessCloseResult,
  rejectReadinessOpenOnAbort = false
} = {}) {
  const ledger = new ResourceLedger()
  let nextCorrelation = 1
  const coordinator = new CoreOperationCoordinator({
    now: () => 10,
    createCorrelation: () => `operation-${nextCorrelation++}`,
    resourceLedger: ledger,
    trace: new CoreTraceRecorder(64, 8192)
  })
  const path = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    databaseGeneration: 'database-1',
    validity: 'current'
  }
  const events = new CoreBoundedStream(
    { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const close = jest.fn(async () => {
    events.closeWithReason('owner-released')
    return readinessCloseResult ?? { state: 'released', failures: [] }
  })
  const write = jest.fn((_path, request) => ({
    completion:
      writeCompletion?.promise ??
      Promise.resolve({
        terminal: { correlation: request.operation.correlation, outcome: 'succeeded', cause: null },
        commitState: 'confirmed'
      }),
    requestCancellation: jest.fn(async () => ({ state: 'cancellation-requested' }))
  }))
  const readinessOpen = readiness
    ? jest.fn(options => {
        if (readinessOpenPromise !== undefined && rejectReadinessOpenOnAbort && options?.signal !== undefined) {
          options.signal.addEventListener(
            'abort',
            () => readinessOpenPromise.reject(new Error('readiness open aborted')),
            { once: true }
          )
        }
        return readinessOpenPromise?.promise ?? Promise.resolve({ events, close })
      })
    : { writeWithoutResponseReadiness: undefined }
  const connection = readiness ? { writeWithoutResponseReadiness: readinessOpen } : readinessOpen
  const database = {
    connection,
    monotonicNow: () => 10,
    assertPath: jest.fn(candidate => {
      if (candidate !== path) throw new Error('stale path')
    })
  }
  return {
    backend: {
      gatt: { write },
      connections: readiness ? { writeWithoutResponseReadiness: () => undefined } : {},
      features: readinessCapability
        ? {
            registrations: [{ id: 'gatt:write-without-response-readiness', state: readinessCapabilityState }],
            descriptors: []
          }
        : { registrations: [], descriptors: [] }
    },
    coordinator,
    database,
    path,
    events,
    close,
    readinessOpen,
    write,
    ledger
  }
}

function options(signal = null, operationDeadline = null) {
  return { signal, deadline: operationDeadline, mode: 'without-response' }
}

function readinessEvent(ready, ordinal) {
  return {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    ready,
    observedAtMonotonicMs: 10 + ordinal,
    ordinal
  }
}

describe('writeCoreCharacteristicWhenReady', () => {
  test('copies caller bytes, waits for authoritative readiness, and submits once without response', async () => {
    const fixture = createFixture()
    const callerBytes = new Uint8Array([1, 2, 3])
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      callerBytes,
      options()
    )
    callerBytes[0] = 99

    await flushMicrotasks()
    expect(fixture.write).not.toHaveBeenCalled()
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(3)
    fixture.events.emit(readinessEvent(false, 1), 128)
    await flushMicrotasks()
    expect(fixture.write).not.toHaveBeenCalled()

    fixture.events.emit(readinessEvent(true, 2), 128)
    await expect(result).resolves.toMatchObject({ commitState: 'confirmed' })
    expect(fixture.write).toHaveBeenCalledTimes(1)
    expect(fixture.write.mock.calls[0][1]).toMatchObject({ mode: 'without-response' })
    expect([...fixture.write.mock.calls[0][1].bytes]).toEqual([1, 2, 3])
    expect(fixture.close).toHaveBeenCalledTimes(1)
    expect(fixture.ledger.isZero()).toBe(true)
  })

  test('aborts while waiting without native submission and releases copied bytes', async () => {
    const fixture = createFixture()
    const abortController = new AbortController()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([4, 5, 6, 7]),
      options(abortController.signal)
    )

    await flushMicrotasks()
    abortController.abort()
    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(fixture.write).not.toHaveBeenCalled()
    expect(fixture.close).toHaveBeenCalledTimes(1)
    await fixture.coordinator.waitForQuarantineDrain()
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test('does not let a never-settling readiness open block cancellation or teardown', async () => {
    const readinessOpenPromise = deferred()
    const fixture = createFixture({ readinessOpenPromise, rejectReadinessOpenOnAbort: true })
    const abortController = new AbortController()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([4, 5]),
      options(abortController.signal)
    )

    await flushMicrotasks()
    abortController.abort()
    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })

    let drained = false
    void fixture.coordinator.waitForQuarantineDrain().then(() => {
      drained = true
    })
    await flushMicrotasks()
    expect(drained).toBe(true)

    expect(fixture.close).not.toHaveBeenCalled()
    expect(fixture.ledger.isZero()).toBe(true)
  })

  test('bounds a never-settling readiness open by the operation deadline without blocking teardown', async () => {
    const readinessOpenPromise = deferred()
    const fixture = createFixture({ readinessOpenPromise })
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([6]),
      options(null, deadline(11))
    )

    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    await expect(fixture.coordinator.waitForQuarantineDrain()).resolves.toBeUndefined()
    expect(fixture.write).not.toHaveBeenCalled()
    readinessOpenPromise.resolve({ events: fixture.events, close: fixture.close })
    await flushMicrotasks()
    expect(fixture.close).toHaveBeenCalledTimes(1)
    expect(fixture.ledger.isZero()).toBe(true)
  })

  test('waits for a late readiness source, closes it, and retains its failure for the connection owner', async () => {
    const readinessOpenPromise = deferred()
    const cleanup = {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'gatt.write-readiness',
          error: {
            code: 'platform.failure',
            domain: 'cleanup',
            operation: 'test.late-readiness-close',
            platform: null,
            retryability: 'never'
          }
        }
      ]
    }
    const fixture = createFixture({ readinessOpenPromise, readinessCloseResult: cleanup })
    const abortController = new AbortController()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([4, 5]),
      options(abortController.signal)
    )

    await flushMicrotasks()
    abortController.abort()
    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })

    let drained = false
    const drain = fixture.coordinator.waitForQuarantineDrain('connection-1').then(() => {
      drained = true
    })
    await flushMicrotasks()
    expect(drained).toBe(false)

    readinessOpenPromise.resolve({ events: fixture.events, close: fixture.close })
    await drain
    await flushMicrotasks()
    expect(fixture.close).toHaveBeenCalledTimes(1)
    expect(fixture.coordinator.takeCleanupFailures('connection-1')).toEqual(cleanup.failures)
    expect(fixture.ledger.isZero()).toBe(true)
  })

  test('does not replay after aborting a submitted write and retains uncertainty until late acknowledgement', async () => {
    const writeCompletion = deferred()
    const fixture = createFixture({ writeCompletion })
    const abortController = new AbortController()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([8, 9]),
      options(abortController.signal)
    )
    fixture.events.emit(readinessEvent(true, 1), 128)
    await flushMicrotasks()
    expect(fixture.write).toHaveBeenCalledTimes(1)

    abortController.abort()
    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(2)
    writeCompletion.resolve({
      terminal: {
        correlation: fixture.write.mock.calls[0][1].operation.correlation,
        outcome: 'succeeded',
        cause: null
      },
      commitState: 'confirmed'
    })
    await fixture.coordinator.waitForQuarantineDrain()
    expect(fixture.write).toHaveBeenCalledTimes(1)
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test('fails closed before copying or dispatching when no readiness backend exists', async () => {
    const fixture = createFixture({ readiness: false })

    await expect(
      writeCoreCharacteristicWhenReady(
        fixture.backend,
        fixture.coordinator,
        byteLimit(512),
        fixture.database,
        fixture.path,
        new Uint8Array([1, 2]),
        options()
      )
    ).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
    expect(fixture.write).not.toHaveBeenCalled()
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test('preserves capability unavailable when readiness is registered but unavailable', async () => {
    const fixture = createFixture({ readinessCapabilityState: 'unavailable' })

    await expect(
      writeCoreCharacteristicWhenReady(
        fixture.backend,
        fixture.coordinator,
        byteLimit(512),
        fixture.database,
        fixture.path,
        new Uint8Array([1]),
        options()
      )
    ).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
    expect(fixture.readinessOpen).not.toHaveBeenCalled()
    expect(fixture.write).not.toHaveBeenCalled()
  })

  test('preserves capability unavailable when the unavailable registration has no backend seam', async () => {
    const fixture = createFixture({ readiness: false, readinessCapabilityState: 'unavailable' })

    await expect(
      writeCoreCharacteristicWhenReady(
        fixture.backend,
        fixture.coordinator,
        byteLimit(512),
        fixture.database,
        fixture.path,
        new Uint8Array([1]),
        options()
      )
    ).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  })

  test('fails closed when a readiness seam exists without an active capability registration', async () => {
    const fixture = createFixture({ readinessCapability: false })
    fixture.readinessOpen.mockImplementation(async () => {
      throw new Error('readiness seam must not be opened')
    })

    await expect(
      writeCoreCharacteristicWhenReady(
        fixture.backend,
        fixture.coordinator,
        byteLimit(512),
        fixture.database,
        fixture.path,
        new Uint8Array([1, 2]),
        options()
      )
    ).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
    expect(fixture.readinessOpen).not.toHaveBeenCalled()
    expect(fixture.write).not.toHaveBeenCalled()
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test('fails closed on readiness stream termination without native submission', async () => {
    const fixture = createFixture()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([3]),
      options()
    )
    await flushMicrotasks()
    fixture.events.closeWithReason('connection-lost')

    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
    expect(fixture.write).not.toHaveBeenCalled()
    expect(fixture.close).toHaveBeenCalledTimes(1)
    await fixture.coordinator.waitForQuarantineDrain()
    expect(Number(fixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test('rejects an externally terminated readiness stream before native dispatch', async () => {
    const fixture = createFixture()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([7]),
      options()
    )

    await flushMicrotasks()
    fixture.events.emit(readinessEvent(true, 1), 128)
    fixture.events.closeWithReason('service-changed')

    await expect(result).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
    expect(fixture.write).not.toHaveBeenCalled()
    expect(fixture.coordinator.activeCounts().dispatched).toBe(0)
  })

  test('rechecks the generation-bound database path in admission before native dispatch', async () => {
    const fixture = createFixture()
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([8]),
      options()
    )

    await flushMicrotasks()
    fixture.database.assertPath.mockImplementation(() => {
      throw contractError('gatt.stale-handle', 'gatt', 'test.stale-path')
    })
    fixture.events.emit(readinessEvent(true, 1), 128)

    await expect(result).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
    expect(fixture.write).not.toHaveBeenCalled()
  })

  test('retains readiness watch cleanup failures for the coordinator owner', async () => {
    const cleanup = {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'gatt.write-readiness',
          error: {
            code: 'platform.failure',
            domain: 'cleanup',
            operation: 'test.readiness.close',
            platform: null,
            retryability: 'never'
          }
        }
      ]
    }
    const fixture = createFixture({ readinessCloseResult: cleanup })
    const result = writeCoreCharacteristicWhenReady(
      fixture.backend,
      fixture.coordinator,
      byteLimit(512),
      fixture.database,
      fixture.path,
      new Uint8Array([9]),
      options()
    )

    fixture.events.emit(readinessEvent(true, 1), 128)
    await expect(result).resolves.toMatchObject({ commitState: 'confirmed' })
    expect(fixture.coordinator.takeCleanupFailures()).toEqual(cleanup.failures)
    expect(fixture.coordinator.takeCleanupFailures()).toEqual([])
  })
})
