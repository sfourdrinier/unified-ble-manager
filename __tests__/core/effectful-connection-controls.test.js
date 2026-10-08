const {
  requestCorePriority,
  requestCoreSubrate,
  requestCorePhy,
  requestCoreMtu,
  readCorePhy
} = require('../../src/core/core-connection-controls')
const { CoreOperationCoordinator } = require('../../src/core/operation-coordinator')
const { ResourceLedger } = require('../../src/core/resource-ledger')
const { CoreTraceRecorder } = require('../../src/core/trace-recorder')
const { opaqueId } = require('../../src/backend-contract/primitives')

function deferred() {
  let resolve
  const promise = new Promise(done => { resolve = done })
  return { promise, resolve }
}
function fixture() {
  let ordinal = 0
  const ledger = new ResourceLedger()
  const coordinator = new CoreOperationCoordinator({
    now: () => 10,
    createCorrelation: () => opaqueId(`control-${++ordinal}`, 'core-operation', 'controls-test'),
    resourceLedger: ledger,
    trace: new CoreTraceRecorder(64, 8192)
  })
  return { coordinator, ledger, connection: { resource: { connectionId: 'connection-1' }, assertCurrent() {} } }
}
afterEach(() => jest.useRealTimers())

const effects = [
  ['priority', 'requestPriority', requestCorePriority, 'balanced'],
  ['subrate', 'requestSubrate', requestCoreSubrate, 'low-power'],
  ['PHY', 'requestPhy', requestCorePhy, { tx: 'le-2m' }],
  ['MTU', 'requestMtu', requestCoreMtu, 185]
]

function backendFor(method, dispatch) { return { connections: { [method]: dispatch } } }

describe.each(effects)('%s effectful shared-core control', (_name, method, request, argument) => {
  test.each([[true, 'abort'], [false, 'abort'], [true, 'deadline'], [false, 'deadline']])('preserves validated accepted=%s response before %s while physical retirement drains', async (accepted, cause) => {
    if (cause === 'deadline') jest.useFakeTimers()
    const { coordinator, ledger, connection } = fixture()
    const controller = new AbortController()
    const physical = deferred()
    const cancellation = jest.fn(async () => ({ state: 'already-terminal' }))
    const backend = backendFor(method, (_connection, options) => ({
      completion: Promise.resolve({ accepted, terminal: { correlation: options.operation.correlation, outcome: 'succeeded', cause: null } }),
      physicalSettlement: physical.promise,
      requestCancellation: cancellation
    }))
    const result = request(backend, coordinator, connection, argument, { signal: controller.signal, deadline: cause === 'deadline' ? 11 : null })
    // The adapter has validated the response, but physical cleanup has not retired.
    await Promise.resolve()
    if (cause === 'abort') controller.abort()
    else jest.advanceTimersByTime(1)
    jest.useRealTimers()
    coordinator.destroy()
    expect(coordinator.hasPendingDrain('connection-1')).toBe(true)
    const drain = coordinator.waitForQuarantineDrain('connection-1')
    let drained = false
    void drain.then(() => { drained = true })
    await Promise.resolve()
    expect(drained).toBe(false)
    physical.resolve()
    await expect(result).resolves.toMatchObject({ accepted })
    await drain
    expect(cancellation).not.toHaveBeenCalled()
    expect(ledger.isZero()).toBe(true)
    jest.useRealTimers()
  })

  test.each(['abort', 'deadline'])('pending %s reports uncertain effect promptly, then drains late response', async cause => {
    jest.useFakeTimers()
    const { coordinator, ledger, connection } = fixture()
    const controller = new AbortController()
    const answer = deferred()
    let correlation
    const cancellation = jest.fn(async () => ({ state: 'cancellation-requested' }))
    const backend = backendFor(method, (_connection, options) => {
      correlation = options.operation.correlation
      return { completion: answer.promise, requestCancellation: cancellation }
    })
    const result = request(backend, coordinator, connection, argument, {
      signal: controller.signal, deadline: cause === 'deadline' ? 11 : null
    })
    const rejected = expect(result).rejects.toMatchObject({ normalized: {
      code: cause === 'abort' ? 'operation.aborted' : 'operation.timed-out', commit: 'uncertain', retryability: 'never'
    } })
    if (cause === 'abort') controller.abort()
    else jest.advanceTimersByTime(1)
    jest.useRealTimers()
    await rejected
    expect(cancellation).toHaveBeenCalledTimes(1)
    answer.resolve({ accepted: true, terminal: { correlation, outcome: 'succeeded', cause: null } })
    await coordinator.waitForQuarantineDrain()
    expect(ledger.isZero()).toBe(true)
    jest.useRealTimers()
  })

  test('queued cancellation removes the request without a backend effect or uncertain commit', async () => {
    const { coordinator, ledger, connection } = fixture()
    const first = deferred()
    const blocker = coordinator.run({
      queueKey: 'connection-1', options: { signal: null, deadline: null }, mayCommit: false,
      dispatch: () => ({ completion: first.promise, requestCancellation: async () => {} })
    })
    const controller = new AbortController()
    const dispatch = jest.fn()
    const result = request(backendFor(method, dispatch), coordinator, connection, argument,
      { signal: controller.signal, deadline: null })
    const rejected = expect(result).rejects.toMatchObject({ normalized: {
      code: 'operation.aborted', retryability: 'caller-decides'
    } })
    controller.abort()
    await rejected
    expect(dispatch).not.toHaveBeenCalled()
    first.resolve('released')
    await blocker
    expect(ledger.isZero()).toBe(true)
  })

  test('pre-dispatch abort has no backend effect', async () => {
    const { coordinator, connection } = fixture()
    const controller = new AbortController()
    controller.abort()
    const dispatch = jest.fn()
    await expect(request(backendFor(method, dispatch), coordinator, connection, argument,
      { signal: controller.signal, deadline: null })).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(dispatch).not.toHaveBeenCalled()
  })
})

test('read-only PHY cancellation remains noncommitting', async () => {
  const { coordinator, connection } = fixture()
  const controller = new AbortController()
  const answer = deferred()
  let correlation
  const result = readCorePhy(backendFor('readPhy', (_connection, options) => {
    correlation = options.operation.correlation
    return { completion: answer.promise, requestCancellation: async () => ({ state: 'cancellation-requested' }) }
  }), coordinator, connection, { signal: controller.signal, deadline: null })
  const rejected = expect(result).rejects.toMatchObject({ normalized: { code: 'operation.aborted', retryability: 'caller-decides' } })
  controller.abort()
  await rejected
  answer.resolve({ terminal: { correlation, outcome: 'succeeded', cause: null } })
  await coordinator.waitForQuarantineDrain()
})
