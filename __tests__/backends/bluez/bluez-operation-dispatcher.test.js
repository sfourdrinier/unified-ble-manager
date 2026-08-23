// __tests__/backends/bluez/bluez-operation-dispatcher.test.js

const { BluezOperationDispatcher } = require('../../../src/backends/bluez/bluez-operation-dispatcher')
const { opaqueId } = require('../../../src/backend-contract/primitives')
const { coreDispatch } = require('../../../src/core/unified-ble-core-helpers')
const { CoreOperationCoordinator } = require('../../../src/core/operation-coordinator')
const { ResourceLedger } = require('../../../src/core/resource-ledger')
const { CoreTraceRecorder } = require('../../../src/core/trace-recorder')

function createCoordinator() {
  const resourceLedger = new ResourceLedger()
  const trace = new CoreTraceRecorder(32, 4096)
  let nextCorrelation = 1
  return new CoreOperationCoordinator({
    now: () => 10,
    createCorrelation: () => {
      const correlation = opaqueId(`bluez-core-operation-${nextCorrelation}`, 'core-operation', 'bluez')
      nextCorrelation += 1
      return correlation
    },
    resourceLedger,
    trace
  })
}

describe('BluezOperationDispatcher', () => {
  test('keeps a non-cancellable D-Bus operation quarantined until its physical promise settles', async () => {
    const dispatcher = new BluezOperationDispatcher(() => 10)
    const abortController = new AbortController()
    let releasePhysical
    const physical = new Promise(resolve => {
      releasePhysical = resolve
    })
    const dispatch = dispatcher.dispatch(
      { signal: abortController.signal, deadline: null },
      'bluez.test.non-cancellable',
      async () => physical
    )

    expect(dispatcher.activeCount()).toBe(1)
    await new Promise(resolve => setImmediate(resolve))
    abortController.abort()
    await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(dispatcher.activeCount()).toBe(1)
    await expect(dispatch.requestCancellation()).resolves.toMatchObject({ state: 'not-cancellable' })

    let idleSettled = false
    const idle = dispatcher.waitForIdle().then(() => {
      idleSettled = true
    })
    await Promise.resolve()
    expect(idleSettled).toBe(false)
    releasePhysical('late-result')
    await idle
    expect(dispatcher.activeCount()).toBe(0)
    await expect(dispatch.requestCancellation()).resolves.toMatchObject({ state: 'already-terminal' })
  })

  test('keeps the idle barrier pending until a cancellation hook settles', async () => {
    const dispatcher = new BluezOperationDispatcher(() => 10)
    const abortController = new AbortController()
    let releaseCancellation
    const cancellation = new Promise(resolve => {
      releaseCancellation = resolve
    })
    const dispatch = dispatcher.dispatch(
      { signal: abortController.signal, deadline: null },
      'bluez.test.cancellation-hook',
      async () => undefined,
      async () => cancellation
    )

    abortController.abort()
    await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await new Promise(resolve => setImmediate(resolve))
    expect(dispatcher.activeCount()).toBe(1)

    let idleSettled = false
    const idle = dispatcher.waitForIdle().then(() => {
      idleSettled = true
    })
    await Promise.resolve()
    expect(idleSettled).toBe(false)
    releaseCancellation()
    await idle
    expect(dispatcher.activeCount()).toBe(0)
  })

  test('does not start a queued operation after immediate cancellation', async () => {
    const dispatcher = new BluezOperationDispatcher(() => 10)
    const abortController = new AbortController()
    const operation = jest.fn(async () => undefined)
    const dispatch = dispatcher.dispatch(
      { signal: abortController.signal, deadline: null },
      'bluez.test.queued-cancellation',
      operation
    )

    abortController.abort()
    await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await dispatch.physicalSettlement
    expect(operation).not.toHaveBeenCalled()
  })

  test('does not release core quarantine before a cancelled native operation settles', async () => {
    let resolvePending
    const pending = new Promise(resolve => {
      resolvePending = resolve
    })
    const controller = new AbortController()
    const dispatcher = new BluezOperationDispatcher(() => 10)
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
          'bluez.gatt.read',
          async () => pending
        )
        return coreDispatch(dispatch, operationCorrelation, value => value.terminal)
      }
    })

    await new Promise(resolve => setImmediate(resolve))
    controller.abort()

    await expect(result).resolves.toMatchObject({ outcome: 'aborted' })
    expect(dispatcher.activeCount()).toBe(1)
    expect(coordinator.activeCounts()).toMatchObject({ quarantined: 1 })

    let drained = false
    void coordinator.waitForQuarantineDrain().then(() => {
      drained = true
    })
    await new Promise(resolve => setImmediate(resolve))
    expect(drained).toBe(false)

    resolvePending({ terminal: { correlation, outcome: 'succeeded', cause: null } })
    await coordinator.waitForQuarantineDrain()
    expect(dispatcher.activeCount()).toBe(0)
    expect(coordinator.activeCounts()).toMatchObject({ quarantined: 0 })
  })
})
