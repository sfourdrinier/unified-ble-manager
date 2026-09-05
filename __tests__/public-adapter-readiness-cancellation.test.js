// __tests__/public-adapter-readiness-cancellation.test.js
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { createPublicBleManager } = require('../src/public/ble-manager')

const limits = {
  itemCapacity: capacity(4),
  byteCapacity: capacity(256),
  reservedControlCapacity: capacity(1)
}

function state(power = 'off') {
  return {
    availability: 'available',
    authorization: 'granted',
    power,
    backendGeneration: 'backend-1',
    updatedAt: 1,
    safeReason: null
  }
}

function publicInternal(watch, onAbort) {
  return {
    identity: { attachment: { adapter: { adapterId: 'adapter-1' } } },
    attachedBackend: undefined,
    capability: () => null,
    capabilities: () => [],
    adapterState: async () => watch.initial,
    adapterStates: async options => {
      if (onAbort !== undefined) options.signal?.addEventListener('abort', onAbort, { once: true })
      return watch
    },
    destroy: async () => ({ state: 'released', failures: [] })
  }
}

test('aborting readiness while its bounded stream is pending reports operation.aborted and closes once', async () => {
  const source = new CoreBoundedStream(limits, 'latest')
  let stopped = false
  const stop = jest.fn(async () => {
    if (stopped) return { state: 'released', failures: [] }
    stopped = true
    return source.close()
  })
  const controller = new AbortController()
  const watch = { initial: state(), values: source, stop }
  const manager = await createPublicBleManager(
    publicInternal(watch, () => source.close()),
    () => 100
  )

  const readiness = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 10_000 })
  await Promise.resolve()
  controller.abort()

  await expect(readiness).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(stop).toHaveBeenCalledTimes(1)
})

test('aborting readiness invokes a noncooperative iterator return without waiting for its deadline', async () => {
  let returned = 0
  let closed = 0
  const values = {
    limits,
    overflowPolicy: 'latest',
    [Symbol.asyncIterator]() {
      return {
        next: () => new Promise(() => undefined),
        return: async () => {
          returned += 1
          return { done: true, value: undefined }
        }
      }
    },
    close: async () => {
      closed += 1
      return { state: 'released', failures: [] }
    }
  }
  const controller = new AbortController()
  let stopped = false
  const stop = async () => {
    if (stopped) return { state: 'released', failures: [] }
    stopped = true
    return values.close()
  }
  const watch = { initial: state(), values, stop }
  const manager = await createPublicBleManager(publicInternal(watch), () => 100)
  const readiness = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 10_000 })
  await Promise.resolve()
  controller.abort()

  await expect(readiness).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(returned).toBe(1)
  expect(closed).toBe(1)
})

test('aborting during watch acquisition cleans a late ready watch and reports operation.aborted', async () => {
  const source = new CoreBoundedStream(limits, 'latest')
  const stop = jest.fn(async () => source.close())
  const watch = { initial: state('on'), values: source, stop }
  let resolveAcquisition
  const acquisition = new Promise(resolve => {
    resolveAcquisition = resolve
  })
  const internal = publicInternal(watch)
  internal.adapterStates = async () => acquisition
  const manager = await createPublicBleManager(internal, () => 100)
  const controller = new AbortController()
  const readiness = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 10_000 })

  controller.abort()
  resolveAcquisition(watch)

  await expect(readiness).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(stop).toHaveBeenCalledTimes(1)
})

test('a ready watch acquired after the caller deadline is released instead of returning late success', async () => {
  let now = 100
  const source = new CoreBoundedStream(limits, 'latest')
  const stop = jest.fn(async () => source.close())
  const watch = { initial: state('on'), values: source, stop }
  const internal = publicInternal(watch)
  internal.adapterStates = async () => {
    now = 111
    return watch
  }
  const manager = await createPublicBleManager(internal, () => now)
  await expect(manager.adapter.waitUntilReady({ timeoutMs: 10 })).rejects.toMatchObject({ code: 'operation.timed-out' })
  expect(stop).toHaveBeenCalledTimes(1)
})

async function actualWebManager() {
  const { createWebBluetoothProvider, WEB_BLUETOOTH_ADAPTER_ID } = require('../src/web/web-bluetooth-backend')
  const { createBleManagerFromBackend, DEFAULT_BLE_MANAGER_OPTIONS } = require('../src/manager/ble-manager')
  const { opaqueId } = require('../src/backend-contract/primitives')
  const boundary = {
    implementationVersion: 'readiness-acquisition-test',
    browserEngine: 'test-browser',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => true,
    requestDevice: async () => {
      throw new Error('chooser not expected')
    },
    now: () => Date.now(),
    setTimer: (callback, ms) => setTimeout(callback, ms),
    clearTimer: clearTimeout,
    addPageLifecycleListener: () => () => undefined
  }
  const provider = createWebBluetoothProvider(boundary)
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const internal = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: {
        clientId: opaqueId('readiness-client', 'client', 'web-bluetooth:browser'),
        managerId: opaqueId('readiness-manager', 'manager', 'web-bluetooth:browser'),
        ownerMode: 'owning'
      }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => Date.now() }
  )
  return { manager: await createPublicBleManager(internal, () => Date.now()), boundary }
}

describe('real Web backend asynchronous initial availability', () => {
  beforeEach(() => jest.useFakeTimers())
  afterEach(() => jest.useRealTimers())
  test.each(['abort', 'deadline'])(
    '%s settles before initial availability resolves and late watch is released',
    async cause => {
      const { manager, boundary } = await actualWebManager()
      let resolveAvailability
      boundary.bluetoothAvailable = () =>
        new Promise(resolve => {
          resolveAvailability = resolve
        })
      const controller = new AbortController()
      const pending = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 100 })
      const failure = expect(pending).rejects.toMatchObject({
        code: cause === 'abort' ? 'operation.aborted' : 'operation.timed-out'
      })
      if (cause === 'abort') controller.abort()
      else await jest.advanceTimersByTimeAsync(100)
      await failure
      // Acquisition remains owned; destruction cannot falsely claim release.
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      resolveAvailability(true)
      await jest.advanceTimersByTimeAsync(0)
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    }
  )
})

test('deadline remains a timeout when abort closes the already acquired source first', async () => {
  jest.useFakeTimers()
  try {
    const source = new CoreBoundedStream(limits, 'latest')
    const stop = jest.fn(() => source.close())
    const manager = await createPublicBleManager(
      publicInternal({ initial: state(), values: source, stop }, () => source.close()),
      () => Date.now()
    )
    const pending = manager.adapter.waitUntilReady({ timeoutMs: 100 })
    const failure = expect(pending).rejects.toMatchObject({ code: 'operation.timed-out' })
    await jest.advanceTimersByTimeAsync(100)
    await failure
    expect(stop).toHaveBeenCalledTimes(1)
  } finally {
    jest.useRealTimers()
  }
})

test.each(['deadline', 'abort'])(
  '%s preserves its primary failure alongside the complete failed-stop receipt',
  async cause => {
    jest.useFakeTimers()
    try {
      const source = new CoreBoundedStream(limits, 'latest')
      const cleanup = {
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'adapter',
            error: {
              code: 'platform.failure',
              domain: 'cleanup',
              operation: 'readiness-test.stop',
              platform: null,
              retryability: 'caller-decides'
            }
          }
        ]
      }
      const stop = jest.fn(async () => cleanup)
      const manager = await createPublicBleManager(publicInternal({ initial: state(), values: source, stop }), () =>
        Date.now()
      )
      const controller = new AbortController()
      const pending = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 100 })
      const observed = pending.catch(error => error)
      await jest.advanceTimersByTimeAsync(1)
      if (cause === 'abort') controller.abort()
      else await jest.advanceTimersByTimeAsync(99)
      const failure = await observed
      expect(failure).toBeInstanceOf(AggregateError)
      expect(failure.errors[0].normalized).toMatchObject({
        code: cause === 'deadline' ? 'operation.timed-out' : 'operation.aborted'
      })
      expect(failure.errors[1]).toBeInstanceOf(AggregateError)
      expect(failure.errors[1].errors[0]).toMatchObject({ name: 'BleCleanupError', cleanup })
      expect(stop).toHaveBeenCalledTimes(1)
    } finally {
      jest.useRealTimers()
    }
  }
)

test('a caller abort stays aborted when cleanup finishes after the readiness deadline', async () => {
  jest.useFakeTimers()
  try {
    const source = new CoreBoundedStream(limits, 'latest')
    let finishStop
    const stop = jest.fn(
      () =>
        new Promise(resolve => {
          finishStop = resolve
        })
    )
    const manager = await createPublicBleManager(publicInternal({ initial: state(), values: source, stop }), () =>
      Date.now()
    )
    const controller = new AbortController()
    const pending = manager.adapter.waitUntilReady({ signal: controller.signal, timeoutMs: 100 })
    const observed = pending.catch(error => error)
    await jest.advanceTimersByTimeAsync(1)
    controller.abort()
    await jest.advanceTimersByTimeAsync(200)
    finishStop({ state: 'released', failures: [] })
    await expect(observed).resolves.toMatchObject({ code: 'operation.aborted' })
    expect(stop).toHaveBeenCalledTimes(1)
  } finally {
    jest.useRealTimers()
  }
})
