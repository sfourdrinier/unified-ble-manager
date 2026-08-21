const { createConnectionSupervisor } = require('../src/public/connection-supervisor')

function lifecycleEvents() {
  let closed = false
  let resolveNext
  const queue = [
    {
      kind: 'connection-lifecycle',
      previous: 'connecting',
      current: 'connected',
      cause: 'connected',
      connectionGeneration: 'generation-1',
      sequence: 1
    }
  ]
  return {
    [Symbol.asyncIterator]() {
      return {
        next: async () => {
          if (queue.length > 0) return { done: false, value: queue.shift() }
          if (closed) return { done: true, value: undefined }
          return new Promise(resolve => {
            resolveNext = resolve
          })
        },
        return: async () => {
          closed = true
          if (resolveNext !== undefined) resolveNext({ done: true, value: undefined })
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

function connection() {
  return {
    peer: { id: 'peer-1', name: null, rssi: null, reference: null, sources: [], lastAdvertisement: null },
    lifecycleEvents: lifecycleEvents(),
    release: jest.fn(async () => ({ state: 'released', failures: [] })),
    disconnect: jest.fn(async () => ({ state: 'released', failures: [] })),
    discover: jest.fn(async () => undefined)
  }
}

function manager(connectionValue) {
  const adapterState = jest.fn(async () => ({
    availability: 'available',
    authorization: 'granted',
    power: 'on',
    backendGeneration: '1',
    updatedAt: 1,
    safeReason: null
  }))
  return {
    adapter: {
      state: adapterState,
      waitUntilReady: jest.fn(async () => adapterState())
    },
    connect: jest.fn(async () => connectionValue),
    capabilities: { supports: () => false },
    peers: { resolve: jest.fn(async () => null) }
  }
}

function wait(milliseconds = 10) {
  return new Promise(resolve => setTimeout(resolve, milliseconds))
}

function deferred() {
  let resolve
  let reject
  const promise = new Promise((resolveValue, rejectValue) => {
    resolve = resolveValue
    reject = rejectValue
  })
  return { promise, resolve, reject }
}

describe('public connection supervisor', () => {
  test('is opt-in and runs one configure callback per connected generation', async () => {
    const current = connection()
    const ble = manager(current)
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0, maximumAttempts: 1 },
      configure: async () => 'session-1'
    })

    expect(ble.connect).not.toHaveBeenCalled()
    supervisor.start()
    await new Promise(resolve => setTimeout(resolve, 10))

    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('connected')
    expect(supervisor.snapshot.session).toBe('session-1')
    await supervisor.stop()
    expect(current.release).toHaveBeenCalledTimes(1)
  })

  test('rejects invalid retry policy before starting work', () => {
    const ble = manager(connection())
    expect(() =>
      createConnectionSupervisor(ble, 'peer-1', {
        retry: { initialDelayMs: -1, maximumDelayMs: 1, multiplier: 1, jitter: 0 }
      })
    ).toThrow('argument.invalid')
    expect(ble.connect).not.toHaveBeenCalled()
  })

  test('finalizes ownership after natural retry exhaustion', async () => {
    const ble = manager(null)
    ble.connect.mockRejectedValue(new Error('unavailable'))
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 1 }
    })

    supervisor.start()
    await wait()

    expect(supervisor.snapshot.state).toBe('stopped')
    expect(() =>
      createConnectionSupervisor(ble, 'peer-1', {
        retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 1 }
      })
    ).not.toThrow()
  })

  test('attempts connection release even when lifecycle iterator cleanup fails', async () => {
    const current = connection()
    const iterator = current.lifecycleEvents[Symbol.asyncIterator]()
    iterator.return = jest.fn(async () => {
      throw new Error('iterator cleanup failed')
    })
    current.lifecycleEvents = { [Symbol.asyncIterator]: () => iterator }
    const ble = manager(current)
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0, maximumAttempts: 1 }
    })

    supervisor.start()
    await wait()
    const cleanup = await supervisor.stop()

    expect(cleanup.state).toBe('release-failed')
    expect(current.release).toHaveBeenCalledTimes(1)
  })

  test('retains a late connection when cancellation cleanup fails', async () => {
    const late = connection()
    late.release.mockRejectedValue(new Error('late release failed'))
    const pending = deferred()
    const ble = manager(late)
    ble.connect.mockReturnValue(pending.promise)
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0, maximumAttempts: 1 }
    })

    supervisor.start()
    await wait(1)
    const stopping = supervisor.stop()
    pending.resolve(late)
    const cleanup = await stopping

    expect(cleanup.state).toBe('release-failed')
    expect(late.release).toHaveBeenCalled()
  })

  test('does not retry after configure cleanup fails', async () => {
    const current = connection()
    current.release.mockResolvedValue({
      state: 'release-failed',
      failures: [{ resourceKind: 'connection', error: { code: 'platform.failure' } }]
    })
    const ble = manager(current)
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 3 },
      configure: async () => {
        throw new Error('configure failed')
      }
    })

    supervisor.start()
    await wait()
    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('cleanup-failed')
  })

  test('stop settles while adapter state, gate, or configure is pending', async () => {
    const statePending = deferred()
    const stateBle = manager(connection())
    stateBle.adapter.state.mockReturnValue(statePending.promise)
    const stateSupervisor = createConnectionSupervisor(stateBle, 'peer-state', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 }
    })
    stateSupervisor.start()
    await wait(1)
    await expect(stateSupervisor.stop()).resolves.toMatchObject({ state: 'released' })
    statePending.resolve({
      availability: 'available',
      authorization: 'granted',
      power: 'on',
      backendGeneration: '1',
      updatedAt: 1,
      safeReason: null
    })

    const gatePending = deferred()
    const gateBle = manager(connection())
    const gateSupervisor = createConnectionSupervisor(gateBle, 'peer-gate', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      gate: () => gatePending.promise
    })
    gateSupervisor.start()
    await wait(1)
    await expect(gateSupervisor.stop()).resolves.toMatchObject({ state: 'released' })
    gatePending.resolve('allow')

    const configurePending = deferred()
    const configureConnection = connection()
    const configureBle = manager(configureConnection)
    const configureSupervisor = createConnectionSupervisor(configureBle, 'peer-configure', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      configure: () => configurePending.promise
    })
    configureSupervisor.start()
    await wait(5)
    await expect(configureSupervisor.stop()).resolves.toMatchObject({ state: 'released' })
    configurePending.resolve('late-session')
  })

  test('retains a late configure-session disposal failure after stop finalized', async () => {
    const configurePending = deferred()
    const connectionValue = connection()
    const supervisor = createConnectionSupervisor(manager(connectionValue), 'peer-late-session', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      configure: () => configurePending.promise,
      disposeSession: async () => {
        throw new Error('late session disposal failed')
      }
    })
    supervisor.start()
    await wait(5)
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
    configurePending.resolve('late-session')
    await wait(5)
    expect(supervisor.snapshot.state).toBe('cleanup-failed')
    expect(supervisor.snapshot.lastError).toMatchObject({ code: 'connection.failed' })
    expect(connectionValue.release).toHaveBeenCalled()
  })
})
