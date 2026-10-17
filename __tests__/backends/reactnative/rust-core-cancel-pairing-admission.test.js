const { RustCoreSecurityBackend } = require('../../../src/backends/reactnative/react-native-rust-core-security')
const { contractError } = require('../../../src/backend-contract/errors')
const {
  rustCoreHarness,
  environment,
  scanOptions,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

function deferred() {
  let resolve, reject
  const promise = new Promise((a, b) => {
    resolve = a
    reject = b
  })
  return { promise, resolve, reject }
}
const state = {
  bond: 'bonded',
  encryption: 'unknown',
  authentication: 'unsupported',
  secureConnections: 'unsupported',
  pairingPossible: true
}
function fixture() {
  const pair = deferred(),
    ack = deferred()
  let id = 0
  const host = {
    now: () => Date.now(),
    nativePeerId: p => p,
    mintOperationId: () => `op-${++id}`,
    budget: opts => (opts.deadline === null ? {} : { budgetMs: opts.deadline - Date.now() }),
    watchAbort: jest.fn(() => () => {}),
    pair: jest.fn(() => pair.promise),
    cancelPairing: jest.fn(() => ack.promise)
  }
  const backend = new RustCoreSecurityBackend(host)
  const pairing = backend.pair('peer', {
    signal: null,
    deadline: null,
    ceremony: 'system',
    protection: 'system-default',
    transport: 'le'
  })
  return { backend, host, pair, ack, pairing }
}
afterEach(() => jest.useRealTimers())

test.each(['abort', 'deadline'])('cancel pre-%s admission dispatches no native cancellation', async cause => {
  jest.useFakeTimers({ doNotFake: ['setImmediate'] }).setSystemTime(1000)
  const f = fixture(),
    controller = new AbortController()
  if (cause === 'abort') controller.abort()
  const cancellation = f.backend.cancelPairing('peer', {
    signal: controller.signal,
    deadline: cause === 'deadline' ? 1000 : null
  })
  const outcome = cancellation.then(
    value => ({ value }),
    error => ({ error })
  )
  await settle()
  expect(f.host.cancelPairing).not.toHaveBeenCalled()
  expect((await outcome).error).toMatchObject({
    normalized: { code: cause === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
  })
  f.pair.resolve({ outcome: 'paired', state })
  await f.pairing
})

test.each(['ack', 'result'].flatMap(phase => ['abort', 'deadline'].map(cause => [phase, cause])))(
  '%s wait is bounded by %s without retiring original pairing',
  async (phase, cause) => {
    jest.useFakeTimers({ doNotFake: ['setImmediate'] }).setSystemTime(1000)
    const f = fixture(),
      controller = new AbortController()
    const cancellation = f.backend.cancelPairing('peer', {
      signal: controller.signal,
      deadline: cause === 'deadline' ? 1010 : null
    })
    const outcome = cancellation.then(
      value => ({ value }),
      error => ({ error })
    )
    if (phase === 'result') {
      f.ack.resolve()
      await settle()
    }
    if (cause === 'abort') controller.abort()
    else await jest.advanceTimersByTimeAsync(10)
    await settle()
    let settled = false
    outcome.then(() => {
      settled = true
    })
    await settle()
    expect(settled).toBe(true)
    expect((await outcome).error).toMatchObject({
      normalized: { code: cause === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
    })
    await expect(
      f.backend.pair('peer', {
        signal: null,
        deadline: null,
        ceremony: 'system',
        protection: 'system-default',
        transport: 'le'
      })
    ).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
    f.ack.resolve()
    f.pair.resolve({ outcome: 'paired', state })
    await expect(f.pairing).resolves.toMatchObject({ outcome: 'paired' })
  }
)

test('native cancellation refusal stays specific and repeated callers observe original actual outcome', async () => {
  const f = fixture()
  f.host.cancelPairing.mockRejectedValueOnce(contractError('permission.denied', 'platform', 'native.cancel-pairing'))
  await expect(f.backend.cancelPairing('peer', { signal: null, deadline: null })).rejects.toMatchObject({
    normalized: { code: 'permission.denied' }
  })
  const again = f.backend.cancelPairing('peer', { signal: null, deadline: null })
  f.ack.resolve()
  f.pair.resolve({ outcome: 'paired', state })
  await expect(again).resolves.toEqual({ outcome: 'paired' })
  await expect(f.pairing).resolves.toMatchObject({ outcome: 'paired' })
})

test('backend shutdown settles a held cancellation wait without inventing original pairing outcome', async () => {
  const f = fixture()
  const outcome = f.backend.cancelPairing('peer', { signal: null, deadline: null }).then(
    value => ({ value }),
    error => ({ error })
  )
  f.backend.close()
  await settle()
  let settled = false
  outcome.then(() => {
    settled = true
  })
  await settle()
  expect(settled).toBe(true)
  expect((await outcome).error).toMatchObject({ normalized: { code: 'lifecycle.destroyed' } })
  f.ack.resolve()
  f.pair.resolve({ outcome: 'paired', state })
  await expect(f.pairing).resolves.toMatchObject({ outcome: 'paired' })
})

test('ordinary Android factory preserves native unsupported cancellation while pairing stays owned', async () => {
  const h = rustCoreHarness({ platform: 'android' })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(h))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  try {
    expect(publicManager.capabilities.get('security:cancel-pairing').state).toBe('limited')
    const scan = await publicManager.scan()
    h.native.emitAdvertisement()
    const peer = (await scan.observations[Symbol.asyncIterator]().next()).value.value.peer
    await scan.stop()
    h.native.deferNextPair()
    const original = manager.securityBackend().pair(peer.id, {
      signal: null,
      deadline: null,
      transport: 'le',
      protection: 'system-default',
      ceremony: 'system'
    })
    await settle()
    h.native.failNext(
      'security.cancel-pairing',
      'capability.unsupported',
      'capability',
      'android.security.cancel-pairing'
    )
    await expect(publicManager.security.cancelPairing(peer, { timeoutMs: 100 })).rejects.toMatchObject({
      code: 'capability.unsupported',
      operation: 'android.security.cancel-pairing'
    })
    const cancellationCall = h.native.calls
      .filter(call => call[0] === 'invoke' && call[2] === 'security.cancel-pairing')
      .at(-1)
    expect(JSON.parse(cancellationCall[3]).budgetMs).toBe(100)
    h.native.pendingPair()
    await expect(original).resolves.toEqual({ outcome: 'cancelled' })
  } finally {
    await manager.destroy()
  }
})

test('overlapping cancellation callers keep independent waits and original pairing ownership', async () => {
  const f = fixture(),
    controller = new AbortController()
  const first = f.backend.cancelPairing('peer', { signal: controller.signal, deadline: null }).then(
    value => ({ value }),
    error => ({ error })
  )
  const second = f.backend.cancelPairing('peer', { signal: null, deadline: null })
  controller.abort()
  expect((await first).error).toMatchObject({ normalized: { code: 'operation.aborted' } })
  f.ack.resolve()
  f.pair.resolve({ outcome: 'paired', state })
  await expect(second).resolves.toEqual({ outcome: 'paired' })
  await expect(f.pairing).resolves.toMatchObject({ outcome: 'paired' })
  expect(f.host.cancelPairing.mock.calls.map(([args]) => args.operationId)).toEqual(['op-2', 'op-3'])
})

test('native cancel acknowledgment stays observed when dispatch crosses its deadline', async () => {
  jest.useFakeTimers({ doNotFake: ['setImmediate'] }).setSystemTime(1000)
  const f = fixture()
  const observe = jest.spyOn(f.ack.promise, 'then')
  f.host.cancelPairing.mockImplementation(() => {
    jest.setSystemTime(1020)
    return f.ack.promise
  })
  try {
    await expect(f.backend.cancelPairing('peer', { signal: null, deadline: 1010 })).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(observe).toHaveBeenCalled()
    f.ack.reject(contractError('permission.denied', 'platform', 'native.late-cancel-ack'))
    await settle()
  } finally {
    f.ack.resolve()
    f.pair.resolve({ outcome: 'paired', state })
    await f.pairing
  }
})

test.each(['abort', 'deadline'])('cancel rechecks %s immediately before native submission', async cause => {
  jest.useFakeTimers({ doNotFake: ['setImmediate'] }).setSystemTime(1000)
  const f = fixture(),
    controller = new AbortController()
  f.host.watchAbort.mockImplementation(() => {
    if (cause === 'abort') controller.abort()
    else jest.setSystemTime(1020)
    return () => {}
  })
  const result = f.backend.cancelPairing('peer', { signal: controller.signal, deadline: 1010 }).then(
    value => ({ value }),
    error => ({ error })
  )
  await settle()
  try {
    expect(f.host.cancelPairing).not.toHaveBeenCalled()
    expect((await result).error).toMatchObject({
      normalized: { code: cause === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
    })
  } finally {
    f.ack.resolve()
    f.pair.resolve({ outcome: 'paired', state })
    await f.pairing
    await result
  }
})
