// __tests__/public-adapter-watch.test.js

const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
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
  test('polls bounded state changes and does not fabricate unchanged transitions', async () => {
    const states = [adapterState()]
    const adapterStateCall = jest.fn(async () => states[states.length - 1])
    const ipc = {
      bootstrap: ipcBootstrap(),
      capabilities: ipcCapabilities(),
      adapterState: adapterStateCall
    }
    const manager = new IpcPublicManagerAdapter(ipc)
    const publicWatch = await manager.adapter.watchState()
    const iterator = publicWatch.values[Symbol.asyncIterator]()
    const pending = iterator.next()

    await new Promise(resolve => setTimeout(resolve, 40))
    await expect(Promise.race([pending.then(() => 'emitted'), Promise.resolve('pending')])).resolves.toBe('pending')
    states.push(adapterState({ power: 'off', updatedAt: 2, safeReason: 'poll-change' }))
    await new Promise(resolve => setTimeout(resolve, 40))
    await expect(pending).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: { power: 'off', updatedAt: 2, safeReason: 'poll-change' } }
    })
    await expect(publicWatch.stop()).resolves.toMatchObject({ state: 'released' })
    expect(adapterStateCall).toHaveBeenCalled()
  })
})
