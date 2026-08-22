// __tests__/backends/corebluetooth/corebluetooth-operation-dispatcher.test.js

const { CoreBluetoothOperationDispatcher } = require('../../../src/backends/corebluetooth/corebluetooth-operation-dispatcher')
const { opaqueId } = require('../../../src/backend-contract/primitives')
const { coreDispatch } = require('../../../src/core/unified-ble-core-helpers')
const { CoreOperationCoordinator } = require('../../../src/core/operation-coordinator')
const { ResourceLedger } = require('../../../src/core/resource-ledger')
const { CoreTraceRecorder } = require('../../../src/core/trace-recorder')

function deferred() {
  let resolve = null
  let reject = null
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

function createCoordinator() {
  const resourceLedger = new ResourceLedger()
  const trace = new CoreTraceRecorder(32, 4096)
  let nextCorrelation = 1
  return new CoreOperationCoordinator({
    now: () => 100,
    createCorrelation: () => {
      const correlation = opaqueId(`corebluetooth-core-operation-${nextCorrelation}`, 'core-operation', 'corebluetooth')
      nextCorrelation += 1
      return correlation
    },
    resourceLedger,
    trace
  })
}

describe('CoreBluetoothOperationDispatcher cancellation admission', () => {
  test('rejects an aborted public completion while quarantining its late native result', async () => {
    const pending = deferred()
    const controller = new AbortController()
    const dispatcher = new CoreBluetoothOperationDispatcher(() => 100)
    const dispatch = dispatcher.dispatch(
      { signal: controller.signal, deadline: null },
      'corebluetooth.read',
      () => pending.promise
    )

    controller.abort()

    await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(dispatch.requestCancellation()).resolves.toMatchObject({ state: 'not-cancellable' })
    expect(dispatcher.activeCount()).toBe(1)
    pending.resolve('late native value')
    await Promise.resolve()
    expect(dispatcher.activeCount()).toBe(0)
  })

  test('rejects a deadline-expired public completion while retaining physical ownership until native settlement', async () => {
    jest.useFakeTimers()
    const pending = deferred()
    const dispatcher = new CoreBluetoothOperationDispatcher(() => 100)
    const dispatch = dispatcher.dispatch(
      { signal: null, deadline: 101 },
      'corebluetooth.read',
      () => pending.promise
    )

    jest.advanceTimersByTime(1)

    await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(dispatcher.activeCount()).toBe(1)
    pending.resolve('late native value')
    await Promise.resolve()
    expect(dispatcher.activeCount()).toBe(0)
    jest.useRealTimers()
  })

  test('cancelAll rejects every public completion and quarantines late native results', async () => {
    const first = deferred()
    const second = deferred()
    const dispatcher = new CoreBluetoothOperationDispatcher(() => 100)
    const firstDispatch = dispatcher.dispatch(
      { signal: null, deadline: null },
      'corebluetooth.first',
      () => first.promise
    )
    const secondDispatch = dispatcher.dispatch(
      { signal: null, deadline: null },
      'corebluetooth.second',
      () => second.promise
    )

    dispatcher.cancelAll()

    await expect(firstDispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(secondDispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(dispatcher.activeCount()).toBe(2)
    first.resolve('late first result')
    second.resolve('late second result')
    await Promise.resolve()
    expect(dispatcher.activeCount()).toBe(0)
  })

  test('does not dispatch a second operation for a connection until its cancelled native work settles', async () => {
    const pending = deferred()
    const controller = new AbortController()
    const dispatcher = new CoreBluetoothOperationDispatcher(() => 100)
    const first = dispatcher.dispatch(
      { signal: controller.signal, deadline: null },
      'corebluetooth.read',
      () => pending.promise,
      'connection-1'
    )
    controller.abort()
    await expect(first.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const secondOperation = jest.fn(async () => 'second')
    const second = dispatcher.dispatch(
      { signal: null, deadline: null },
      'corebluetooth.write',
      secondOperation,
      'connection-1'
    )
    await expect(second.completion).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    expect(secondOperation).not.toHaveBeenCalled()
    pending.resolve('late first result')
    await Promise.resolve()
    const third = dispatcher.dispatch(
      { signal: null, deadline: null },
      'corebluetooth.write',
      async () => 'third',
      'connection-1'
    )
    await expect(third.completion).resolves.toBe('third')
  })

  test('does not release core quarantine before a cancelled native source settles', async () => {
    const pending = deferred()
    const controller = new AbortController()
    const dispatcher = new CoreBluetoothOperationDispatcher(() => 100)
    const coordinator = createCoordinator()
    let correlation
    const result = coordinator.run({
      queueKey: 'connection-1',
      options: { signal: controller.signal, deadline: null },
      mayCommit: false,
      dispatch: operationCorrelation => {
        correlation = operationCorrelation
        const dispatch = dispatcher.dispatch(
          { signal: controller.signal, deadline: null },
          'corebluetooth.read',
          () => pending.promise,
          'connection-1'
        )
        return coreDispatch(dispatch, operationCorrelation, value => value.terminal)
      }
    })

    controller.abort()

    await expect(result).resolves.toMatchObject({ outcome: 'aborted' })
    expect(dispatcher.activeCount()).toBe(1)
    expect(dispatcher.activeCount('connection-1')).toBe(1)
    expect(dispatcher.activeCount('other-connection')).toBe(0)
    expect(coordinator.activeCounts()).toMatchObject({ quarantined: 1 })

    let drained = false
    void coordinator.waitForQuarantineDrain().then(() => {
      drained = true
    })
    await new Promise(resolve => setImmediate(resolve))
    expect(drained).toBe(false)

    pending.resolve({
      terminal: { correlation, outcome: 'succeeded', cause: null }
    })
    await coordinator.waitForQuarantineDrain()
    expect(dispatcher.activeCount()).toBe(0)
    expect(coordinator.activeCounts()).toMatchObject({ quarantined: 0 })
  })
})
