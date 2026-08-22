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

function admissionGate({ deferClose = false, cleanup = { state: 'released', failures: [] } } = {}) {
  let ready = false
  let closed = false
  let closeCalls = 0
  const waiters = []
  const closeDeferred = deferred()
  const handle = {
    waitUntilReady: () => {
      if (ready) return Promise.resolve()
      return new Promise((resolve, reject) => waiters.push({ resolve, reject }))
    },
    isReady: () => ready && !closed,
    close: () => {
      closeCalls += 1
      closed = true
      ready = false
      for (const waiter of waiters.splice(0)) waiter.reject(new Error('admission closed'))
      return deferClose ? closeDeferred.promise : Promise.resolve(cleanup)
    }
  }
  return {
    handle,
    signalReady() {
      ready = true
      for (const waiter of waiters.splice(0)) waiter.resolve()
    },
    signalNotReady() {
      ready = false
    },
    resolveClose(result = cleanup) {
      closeDeferred.resolve(result)
    },
    closeCalls: () => closeCalls
  }
}

describe('CoreOperationCoordinator', () => {
  test('does not add an admission cleanup turn to ordinary dispatch completion', async () => {
    const { coordinator } = createCoordinator()
    let settled = false
    const result = coordinator.run(operation(async () => 'ordinary-value'))
    void result.then(() => {
      settled = true
    })

    await Promise.resolve()
    await Promise.resolve()
    expect(settled).toBe(true)
    await expect(result).resolves.toMatchObject({ outcome: 'succeeded', value: 'ordinary-value' })
  })

  test('honors admission for an operation without a connection queue', async () => {
    const { coordinator, ledger } = createCoordinator()
    const gate = admissionGate()
    const dispatch = jest.fn(async () => 'admitted-value')
    const result = coordinator.run({
      ...operation(dispatch, null, true, 4, null),
      admission: () => gate.handle
    })

    await Promise.resolve()
    expect(dispatch).not.toHaveBeenCalled()
    expect(gate.closeCalls()).toBe(0)
    gate.signalReady()
    await expect(result).resolves.toMatchObject({ outcome: 'succeeded', value: 'admitted-value' })
    expect(dispatch).toHaveBeenCalledTimes(1)
    expect(gate.closeCalls()).toBe(1)
    expect(ledger.isZero()).toBe(true)
  })

  test('waits at the FIFO head and rechecks admission synchronously before native dispatch', async () => {
    const { coordinator, ledger } = createCoordinator()
    const first = deferred()
    const gate = admissionGate()
    const started = []
    let admissionStarts = 0
    const firstResult = coordinator.run(
      operation(() => {
        started.push('first')
        return first.promise
      })
    )
    const admittedResult = coordinator.run({
      ...operation(async () => {
        started.push('admitted')
        return 'admitted-value'
      }, null, true, 5),
      admission: () => {
        admissionStarts += 1
        return gate.handle
      }
    })

    expect(started).toEqual(['first'])
    expect(admissionStarts).toBe(0)
    first.resolve('first-value')
    await expect(firstResult).resolves.toMatchObject({ outcome: 'succeeded' })
    await Promise.resolve()
    expect(admissionStarts).toBe(1)
    expect(started).toEqual(['first'])
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(5)

    gate.signalReady()
    gate.signalNotReady()
    await Promise.resolve()
    await Promise.resolve()
    expect(started).toEqual(['first'])

    gate.signalReady()
    await expect(admittedResult).resolves.toMatchObject({ outcome: 'succeeded', value: 'admitted-value' })
    expect(started).toEqual(['first', 'admitted'])
    expect(gate.closeCalls()).toBe(1)
    expect(ledger.isZero()).toBe(true)
  })

  test('aborts an admitting operation without dispatch or commit uncertainty and drains cleanup once', async () => {
    const { coordinator, ledger } = createCoordinator()
    const gate = admissionGate({ deferClose: true })
    const abortController = new AbortController()
    const dispatch = jest.fn(async () => 'must-not-dispatch')
    const result = coordinator.run({
      ...operation(dispatch, abortController.signal, true, 7),
      admission: () => gate.handle
    })

    await Promise.resolve()
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(7)
    abortController.abort()
    await expect(result).resolves.toMatchObject({ outcome: 'aborted', commitState: 'not-applicable' })
    expect(dispatch).not.toHaveBeenCalled()
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(0)
    expect(gate.closeCalls()).toBe(1)

    let drained = false
    void coordinator.waitForQuarantineDrain().then(() => {
      drained = true
    })
    await Promise.resolve()
    expect(drained).toBe(false)
    gate.resolveClose()
    await coordinator.waitForQuarantineDrain()
    expect(drained).toBe(true)
    expect(gate.closeCalls()).toBe(1)
    expect(ledger.isZero()).toBe(true)
  })

  test('times out an admitting operation before native submission and releases its payload', async () => {
    const { coordinator, ledger } = createCoordinator()
    const gate = admissionGate()
    const dispatch = jest.fn(async () => 'must-not-dispatch')
    const result = coordinator.run({
      ...operation(dispatch, null, true, 9),
      options: { signal: null, deadline: deadline(11) },
      admission: () => gate.handle
    })

    await expect(result).resolves.toMatchObject({ outcome: 'timed-out', commitState: 'not-applicable' })
    expect(dispatch).not.toHaveBeenCalled()
    expect(gate.closeCalls()).toBe(1)
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(0)
    expect(ledger.isZero()).toBe(true)
  })

  test('counts an admitting operation against per-connection queue capacity', async () => {
    const { coordinator, ledger } = createCoordinator(1)
    const gate = admissionGate()
    const admitting = coordinator.run({
      ...operation(async () => 'admitting-value', null, true, 4),
      admission: () => gate.handle
    })
    await Promise.resolve()

    const overflow = coordinator.run(operation(async () => 'must-not-dispatch', null, false, 5))
    await expect(overflow).resolves.toMatchObject({
      outcome: 'failed',
      error: { code: 'stream.quota', operation: 'operation-coordinator.queue-capacity' }
    })
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(4)

    gate.signalReady()
    await expect(admitting).resolves.toMatchObject({ outcome: 'succeeded', value: 'admitting-value' })
    expect(ledger.isZero()).toBe(true)
  })

  test('keeps the FIFO head until admission cleanup finishes after synchronous dispatch failure', async () => {
    const { coordinator, ledger } = createCoordinator()
    const gate = admissionGate({ deferClose: true })
    gate.signalReady()
    const started = []
    const failed = coordinator.run({
      ...operation(() => {
        started.push('failed')
        throw new Error('synchronous dispatch failure')
      }, null, true, 3),
      admission: () => gate.handle
    })
    const following = coordinator.run(
      operation(async () => {
        started.push('following')
        return 'following-value'
      })
    )

    await Promise.resolve()
    await Promise.resolve()
    expect(started).toEqual(['failed'])
    expect(gate.closeCalls()).toBe(1)
    gate.resolveClose()
    await expect(failed).resolves.toMatchObject({ outcome: 'failed', commitState: 'unknown' })
    await expect(following).resolves.toMatchObject({ outcome: 'succeeded', value: 'following-value' })
    expect(started).toEqual(['failed', 'following'])
    expect(ledger.isZero()).toBe(true)
  })

  test.each([
    ['disconnect', coordinator => coordinator.cancelQueue('connection-1', 'disconnected'), 'disconnected'],
    ['destroy', coordinator => coordinator.destroy(), 'destroyed']
  ])('settles an admitting operation exactly once on %s without native dispatch', async (_label, terminate, outcome) => {
    const { coordinator, ledger } = createCoordinator()
    const gate = admissionGate()
    const dispatch = jest.fn(async () => 'must-not-dispatch')
    const result = coordinator.run({
      ...operation(dispatch, null, true, 4),
      admission: () => gate.handle
    })
    let settlements = 0
    void result.then(() => {
      settlements += 1
    })

    await Promise.resolve()
    terminate(coordinator)
    await expect(result).resolves.toMatchObject({ outcome, commitState: 'not-applicable' })
    await Promise.resolve()
    expect(settlements).toBe(1)
    expect(dispatch).not.toHaveBeenCalled()
    expect(gate.closeCalls()).toBe(1)
    expect(Number(ledger.current('retainedByteBuffers'))).toBe(0)
    expect(ledger.isZero()).toBe(true)
  })

  test('rejects queued overflow with one quota result without dispatching or retaining its payload', async () => {
    const { coordinator, ledger, trace } = createCoordinator(1)
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
    expect(trace.snapshot()).toEqual(
      expect.arrayContaining([expect.objectContaining({ cause: 'stream.quota', transition: 'queue-rejected' })])
    )

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

  test('scopes admission drain waits and retained cleanup failures by connection queue', async () => {
    const { coordinator } = createCoordinator()
    const connectionAFailure = {
      resourceKind: 'connection-a-readiness',
      error: {
        code: 'platform.failure',
        domain: 'cleanup',
        operation: 'test.connection-a',
        platform: null,
        retryability: 'never'
      }
    }
    const connectionBFailure = {
      resourceKind: 'connection-b-readiness',
      error: {
        code: 'platform.failure',
        domain: 'cleanup',
        operation: 'test.connection-b',
        platform: null,
        retryability: 'never'
      }
    }
    const connectionA = admissionGate({ deferClose: true })
    const connectionB = admissionGate({ deferClose: true })
    const abortA = new AbortController()
    const abortB = new AbortController()
    const resultA = coordinator.run({
      ...operation(async () => 'must-not-dispatch', abortA.signal, false, 0, 'connection-a'),
      admission: () => connectionA.handle
    })
    const resultB = coordinator.run({
      ...operation(async () => 'must-not-dispatch', abortB.signal, false, 0, 'connection-b'),
      admission: () => connectionB.handle
    })

    await Promise.resolve()
    abortA.abort()
    abortB.abort()
    await expect(resultA).resolves.toMatchObject({ outcome: 'aborted' })
    await expect(resultB).resolves.toMatchObject({ outcome: 'aborted' })

    let connectionADrained = false
    const connectionADrain = coordinator.waitForQuarantineDrain('connection-a').then(() => {
      connectionADrained = true
    })
    expect(coordinator.hasPendingDrain('connection-a')).toBe(true)
    expect(coordinator.hasPendingDrain('connection-b')).toBe(true)

    connectionB.resolveClose({ state: 'release-failed', failures: [connectionBFailure] })
    await coordinator.waitForQuarantineDrain('connection-b')
    expect(connectionADrained).toBe(false)
    expect(coordinator.takeCleanupFailures('connection-b')).toEqual([connectionBFailure])
    expect(coordinator.takeCleanupFailures('connection-a')).toEqual([])

    connectionA.resolveClose({ state: 'release-failed', failures: [connectionAFailure] })
    await connectionADrain
    expect(coordinator.hasPendingDrain('connection-a')).toBe(false)
    expect(coordinator.takeCleanupFailures('connection-a')).toEqual([connectionAFailure])
    expect(coordinator.takeCleanupFailures('connection-b')).toEqual([])
  })
})
