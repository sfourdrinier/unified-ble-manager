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
  useDiscoveredPeers
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
  })()
) {
  return {
    observations,
    stop: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
  }
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
