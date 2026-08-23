const React = require('react')

const hookHarness = {
  contextValue: null,
  errorContextValue: null,
  contexts: [],
  effects: [],
  stateValues: [],
  stateIndex: 0,
  effectIndex: 0,
  memoValues: [],
  memoDependencies: [],
  memoIndex: 0,
  reset() {
    this.effects = []
    this.stateIndex = 0
    this.effectIndex = 0
    this.memoValues = []
    this.memoDependencies = []
    this.memoIndex = 0
  },
  rerender() {
    this.effects = []
    this.stateIndex = 0
    this.effectIndex = 0
    this.memoIndex = 0
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
  useRef: jest.fn(initialValue => ({ current: initialValue })),
  useState: jest.fn(initialValue => hookHarness.nextState(initialValue))
}))

const {
  BleProvider,
  getAdapterState,
  getBleCapability,
  useAdapterState,
  useBle,
  useBleCapability,
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

function manager(overrides = {}) {
  return {
    adapter: {
      state: jest.fn().mockResolvedValue({ availability: 'available', power: 'on', authorization: 'granted' }),
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

function scanSession() {
  return {
    observations: (async function* () {
      yield { kind: 'terminal' }
    })(),
    stop: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
  }
}

function characteristicSubscription() {
  return {
    values: (async function* () {
      yield { kind: 'terminal' }
    })(),
    remove: jest.fn().mockResolvedValue({ state: 'released', failures: [] })
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

  test('provides useBle readiness, adapter state, and capability hooks plus imperative equivalents', async () => {
    const adapterState = { availability: 'available', power: 'on', authorization: 'granted' }
    const capability = { id: 'scan', state: 'supported', limitations: [] }
    const createdManager = manager({
      adapter: { state: jest.fn().mockResolvedValue(adapterState) },
      capabilities: { get: jest.fn().mockReturnValue(capability) }
    })
    hookHarness.contextValue = { manager: createdManager, loading: false, error: null }

    expect(useBle()).toEqual({ manager: createdManager, loading: false, error: null })
    expect(useBleCapability('scan')).toBe(capability)
    expect(getBleCapability(createdManager, 'scan')).toBe(capability)
    await expect(getAdapterState(createdManager)).resolves.toBe(adapterState)

    hookHarness.stateValues = []
    hookHarness.reset()
    const adapterResult = useAdapterState()
    expect(adapterResult).toEqual({ state: null, loading: true, error: null })
    const cleanup = hookHarness.effects[0]()
    expect(cleanup).toEqual(expect.any(Function))
    await flush()
    expect(hookHarness.stateValues[0]).toEqual({ state: adapterState, loading: false, error: null })
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
