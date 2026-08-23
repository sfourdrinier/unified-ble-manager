const React = require('react')

const hookHarness = {
  contextValue: null,
  effects: [],
  stateValues: [],
  stateIndex: 0,
  effectIndex: 0,
  reset() {
    this.effects = []
    this.stateIndex = 0
    this.effectIndex = 0
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
  nextEffect(effect) {
    const index = this.effectIndex++
    this.effects[index] = effect
  }
}

jest.mock('react', () => ({
  createContext: jest.fn(defaultValue => ({ defaultValue, Provider: Symbol('Provider') })),
  createElement: jest.fn((type, props, children) => ({ type, props, children })),
  useContext: jest.fn(() => hookHarness.contextValue),
  useEffect: jest.fn(effect => hookHarness.nextEffect(effect)),
  useMemo: jest.fn(factory => factory()),
  useRef: jest.fn(initialValue => ({ current: initialValue })),
  useState: jest.fn(initialValue => hookHarness.nextState(initialValue))
}))

const {
  BleProvider,
  getAdapterState,
  getBleCapability,
  useAdapterState,
  useBle,
  useBleCapability
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

async function flush() {
  for (let index = 0; index < 5; index += 1) await Promise.resolve()
  await new Promise(resolve => setImmediate(resolve))
}

describe('React host surface', () => {
  beforeEach(() => {
    hookHarness.contextValue = null
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
})
