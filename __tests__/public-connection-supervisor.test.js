const { createConnectionSupervisor } = require('../src/public/connection-supervisor')
const { BleError } = require('../src/public/errors')

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

  test('projects cleanup embedded in supervisor events into an immutable public snapshot', async () => {
    const current = connection()
    const cleanup = {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'connection',
          error: {
            code: 'platform.failure',
            domain: 'cleanup',
            operation: 'supervisor-event.cleanup',
            platform: {
              domain: 'native',
              code: 'E_CLEANUP',
              safeMessage: 'cleanup failed',
              metadata: { nested: { bytes: new Uint8Array([1, 2, 3]) } }
            },
            retryability: 'caller-decides'
          }
        }
      ]
    }
    current.release.mockResolvedValue(cleanup)
    const ble = manager(current)
    const supervisor = createConnectionSupervisor(ble, 'peer-1', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0, maximumAttempts: 1 }
    })
    const events = supervisor.events[Symbol.asyncIterator]()

    supervisor.start()
    await wait()
    await supervisor.stop()

    let cleanupEvent
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const next = await events.next()
      if (next.done) break
      if (next.value.kind === 'value' && next.value.value.cleanup !== undefined) {
        cleanupEvent = next.value.value
        break
      }
    }
    expect(cleanupEvent).toBeDefined()
    const projected = cleanupEvent.cleanup
    expect(projected).toMatchObject({ state: 'release-failed' })
    expect(projected).not.toBe(cleanup)
    expect(projected.failures[0].error.platform.metadata.nested.bytes).not.toBe(
      cleanup.failures[0].error.platform.metadata.nested.bytes
    )
    expect(Object.isFrozen(projected)).toBe(true)
    expect(Object.isFrozen(projected.failures[0].error.platform.metadata.nested)).toBe(true)
    cleanup.failures[0].error.platform.metadata.nested.bytes[0] = 9
    expect(projected.failures[0].error.platform.metadata.nested.bytes[0]).toBe(1)
    await events.return()
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

  test('releases a connected generation before reconnecting after immediate pause and resume', async () => {
    const first = connection()
    const second = connection()
    const ble = manager(first)
    ble.connect.mockResolvedValueOnce(first).mockResolvedValueOnce(second)
    const supervisor = createConnectionSupervisor(ble, 'peer-pause-resume', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0 }
    })

    supervisor.start()
    await wait(5)
    await supervisor.pause('handoff')
    supervisor.resume()
    await wait(5)

    expect(ble.connect).toHaveBeenCalledTimes(2)
    expect(first.release).toHaveBeenCalledTimes(1)
    expect(first.release.mock.invocationCallOrder[0]).toBeLessThan(ble.connect.mock.invocationCallOrder[1])
    await supervisor.stop()
  })

  test('arbitrates a string peer and equivalent referenced peer as one supervisor', () => {
    const ble = manager(connection())
    createConnectionSupervisor(ble, 'peer-equivalent', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 }
    })
    expect(() =>
      createConnectionSupervisor(
        ble,
        {
          id: 'peer-equivalent',
          name: null,
          rssi: null,
          reference: null,
          sources: [],
          lastAdvertisement: null
        },
        { retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 } }
      )
    ).toThrow('connection.already-owned')

    const bleReference = manager(connection())
    const reference = { version: 1, backendId: 'test', scope: 'system', opaqueId: 'canonical-peer' }
    createConnectionSupervisor(bleReference, reference, {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 }
    })
    expect(() =>
      createConnectionSupervisor(
        bleReference,
        {
          id: 'display-name',
          name: 'Display name',
          rssi: null,
          reference,
          sources: [],
          lastAdvertisement: null
        },
        { retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 } }
      )
    ).toThrow('connection.already-owned')
  })

  test('waits for a paused configure cleanup before starting a successor generation', async () => {
    const first = connection()
    const second = connection()
    const configurePending = deferred()
    let configureCalls = 0
    const ble = manager(first)
    ble.connect.mockResolvedValueOnce(first).mockResolvedValueOnce(second)
    const supervisor = createConnectionSupervisor(ble, 'peer-configure-generation', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0 },
      configure: () => {
        configureCalls += 1
        return configureCalls === 1 ? configurePending.promise : Promise.resolve('successor-session')
      },
      disposeSession: jest.fn(async () => undefined)
    })

    supervisor.start()
    await wait(1)
    await supervisor.pause('configure-handoff')
    supervisor.resume()
    await wait(5)
    expect(ble.connect).toHaveBeenCalledTimes(1)

    configurePending.resolve('late-session')
    await wait(5)
    expect(ble.connect).toHaveBeenCalledTimes(2)
    await supervisor.stop()
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
    await expect(configureSupervisor.stop()).resolves.toMatchObject({ state: 'release-failed' })
    configurePending.resolve('late-session')
    await wait(5)
    await expect(configureSupervisor.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('pausing a pending gate keeps the supervisor resumable after the gate settles', async () => {
    const gatePending = deferred()
    const ble = manager(connection())
    const supervisor = createConnectionSupervisor(ble, 'peer-paused-gate', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      gate: () => gatePending.promise
    })

    supervisor.start()
    await wait(1)
    await supervisor.pause('user-requested')
    supervisor.resume()
    gatePending.resolve('allow')
    await wait(5)
    expect(ble.connect).toHaveBeenCalledTimes(1)
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('stop settles when pause has deferred a still-pending gate', async () => {
    const gatePending = deferred()
    const ble = manager(connection())
    const supervisor = createConnectionSupervisor(ble, 'peer-paused-stop', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      gate: () => gatePending.promise
    })

    supervisor.start()
    await wait(1)
    await supervisor.pause('stop-requested')
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('stops after a non-retryable connection error without another attempt', async () => {
    const ble = manager(null)
    ble.connect.mockRejectedValue(new BleError('permission.denied', 'connection', 'test.denied'))
    const supervisor = createConnectionSupervisor(ble, 'peer-denied', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 3 }
    })

    supervisor.start()
    await wait()

    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('stopped')
  })

  test('does not reconnect after a stale connection error', async () => {
    const ble = manager(null)
    ble.connect.mockRejectedValue(new BleError('connection.stale', 'connection', 'test.stale'))
    const supervisor = createConnectionSupervisor(ble, 'peer-stale', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 3 }
    })

    supervisor.start()
    await wait()

    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('stopped')
  })

  test('does not start another attempt after a backoff exhausts the elapsed retry budget', async () => {
    let now = 0
    const ble = manager(null)
    ble.connect.mockRejectedValue(new BleError('connection.failed', 'connection', 'test.elapsed'))
    const supervisor = createConnectionSupervisor(ble, 'peer-elapsed', {
      retry: { initialDelayMs: 1_000, maximumDelayMs: 1_000, multiplier: 1, jitter: 0, maximumElapsedMs: 100 },
      now: () => now,
      setTimeout: callback => {
        now = 200
        callback()
        return 1
      },
      clearTimeout: () => undefined
    })

    supervisor.start()
    await wait()

    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('stopped')
  })

  test('records a rejected asynchronous gate without an unhandled run rejection', async () => {
    const supervisor = createConnectionSupervisor(manager(connection()), 'peer-gate-rejection', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      gate: async () => {
        throw new Error('gate failed')
      }
    })

    supervisor.start()
    await wait()

    expect(supervisor.snapshot.state).toBe('stopped')
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not reconnect after configure fails with a released connection', async () => {
    const current = connection()
    const ble = manager(current)
    const supervisor = createConnectionSupervisor(ble, 'peer-configure-failure', {
      retry: { initialDelayMs: 0, maximumDelayMs: 0, multiplier: 1, jitter: 0, maximumAttempts: 3 },
      configure: async () => {
        throw new Error('configuration failed')
      }
    })

    supervisor.start()
    await wait()

    expect(ble.connect).toHaveBeenCalledTimes(1)
    expect(supervisor.snapshot.state).toBe('stopped')
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
  })

  test('retains a late configure-session disposal failure after stop finalized', async () => {
    const configurePending = deferred()
    const connectionValue = connection()
    let disposalAttempts = 0
    const supervisor = createConnectionSupervisor(manager(connectionValue), 'peer-late-session', {
      retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
      configure: () => configurePending.promise,
      disposeSession: async () => {
        disposalAttempts += 1
        if (disposalAttempts === 1) throw new Error('late session disposal failed')
      }
    })
    supervisor.start()
    await wait(5)
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'release-failed' })
    configurePending.resolve('late-session')
    await wait(5)
    expect(supervisor.snapshot.state).toBe('cleanup-failed')
    expect(supervisor.snapshot.lastError).toMatchObject({ code: 'connection.failed' })
    expect(connectionValue.release).toHaveBeenCalled()
    await expect(supervisor.stop()).resolves.toMatchObject({ state: 'released' })
    expect(disposalAttempts).toBe(2)
  })
})
