const React = require('react')

const hookHarness = {
  contextValue: null,
  errorContextValue: null,
  contexts: [],
  effects: [],
  stateValues: [],
  stateIndex: 0,
  effectIndex: 0,
  refs: [],
  refIndex: 0,
  memoValues: [],
  memoDependencies: [],
  memoIndex: 0,
  externalStores: [],
  externalStoreIndex: 0,
  reset() {
    this.effects = []
    this.stateIndex = 0
    this.effectIndex = 0
    this.refs = []
    this.refIndex = 0
    this.memoValues = []
    this.memoDependencies = []
    this.memoIndex = 0
    this.externalStores = []
    this.externalStoreIndex = 0
  },
  rerender() {
    this.effects = []
    this.stateIndex = 0
    this.effectIndex = 0
    this.refIndex = 0
    this.memoIndex = 0
    this.externalStoreIndex = 0
  },
  nextState(initialValue) {
    const index = this.stateIndex++
    if (!(index in this.stateValues)) {
      this.stateValues[index] = typeof initialValue === 'function' ? initialValue() : initialValue
    }
    return [
      this.stateValues[index],
      value => {
        this.stateValues[index] = typeof value === 'function' ? value(this.stateValues[index]) : value
      }
    ]
  },
  nextRef(initialValue) {
    const index = this.refIndex++
    if (!(index in this.refs)) this.refs[index] = { current: initialValue }
    return this.refs[index]
  },
  nextMemo(factory, dependencies) {
    const index = this.memoIndex++
    const previousDependencies = this.memoDependencies[index]
    const changed =
      previousDependencies === undefined ||
      dependencies.length !== previousDependencies.length ||
      dependencies.some((dependency, dependencyIndex) => !Object.is(dependency, previousDependencies[dependencyIndex]))
    if (changed) {
      this.memoValues[index] = factory()
      this.memoDependencies[index] = dependencies
    }
    return this.memoValues[index]
  },
  nextEffect(effect) {
    const index = this.effectIndex++
    this.effects[index] = effect
  },
  nextExternalStore(subscribe, getSnapshot, getServerSnapshot) {
    const index = this.externalStoreIndex++
    const record = { subscribe, getSnapshot, getServerSnapshot }
    this.externalStores[index] = record
    return getSnapshot()
  }
}

jest.mock('react', () => ({
  createContext: jest.fn(defaultValue => {
    const context = { defaultValue, Provider: Symbol('Provider') }
    hookHarness.contexts.push(context)
    return context
  }),
  createElement: jest.fn((type, props, children) => ({ type, props, children })),
  useContext: jest.fn(context =>
    context === hookHarness.contexts[1] ? hookHarness.errorContextValue : hookHarness.contextValue
  ),
  useEffect: jest.fn(effect => hookHarness.nextEffect(effect)),
  useMemo: jest.fn((factory, dependencies) => hookHarness.nextMemo(factory, dependencies)),
  useRef: jest.fn(initialValue => hookHarness.nextRef(initialValue)),
  useState: jest.fn(initialValue => hookHarness.nextState(initialValue)),
  useSyncExternalStore: jest.fn((subscribe, getSnapshot, getServerSnapshot) =>
    hookHarness.nextExternalStore(subscribe, getSnapshot, getServerSnapshot)
  )
}))

const {
  BleProvider,
  getAdapterState,
  getBleCapability,
  useAdapterState,
  useBle,
  useBleCapability,
  useBleReadiness,
  useCharacteristicValue,
  useConnectionState,
  useDiscoveredPeers,
  inspectReactAdapterWatchOwnershipForTests
} = require('../src/react')

function deferred() {
  let resolve
  let reject
  const promise = new Promise((promiseResolve, promiseReject) => {
    resolve = promiseResolve
    reject = promiseReject
  })
  return { promise, resolve, reject }
}

function adapterState(overrides = {}) {
  return {
    availability: 'available',
    power: 'on',
    authorization: 'granted',
    backendGeneration: 'backend-1',
    updatedAt: 1,
    safeReason: null,
    ...overrides
  }
}

function adapterWatch(initial, values) {
  return {
    initial,
    values,
    stop: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
  }
}

function manager(overrides = {}) {
  return {
    adapter: {
      state: jest.fn().mockResolvedValue({ availability: 'available', power: 'on', authorization: 'granted' }),
      watchState: jest.fn().mockResolvedValue(
        adapterWatch(
          adapterState(),
          (async function* () {
            await new Promise(() => undefined)
          })()
        )
      ),
      ...overrides.adapter
    },
    capabilities: {
      get: jest.fn().mockReturnValue({ id: 'scan', state: 'supported', limitations: [] }),
      ...overrides.capabilities
    },
    destroy: jest.fn().mockResolvedValue({ state: 'released', failures: [] }),
    ...overrides
  }
}

function scanSession(
  observations = (async function* () {
    yield { kind: 'terminal' }
  })(),
  events
) {
  return {
    observations,
    ...(events === undefined ? {} : { events }),
    stop: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
  }
}

function discoveredPeer(id, overrides = {}) {
  return {
    id,
    name: overrides.name ?? null,
    rssi: overrides.rssi ?? -50,
    reference: null,
    sources: [],
    lastAdvertisement: overrides.lastAdvertisement ?? null
  }
}

function observationItem(id, overrides = {}) {
  const peer = discoveredPeer(id, overrides)
  return {
    kind: 'value',
    value: {
      peer,
      observedAtMonotonicMs: 1,
      localName: peer.name,
      rssi: peer.rssi,
      connectable: true,
      serviceUuids: [],
      manufacturerData: peer.lastAdvertisement?.manufacturerData ?? null,
      serviceData: peer.lastAdvertisement?.serviceData ?? null
    }
  }
}

function observedEvent(id, overrides = {}) {
  return { kind: 'observed', peer: discoveredPeer(id, overrides) }
}

function lostEvent(id) {
  return {
    kind: 'lost',
    peer: discoveredPeer(id),
    lastObservedAt: 1,
    derivedAt: 2,
    reason: 'observation-timeout'
  }
}

function richAdvertisement(byteLength) {
  return {
    localName: null,
    rssi: -50,
    connectable: true,
    serviceUuids: [],
    manufacturerData: [{ companyId: 1, data: new Uint8Array(byteLength) }],
    serviceData: []
  }
}

function createControllableAsyncIterator() {
  const waiters = []
  const queued = []
  let finished = false
  let failure = null
  const iterator = {
    next() {
      if (failure !== null) return Promise.reject(failure)
      if (queued.length > 0) return Promise.resolve(queued.shift())
      if (finished) return Promise.resolve({ done: true, value: undefined })
      return new Promise((resolve, reject) => {
        waiters.push({ resolve, reject })
      })
    },
    return: jest.fn(async () => {
      finished = true
      while (waiters.length > 0) {
        waiters.shift().resolve({ done: true, value: undefined })
      }
      return { done: true, value: undefined }
    }),
    [Symbol.asyncIterator]() {
      return this
    }
  }
  return {
    iterable: iterator,
    iterator,
    returnFn: iterator.return,
    push(value) {
      const result = { done: false, value }
      if (waiters.length > 0) waiters.shift().resolve(result)
      else queued.push(result)
    },
    end() {
      finished = true
      if (waiters.length > 0) {
        while (waiters.length > 0) waiters.shift().resolve({ done: true, value: undefined })
      } else queued.push({ done: true, value: undefined })
    },
    fail(error) {
      failure = error
      while (waiters.length > 0) waiters.shift().reject(error)
    }
  }
}

function peerIds(result = hookHarness.stateValues[0]) {
  return result.peers.map(peer => peer.id)
}

function characteristicSubscription(
  values = (async function* () {
    yield { kind: 'terminal' }
  })()
) {
  return {
    values,
    remove: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
  }
}

function overflowNotice() {
  return {
    kind: 'overflow',
    policy: 'drop-oldest',
    droppedItems: 1,
    droppedBytes: 2,
    replacedItems: 0
  }
}

function connectionWithState(current) {
  let emitted = false
  const iterator = {
    next: jest.fn(() => {
      if (emitted) return new Promise(() => undefined)
      emitted = true
      return Promise.resolve({
        done: false,
        value: {
          kind: 'connection-lifecycle',
          previous: 'connecting',
          current,
          cause: 'caller',
          connectionGeneration: `${current}-generation`,
          sequence: 1
        }
      })
    }),
    return: jest.fn().mockResolvedValue({ done: true })
  }
  return {
    iterator,
    connection: { lifecycleEvents: { [Symbol.asyncIterator]: () => iterator } }
  }
}

async function flush() {
  for (let index = 0; index < 5; index += 1) await Promise.resolve()
  await new Promise(resolve => setImmediate(resolve))
}

async function flushMany(count = 400) {
  for (let index = 0; index < count; index += 1) await Promise.resolve()
  await new Promise(resolve => setImmediate(resolve))
}

describe('React host surface', () => {
  beforeEach(() => {
    hookHarness.contextValue = null
    hookHarness.errorContextValue = null
    hookHarness.stateValues = []
    hookHarness.reset()
    jest.clearAllMocks()
  })

  test('shares one manager creation promise and protects a Strict Mode effect cleanup', async () => {
    const creation = deferred()
    const createdManager = manager()
    const createManager = jest.fn(() => creation.promise)

    BleProvider({ createManager, children: null })
    const firstCleanup = hookHarness.effects[0]()
    firstCleanup()
    const secondCleanup = hookHarness.effects[0]()

    expect(createManager).toHaveBeenCalledTimes(1)

    creation.resolve(createdManager)
    await flush()
    expect(createdManager.destroy).not.toHaveBeenCalled()

    secondCleanup()
    await flush()
    expect(createdManager.destroy).toHaveBeenCalledTimes(1)
    expect(firstCleanup).not.toBe(secondCleanup)
  })

  test('waits for a replaced provider to release before creating its replacement manager', async () => {
    const destruction = deferred()
    const firstManager = manager({ destroy: jest.fn(() => destruction.promise) })
    const secondManager = manager()
    const firstCreateManager = jest.fn().mockResolvedValue(firstManager)
    const secondCreateManager = jest.fn().mockResolvedValue(secondManager)

    BleProvider({ createManager: firstCreateManager, children: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()

    hookHarness.stateValues = []
    hookHarness.reset()
    BleProvider({ createManager: secondCreateManager, children: null })
    const secondEffect = hookHarness.effects[0]

    firstCleanup()
    secondEffect()
    expect(secondCreateManager).not.toHaveBeenCalled()

    await flush()
    expect(firstManager.destroy).toHaveBeenCalledTimes(1)
    expect(secondCreateManager).not.toHaveBeenCalled()

    destruction.resolve({ state: 'released', failures: [] })
    await flush()
    expect(secondCreateManager).toHaveBeenCalledTimes(1)
  })

  test('keeps a replacement blocked after release-failed cleanup until an explicit retry succeeds', async () => {
    const firstManager = manager({
      destroy: jest
        .fn()
        .mockResolvedValueOnce({ state: 'release-failed', failures: [{ resourceKind: 'manager' }] })
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    })
    const secondManager = manager()
    const firstCreateManager = jest.fn().mockResolvedValue(firstManager)
    const secondCreateManager = jest.fn().mockResolvedValue(secondManager)
    const onError = jest.fn()

    BleProvider({ createManager: firstCreateManager, onError, children: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()

    hookHarness.stateValues = []
    hookHarness.reset()
    BleProvider({ createManager: secondCreateManager, children: null })
    const secondEffect = hookHarness.effects[0]

    firstCleanup()
    secondEffect()
    await flush()
    const replacementCallsBeforeRetry = secondCreateManager.mock.calls.length

    firstCleanup()
    await flush()

    expect(replacementCallsBeforeRetry).toBe(0)
    expect(firstManager.destroy).toHaveBeenCalledTimes(2)
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ cleanup: expect.any(Object) }))
    expect(secondCreateManager).toHaveBeenCalledTimes(1)
  })

  test('a replacement provider retries a failed prior release without a second old cleanup', async () => {
    const firstManager = manager({
      destroy: jest
        .fn()
        .mockResolvedValueOnce({ state: 'release-failed', failures: [{ resourceKind: 'manager' }] })
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    })
    const secondManager = manager()
    const firstCreateManager = jest.fn().mockResolvedValue(firstManager)
    const secondCreateManager = jest.fn().mockResolvedValue(secondManager)
    const onError = jest.fn()

    BleProvider({ createManager: firstCreateManager, onError, children: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    firstCleanup()
    await flush()

    hookHarness.stateValues = []
    hookHarness.reset()
    BleProvider({ createManager: secondCreateManager, children: null })
    hookHarness.effects[0]()
    await flush()

    expect(firstManager.destroy).toHaveBeenCalledTimes(2)
    expect(secondCreateManager).toHaveBeenCalledTimes(1)
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ cleanup: expect.any(Object) }))
  })

  test('keeps a replacement blocked after rejected cleanup until an explicit retry succeeds', async () => {
    const destructionError = new Error('destroy failed')
    const firstManager = manager({
      destroy: jest
        .fn()
        .mockRejectedValueOnce(destructionError)
        .mockResolvedValueOnce({ state: 'released', failures: [] })
    })
    const secondManager = manager()
    const firstCreateManager = jest.fn().mockResolvedValue(firstManager)
    const secondCreateManager = jest.fn().mockResolvedValue(secondManager)
    const onError = jest.fn()

    BleProvider({ createManager: firstCreateManager, onError, children: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()

    hookHarness.stateValues = []
    hookHarness.reset()
    BleProvider({ createManager: secondCreateManager, children: null })
    const secondEffect = hookHarness.effects[0]

    firstCleanup()
    secondEffect()
    await flush()
    const replacementCallsBeforeRetry = secondCreateManager.mock.calls.length

    firstCleanup()
    await flush()

    expect(replacementCallsBeforeRetry).toBe(0)
    expect(firstManager.destroy).toHaveBeenCalledTimes(2)
    expect(onError).toHaveBeenCalledWith(destructionError)
    expect(secondCreateManager).toHaveBeenCalledTimes(1)
  })

  test('destroys a manager that resolves after provider unmount and awaits destroy', async () => {
    const creation = deferred()
    const destruction = deferred()
    const createdManager = manager({ destroy: jest.fn(() => destruction.promise) })
    const onError = jest.fn()

    BleProvider({ createManager: () => creation.promise, onError, children: null })
    const cleanup = hookHarness.effects[0]()
    cleanup()
    await flush()

    creation.resolve(createdManager)
    await flush()
    expect(createdManager.destroy).toHaveBeenCalledTimes(1)
    expect(onError).not.toHaveBeenCalled()

    expect(destruction).toBeDefined()
    destruction.resolve({ state: 'released', failures: [] })
    await flush()
    expect(onError).not.toHaveBeenCalled()
  })

  test('reports creation and awaited destruction failures through onError', async () => {
    const creationError = new Error('create failed')
    const onError = jest.fn()
    const createManager = jest.fn().mockRejectedValue(creationError)

    BleProvider({ createManager, onError, children: null })
    hookHarness.effects[0]()
    await flush()
    expect(onError).toHaveBeenCalledWith(creationError)

    hookHarness.stateValues = []
    hookHarness.reset()
    const createdManager = manager({ destroy: jest.fn().mockRejectedValue(new Error('destroy failed')) })
    const destroyOnError = jest.fn()
    BleProvider({ createManager: () => Promise.resolve(createdManager), onError: destroyOnError, children: null })
    const cleanup = hookHarness.effects[0]()
    cleanup()
    await flush()
    expect(createdManager.destroy).toHaveBeenCalledTimes(1)
    expect(destroyOnError).toHaveBeenCalledWith(expect.objectContaining({ message: 'destroy failed' }))
  })

  test('provides useBle adapter state and capability hooks plus imperative equivalents', async () => {
    const currentAdapterState = adapterState()
    const capability = { id: 'scan', state: 'supported', limitations: [] }
    const createdManager = manager({
      adapter: {
        state: jest.fn().mockResolvedValue(currentAdapterState),
        watchState: jest.fn().mockResolvedValue(
          adapterWatch(
            currentAdapterState,
            (async function* () {
              await new Promise(() => undefined)
            })()
          )
        )
      },
      capabilities: { get: jest.fn().mockReturnValue(capability) }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    expect(useBle()).toEqual({ manager: createdManager, loading: false, error: null })
    expect(useBleCapability('scan')).toBe(capability)
    expect(getBleCapability(createdManager, 'scan')).toBe(capability)
    await expect(getAdapterState(createdManager)).resolves.toBe(currentAdapterState)

    hookHarness.reset()
    const adapterResult = useAdapterState()
    expect(adapterResult).toEqual({ state: null, loading: true, error: null })
    const store = hookHarness.externalStores[0]
    expect(store.getServerSnapshot()).toBe(store.getServerSnapshot())
    const cleanup = store.subscribe(jest.fn())
    await flush()
    expect(store.getSnapshot()).toEqual({ state: currentAdapterState, loading: false, error: null })
    cleanup()
  })

  test('resets readiness when the provider manager is replaced', async () => {
    const firstManager = manager({
      readiness: jest.fn().mockResolvedValue({ state: 'ready', adapter: adapterState(), actions: [] }),
      adapter: {
        watchState: jest.fn().mockResolvedValue(
          adapterWatch(
            adapterState(),
            (async function* () {
              await new Promise(() => undefined)
            })()
          )
        )
      }
    })
    const secondManager = manager({
      readiness: jest
        .fn()
        .mockResolvedValue({ state: 'ready', adapter: adapterState({ backendGeneration: 'backend-2' }), actions: [] }),
      adapter: {
        watchState: jest.fn().mockResolvedValue(
          adapterWatch(
            adapterState({ backendGeneration: 'backend-2' }),
            (async function* () {
              await new Promise(() => undefined)
            })()
          )
        )
      }
    })

    hookHarness.contextValue = { manager: firstManager, loading: false, error: null }
    expect(useBleReadiness()).toEqual({ readiness: null, loading: true, error: null })
    const firstStore = hookHarness.externalStores[0]
    const firstCleanup = firstStore.subscribe(jest.fn())
    await flush()
    expect(firstStore.getSnapshot()).toMatchObject({ readiness: { state: 'ready' }, loading: false, error: null })

    firstCleanup()
    hookHarness.rerender()
    hookHarness.contextValue = { manager: secondManager, loading: false, error: null }
    expect(useBleReadiness()).toEqual({ readiness: null, loading: true, error: null })
    const secondStore = hookHarness.externalStores[0]
    const secondCleanup = secondStore.subscribe(jest.fn())
    await flush()
    expect(secondStore.getSnapshot()).toMatchObject({ readiness: { state: 'ready' }, loading: false, error: null })
    secondCleanup()
  })

  test('seeds the initial adapter snapshot, shares one watch, and stops after the final unsubscribe', async () => {
    const initial = adapterState()
    const next = deferred()
    const watch = adapterWatch(
      initial,
      (async function* () {
        yield { kind: 'value', value: await next.promise }
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(watch) } })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useAdapterState()
    const firstStore = hookHarness.externalStores[0]
    hookHarness.rerender()
    useAdapterState()
    const secondStore = hookHarness.externalStores[0]
    const firstListener = jest.fn()
    const secondListener = jest.fn()
    const firstCleanup = firstStore.subscribe(firstListener)
    const secondCleanup = secondStore.subscribe(secondListener)

    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(1)
    expect(firstStore.getSnapshot().state).toBe(initial)
    expect(secondStore.getSnapshot()).toBe(firstStore.getSnapshot())

    const updated = adapterState({ updatedAt: 2 })
    next.resolve(updated)
    await flush()
    expect(firstStore.getSnapshot().state).toBe(updated)
    expect(secondStore.getSnapshot()).toBe(firstStore.getSnapshot())

    firstCleanup()
    await flush()
    expect(watch.stop).not.toHaveBeenCalled()
    secondCleanup()
    await flush()
    expect(watch.stop).toHaveBeenCalledTimes(1)
  })

  test('reports a watch cleanup release failure when the final subscriber leaves', async () => {
    const cleanup = { state: 'release-failed', failures: [new Error('watch cleanup failed')] }
    const watch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    watch.stop.mockResolvedValue(cleanup)
    const onError = jest.fn()
    hookHarness.errorContextValue = onError
    const createdManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(watch) } })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    unsubscribe()
    await flush()

    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ cleanup }))
  })

  test('suppresses transitions from a stopped manager watch after replacement', async () => {
    const staleTransition = deferred()
    const staleWatch = adapterWatch(
      adapterState(),
      (async function* () {
        yield { kind: 'value', value: await staleTransition.promise }
        await new Promise(() => undefined)
      })()
    )
    const current = adapterState({ backendGeneration: 'backend-2' })
    const currentWatch = adapterWatch(
      current,
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const firstManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(staleWatch) } })
    const secondManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(currentWatch) } })

    hookHarness.contextValue = { manager: firstManager, loading: false, error: null }
    useAdapterState()
    const firstStore = hookHarness.externalStores[0]
    const unsubscribeFirst = firstStore.subscribe(jest.fn())
    await flush()
    unsubscribeFirst()

    hookHarness.rerender()
    hookHarness.contextValue = { manager: secondManager, loading: false, error: null }
    useAdapterState()
    const secondStore = hookHarness.externalStores[0]
    const unsubscribeSecond = secondStore.subscribe(jest.fn())
    await flush()

    staleTransition.resolve(adapterState({ updatedAt: 99 }))
    await flush()
    expect(secondStore.getSnapshot().state).toBe(current)
    unsubscribeSecond()
  })

  test('refreshes capabilities only when the adapter backend generation changes', async () => {
    const ordinary = deferred()
    const regenerated = deferred()
    const firstCapability = { id: 'scan', state: 'supported', limitations: [] }
    const secondCapability = { id: 'scan', state: 'limited', limitations: ['restarted'] }
    const watch = adapterWatch(
      adapterState(),
      (async function* () {
        yield { kind: 'value', value: await ordinary.promise }
        yield { kind: 'value', value: await regenerated.promise }
        await new Promise(() => undefined)
      })()
    )
    const capabilityGet = jest.fn().mockReturnValueOnce(firstCapability).mockReturnValueOnce(secondCapability)
    const createdManager = manager({
      adapter: { watchState: jest.fn().mockResolvedValue(watch) },
      capabilities: { get: capabilityGet }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    expect(useBleCapability('scan')).toBe(firstCapability)
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(store.getSnapshot()).toBe(firstCapability)

    ordinary.resolve(adapterState({ updatedAt: 2 }))
    await flush()
    expect(capabilityGet).toHaveBeenCalledTimes(1)
    expect(store.getSnapshot()).toBe(firstCapability)

    regenerated.resolve(adapterState({ backendGeneration: 'backend-2', updatedAt: 3 }))
    await flush()
    expect(capabilityGet).toHaveBeenCalledTimes(2)
    expect(store.getSnapshot()).toBe(secondCapability)
    unsubscribe()
  })

  test('derives Expo readiness from the watched adapter snapshot and rejects non-Expo managers', async () => {
    const initial = adapterState()
    const updated = adapterState({ power: 'off', updatedAt: 2 })
    const next = deferred()
    const watch = adapterWatch(
      initial,
      (async function* () {
        yield { kind: 'value', value: await next.promise }
        await new Promise(() => undefined)
      })()
    )
    const readiness = jest
      .fn()
      .mockResolvedValueOnce({ state: 'ready', adapter: initial, actions: [] })
      .mockResolvedValueOnce({
        state: 'action-required',
        adapter: updated,
        actions: [{ kind: 'enable-bluetooth', systemUiOnly: true }]
      })
    const expoManager = manager({
      readiness,
      adapter: { watchState: jest.fn().mockResolvedValue(watch) }
    })
    hookHarness.contextValue = { manager: expoManager, loading: false, error: null }

    expect(useBleReadiness()).toEqual({ readiness: null, loading: true, error: null })
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(store.getSnapshot().readiness.adapter).toBe(initial)
    expect(readiness).toHaveBeenCalledTimes(1)

    next.resolve(updated)
    await flush()
    expect(store.getSnapshot().readiness.adapter).toBe(updated)
    expect(store.getSnapshot().readiness.state).toBe('action-required')
    expect(readiness).toHaveBeenCalledTimes(2)
    unsubscribe()

    hookHarness.rerender()
    const bareManager = manager()
    hookHarness.contextValue = { manager: bareManager, loading: false, error: null }
    expect(useBleReadiness()).toEqual({
      readiness: null,
      loading: false,
      error: new Error('BLE readiness is available only from an Expo host manager.')
    })
    expect(bareManager.adapter.watchState).not.toHaveBeenCalled()
  })

  test('preserves Expo runtime readiness policy in the external-store hook', async () => {
    const initial = adapterState()
    const watch = adapterWatch(
      initial,
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const expoManager = manager({
      readiness: jest.fn().mockResolvedValue({
        state: 'unavailable',
        adapter: initial,
        actions: [{ kind: 'rebuild-native-app', reason: 'legacy location is not configured' }]
      }),
      adapter: { watchState: jest.fn().mockResolvedValue(watch) }
    })
    hookHarness.contextValue = { manager: expoManager, loading: false, error: null }

    expect(useBleReadiness()).toEqual({ readiness: null, loading: true, error: null })
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()

    expect(store.getSnapshot().readiness).toMatchObject({
      state: 'unavailable',
      actions: [{ kind: 'rebuild-native-app' }]
    })
    unsubscribe()
  })

  test('watchState rejection clears the resource-free run and a later subscriber retries', async () => {
    const createdManager = manager({
      adapter: {
        watchState: jest
          .fn()
          .mockRejectedValueOnce(new Error('watch create failed'))
          .mockResolvedValue(
            adapterWatch(
              adapterState(),
              (async function* () {
                await new Promise(() => undefined)
              })()
            )
          )
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const firstUnsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(store.getSnapshot()).toMatchObject({ loading: false, error: expect.any(Error) })
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager)).toEqual({
      runCount: 0,
      phase: 'idle',
      hasWatch: false
    })
    firstUnsubscribe()
    const secondUnsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(2)
    expect(store.getSnapshot().error).toBeNull()
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager).phase).toBe('active')
    secondUnsubscribe()
    await flush()
  })

  test('source terminal stops the owned watch before replacement', async () => {
    const firstWatch = adapterWatch(
      adapterState(),
      (async function* () {
        yield { kind: 'terminal', reason: 'closed' }
      })()
    )
    const secondWatch = adapterWatch(
      adapterState({ updatedAt: 2 }),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({
      adapter: {
        watchState: jest.fn().mockResolvedValueOnce(firstWatch).mockResolvedValueOnce(secondWatch)
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(firstWatch.stop).toHaveBeenCalledTimes(1)
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(2)
    expect(store.getSnapshot().state).toEqual(adapterState({ updatedAt: 2 }))
    unsubscribe()
    await flush()
  })

  test('unexpected watch iterator end stops the owned watch before replacement', async () => {
    const firstWatch = adapterWatch(adapterState(), (async function* () {})())
    const secondWatch = adapterWatch(
      adapterState({ updatedAt: 2 }),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({
      adapter: {
        watchState: jest.fn().mockResolvedValueOnce(firstWatch).mockResolvedValueOnce(secondWatch)
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(firstWatch.stop).toHaveBeenCalledTimes(1)
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(2)
    unsubscribe()
    await flush()
  })

  test('release-failed stop retains the old run and blocks replacement', async () => {
    const watch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    watch.stop.mockResolvedValue({
      state: 'release-failed',
      failures: [{ resourceKind: 'adapter', error: { code: 'platform.failure' } }]
    })
    const onError = jest.fn()
    hookHarness.errorContextValue = onError
    const createdManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(watch) } })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    unsubscribe()
    await flush()
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager)).toEqual({
      runCount: 1,
      phase: 'cleanup-failed',
      hasWatch: true
    })
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(1)
    expect(onError).toHaveBeenCalled()
  })

  test('later subscriber retries failed cleanup before creating a watch', async () => {
    const firstWatch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const secondWatch = adapterWatch(
      adapterState({ updatedAt: 2 }),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    firstWatch.stop
      .mockResolvedValueOnce({
        state: 'release-failed',
        failures: [{ resourceKind: 'adapter', error: { code: 'platform.failure' } }]
      })
      .mockResolvedValueOnce({ state: 'released', failures: [] })
    hookHarness.errorContextValue = jest.fn()
    const createdManager = manager({
      adapter: {
        watchState: jest.fn().mockResolvedValueOnce(firstWatch).mockResolvedValueOnce(secondWatch)
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const firstUnsubscribe = store.subscribe(jest.fn())
    await flush()
    firstUnsubscribe()
    await flush()
    expect(firstWatch.stop).toHaveBeenCalledTimes(1)
    const secondUnsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(firstWatch.stop).toHaveBeenCalledTimes(2)
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(2)
    expect(secondWatch.stop).not.toHaveBeenCalled()
    expect(store.getSnapshot().state).toEqual(adapterState({ updatedAt: 2 }))
    secondUnsubscribe()
    await flush()
  })

  test('manual unsubscribe and terminal race share one stop attempt', async () => {
    const terminal = deferred()
    let terminalSent = false
    const watch = adapterWatch(adapterState(), {
      [Symbol.asyncIterator]: () => ({
        next: () => {
          if (terminalSent) {
            return new Promise(() => undefined)
          }
          return terminal.promise.then(() => {
            terminalSent = true
            return { done: false, value: { kind: 'terminal', reason: 'closed' } }
          })
        },
        return: async () => ({ done: true, value: undefined })
      })
    })
    const hangingWatch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({
      adapter: {
        watchState: jest.fn().mockResolvedValueOnce(watch).mockResolvedValue(hangingWatch)
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    terminal.resolve()
    unsubscribe()
    await flush()
    expect(watch.stop).toHaveBeenCalledTimes(1)
  })

  test('StrictMode and rapid remount never own two watches', async () => {
    const firstStop = deferred()
    const firstWatch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    firstWatch.stop.mockReturnValue(firstStop.promise)
    const secondWatch = adapterWatch(
      adapterState({ updatedAt: 2 }),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({
      adapter: {
        watchState: jest.fn().mockResolvedValueOnce(firstWatch).mockResolvedValueOnce(secondWatch)
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const firstUnsubscribe = store.subscribe(jest.fn())
    await flush()
    firstUnsubscribe()
    const secondUnsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(1)
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager).phase).toBe('stopping')
    firstStop.resolve({ state: 'released', failures: [] })
    await flush()
    expect(createdManager.adapter.watchState).toHaveBeenCalledTimes(2)
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager).phase).toBe('active')
    secondUnsubscribe()
    await flush()
  })

  test('adapter readiness and capability snapshots resume after recovery', async () => {
    const firstWatch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const recovered = adapterState({ backendGeneration: 'backend-2', updatedAt: 2 })
    const secondWatch = adapterWatch(
      recovered,
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    firstWatch.stop
      .mockResolvedValueOnce({
        state: 'release-failed',
        failures: [{ resourceKind: 'adapter', error: { code: 'platform.failure' } }]
      })
      .mockResolvedValueOnce({ state: 'released', failures: [] })
    hookHarness.errorContextValue = jest.fn()
    const readiness = jest
      .fn()
      .mockResolvedValueOnce({ state: 'ready', adapter: adapterState(), actions: [] })
      .mockResolvedValueOnce({ state: 'ready', adapter: recovered, actions: [] })
    const createdManager = manager({
      readiness,
      adapter: { watchState: jest.fn().mockResolvedValueOnce(firstWatch).mockResolvedValueOnce(secondWatch) },
      capabilities: {
        get: jest
          .fn()
          .mockReturnValueOnce({ id: 'scan', state: 'supported', limitations: [] })
          .mockReturnValueOnce({ id: 'scan', state: 'limited', limitations: ['recovered'] })
      }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    expect(useAdapterState()).toEqual({ state: null, loading: true, error: null })
    const adapterStore = hookHarness.externalStores[0]
    expect(useBleReadiness()).toEqual({ readiness: null, loading: true, error: null })
    const readinessStore = hookHarness.externalStores[1]
    expect(useBleCapability('scan')).toEqual({ id: 'scan', state: 'supported', limitations: [] })
    const firstUnsubscribe = adapterStore.subscribe(jest.fn())
    await flush()
    expect(readinessStore.getSnapshot().readiness.adapter).toEqual(adapterState())
    firstUnsubscribe()
    await flush()
    const secondUnsubscribe = adapterStore.subscribe(jest.fn())
    await flush()
    await flush()
    expect(adapterStore.getSnapshot().state).toEqual(recovered)
    expect(readinessStore.getSnapshot().readiness.adapter).toEqual(recovered)
    expect(useBleCapability('scan')).toEqual({ id: 'scan', state: 'limited', limitations: ['recovered'] })
    secondUnsubscribe()
    await flush()
  })

  test('final unmount leaves zero React-owned watch runs', async () => {
    const watch = adapterWatch(
      adapterState(),
      (async function* () {
        await new Promise(() => undefined)
      })()
    )
    const createdManager = manager({ adapter: { watchState: jest.fn().mockResolvedValue(watch) } })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    useAdapterState()
    const store = hookHarness.externalStores[0]
    const unsubscribe = store.subscribe(jest.fn())
    await flush()
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager).runCount).toBe(1)
    unsubscribe()
    await flush()
    expect(watch.stop).toHaveBeenCalledTimes(1)
    expect(inspectReactAdapterWatchOwnershipForTests(createdManager)).toEqual({
      runCount: 0,
      phase: 'idle',
      hasWatch: false
    })
  })

  test('resets connection state when the observed connection is replaced', async () => {
    const first = connectionWithState('connected')
    const second = connectionWithState('connecting')

    const firstResult = useConnectionState(first.connection)
    expect(firstResult).toEqual({ state: null, loading: true, error: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    expect(hookHarness.stateValues[0]).toEqual({ state: 'connected', loading: false, error: null })

    firstCleanup()
    hookHarness.rerender()
    const secondResult = useConnectionState(second.connection)
    expect(secondResult).toEqual({ state: null, loading: true, error: null })
    const secondCleanup = hookHarness.effects[0]()
    await flush()
    expect(hookHarness.stateValues[0]).toEqual({ state: 'connecting', loading: false, error: null })
    secondCleanup()
  })

  test('resets characteristic value when the observed characteristic is replaced', async () => {
    const firstValue = { value: new Uint8Array([1]), delivery: 'notification', observedAtMonotonicMs: 1, sequence: 1 }
    const secondValue = { value: new Uint8Array([2]), delivery: 'notification', observedAtMonotonicMs: 2, sequence: 2 }
    const firstSubscription = characteristicSubscription(
      (async function* () {
        yield { kind: 'value', value: firstValue }
        await new Promise(() => undefined)
      })()
    )
    const secondSubscription = characteristicSubscription(
      (async function* () {
        yield { kind: 'value', value: secondValue }
        await new Promise(() => undefined)
      })()
    )
    const firstCharacteristic = { subscribe: jest.fn().mockResolvedValue(firstSubscription) }
    const secondCharacteristic = { subscribe: jest.fn().mockResolvedValue(secondSubscription) }

    expect(useCharacteristicValue(firstCharacteristic)).toEqual({ value: null, loading: true, error: null })
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    expect(hookHarness.stateValues[0]).toEqual({ value: firstValue, loading: false, error: null })

    firstCleanup()
    hookHarness.rerender()
    expect(useCharacteristicValue(secondCharacteristic)).toEqual({ value: null, loading: true, error: null })
    const secondCleanup = hookHarness.effects[0]()
    await flush()
    expect(hookHarness.stateValues[0]).toEqual({ value: secondValue, loading: false, error: null })
    secondCleanup()
  })

  test('restarts scans when timeout or AbortSignal identity changes and cleans up the prior session', async () => {
    const firstSignal = new AbortController().signal
    const secondSignal = new AbortController().signal
    const firstSession = scanSession()
    const secondSession = scanSession()
    const thirdSession = scanSession()
    const createdManager = manager({
      scan: jest
        .fn()
        .mockResolvedValueOnce(firstSession)
        .mockResolvedValueOnce(secondSession)
        .mockResolvedValueOnce(thirdSession)
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    const firstOptions = { timeoutMs: 1_000, signal: firstSignal }
    useDiscoveredPeers(firstOptions)
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    expect(createdManager.scan).toHaveBeenNthCalledWith(1, firstOptions)

    firstCleanup()
    hookHarness.rerender()
    const secondOptions = { timeoutMs: 2_000, signal: firstSignal }
    useDiscoveredPeers(secondOptions)
    const secondCleanup = hookHarness.effects[0]()
    await flush()
    expect(firstSession.stop).toHaveBeenCalledTimes(1)
    expect(createdManager.scan).toHaveBeenNthCalledWith(2, secondOptions)

    secondCleanup()
    hookHarness.rerender()
    const thirdOptions = { timeoutMs: 2_000, signal: secondSignal }
    useDiscoveredPeers(thirdOptions)
    const thirdCleanup = hookHarness.effects[0]()
    await flush()
    expect(secondSession.stop).toHaveBeenCalledTimes(1)
    expect(createdManager.scan).toHaveBeenNthCalledWith(3, thirdOptions)

    thirdCleanup()
    expect(thirdSession.stop).toHaveBeenCalledTimes(1)
  })

  test('reports a scan stop release failure through the provider error callback', async () => {
    const cleanup = { state: 'release-failed', failures: [] }
    const onError = jest.fn()
    const session = scanSession()
    session.stop.mockResolvedValue(cleanup)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    hookHarness.errorContextValue = onError

    useDiscoveredPeers()
    const unmount = hookHarness.effects[0]()
    await flush()
    unmount()
    await flush()

    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ cleanup }))
  })

  test('surfaces scan overflow notices in the result error', async () => {
    const session = scanSession(
      (async function* () {
        yield overflowNotice()
        yield { kind: 'terminal' }
      })()
    )
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()

    expect(hookHarness.stateValues[0].error).toMatchObject({ normalized: { code: 'stream.overflow' } })
    cleanup()
  })

  test('lost discovery event removes the peer when events are present', async () => {
    const observations = createControllableAsyncIterator()
    const events = createControllableAsyncIterator()
    const session = scanSession(observations.iterable, events.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    events.push(observedEvent('keep'))
    events.push(observedEvent('drop-me'))
    await flush()
    expect(peerIds()).toEqual(['keep', 'drop-me'])

    events.push(lostEvent('drop-me'))
    await flush()
    expect(peerIds()).toEqual(['keep'])
    expect(hookHarness.stateValues[0].state).toBe('active')
    cleanup()
  })

  test('observed discovery event refreshes one peer without duplication', async () => {
    const observations = createControllableAsyncIterator()
    const events = createControllableAsyncIterator()
    const session = scanSession(observations.iterable, events.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    events.push(observedEvent('sensor', { name: null }))
    await flush()
    events.push(observedEvent('sensor', { name: 'Heart Strap' }))
    await flush()
    expect(peerIds()).toEqual(['sensor'])
    expect(hookHarness.stateValues[0].peers[0].name).toBe('Heart Strap')

    events.push(observedEvent('other'))
    await flush()
    events.push(observedEvent('sensor', { name: 'Heart Strap' }))
    await flush()
    expect(peerIds()).toEqual(['other', 'sensor'])
    cleanup()
  })

  test('observations provide presence when events are absent', async () => {
    const observations = createControllableAsyncIterator()
    const session = scanSession(observations.iterable, undefined)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    observations.push(observationItem('alpha', { name: 'A' }))
    await flush()
    observations.push(observationItem('beta'))
    await flush()
    observations.push(observationItem('alpha', { name: 'A2' }))
    await flush()
    expect(peerIds()).toEqual(['beta', 'alpha'])
    expect(hookHarness.stateValues[0].peers[1].name).toBe('A2')
    cleanup()
  })

  test('observation values do not double-insert when events are present', async () => {
    const observations = createControllableAsyncIterator()
    const events = createControllableAsyncIterator()
    const session = scanSession(observations.iterable, events.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    observations.push(observationItem('ghost'))
    await flush()
    expect(peerIds()).toEqual([])

    events.push(observedEvent('sensor', { name: 'first' }))
    await flush()
    observations.push(observationItem('sensor', { name: 'richer' }))
    observations.push(observationItem('ghost'))
    await flush()
    expect(peerIds()).toEqual(['sensor'])
    expect(hookHarness.stateValues[0].peers[0].name).toBe('richer')
    cleanup()
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('peer map evicts oldest observation at 256 entries (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    for (let index = 0; index < 257; index += 1) {
      const id = `peer-${index}`
      if (withEvents) events.push(observedEvent(id))
      else observations.push(observationItem(id))
    }
    await flushMany()
    const ids = peerIds()
    expect(ids).toHaveLength(256)
    expect(ids).not.toContain('peer-0')
    expect(ids).toContain('peer-256')
    expect(hookHarness.stateValues[0].error).toMatchObject({ normalized: { code: 'stream.overflow' } })
    expect(hookHarness.stateValues[0].state).toBe('active')
    cleanup()
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('peer map evicts oldest observation above 256 KiB (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    const advertisement = richAdvertisement(200_000)

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    if (withEvents) {
      events.push(observedEvent('old', { lastAdvertisement: advertisement }))
      events.push(observedEvent('new', { lastAdvertisement: advertisement }))
    } else {
      observations.push(observationItem('old', { lastAdvertisement: advertisement }))
      observations.push(observationItem('new', { lastAdvertisement: advertisement }))
    }
    await flush()
    expect(peerIds()).toEqual(['new'])
    expect(hookHarness.stateValues[0].error).toMatchObject({ normalized: { code: 'stream.overflow' } })
    expect(hookHarness.stateValues[0].state).toBe('active')
    cleanup()
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('cap eviction sets stream.overflow while scan remains active (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    for (let index = 0; index < 257; index += 1) {
      const id = `cap-${index}`
      if (withEvents) events.push(observedEvent(id))
      else observations.push(observationItem(id))
    }
    await flushMany()
    expect(hookHarness.stateValues[0].state).toBe('active')
    expect(hookHarness.stateValues[0].error).toMatchObject({
      normalized: { code: 'stream.overflow', operation: 'react.useDiscoveredPeers.cap' }
    })
    if (withEvents) events.push(observedEvent('after-cap'))
    else observations.push(observationItem('after-cap'))
    await flush()
    expect(hookHarness.stateValues[0].state).toBe('active')
    expect(peerIds()).toContain('after-cap')
    expect(peerIds()).toHaveLength(256)
    cleanup()
  })

  test('options change manager replacement and unmount clear retained state', async () => {
    const firstObservations = createControllableAsyncIterator()
    const secondObservations = createControllableAsyncIterator()
    const thirdObservations = createControllableAsyncIterator()
    const firstSession = scanSession(firstObservations.iterable)
    const secondSession = scanSession(secondObservations.iterable)
    const thirdSession = scanSession(thirdObservations.iterable)
    const firstManager = manager({
      scan: jest.fn().mockResolvedValueOnce(firstSession).mockResolvedValueOnce(secondSession)
    })
    hookHarness.contextValue = { manager: firstManager, loading: false, error: null }

    useDiscoveredPeers({ timeoutMs: 1_000 })
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    firstObservations.push(observationItem('keep'))
    await flush()
    expect(peerIds()).toEqual(['keep'])

    firstCleanup()
    hookHarness.rerender()
    useDiscoveredPeers({ timeoutMs: 2_000 })
    const secondCleanup = hookHarness.effects[0]()
    await flush()
    expect(peerIds()).toEqual([])
    firstObservations.push(observationItem('stale'))
    await flush()
    expect(peerIds()).toEqual([])

    secondObservations.push(observationItem('next'))
    await flush()
    expect(peerIds()).toEqual(['next'])

    secondCleanup()
    hookHarness.rerender()
    const secondManager = manager({ scan: jest.fn().mockResolvedValue(thirdSession) })
    hookHarness.contextValue = { manager: secondManager, loading: false, error: null }
    useDiscoveredPeers({ timeoutMs: 2_000 })
    const thirdCleanup = hookHarness.effects[0]()
    await flush()
    expect(peerIds()).toEqual([])
    secondObservations.push(observationItem('from-old-manager'))
    await flush()
    expect(peerIds()).toEqual([])

    thirdObservations.push(observationItem('third'))
    await flush()
    expect(peerIds()).toEqual(['third'])
    thirdCleanup()
    await flush()
    expect(peerIds()).toEqual([])
    expect(firstSession.stop).toHaveBeenCalledTimes(1)
    expect(secondSession.stop).toHaveBeenCalledTimes(1)
    expect(thirdSession.stop).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('observation and optional event iterators are returned exactly once (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    cleanup()
    await flush()
    expect(observations.returnFn).toHaveBeenCalledTimes(1)
    if (withEvents) expect(events.returnFn).toHaveBeenCalledTimes(1)
    cleanup()
    await flush()
    expect(observations.returnFn).toHaveBeenCalledTimes(1)
    if (withEvents) expect(events.returnFn).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('session stop is attempted exactly once after both iterator returns (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const observationReturn = deferred()
    const eventReturn = deferred()
    observations.returnFn.mockImplementation(() =>
      observationReturn.promise.then(() => {
        observations.end()
        return { done: true }
      })
    )
    if (withEvents) {
      events.returnFn.mockImplementation(() =>
        eventReturn.promise.then(() => {
          events.end()
          return { done: true }
        })
      )
    }
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    cleanup()
    await flush()
    expect(observations.returnFn).toHaveBeenCalledTimes(1)
    if (withEvents) expect(events.returnFn).toHaveBeenCalledTimes(1)
    expect(session.stop).not.toHaveBeenCalled()

    observationReturn.resolve()
    await flush()
    if (withEvents) {
      expect(session.stop).not.toHaveBeenCalled()
      eventReturn.resolve()
      await flush()
    }
    expect(session.stop).toHaveBeenCalledTimes(1)
    cleanup()
    await flush()
    expect(session.stop).toHaveBeenCalledTimes(1)
  })

  test('iterator-return and session-stop failures are all reported', async () => {
    const observations = createControllableAsyncIterator()
    const events = createControllableAsyncIterator()
    const observationError = new Error('observation return failed')
    const eventError = new Error('event return failed')
    const stopError = new Error('session stop failed')
    observations.returnFn.mockImplementation(async () => {
      observations.end()
      throw observationError
    })
    events.returnFn.mockImplementation(async () => {
      events.end()
      throw eventError
    })
    const session = scanSession(observations.iterable, events.iterable)
    session.stop.mockRejectedValue(stopError)
    const onError = jest.fn()
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }
    hookHarness.errorContextValue = onError

    useDiscoveredPeers()
    const cleanup = hookHarness.effects[0]()
    await flush()
    cleanup()
    await flush()

    expect(onError).toHaveBeenCalledWith(observationError)
    expect(onError).toHaveBeenCalledWith(eventError)
    expect(onError).toHaveBeenCalledWith(stopError)
    expect(session.stop).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['events present', true],
    ['events absent', false]
  ])('per-update array length never exceeds 256 (%s)', async (_label, withEvents) => {
    const observations = createControllableAsyncIterator()
    const events = withEvents ? createControllableAsyncIterator() : undefined
    const session = scanSession(observations.iterable, events?.iterable)
    const createdManager = manager({ scan: jest.fn().mockResolvedValue(session) })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    useDiscoveredPeers()
    const lengths = []
    let stored = hookHarness.stateValues[0]
    Object.defineProperty(hookHarness.stateValues, '0', {
      configurable: true,
      enumerable: true,
      get: () => stored,
      set(value) {
        stored = value
        if (value && Array.isArray(value.peers)) lengths.push(value.peers.length)
      }
    })
    const cleanup = hookHarness.effects[0]()
    await flush()
    for (let index = 0; index < 260; index += 1) {
      const id = `len-${index}`
      if (withEvents) events.push(observedEvent(id))
      else observations.push(observationItem(id))
    }
    await flushMany()
    expect(lengths.length).toBeGreaterThan(0)
    expect(Math.max(...lengths)).toBeLessThanOrEqual(256)
    expect(peerIds()).toHaveLength(256)
    cleanup()
    Object.defineProperty(hookHarness.stateValues, '0', {
      configurable: true,
      enumerable: true,
      writable: true,
      value: stored
    })
  })

  test('reports a connection iterator cleanup rejection through the provider error callback', async () => {
    const cleanupError = new Error('iterator cleanup failed')
    const onError = jest.fn()
    const iterator = {
      next: jest.fn(() => new Promise(() => undefined)),
      return: jest.fn().mockRejectedValue(cleanupError)
    }
    const connection = {
      lifecycleEvents: { [Symbol.asyncIterator]: () => iterator }
    }
    hookHarness.errorContextValue = onError

    useConnectionState(connection)
    const unmount = hookHarness.effects[0]()
    await flush()
    unmount()
    await flush()

    expect(onError).toHaveBeenCalledWith(cleanupError)
  })

  test('reports a characteristic removal release failure through the provider error callback', async () => {
    const cleanup = { state: 'release-failed', failures: [] }
    const onError = jest.fn()
    const subscription = characteristicSubscription()
    subscription.remove.mockResolvedValue(cleanup)
    const characteristic = { subscribe: jest.fn().mockResolvedValue(subscription) }
    hookHarness.errorContextValue = onError

    useCharacteristicValue(characteristic)
    const unmount = hookHarness.effects[0]()
    await flush()
    unmount()
    await flush()

    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ cleanup }))
  })

  test('surfaces characteristic overflow notices in the result error', async () => {
    const subscription = characteristicSubscription(
      (async function* () {
        yield overflowNotice()
        yield { kind: 'terminal' }
      })()
    )
    const characteristic = { subscribe: jest.fn().mockResolvedValue(subscription) }

    useCharacteristicValue(characteristic)
    const cleanup = hookHarness.effects[0]()
    await flush()

    expect(hookHarness.stateValues[0].error).toMatchObject({ normalized: { code: 'stream.overflow' } })
    cleanup()
  })

  test('restarts characteristic subscriptions when timeout or AbortSignal identity changes', async () => {
    const firstSignal = new AbortController().signal
    const secondSignal = new AbortController().signal
    const firstSubscription = characteristicSubscription()
    const secondSubscription = characteristicSubscription()
    const thirdSubscription = characteristicSubscription()
    const characteristic = {
      subscribe: jest
        .fn()
        .mockResolvedValueOnce(firstSubscription)
        .mockResolvedValueOnce(secondSubscription)
        .mockResolvedValueOnce(thirdSubscription)
    }

    const firstOptions = { timeoutMs: 1_000, signal: firstSignal }
    useCharacteristicValue(characteristic, firstOptions)
    const firstCleanup = hookHarness.effects[0]()
    await flush()
    expect(characteristic.subscribe).toHaveBeenNthCalledWith(1, {
      timeoutMs: firstOptions.timeoutMs,
      stream: 'balanced',
      signal: expect.any(AbortSignal)
    })
    expect(Object.is(characteristic.subscribe.mock.calls[0][0].signal, firstSignal)).toBe(true)

    firstCleanup()
    hookHarness.rerender()
    const secondOptions = { timeoutMs: 2_000, signal: firstSignal }
    useCharacteristicValue(characteristic, secondOptions)
    const secondCleanup = hookHarness.effects[0]()
    await flush()
    expect(firstSubscription.remove).toHaveBeenCalledTimes(1)
    expect(characteristic.subscribe).toHaveBeenNthCalledWith(2, {
      timeoutMs: secondOptions.timeoutMs,
      stream: 'balanced',
      signal: expect.any(AbortSignal)
    })
    expect(Object.is(characteristic.subscribe.mock.calls[1][0].signal, firstSignal)).toBe(true)

    secondCleanup()
    hookHarness.rerender()
    const thirdOptions = { timeoutMs: 2_000, signal: secondSignal }
    useCharacteristicValue(characteristic, thirdOptions)
    const thirdCleanup = hookHarness.effects[0]()
    await flush()
    expect(secondSubscription.remove).toHaveBeenCalledTimes(1)
    expect(characteristic.subscribe).toHaveBeenNthCalledWith(3, {
      timeoutMs: thirdOptions.timeoutMs,
      stream: 'balanced',
      signal: expect.any(AbortSignal)
    })
    expect(Object.is(characteristic.subscribe.mock.calls[2][0].signal, secondSignal)).toBe(true)

    thirdCleanup()
    expect(thirdSubscription.remove).toHaveBeenCalledTimes(1)
  })
})
