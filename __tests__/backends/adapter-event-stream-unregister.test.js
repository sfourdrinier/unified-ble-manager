const { attachBackend } = require('../../src/backend-contract/backend')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { createCoreBluetoothBackendProvider } = require('../../src/backends/corebluetooth/corebluetooth-provider')
const { inspectCoreBluetoothStreamOwnershipForTests } = require('../../src/backends/corebluetooth/corebluetooth-backend')
const { createBluezBackendProvider } = require('../../src/backends/bluez/bluez-backend-provider')
const { inspectBluezStreamOwnershipForTests } = require('../../src/backends/bluez/bluez-backend')
const { WinRtBackend, inspectWinRtStreamOwnershipForTests } = require('../../src/backends/winrt/winrt-backend')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { inspectDeterministicStreamOwnershipForTests } = require('../../src/testing/deterministic/deterministic-backend-base')
const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryCoreBluetoothBoundary } = require('../../test-support/corebluetooth/in-memory-corebluetooth-boundary')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../test-support/bluez/in-memory-bluez-object-manager')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function emptyOwnership() {
  return { stateWatchers: 0, eventStreams: 0 }
}

function ownership(stateWatchers, eventStreams) {
  return { stateWatchers, eventStreams }
}

async function createCoreBluetoothHarness() {
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () =>
      new InMemoryCoreBluetoothBoundary({
        serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
        characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
      }),
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
  })
  await attachBackend(backend, compatibility())
  return {
    backend,
    inspect: () => inspectCoreBluetoothStreamOwnershipForTests(backend)
  }
}

async function createBluezHarness() {
  const adapterPath = '/org/bluez/hci0'
  const boundary = new InMemoryBluezBoundary({
    objects: [
      {
        path: adapterPath,
        interfaces: [
          {
            name: BLUEZ_ADAPTER_INTERFACE,
            properties: {
              Address: { signature: 's', value: '00:11:22:33:44:55' },
              Alias: { signature: 's', value: 'primary' },
              Powered: { signature: 'b', value: true }
            }
          }
        ]
      },
      {
        path: `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`,
        interfaces: [
          {
            name: BLUEZ_DEVICE_INTERFACE,
            properties: {
              Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
              Alias: { signature: 's', value: 'peer' },
              RSSI: { signature: 'n', value: -40 },
              UUIDs: { signature: 'as', value: [] },
              Connected: { signature: 'b', value: false },
              ServicesResolved: { signature: 'b', value: false },
              Paired: { signature: 'b', value: false }
            }
          }
        ]
      }
    ]
  })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 100
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  await attachBackend(backend, compatibility())
  return {
    backend,
    inspect: () => inspectBluezStreamOwnershipForTests(backend)
  }
}

function createWinRtHarness() {
  const adapterState = { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
  const backend = new WinRtBackend(
    {
      adapterSnapshot: () => adapterState,
      onConnectionLost: () => () => undefined,
      onDatabaseChanged: () => () => undefined,
      onScanTerminal: () => () => undefined,
      onAdapterState: () => () => undefined
    },
    { nativeAdapterId: 'adapter', displayName: 'adapter', state: adapterState, deployment: 'unpackaged' },
    () => 1,
    'node'
  )
  return {
    backend,
    inspect: () => inspectWinRtStreamOwnershipForTests(backend)
  }
}

function createDeterministicHarness() {
  const fixture = createDeterministicTestBackend()
  return {
    backend: fixture.backend,
    fixture,
    inspect: () => inspectDeterministicStreamOwnershipForTests(fixture.backend)
  }
}

async function createWebHarness() {
  let available = true
  const timers = new Set()
  const boundary = {
    implementationVersion: 'adapter-event-unregister',
    browserEngine: 'mock-engine',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => available,
    requestDevice: async () => {
      throw new Error('chooser unused')
    },
    now: () => 10,
    setTimer: callback => {
      const handle = { callback }
      timers.add(handle)
      return handle
    },
    clearTimer: handle => timers.delete(handle),
    addPageLifecycleListener: () => () => undefined
  }
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  return {
    backend,
    setAvailable: value => {
      available = value
    }
  }
}

async function overflowEvents(backend) {
  const stream = backend.events()
  const capacity = Number(stream.limits.itemCapacity)
  for (let index = 0; index < capacity + 1; index += 1) {
    stream.emit(
      {
        attachment: {
          attachmentId: 'a',
          backendInstanceId: 'b',
          backendGeneration: 'g',
          adapter: { adapterId: 'adapter', adapterGeneration: 'ag', state: { availability: 'available' } }
        },
        attachmentId: 'a',
        kind: 'generic',
        ingressOrdinal: index + 1
      },
      128
    )
  }
  return stream
}

describe('adapter and event stream unregister', () => {
  test('corebluetooth state and event close unregisters', async () => {
    const harness = await createCoreBluetoothHarness()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    expect(harness.inspect()).toEqual(ownership(1, 1))
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
  })

  test('bluez state and event close unregisters', async () => {
    const harness = await createBluezHarness()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    expect(harness.inspect()).toEqual(ownership(1, 1))
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
  })

  test('deterministic state and event close unregisters', async () => {
    const harness = createDeterministicHarness()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    expect(harness.inspect()).toEqual(ownership(1, 1))
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
  })

  test('deterministic counts return to zero after close', async () => {
    const harness = createDeterministicHarness()
    const before = harness.backend.resourceCounters()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
    expect(harness.backend.resourceCounters().retainedByteBuffers).toBe(before.retainedByteBuffers)
  })

  test('terminal reason unregisters', async () => {
    const harnesses = [
      await createCoreBluetoothHarness(),
      await createBluezHarness(),
      createDeterministicHarness(),
      createWinRtHarness()
    ]
    for (const harness of harnesses) {
      const watch = await harness.backend.adapter.watchState()
      const events = harness.backend.events()
      watch.transitions.closeWithReason('source-failed')
      events.closeWithReason('source-failed')
      expect(harness.inspect()).toEqual(emptyOwnership())
    }
  })

  test('overflow terminal unregisters', async () => {
    const harnesses = [
      await createCoreBluetoothHarness(),
      await createBluezHarness(),
      createDeterministicHarness(),
      createWinRtHarness()
    ]
    for (const harness of harnesses) {
      await overflowEvents(harness.backend)
      expect(harness.inspect().eventStreams).toBe(0)
    }
  })

  test('web adapter/event close remains unregistered (control)', async () => {
    const harness = await createWebHarness()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    await watch.transitions.close()
    await events.close()
    const emit = jest.spyOn(CoreBoundedStream.prototype, 'emit')
    emit.mockClear()
    harness.setAvailable(false)
    await harness.backend.adapter.currentState()
    expect(emit).not.toHaveBeenCalled()
    emit.mockRestore()
    await harness.backend.destroy()
  })

  test('winrt adapter/event close remains unregistered (control)', async () => {
    const harness = createWinRtHarness()
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    expect(harness.inspect()).toEqual(ownership(1, 1))
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
  })

  test('repeated watch and event-stream churn keeps every registry bounded', async () => {
    const harnesses = [
      await createCoreBluetoothHarness(),
      await createBluezHarness(),
      createDeterministicHarness(),
      createWinRtHarness()
    ]
    for (const harness of harnesses) {
      for (let index = 0; index < 8; index += 1) {
        const watch = await harness.backend.adapter.watchState()
        const events = harness.backend.events()
        await watch.transitions.close()
        await events.close()
      }
      expect(harness.inspect()).toEqual(emptyOwnership())
    }
  })

  test('deterministic aggregate quota and zero-resource evidence include stream ownership', async () => {
    const harness = createDeterministicHarness()
    const before = harness.backend.resourceCounters()
    expect(harness.inspect()).toEqual(emptyOwnership())
    const watch = await harness.backend.adapter.watchState()
    const events = harness.backend.events()
    expect(harness.inspect()).toEqual(ownership(1, 1))
    expect(Number(harness.backend.resourceCounters().retainedByteBuffers)).toBeGreaterThan(
      Number(before.retainedByteBuffers)
    )
    await watch.transitions.close()
    await events.close()
    expect(harness.inspect()).toEqual(emptyOwnership())
    expect(harness.backend.resourceCounters()).toMatchObject({
      retainedByteBuffers: before.retainedByteBuffers
    })
  })

  test('core retry after release-failed backend close unregisters only after success', async () => {
    const harness = createDeterministicHarness()
    const events = harness.backend.events()
    const originalClose = CoreBoundedStream.prototype.close
    let attempts = 0
    CoreBoundedStream.prototype.close = function closeForRetry() {
      attempts += 1
      if (attempts === 1) {
        return Promise.resolve({
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'stream',
              error: {
                code: 'platform.failure',
                domain: 'cleanup',
                operation: 'stream.close',
                platform: null,
                retryability: 'never'
              }
            }
          ]
        })
      }
      return originalClose.call(this)
    }
    try {
      await expect(events.close()).resolves.toMatchObject({ state: 'release-failed' })
      expect(harness.inspect()).toEqual(ownership(0, 1))
      await expect(events.close()).resolves.toMatchObject({ state: 'released' })
      expect(harness.inspect()).toEqual(emptyOwnership())
      expect(attempts).toBe(2)
    } finally {
      CoreBoundedStream.prototype.close = originalClose
    }
  })
})
