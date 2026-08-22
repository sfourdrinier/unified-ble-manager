// __tests__/backends/bluez/bluez-operation-dispatcher.test.js

const { BluezOperationDispatcher } = require('../../../src/backends/bluez/bluez-operation-dispatcher')

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
    await dispatch.physicalSettled
    expect(operation).not.toHaveBeenCalled()
  })
})
