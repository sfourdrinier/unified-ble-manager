const { attachBackend } = require('../../src/backend-contract/backend')
const { capacity, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { createCoreBluetoothBackendProvider } = require('../../src/backends/corebluetooth/corebluetooth-provider')
const { createBluezBackendProvider } = require('../../src/backends/bluez/bluez-backend-provider')
const { createWinRtBackendProvider } = require('../../src/backends/winrt/winrt-provider')
const { InMemoryCoreBluetoothBoundary } = require('../../test-support/corebluetooth/in-memory-corebluetooth-boundary')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../test-support/bluez/in-memory-bluez-object-manager')

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function completed(value) {
  return { completion: Promise.resolve(value), cancel: async () => 'already-terminal' }
}

function pending(completion) {
  return { completion, cancel: async () => 'cancellation-requested' }
}

class ScanWinRtBoundary {
  constructor() {
    this.state = { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
    this.scanHandler = null
    this.scanToken = null
    this.stopScanCalls = 0
    this.failNextStopScan = false
    this.scanListeners = {
      connection: new Set(),
      database: new Set(),
      terminal: new Set(),
      adapter: new Set()
    }
  }

  adapterSnapshot() {
    return this.state
  }

  listAdapters() {
    return completed([
      {
        nativeAdapterId: 'winrt-deterministic-adapter',
        displayName: 'Deterministic WinRT Adapter',
        state: this.state,
        deployment: 'unpackaged'
      }
    ])
  }

  selectAdapter(adapterId) {
    if (adapterId !== 'winrt-deterministic-adapter') {
      return pending(Promise.reject(new Error('Unknown WinRT adapter')))
    }
    return completed(undefined)
  }

  startScan(scanToken, _uuids, handler) {
    this.scanToken = scanToken
    this.scanHandler = handler
    return completed(undefined)
  }

  stopScan(scanToken) {
    this.stopScanCalls += 1
    if (this.failNextStopScan) {
      this.failNextStopScan = false
      return pending(Promise.reject(new Error('Deterministic WinRT scan stop failure')))
    }
    if (this.scanToken !== scanToken) {
      return pending(Promise.reject(new Error('scan token mismatch')))
    }
    this.scanHandler = null
    this.scanToken = null
    return completed(undefined)
  }

  emitAdvertisement() {
    const handler = this.scanHandler
    if (handler === null) {
      return
    }
    handler({
      scanToken: this.scanToken,
      nativePeerId: 'C0FFEE000001',
      localName: 'Polar H10',
      rssi: -48,
      serviceUuids: [serviceUuid],
      connectable: true
    })
  }

  onConnectionLost(listener) {
    this.scanListeners.connection.add(listener)
    return () => this.scanListeners.connection.delete(listener)
  }

  onDatabaseChanged(listener) {
    this.scanListeners.database.add(listener)
    return () => this.scanListeners.database.delete(listener)
  }

  onScanTerminal(listener) {
    this.scanListeners.terminal.add(listener)
    return () => this.scanListeners.terminal.delete(listener)
  }

  onAdapterState(listener) {
    this.scanListeners.adapter.add(listener)
    return () => this.scanListeners.adapter.delete(listener)
  }

  destroy() {
    return completed(undefined)
  }
}

function scanOptions({ itemCapacity, overflowPolicy, allowSharing, signal = null, deadline = null, filterName = null }) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: filterName },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(itemCapacity),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1),
      overflowPolicy
    },
    deadline,
    signal,
    sharing: { mode: 'owner', allowSharing }
  }
}

async function createCoreBluetooth() {
  let boundary = null
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () => {
      boundary = new InMemoryCoreBluetoothBoundary({ serviceUuid, characteristicUuid })
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
  })
  await attachBackend(backend, compatibility())
  return {
    id: 'corebluetooth',
    backend,
    boundary,
    emitAd: () => boundary.emitAdvertisement(),
    nativeScanActive: () => boundary.scanHandler !== null,
    failNextNativeStop: () => {
      const original = boundary.stopScan.bind(boundary)
      let failures = 1
      boundary.stopScan = async () => {
        if (failures > 0) {
          failures -= 1
          throw new Error('CoreBluetooth scan stop failure')
        }
        return original()
      }
    }
  }
}

async function createWinRt() {
  let boundary = null
  const provider = createWinRtBackendProvider({
    boundaryFactory: () => {
      boundary = new ScanWinRtBoundary()
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('winrt-deterministic-adapter', 'adapter', 'winrt')
  })
  await attachBackend(backend, compatibility())
  return {
    id: 'winrt',
    backend,
    boundary,
    emitAd: () => boundary.emitAdvertisement(),
    nativeScanActive: () => boundary.scanHandler !== null,
    failNextNativeStop: () => {
      boundary.failNextStopScan = true
    }
  }
}

async function createBluez() {
  const adapterPath = '/org/bluez/hci0'
  const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
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
        path: devicePath,
        interfaces: [
          {
            name: BLUEZ_DEVICE_INTERFACE,
            properties: {
              Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
              Alias: { signature: 's', value: 'Polar H10' },
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
    id: 'bluez',
    backend,
    boundary,
    emitAd: () => boundary.queueAdvertisement(),
    nativeScanActive: () => Number(backend.resourceCounters().activeScanControllers) > 0,
    failNextNativeStop: () => undefined
  }
}

const factories = {
  corebluetooth: createCoreBluetooth,
  winrt: createWinRt,
  bluez: createBluez
}

async function flush() {
  for (let turn = 0; turn < 20; turn += 1) {
    await Promise.resolve()
  }
}

describe.each(['corebluetooth', 'winrt', 'bluez'])('%s scan overflow native release', backendId => {
  test('overflow error policy terminalizes the overflowing consumer only', async () => {
    const harness = await factories[backendId]()
    const owner = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 8, overflowPolicy: 'error', allowSharing: true }),
      opaqueId(`${backendId}-overflow-owner`, 'client', `${backendId}:overflow`)
    )
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    const joiner = await harness.backend.scanner.join(
      owner.leaseId,
      owner.shareToken,
      opaqueId(`${backendId}-overflow-joiner`, 'client', `${backendId}:overflow`)
    )
    for (let index = 0; index < 8; index += 1) {
      harness.emitAd()
      await expect(ownerIterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value' } })
    }
    harness.emitAd()
    const joinerNext = await joiner.observations[Symbol.asyncIterator]().next()
    expect(joinerNext.value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    await flush()
    harness.emitAd()
    await expect(ownerIterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value' } })
    expect(harness.nativeScanActive()).toBe(true)
    await owner.stop()
  })

  test('sibling consumer in the same group still receives', async () => {
    const harness = await factories[backendId]()
    const owner = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 8, overflowPolicy: 'error', allowSharing: true }),
      opaqueId(`${backendId}-sibling-owner`, 'client', `${backendId}:overflow`)
    )
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    await harness.backend.scanner.join(
      owner.leaseId,
      owner.shareToken,
      opaqueId(`${backendId}-sibling-joiner`, 'client', `${backendId}:overflow`)
    )
    for (let index = 0; index < 8; index += 1) {
      harness.emitAd()
      await ownerIterator.next()
    }
    harness.emitAd()
    await flush()
    harness.emitAd()
    await expect(ownerIterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value' } })
    await owner.stop()
  })

  test('last consumer overflow stops the native scan or notify owner', async () => {
    const harness = await factories[backendId]()
    const before = harness.backend.resourceCounters()
    const owner = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 1, overflowPolicy: 'error', allowSharing: false }),
      opaqueId(`${backendId}-last-owner`, 'client', `${backendId}:overflow`)
    )
    harness.emitAd()
    harness.emitAd()
    await flush()
    expect(harness.nativeScanActive()).toBe(false)
    expect(Number(harness.backend.resourceCounters().activeScanControllers)).toBe(
      Number(before.activeScanControllers)
    )
    expect(Number(harness.backend.resourceCounters().scanConsumers)).toBe(Number(before.scanConsumers))
    await owner.stop()
  })

  test('failed native stop is retained and retried on a later stop', async () => {
    if (backendId === 'bluez') {
      return
    }
    const harness = await factories[backendId]()
    harness.failNextNativeStop()
    const owner = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 1, overflowPolicy: 'error', allowSharing: false }),
      opaqueId(`${backendId}-retry-owner`, 'client', `${backendId}:overflow`)
    )
    harness.emitAd()
    harness.emitAd()
    await flush()
    expectConsoleErrorMatching(
      backendId === 'corebluetooth'
        ? '[CoreBluetoothBackend.handleAdvertisement] Overflow scan cleanup requires retry:'
        : '[WinRtBackend.handleAdvertisement] Overflow scan cleanup requires retry:',
      expect.arrayContaining([expect.objectContaining({ resourceKind: 'scan' })])
    )
    expect(Number(harness.backend.resourceCounters().activeScanControllers)).toBe(1)
    await expect(owner.stop()).resolves.toMatchObject({ state: 'released' })
    expect(Number(harness.backend.resourceCounters().activeScanControllers)).toBe(0)
  })

  test('later native callbacks do not copy into the dead consumer', async () => {
    const harness = await factories[backendId]()
    const owner = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 8, overflowPolicy: 'error', allowSharing: true }),
      opaqueId(`${backendId}-dead-owner`, 'client', `${backendId}:overflow`)
    )
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    const joiner = await harness.backend.scanner.join(
      owner.leaseId,
      owner.shareToken,
      opaqueId(`${backendId}-dead-joiner`, 'client', `${backendId}:overflow`)
    )
    const emit = jest.spyOn(joiner.observations, 'emit')
    for (let index = 0; index < 8; index += 1) {
      harness.emitAd()
      await ownerIterator.next()
    }
    harness.emitAd()
    await flush()
    emit.mockClear()
    harness.emitAd()
    expect(emit).not.toHaveBeenCalled()
    await owner.stop()
  })

  test('overflow racing abort deadline and explicit stop releases the consumer once', async () => {
    const harness = await factories[backendId]()
    const controller = new AbortController()
    const owner = await harness.backend.scanner.start(
      scanOptions({
        itemCapacity: 1,
        overflowPolicy: 'error',
        allowSharing: false,
        signal: controller.signal,
        deadline: 10_000
      }),
      opaqueId(`${backendId}-race-owner`, 'client', `${backendId}:overflow`)
    )
    harness.emitAd()
    const stopPromise = owner.stop()
    controller.abort()
    harness.emitAd()
    await flush()
    await expect(stopPromise).resolves.toMatchObject({ state: 'released' })
    expect(Number(harness.backend.resourceCounters().scanConsumers)).toBe(0)
  })

  test('resource counters return to their exact pre-consumer values', async () => {
    const harness = await factories[backendId]()
    const before = harness.backend.resourceCounters()
    await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 1, overflowPolicy: 'error', allowSharing: false }),
      opaqueId(`${backendId}-counters-owner`, 'client', `${backendId}:overflow`)
    )
    harness.emitAd()
    harness.emitAd()
    await flush()
    expect(harness.backend.resourceCounters()).toEqual(before)
  })
})

describe('CoreBluetooth notification overflow', () => {
  test('CoreBluetooth final notification overflow disables native notification', async () => {
    const harness = await createCoreBluetooth()
    const scan = await harness.backend.scanner.start(
      scanOptions({ itemCapacity: 4, overflowPolicy: 'drop-oldest', allowSharing: false, filterName: 'Polar' }),
      opaqueId('cb-notify-scan', 'client', 'corebluetooth:overflow')
    )
    harness.emitAd()
    const observation = await scan.observations[Symbol.asyncIterator]().next()
    const peerId = observation.value.value.device.id
    await scan.stop()
    const lease = await harness.backend.connections.connect(
      peerId,
      opaqueId('cb-notify-client', 'client', 'corebluetooth:overflow'),
      { signal: null, deadline: null }
    )
    const database = await harness.backend.gatt.discover(lease.connection, { signal: null, deadline: null })
    const snapshot = await database.snapshot()
    const path = snapshot.characteristics[0].path
    const subscription = await harness.backend.gatt.subscribe(path, {
      operation: { signal: null, deadline: null },
      options: {
        signal: null,
        deadline: null,
        delivery: {
          itemCapacity: capacity(1),
          byteCapacity: capacity(64),
          reservedControlCapacity: capacity(1),
          overflowPolicy: 'error'
        }
      }
    }).completion
    const address = {
      nativePeerId: 'native-polar-h10',
      serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid,
      characteristicOccurrence: 0
    }
    harness.boundary.emitNotification(address, new Uint8Array([1]))
    harness.boundary.emitNotification(address, new Uint8Array([2]))
    await flush()
    expect(harness.boundary.stopNotifyCalls).toBe(1)
    expect(harness.boundary.notificationHandlers.size).toBe(0)
    await harness.backend.gatt.unsubscribe(subscription, { signal: null, deadline: null }).completion
    await lease.release()
    await harness.backend.destroy()
  })
})
