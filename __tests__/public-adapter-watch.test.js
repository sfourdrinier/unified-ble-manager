// __tests__/public-adapter-watch.test.js

const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { contractError } = require('../src/backend-contract/errors')
const { createPublicBleManager } = require('../src/public/ble-manager')
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')
const { createDeterministicTestBleManager } = require('../src/testing/deterministic/deterministic-test-manager')
const {
  inspectDeterministicStreamOwnershipForTests
} = require('../src/testing/deterministic/deterministic-backend-base')

function adapterState(overrides = {}) {
  return {
    availability: 'available',
    authorization: 'granted',
    power: 'on',
    backendGeneration: 'backend-1',
    updatedAt: 1,
    safeReason: null,
    ...overrides
  }
}

function stateStream(overflowPolicy = 'drop-newest') {
  return new CoreBoundedStream(
    {
      itemCapacity: capacity(1),
      byteCapacity: capacity(64),
      reservedControlCapacity: capacity(1)
    },
    overflowPolicy
  )
}

function publicInternal(watch) {
  return {
    identity: { attachment: { adapter: { adapterId: 'adapter-1' } } },
    attachedBackend: undefined,
    capability: () => null,
    capabilities: () => [],
    adapterState: async () => watch.initial,
    adapterStates: async () => watch,
    destroy: async () => ({ state: 'released', failures: [] })
  }
}

function failedWatchCleanup(resourceKind = 'adapter-watch') {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind,
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation: `public-adapter-watch.${resourceKind}`,
          platform: {
            domain: 'native',
            code: 'E_WATCH_CLEANUP',
            safeMessage: 'adapter watch cleanup failed',
            metadata: { nested: { bytes: new Uint8Array([7, 8, 9]) } }
          },
          retryability: 'caller-decides'
        }
      }
    ]
  }
}

function ipcBootstrap() {
  return {
    attachment: {
      adapter: { adapterId: 'adapter-1' },
      backendGeneration: 'backend-1'
    },
    discovery: { kind: 'continuous-scan' }
  }
}

function ipcCapabilities() {
  return {
    supports: () => false,
    get: () => undefined,
    require: () => {
      throw new Error('not used')
    },
    list: () => []
  }
}

describe('public adapter watch contract', () => {
  test('maps values while preserving source overflow and terminal notices', async () => {
    const initial = adapterState()
    const source = stateStream()
    const watch = {
      initial,
      values: source,
      stop: async () => source.close()
    }
    const manager = await createPublicBleManager(publicInternal(watch), () => 100)

    source.emit(adapterState({ updatedAt: 2, safeReason: 'changed' }), 40)
    source.emit(adapterState({ updatedAt: 3 }), 40)
    source.finishWithReason('overflow')

    const publicWatch = await manager.adapter.watchState()
    expect(publicWatch.initial).toEqual(initial)
    const iterator = publicWatch.values[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'overflow', policy: 'drop-newest' }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: { updatedAt: 2, safeReason: 'changed' } }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'overflow' }
    })
    await expect(publicWatch.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('aborting a public watch stops the internal watch and closes its stream', async () => {
    const source = stateStream()
    const stop = jest.fn(async () => source.close())
    const controller = new AbortController()
    const watch = {
      initial: adapterState(),
      values: source,
      stop
    }
    const manager = await createPublicBleManager(publicInternal(watch), () => 100)
    const publicWatch = await manager.adapter.watchState({ signal: controller.signal })
    const next = publicWatch.values[Symbol.asyncIterator]().next()

    controller.abort()

    await expect(next).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'closed' }
    })
    await expect(publicWatch.stop()).resolves.toMatchObject({ state: 'released' })
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('stops an allocated source when public adapter stream projection rejects its byte quota', async () => {
    const source = new CoreBoundedStream(
      {
        itemCapacity: capacity(1),
        byteCapacity: capacity(4 * 1024 * 1024 + 1),
        reservedControlCapacity: capacity(1)
      },
      'drop-newest'
    )
    const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
    const manager = await createPublicBleManager(
      publicInternal({ initial: adapterState(), values: source, stop }),
      () => 100
    )

    await expect(manager.adapter.watchState()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'stream'
    })
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('aggregates pre-abort primary failure with projected release-failed cleanup', async () => {
    const cleanup = failedWatchCleanup('pre-abort')
    const stop = jest.fn(async () => cleanup)
    const manager = await createPublicBleManager(
      publicInternal({ initial: adapterState(), values: stateStream(), stop }),
      () => 100
    )
    const controller = new AbortController()
    controller.abort()

    let failure
    try {
      await manager.adapter.watchState({ signal: controller.signal })
    } catch (error) {
      failure = error
    }
    expect(failure).toBeInstanceOf(AggregateError)
    expect(failure.errors).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: 'operation.aborted' }),
        expect.objectContaining({ name: 'BleCleanupError' })
      ])
    )
    const cleanupError = failure.errors.find(error => error.name === 'BleCleanupError')
    expect(cleanupError.cleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([7, 8, 9]))
    expect(Object.isFrozen(cleanupError.cleanup.failures[0].error.platform.metadata.nested)).toBe(true)
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('aggregates projection failure with projected release-failed cleanup', async () => {
    const source = new CoreBoundedStream(
      {
        itemCapacity: capacity(1),
        byteCapacity: capacity(4 * 1024 * 1024 + 1),
        reservedControlCapacity: capacity(1)
      },
      'drop-newest'
    )
    const cleanup = failedWatchCleanup('projection')
    const stop = jest.fn(async () => cleanup)
    const manager = await createPublicBleManager(
      publicInternal({ initial: adapterState(), values: source, stop }),
      () => 100
    )

    let failure
    try {
      await manager.adapter.watchState()
    } catch (error) {
      failure = error
    }
    expect(failure).toBeInstanceOf(AggregateError)
    expect(failure.errors).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: 'protocol.malformed' }),
        expect.objectContaining({ name: 'BleCleanupError' })
      ])
    )
    const cleanupError = failure.errors.find(error => error.name === 'BleCleanupError')
    expect(cleanupError.cleanup.failures[0].error.platform.metadata.nested.bytes).toEqual(new Uint8Array([7, 8, 9]))
    expect(Object.isFrozen(cleanupError.cleanup.failures[0].error.platform.metadata.nested)).toBe(true)
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('aggregates projection failure with rejected watch cleanup', async () => {
    const source = new CoreBoundedStream(
      {
        itemCapacity: capacity(1),
        byteCapacity: capacity(4 * 1024 * 1024 + 1),
        reservedControlCapacity: capacity(1)
      },
      'drop-newest'
    )
    const cleanupError = contractError('platform.failure', 'cleanup', 'public-adapter-watch.rejected-cleanup')
    const stop = jest.fn(async () => {
      throw cleanupError
    })
    const manager = await createPublicBleManager(
      publicInternal({ initial: adapterState(), values: source, stop }),
      () => 100
    )

    let failure
    try {
      await manager.adapter.watchState()
    } catch (error) {
      failure = error
    }
    expect(failure).toBeInstanceOf(AggregateError)
    expect(failure.errors).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: 'protocol.malformed' }),
        expect.objectContaining({ code: 'platform.failure', operation: 'public-adapter-watch.rejected-cleanup' })
      ])
    )
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('accepts the deterministic backend adapter watch byte quota and cleans it up', async () => {
    const { manager, fixture } = await createDeterministicTestBleManager()
    const watch = await manager.adapter.watchState()
    expect(watch.values.limits.byteCapacity).toBe(1024 * 1024)
    await expect(watch.stop()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(inspectDeterministicStreamOwnershipForTests(fixture.backend)).toMatchObject({ stateWatchers: 0 })
  })
})

describe('IPC public adapter watch contract', () => {
  test('keeps a 256-correlations-per-30-seconds native window healthy for 60 seconds with operation headroom', async () => {
    jest.useFakeTimers()
    const completed = []
    const states = [adapterState()]
    const adapterStateCall = jest.fn(async () => {
      const now = Date.now()
      while (completed[0] !== undefined && completed[0] <= now - 30_000) completed.shift()
      if (completed.length >= 256) throw contractError('protocol.violation', 'ipc', 'tauri.correlation-window')
      completed.push(now)
      return states[states.length - 1]
    })
    const ipc = {
      bootstrap: ipcBootstrap(),
      capabilities: ipcCapabilities(),
      adapterState: adapterStateCall
    }
    const manager = new IpcPublicManagerAdapter(ipc)
    try {
      const ordinaryOperations = Array.from({ length: 100 }, () => adapterStateCall())
      await Promise.all(ordinaryOperations)
      const [publicWatch, secondWatch, thirdWatch] = await Promise.all([
        manager.adapter.watchState(),
        manager.adapter.watchState(),
        manager.adapter.watchState()
      ])
      const iterator = publicWatch.values[Symbol.asyncIterator]()
      const pending = iterator.next()

      await jest.advanceTimersByTimeAsync(60_000)
      await Promise.resolve()
      states.push(adapterState({ power: 'off', updatedAt: 2, safeReason: 'poll-change' }))
      await jest.advanceTimersByTimeAsync(500)
      await Promise.resolve()

      await expect(pending).resolves.toMatchObject({
        done: false,
        value: { kind: 'value', value: { power: 'off', updatedAt: 2, safeReason: 'poll-change' } }
      })
      expect(adapterStateCall.mock.calls.length).toBeLessThanOrEqual(230)
      await secondWatch.stop()
      await thirdWatch.stop()
      await expect(publicWatch.stop()).resolves.toMatchObject({ state: 'released' })
    } finally {
      jest.useRealTimers()
    }
  })

  test('does not schedule another adapter-state poll after stop', async () => {
    jest.useFakeTimers()
    const adapterStateCall = jest.fn(async () => adapterState())
    const ipc = {
      bootstrap: ipcBootstrap(),
      capabilities: ipcCapabilities(),
      adapterState: adapterStateCall
    }
    const manager = new IpcPublicManagerAdapter(ipc)
    try {
      const publicWatch = await manager.adapter.watchState()
      expect(adapterStateCall).toHaveBeenCalledTimes(1)
      await publicWatch.stop()
      await jest.advanceTimersByTimeAsync(5_000)
      expect(adapterStateCall).toHaveBeenCalledTimes(1)
    } finally {
      jest.useRealTimers()
    }
  })
})

describe('IPC adapter shared owner races', () => {
  const deferred = () => {
    let resolve, reject
    const promise = new Promise((yes, no) => {
      resolve = yes
      reject = no
    })
    return { promise, resolve, reject }
  }
  const managerFor = adapterStateCall =>
    new IpcPublicManagerAdapter({
      bootstrap: ipcBootstrap(),
      capabilities: ipcCapabilities(),
      adapterState: adapterStateCall
    })
  beforeEach(() => jest.useFakeTimers())
  afterEach(() => jest.useRealTimers())

  test('last stop aborts pending poll and a new owner runs independently of its stale completion', async () => {
    const oldPoll = deferred()
    const read = jest
      .fn()
      .mockResolvedValue(adapterState())
      .mockImplementationOnce(async () => adapterState())
      .mockImplementationOnce(() => oldPoll.promise)
    const manager = managerFor(read)
    const first = await manager.adapter.watchState()
    await jest.advanceTimersByTimeAsync(500)
    const signal = read.mock.calls[1][0].signal
    await first.stop()
    expect(signal.aborted).toBe(true)
    const next = await manager.adapter.watchState()
    const event = next.values[Symbol.asyncIterator]().next()
    oldPoll.resolve(adapterState({ power: 'off', updatedAt: 9 }))
    await jest.advanceTimersByTimeAsync(500)
    expect(read).toHaveBeenCalledTimes(4)
    await next.stop()
    await expect(event).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    await jest.advanceTimersByTimeAsync(1000)
    expect(read).toHaveBeenCalledTimes(4)
  })

  test('one initial waiter can abort promptly without cancelling another; last abort permits fresh read', async () => {
    const initial = deferred()
    const read = jest
      .fn()
      .mockImplementationOnce(() => initial.promise)
      .mockResolvedValue(adapterState({ power: 'off' }))
    const manager = managerFor(read)
    const a = new AbortController(),
      b = new AbortController()
    const first = manager.adapter.watchState({ signal: a.signal })
    const firstRejected = expect(first).rejects.toMatchObject({ code: 'operation.aborted' })
    const second = manager.adapter.watchState({ signal: b.signal })
    const secondRejected = expect(second).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(read).toHaveBeenCalledTimes(1)
    const ownedSignal = read.mock.calls[0][0].signal
    a.abort()
    await firstRejected
    expect(ownedSignal.aborted).toBe(false)
    b.abort()
    await secondRejected
    expect(ownedSignal.aborted).toBe(true)
    const fresh = await manager.adapter.watchState()
    expect(fresh.initial.power).toBe('off')
    initial.resolve(adapterState())
    await jest.advanceTimersByTimeAsync(500)
    expect(read).toHaveBeenCalledTimes(3)
    await fresh.stop()
  })

  test('one aborted initial waiter leaves the other subscribed and suppresses unchanged polls', async () => {
    const initial = deferred()
    const read = jest
      .fn()
      .mockImplementationOnce(() => initial.promise)
      .mockResolvedValue(adapterState())
    const manager = managerFor(read)
    const a = new AbortController()
    const first = manager.adapter.watchState({ signal: a.signal })
    const rejected = expect(first).rejects.toMatchObject({ code: 'operation.aborted' })
    const second = manager.adapter.watchState()
    a.abort()
    await rejected
    initial.resolve(adapterState())
    const watch = await second
    let settled = false
    const next = watch.values[Symbol.asyncIterator]()
      .next()
      .then(value => {
        settled = true
        return value
      })
    await jest.advanceTimersByTimeAsync(1500)
    expect(settled).toBe(false)
    await watch.stop()
    await expect(next).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
  })

  test('poll failure terminates all subscribers and next watch acquires fresh state', async () => {
    const read = jest
      .fn()
      .mockResolvedValue(adapterState())
      .mockImplementationOnce(async () => adapterState())
      .mockRejectedValueOnce(new Error('route failure'))
    const manager = managerFor(read)
    const [a, b] = await Promise.all([manager.adapter.watchState(), manager.adapter.watchState()])
    const an = a.values[Symbol.asyncIterator]().next(),
      bn = b.values[Symbol.asyncIterator]().next()
    await jest.advanceTimersByTimeAsync(500)
    await expect(an).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'source-failed' } })
    await expect(bn).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'source-failed' } })
    const fresh = await manager.adapter.watchState()
    expect(read).toHaveBeenCalledTimes(3)
    await a.stop()
    await b.stop()
    await jest.advanceTimersByTimeAsync(500)
    expect(read).toHaveBeenCalledTimes(4)
    await fresh.stop()
  })

  test('a stale rejected poll cannot terminate its replacement owner', async () => {
    const pending = deferred()
    const read = jest
      .fn()
      .mockResolvedValue(adapterState())
      .mockResolvedValueOnce(adapterState())
      .mockImplementationOnce(() => pending.promise)
    const manager = managerFor(read)
    const old = await manager.adapter.watchState()
    await jest.advanceTimersByTimeAsync(500)
    await old.stop()
    const fresh = await manager.adapter.watchState()
    pending.reject(new Error('late old route failure'))
    await jest.advanceTimersByTimeAsync(500)
    expect(read).toHaveBeenCalledTimes(4)
    await fresh.stop()
  })

  test('failed shared acquisition rejects every waiter and next acquisition starts fresh', async () => {
    const pending = deferred()
    const read = jest
      .fn()
      .mockImplementationOnce(() => pending.promise)
      .mockResolvedValue(adapterState())
    const manager = managerFor(read)
    const first = manager.adapter.watchState(),
      second = manager.adapter.watchState()
    const failures = Promise.allSettled([first, second])
    pending.reject(new Error('initial read failure'))
    expect((await failures).map(result => result.status)).toEqual(['rejected', 'rejected'])
    const fresh = await manager.adapter.watchState()
    expect(read).toHaveBeenCalledTimes(2)
    await fresh.stop()
  })

  test('each watcher owns its bounded queue; a slow consumer does not overflow its peer', async () => {
    let sequence = 0
    const read = jest.fn(async () => adapterState({ updatedAt: sequence++ }))
    const manager = managerFor(read)
    const [slow, fast] = await Promise.all([manager.adapter.watchState(), manager.adapter.watchState()])
    const fastIterator = fast.values[Symbol.asyncIterator]()
    for (let n = 0; n < 130; n++) {
      const event = fastIterator.next()
      await jest.advanceTimersByTimeAsync(500)
      await expect(event).resolves.toMatchObject({ value: { kind: 'value', value: { updatedAt: n + 1 } } })
    }
    await expect(slow.values[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'overflow', policy: 'drop-oldest' }
    })
    await slow.stop()
    await fast.stop()
  })

  test('stopping one subscriber retains shared polling; last stop closes timer', async () => {
    const read = jest.fn().mockResolvedValue(adapterState())
    const manager = managerFor(read)
    const [a, b] = await Promise.all([manager.adapter.watchState(), manager.adapter.watchState()])
    await a.stop()
    await jest.advanceTimersByTimeAsync(1000)
    expect(read).toHaveBeenCalledTimes(3)
    await b.stop()
    await jest.advanceTimersByTimeAsync(1000)
    expect(read).toHaveBeenCalledTimes(3)
  })
})

describe('IPC adapter readiness cadence', () => {
  const managerFor = read =>
    new IpcPublicManagerAdapter({ bootstrap: ipcBootstrap(), capabilities: ipcCapabilities(), adapterState: read })
  beforeEach(() => jest.useFakeTimers())
  afterEach(() => jest.useRealTimers())
  test('three readiness callers and a watch share one route budget for sixty seconds', async () => {
    const routes = []
    let powered = false
    const read = jest.fn(async () => {
      routes.push(performance.now())
      expect(routes.filter(at => at > performance.now() - 30000).length + 100).toBeLessThanOrEqual(256)
      return adapterState({ power: powered ? 'on' : 'off' })
    })
    const manager = managerFor(read)
    const watch = await manager.adapter.watchState()
    const ready = [0, 1, 2].map(() => manager.adapter.waitUntilReady({ timeoutMs: 65000 }))
    await jest.advanceTimersByTimeAsync(60000)
    expect(read).toHaveBeenCalledTimes(121)
    powered = true
    await jest.advanceTimersByTimeAsync(500)
    await expect(Promise.all(ready)).resolves.toHaveLength(3)
    await watch.stop()
  })

  test('one cancelled waiter leaves its peers and watch live', async () => {
    let powered = false
    const read = jest.fn(async () => adapterState({ power: powered ? 'on' : 'off' }))
    const manager = managerFor(read)
    const watch = await manager.adapter.watchState()
    const controller = new AbortController()
    const first = manager.adapter.waitUntilReady({ signal: controller.signal })
    const rejected = expect(first).rejects.toMatchObject({ code: 'operation.aborted' })
    const second = manager.adapter.waitUntilReady()
    await jest.advanceTimersByTimeAsync(100)
    controller.abort()
    await rejected
    expect(read.mock.calls[0][0].signal.aborted).toBe(false)
    powered = true
    await jest.advanceTimersByTimeAsync(400)
    await expect(second).resolves.toMatchObject({ power: 'on' })
    expect(read).toHaveBeenCalledTimes(2)
    await watch.stop()
  })

  test('all readiness callers cancelling releases the shared owner', async () => {
    const read = jest.fn().mockResolvedValue(adapterState({ power: 'off' }))
    const manager = managerFor(read)
    const controllers = [new AbortController(), new AbortController(), new AbortController()]
    const waits = controllers.map(controller => manager.adapter.waitUntilReady({ signal: controller.signal }))
    const outcomes = Promise.allSettled(waits)
    await jest.advanceTimersByTimeAsync(100)
    controllers.forEach(controller => controller.abort())
    expect((await outcomes).map(result => result.reason.code)).toEqual(Array(3).fill('operation.aborted'))
    expect(read.mock.calls[0][0].signal.aborted).toBe(true)
    await jest.advanceTimersByTimeAsync(60000)
    expect(read).toHaveBeenCalledTimes(1)
  })

  test('short acquisition deadline releases only its own pending watch', async () => {
    let finish
    const read = jest.fn(
      () =>
        new Promise(resolve => {
          finish = resolve
        })
    )
    const manager = managerFor(read)
    const ordinaryWatch = manager.adapter.watchState()
    const ready = manager.adapter.waitUntilReady({ timeoutMs: 50 })
    const rejected = expect(ready).rejects.toMatchObject({ code: 'operation.timed-out' })
    await jest.advanceTimersByTimeAsync(50)
    await rejected
    expect(read.mock.calls[0][0].signal.aborted).toBe(false)
    finish(adapterState())
    await (await ordinaryWatch).stop()
    expect(read.mock.calls[0][0].signal.aborted).toBe(true)
  })

  test('abort during acquisition reports failed cleanup as well as cancellation', async () => {
    const read = jest.fn(() => new Promise(() => {}))
    const manager = managerFor(read)
    const controller = new AbortController()
    const ready = manager.adapter.waitUntilReady({ signal: controller.signal })
    const outcome = ready.catch(error => error)
    const close = jest.spyOn(CoreBoundedStream.prototype, 'close').mockResolvedValueOnce(failedWatchCleanup())
    try {
      controller.abort()
      const error = await outcome
      expect(error).toBeInstanceOf(AggregateError)
      expect(error.errors).toHaveLength(2)
      expect(error.errors[0]).toMatchObject({ code: 'operation.aborted' })
      expect(error.errors[1]).toMatchObject({ name: 'BleCleanupError' })
      expect(read.mock.calls[0][0].signal.aborted).toBe(true)
    } finally {
      close.mockRestore()
    }
  })

  test('value-wait cancellation preserves failed cleanup', async () => {
    const read = jest.fn().mockResolvedValue(adapterState({ power: 'off' }))
    const manager = managerFor(read)
    const controller = new AbortController()
    const ready = manager.adapter.waitUntilReady({ signal: controller.signal })
    const outcome = ready.catch(error => error)
    await jest.advanceTimersByTimeAsync(100)
    const close = jest.spyOn(CoreBoundedStream.prototype, 'close').mockResolvedValueOnce(failedWatchCleanup())
    try {
      controller.abort()
      const error = await outcome
      expect(error).toBeInstanceOf(AggregateError)
      expect(error.errors[0]).toMatchObject({ code: 'operation.aborted' })
      expect(error.errors[1]).toMatchObject({ name: 'BleCleanupError' })
    } finally {
      close.mockRestore()
    }
  })

  test.each([
    [{ availability: 'unsupported' }, 'capability.unsupported'],
    [{ power: 'unsupported' }, 'capability.unsupported'],
    [{ authorization: 'denied' }, 'permission.denied'],
    [{ authorization: 'restricted' }, 'permission.denied'],
    [{ authorization: 'unavailable' }, 'permission.denied']
  ])('preserves state readiness error semantics for %j', async (state, code) => {
    const read = jest.fn().mockResolvedValue(adapterState(state))
    await expect(managerFor(read).adapter.waitUntilReady()).rejects.toMatchObject({ code })
    expect(read.mock.calls[0][0].signal.aborted).toBe(true)
  })

  test('invalid options retain the public error boundary', async () => {
    const read = jest.fn()
    await expect(managerFor(read).adapter.waitUntilReady({ timeoutMs: -1 })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(read).not.toHaveBeenCalled()
  })

  test('observes a ready transition on the shared 500 ms readiness cadence', async () => {
    const read = jest
      .fn()
      .mockResolvedValue(adapterState())
      .mockResolvedValueOnce(adapterState({ power: 'off' }))
    const ready = managerFor(read).adapter.waitUntilReady({ timeoutMs: 2000 })
    await jest.advanceTimersByTimeAsync(499)
    expect(read).toHaveBeenCalledTimes(1)
    await jest.advanceTimersByTimeAsync(1)
    await expect(ready).resolves.toMatchObject({ power: 'on' })
    expect(read).toHaveBeenCalledTimes(2)
  })
  test('a deadline shorter than the cadence does not issue a late route', async () => {
    const read = jest.fn().mockResolvedValue(adapterState({ power: 'off' }))
    const ready = managerFor(read).adapter.waitUntilReady({ timeoutMs: 200 })
    const rejected = expect(ready).rejects.toMatchObject({ code: 'operation.timed-out' })
    await jest.advanceTimersByTimeAsync(200)
    await rejected
    expect(read).toHaveBeenCalledTimes(1)
  })
  test('abort interrupts a pending cadence wait without another route', async () => {
    const read = jest.fn().mockResolvedValue(adapterState({ power: 'off' }))
    const controller = new AbortController()
    const ready = managerFor(read).adapter.waitUntilReady({ signal: controller.signal })
    const rejected = expect(ready).rejects.toMatchObject({ code: 'operation.aborted' })
    await jest.advanceTimersByTimeAsync(100)
    controller.abort()
    await rejected
    await jest.advanceTimersByTimeAsync(1000)
    expect(read).toHaveBeenCalledTimes(1)
  })
})

describe('IPC watch cleanup retry', () => {
  test('retains an unsuccessful stop for explicit retry without detaching a replacement owner', async () => {
    jest.useFakeTimers()
    const read = jest.fn().mockResolvedValue(adapterState())
    const manager = new IpcPublicManagerAdapter({
      bootstrap: ipcBootstrap(),
      capabilities: ipcCapabilities(),
      adapterState: read
    })
    const old = await manager.adapter.watchState()
    const close = jest.spyOn(CoreBoundedStream.prototype, 'close').mockResolvedValueOnce(failedWatchCleanup())
    try {
      await expect(old.stop()).resolves.toMatchObject({ state: 'release-failed' })
      const next = await manager.adapter.watchState()
      await expect(old.stop()).resolves.toMatchObject({ state: 'released' })
      await jest.advanceTimersByTimeAsync(500)
      expect(read).toHaveBeenCalledTimes(3)
      await next.stop()
      expect(close).toHaveBeenCalledTimes(3)
    } finally {
      close.mockRestore()
      jest.useRealTimers()
    }
  })
})
