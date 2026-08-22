// __tests__/core/operation-coordinator.test.js

const { deadline, opaqueId } = require('../../src/backend-contract/primitives')
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

function createCoordinator(maximumQueuedOperationsPerConnection) {
  const ledger = new ResourceLedger()
  const trace = new CoreTraceRecorder(32, 4096)
  let nextCorrelation = 1
  const coordinator = new CoreOperationCoordinator({
    now: () => 10,
    createCorrelation: () => {
      const id = opaqueId(`operation-${nextCorrelation}`, 'core-operation', `attachment:operation-${nextCorrelation}`)
      nextCorrelation += 1
      return id
    },
    resourceLedger: ledger,
    trace,
    maximumQueuedOperationsPerConnection
  })
  return { coordinator, ledger, trace }
}

function operation(
  dispatch,
  signal = null,
  mayCommit = false,
  retainedPayloadBytes = 0,
  queueKey = 'connection-1',
  fairnessKey
) {
  return {
    queueKey,
    fairnessKey,
    options: { signal, deadline: null },
    mayCommit,
    retainedPayloadBytes,
    dispatch: correlation => ({
      completion: dispatch(correlation),
      requestCancellation: async () => {}
    })
  }
}

describe('CoreOperationCoordinator', () => {
  test('rejects queued overflow with one quota result without dispatching or retaining its payload', async () => {
    const { coordinator, ledger } = createCoordinator(1)
    const first = deferred()
    const started = []
    const firstResult = coordinator.run(
      operation(async () => {
        started.push('first')
        return first.promise
      })
    )
    const queuedResult = coordinator.run(
      operation(async () => {
        started.push('queued')
        return 'queued'
      }, null, false, 5)
    )
    let overflowSettlements = 0
    let overflowSettlement = null
    const overflowResult = coordinator.run(
      operation(async () => {
        started.push('overflow')
        return 'must-not-dispatch'
      }, null, false, 11)
    )
    void overflowResult.then(result => {
      overflowSettlements += 1
      overflowSettlement = result
    })

    await Promise.resolve()
    expect(overflowSettlements).toBe(1)
    expect(overflowSettlement).toMatchObject({
      outcome: 'failed',
      value: null,
      error: {
        code: 'stream.quota',
        domain: 'core',
        operation: 'operation-coordinator.queue-capacity'
      }
    })
    expect(started).toEqual(['first'])
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(5)
    expect(coordinator.activeCounts()).toEqual({ queued: 1, dispatched: 1, quarantined: 0 })

    first.resolve('first-value')
    await expect(firstResult).resolves.toMatchObject({ outcome: 'succeeded' })
    await expect(queuedResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'queued' })
    expect(overflowSettlements).toBe(1)
    expect(started).toEqual(['first', 'queued'])
    expect(ledger.isZero()).toBe(true)
  })

  test('preserves pre-abort and deadline admission precedence when the connection queue is full', async () => {
    const { coordinator, ledger } = createCoordinator(1)
    const first = deferred()
    const firstResult = coordinator.run(operation(() => first.promise))
    const queuedResult = coordinator.run(operation(async () => 'queued', null, false, 3))
    const abortController = new AbortController()
    abortController.abort()

    await expect(
      coordinator.run(operation(async () => 'not-dispatched', abortController.signal, false, 7))
    ).resolves.toMatchObject({ outcome: 'aborted' })
    await expect(
      coordinator.run({
        ...operation(async () => 'not-dispatched', null, false, 7),
        options: { signal: null, deadline: deadline(10) }
      })
    ).resolves.toMatchObject({ outcome: 'timed-out' })
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(3)
    expect(coordinator.activeCounts()).toEqual({ queued: 1, dispatched: 1, quarantined: 0 })

    first.resolve('first-value')
    await expect(firstResult).resolves.toMatchObject({ outcome: 'succeeded' })
    await expect(queuedResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'queued' })
    expect(ledger.isZero()).toBe(true)
  })

  test('keeps each connection FIFO while admitting different connections concurrently', async () => {
    const { coordinator, ledger } = createCoordinator(1)
    const firstConnection = deferred()
    const secondConnection = deferred()
    const started = []
    const firstResult = coordinator.run(
      operation(async () => {
        started.push('connection-1:first')
        return firstConnection.promise
      })
    )
    const queuedResult = coordinator.run(
      operation(async () => {
        started.push('connection-1:queued')
        return 'queued-value'
      })
    )
    const otherResult = coordinator.run(
      operation(async () => {
        started.push('connection-2:first')
        return secondConnection.promise
      }, null, false, 0, 'connection-2')
    )

    expect(started).toEqual(['connection-1:first', 'connection-2:first'])
    firstConnection.resolve('first-value')
    await expect(firstResult).resolves.toMatchObject({ outcome: 'succeeded' })
    await expect(queuedResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'queued-value' })
    expect(started).toEqual(['connection-1:first', 'connection-2:first', 'connection-1:queued'])

    secondConnection.resolve('other-value')
    await expect(otherResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'other-value' })
    expect(ledger.isZero()).toBe(true)
  })

  test('round-robins non-empty fairness classes while preserving FIFO within each class', async () => {
    const { coordinator, ledger } = createCoordinator()
    const firstRead = deferred()
    const firstWrite = deferred()
    const secondRead = deferred()
    const secondWrite = deferred()
    const thirdRead = deferred()
    const started = []

    const firstReadResult = coordinator.run(
      operation(
        () => {
          started.push('read:first')
          return firstRead.promise
        },
        null,
        false,
        0,
        'connection-1',
        'read'
      )
    )
    const secondReadResult = coordinator.run(
      operation(
        () => {
          started.push('read:second')
          return secondRead.promise
        },
        null,
        false,
        0,
        'connection-1',
        'read'
      )
    )
    const thirdReadResult = coordinator.run(
      operation(
        () => {
          started.push('read:third')
          return thirdRead.promise
        },
        null,
        false,
        0,
        'connection-1',
        'read'
      )
    )
    const firstWriteResult = coordinator.run(
      operation(
        () => {
          started.push('write:first')
          return firstWrite.promise
        },
        null,
        false,
        0,
        'connection-1',
        'write'
      )
    )
    const secondWriteResult = coordinator.run(
      operation(
        () => {
          started.push('write:second')
          return secondWrite.promise
        },
        null,
        false,
        0,
        'connection-1',
        'write'
      )
    )

    expect(started).toEqual(['read:first'])
    firstRead.resolve('first-read')
    await expect(firstReadResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'first-read' })
    expect(started).toEqual(['read:first', 'write:first'])

    firstWrite.resolve('first-write')
    await expect(firstWriteResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'first-write' })
    expect(started).toEqual(['read:first', 'write:first', 'read:second'])

    secondRead.resolve('second-read')
    await expect(secondReadResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'second-read' })
    expect(started).toEqual(['read:first', 'write:first', 'read:second', 'write:second'])

    secondWrite.resolve('second-write')
    await expect(secondWriteResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'second-write' })
    expect(started).toEqual(['read:first', 'write:first', 'read:second', 'write:second', 'read:third'])

    thirdRead.resolve('third-read')
    await expect(thirdReadResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'third-read' })
    expect(coordinator.activeCounts()).toEqual({ queued: 0, dispatched: 0, quarantined: 0 })
    expect(ledger.isZero()).toBe(true)
  })

  test('removing a queued fairness class does not corrupt the next round-robin selection', async () => {
    const { coordinator, ledger } = createCoordinator()
    const firstRead = deferred()
    const queuedRead = deferred()
    const control = deferred()
    const cancelledWrite = new AbortController()
    const started = []

    const firstReadResult = coordinator.run(
      operation(
        () => {
          started.push('read:first')
          return firstRead.promise
        },
        null,
        false,
        0,
        'connection-1',
        'read'
      )
    )
    const queuedReadResult = coordinator.run(
      operation(
        () => {
          started.push('read:queued')
          return queuedRead.promise
        },
        null,
        false,
        0,
        'connection-1',
        'read'
      )
    )
    const cancelledWriteResult = coordinator.run(
      operation(
        () => {
          started.push('write:cancelled')
          return 'must-not-dispatch'
        },
        cancelledWrite.signal,
        false,
        0,
        'connection-1',
        'write'
      )
    )
    const controlResult = coordinator.run(
      operation(
        () => {
          started.push('control:first')
          return control.promise
        },
        null,
        false,
        0,
        'connection-1',
        'control'
      )
    )

    cancelledWrite.abort()
    await expect(cancelledWriteResult).resolves.toMatchObject({ outcome: 'aborted' })
    firstRead.resolve('first-read')
    await expect(firstReadResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'first-read' })
    expect(started).toEqual(['read:first', 'control:first'])

    control.resolve('control-value')
    await expect(controlResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'control-value' })
    expect(started).toEqual(['read:first', 'control:first', 'read:queued'])

    queuedRead.resolve('queued-read')
    await expect(queuedReadResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'queued-read' })
    expect(coordinator.activeCounts()).toEqual({ queued: 0, dispatched: 0, quarantined: 0 })
    expect(ledger.isZero()).toBe(true)
  })

  test('keeps a cancelled dispatched operation at the FIFO head until its late acknowledgement arrives', async () => {
    const { coordinator, ledger, trace } = createCoordinator()
    const first = deferred()
    const second = deferred()
    const started = []
    const abortController = new AbortController()
    const firstResult = coordinator.run(
      operation(
        async () => {
          started.push('first')
          return first.promise
        },
        abortController.signal,
        true
      )
    )
    const secondResult = coordinator.run(
      operation(async () => {
        started.push('second')
        return second.promise
      })
    )

    expect(started).toEqual(['first'])
    abortController.abort()
    await expect(firstResult).resolves.toMatchObject({ outcome: 'aborted', commitState: 'unknown' })
    expect(started).toEqual(['first'])
    expect(coordinator.activeCounts()).toEqual({ queued: 1, dispatched: 0, quarantined: 1 })

    first.resolve('late-first')
    await new Promise(resolve => setImmediate(resolve))
    expect(started).toEqual(['first', 'second'])
    second.resolve('second-value')
    await expect(secondResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'second-value' })
    expect(coordinator.activeCounts()).toEqual({ queued: 0, dispatched: 0, quarantined: 0 })
    expect(ledger.isZero()).toBe(true)
    expect(trace.snapshot().some(record => record.transition === 'late-success')).toBe(true)
  })

  test('rejects pre-aborted and expired operations before allocating queue state', async () => {
    const { coordinator, ledger } = createCoordinator()
    const abortController = new AbortController()
    abortController.abort()

    await expect(
      coordinator.run(operation(async () => 'not-dispatched', abortController.signal))
    ).resolves.toMatchObject({
      outcome: 'aborted'
    })
    await expect(
      coordinator.run({
        ...operation(async () => 'not-dispatched'),
        options: { signal: null, deadline: deadline(10) }
      })
    ).resolves.toMatchObject({ outcome: 'timed-out' })
    expect(ledger.isZero()).toBe(true)
  })

  test('normalizes a rejected backend dispatch, releases accounting, and leaves no unhandled work', async () => {
    const { coordinator, ledger } = createCoordinator()

    await expect(
      coordinator.run(
        operation(async () => {
          throw new Error('backend failure')
        })
      )
    ).resolves.toMatchObject({ outcome: 'failed', error: { code: 'platform.failure' } })
    expect(ledger.isZero()).toBe(true)
  })

  test('accounts exact payload bytes through queued, dispatched, quarantined, terminal, and destroyed states', async () => {
    const firstFixture = createCoordinator()
    const first = deferred()
    const secondAbort = new AbortController()
    firstFixture.ledger.setRetainedStreamBytes(2)
    const firstResult = firstFixture.coordinator.run(operation(() => first.promise, null, true, 3))
    const secondResult = firstFixture.coordinator.run(
      operation(async () => 'never-dispatched', secondAbort.signal, false, 5)
    )

    expect(Number(firstFixture.ledger.current('retainedByteBuffers'))).toBe(10)
    secondAbort.abort()
    await expect(secondResult).resolves.toMatchObject({ outcome: 'aborted' })
    expect(Number(firstFixture.ledger.current('retainedByteBuffers'))).toBe(5)

    firstFixture.coordinator.cancelQueue('connection-1', 'aborted')
    await expect(firstResult).resolves.toMatchObject({ outcome: 'aborted', commitState: 'unknown' })
    expect(Number(firstFixture.ledger.current('retainedByteBuffers'))).toBe(5)
    first.resolve('late-success')
    await new Promise(resolve => setImmediate(resolve))
    expect(Number(firstFixture.ledger.current('retainedByteBuffers'))).toBe(2)

    const lateFailureFixture = createCoordinator()
    const lateFailure = deferred()
    const lateFailureAbort = new AbortController()
    const lateFailureResult = lateFailureFixture.coordinator.run(
      operation(() => lateFailure.promise, lateFailureAbort.signal, true, 6)
    )
    lateFailureAbort.abort()
    await expect(lateFailureResult).resolves.toMatchObject({ outcome: 'aborted', commitState: 'unknown' })
    expect(Number(lateFailureFixture.ledger.current('retainedByteBuffers'))).toBe(6)
    lateFailure.reject(new Error('late failure'))
    await new Promise(resolve => setImmediate(resolve))
    expect(Number(lateFailureFixture.ledger.current('retainedByteBuffers'))).toBe(0)

    const normalFixture = createCoordinator()
    const normal = deferred()
    const normalResult = normalFixture.coordinator.run(operation(() => normal.promise, null, false, 7))
    expect(Number(normalFixture.ledger.current('retainedByteBuffers'))).toBe(7)
    normal.resolve('complete')
    await expect(normalResult).resolves.toMatchObject({ outcome: 'succeeded' })
    expect(Number(normalFixture.ledger.current('retainedByteBuffers'))).toBe(0)

    await expect(
      normalFixture.coordinator.run(
        operation(
          () => {
            throw new Error('synchronous dispatch failure')
          },
          null,
          false,
          11
        )
      )
    ).resolves.toMatchObject({ outcome: 'failed' })
    expect(Number(normalFixture.ledger.current('retainedByteBuffers'))).toBe(0)

    const rejected = deferred()
    const rejectedResult = normalFixture.coordinator.run(operation(() => rejected.promise, null, false, 13))
    expect(Number(normalFixture.ledger.current('retainedByteBuffers'))).toBe(13)
    rejected.reject(new Error('asynchronous dispatch failure'))
    await expect(rejectedResult).resolves.toMatchObject({ outcome: 'failed' })
    expect(Number(normalFixture.ledger.current('retainedByteBuffers'))).toBe(0)
  })

  test.each([
    ['late success', deferred => deferred.resolve('late-after-destroy')],
    ['late failure', deferred => deferred.reject(new Error('late-after-destroy'))]
  ])('retains dispatched payload ownership through destroy until %s acknowledgement', async (_label, settleLate) => {
    const { coordinator, ledger } = createCoordinator()
    const backend = deferred()
    const destroyedResult = coordinator.run(operation(() => backend.promise, null, true, 17))
    const queuedResult = coordinator.run(operation(async () => 'must-not-dispatch', null, false, 5))
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(22)

    coordinator.destroy()
    const drain = coordinator.waitForQuarantineDrain()
    let drained = false
    void drain.then(() => {
      drained = true
    })
    await expect(destroyedResult).resolves.toMatchObject({ outcome: 'destroyed' })
    await expect(queuedResult).resolves.toMatchObject({ outcome: 'destroyed' })
    await Promise.resolve()
    expect(drained).toBe(false)
    expect(coordinator.activeCounts()).toEqual({ queued: 0, dispatched: 0, quarantined: 1 })
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(17)

    settleLate(backend)
    await drain
    expect(drained).toBe(true)
    expect(coordinator.activeCounts()).toEqual({ queued: 0, dispatched: 0, quarantined: 0 })
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(0)
    await Promise.resolve()
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(0)
  })
})
