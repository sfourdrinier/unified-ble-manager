const { createPublicBleManager } = require('../src/public/ble-manager')
const { contractError } = require('../src/backend-contract/errors')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')

function deferred() {
  let resolve, reject
  const promise = new Promise((done, fail) => {
    resolve = done
    reject = fail
  })
  return { promise, resolve, reject }
}
async function turns() {
  for (let i = 0; i < 8; i++) await Promise.resolve()
}
function watch() {
  const events = new CoreBoundedStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const close = jest.fn(async () => {
    events.close()
    return { state: 'released', failures: [] }
  })
  return { events, close }
}
async function opened(kind, open) {
  const connection = {
    connectionId: 'c',
    connectionGeneration: 'g',
    events: { async *[Symbol.asyncIterator]() {} },
    [kind === 'parameters' ? 'parameterEvents' : 'writeWithoutResponseReadiness']: open
  }
  const supported = new Set(['connection:direct', 'connection:parameters', 'gatt:write-without-response-readiness'])
  const manager = await createPublicBleManager(
    {
      supports: id => supported.has(id),
      capability: id => (supported.has(id) ? { state: 'supported', limitations: [] } : null),
      capabilities: () => [],
      connect: async () => connection
    },
    () => 100,
    { peerId: value => value }
  )
  const controls = (await manager.connect('p')).controls
  return (kind === 'parameters' ? controls.parameterEvents() : controls.writeReadiness('without-response'))[
    Symbol.asyncIterator
  ]()
}

describe.each(['parameters', 'readiness'])('%s watch acquisition owns cancellation', kind => {
  test('return aborts a stalled admission before awaiting it and pending next completes normally', async () => {
    const admission = deferred()
    let signal
    const iterator = await opened(kind, options => {
      signal = options?.signal
      signal?.addEventListener(
        'abort',
        () => admission.reject(contractError('operation.aborted', 'connection', 'native.watch.open')),
        { once: true }
      )
      return admission.promise
    })
    const pending = iterator.next().then(
      value => ({ value }),
      error => ({ error })
    )
    await turns()
    const returned = iterator.return()
    const cancelled = signal?.aborted === true
    // Bounded baseline control: release an un-signalled admission rather than hanging the test.
    if (!cancelled) admission.reject(contractError('operation.aborted', 'connection', 'native.watch.open'))
    await expect(returned).resolves.toMatchObject({ done: true })
    expect(cancelled).toBe(true)
    await expect(pending).resolves.toEqual({ value: { done: true, value: undefined } })
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
  })

  test('return before deferred dispatch has no backend admission', async () => {
    const open = jest.fn(async () => watch())
    const iterator = await opened(kind, open)
    const pending = iterator.next()
    const returned = iterator.return()
    await Promise.all([pending, returned])
    expect(open).not.toHaveBeenCalled()
  })

  test('an abort-ignoring late watch is closed once and never delivered', async () => {
    const admission = deferred()
    let signal
    const open = jest.fn(options => {
      signal = options?.signal
      return admission.promise
    })
    const iterator = await opened(kind, open)
    const pending = [iterator.next(), iterator.next()]
    await turns()
    const returned = [iterator.return(), iterator.return()]
    const cancelled = signal?.aborted === true
    const acquired = watch()
    admission.resolve(acquired)
    await expect(Promise.all([...pending, ...returned])).resolves.toEqual(
      Array(4).fill({ done: true, value: undefined })
    )
    expect(cancelled).toBe(true)
    expect(open).toHaveBeenCalledTimes(1)
    expect(acquired.close).toHaveBeenCalledTimes(1)
    await iterator.return()
    expect(acquired.close).toHaveBeenCalledTimes(1)
  })

  test('a late watch cleanup failure retains debt for a real retry', async () => {
    const admission = deferred()
    const iterator = await opened(kind, () => admission.promise)
    const pending = iterator.next().then(
      value => ({ value }),
      error => ({ error })
    )
    await turns()
    const returned = iterator.return()
    const failure = expect(returned).rejects.toMatchObject({ cleanup: { state: 'release-failed' } })
    const acquired = watch()
    acquired.close.mockResolvedValueOnce({
      state: 'release-failed',
      failures: [
        { resourceKind: 'watch', error: contractError('platform.failure', 'cleanup', 'native.watch.close').normalized }
      ]
    })
    admission.resolve(acquired)
    await failure
    expect((await pending).error).toMatchObject({ cleanup: { state: 'release-failed' } })
    await expect(iterator.return()).resolves.toMatchObject({ done: true })
    expect(acquired.close).toHaveBeenCalledTimes(2)
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
  })

  test('a genuine late opening failure remains the source error after return requests cancellation', async () => {
    const admission = deferred()
    const iterator = await opened(kind, () => admission.promise)
    const pending = iterator.next()
    const failure = expect(pending).rejects.toMatchObject({
      code: 'adapter.powered-off',
      operation: 'native.watch.open'
    })
    await turns()
    const returned = iterator.return()
    admission.reject(contractError('adapter.powered-off', 'adapter', 'native.watch.open'))
    await failure
    await expect(returned).resolves.toMatchObject({ done: true })
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
  })
})
