// __tests__/backends/same-peer-join.test.js
//
// FX1B (RV1 finding 1): same-peer connect answers the same way on every
// backend. For every backend this harness can drive, a second connect to a
// connected peer JOINS the live link: generations differ, physicalLinks stays
// 1 with 2 leases, the first lease keeps working after the joiner releases,
// a link drop ends every lease, and teardown leaves no counters behind.
// A backend that answers `connection.already-owned` for a live same-process
// link fails this test.

const { attachBackend } = require('../../src/backend-contract/backend')
const {
  capacity,
  opaqueId,
  version,
  versionRange
} = require('../../src/backend-contract/primitives')
const {
  createDeterministicTestBackend
} = require('../../src/testing/deterministic/deterministic-test-backend')
const {
  createCoreBluetoothBackendProvider
} = require('../../src/backends/corebluetooth/corebluetooth-provider')
const {
  InMemoryCoreBluetoothBoundary
} = require('../../test-support/corebluetooth/in-memory-corebluetooth-boundary')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function operation(signal = null) {
  return { signal, deadline: null }
}

function delivery(itemCapacity = 4) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(4096),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
}

function client(name, scope) {
  return opaqueId(name, 'client', scope)
}

async function flushMicrotasks(turns = 16) {
  for (let turn = 0; turn < turns; turn += 1) {
    await Promise.resolve()
  }
}

async function settleDeterministic(controller, promise) {
  let settled = false
  promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  ).catch(() => undefined)
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

function scanOptions() {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: true }
  }
}

async function observeFirstPeer(backend, emit) {
  const scan = await backend.scanner.start(scanOptions(), client('same-peer-join-observer', 'scan'))
  const observation = scan.observations[Symbol.asyncIterator]().next()
  await emit()
  const observed = await observation
  await scan.stop()
  if (observed.done || observed.value.kind !== 'value') {
    throw new Error('same-peer-join fixture did not observe a peer')
  }
  return observed.value.value.device.id
}

// --- deterministic leg (reference join semantics) ---

async function deterministicLeg() {
  const fixture = createDeterministicTestBackend()
  await attachBackend(fixture.backend, compatibility())
  const peerId = opaqueId('deterministic-peer', 'peer', 'deterministic')
  return {
    label: 'deterministic',
    backend: fixture.backend,
    peerId,
    connect: (clientId, options) => settleDeterministic(fixture.controller, fixture.backend.connections.connect(peerId, clientId, options)),
    release: lease => settleDeterministic(fixture.controller, lease.release()),
    discover: connection => settleDeterministic(fixture.controller, fixture.backend.gatt.discover(connection, operation())),
    dropLink: () => fixture.controller.forceDisconnect(peerId),
    settle: () => settleDeterministic(fixture.controller, Promise.resolve()),
    destroy: () => settleDeterministic(fixture.controller, fixture.backend.destroy())
  }
}

// --- CoreBluetooth leg (direct; also covers React Native/Android shares) ---

async function coreBluetoothLeg() {
  let boundary = null
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () => {
      boundary = new InMemoryCoreBluetoothBoundary({
        serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
        characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
      })
      return boundary
    },
    now: () => 20
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
  })
  await attachBackend(backend, compatibility())
  const peerId = await observeFirstPeer(backend, async () => boundary.emitAdvertisement())
  return {
    label: 'corebluetooth',
    backend,
    peerId,
    connect: (clientId, options) => backend.connections.connect(peerId, clientId, options),
    release: lease => lease.release(),
    discover: connection => backend.gatt.discover(connection, operation()),
    dropLink: async () => boundary.forceDisconnect('native-polar-h10'),
    settle: () => flushMicrotasks(),
    destroy: () => backend.destroy()
  }
}

// --- BlueZ leg ---

const {
  createBluezBackendProvider
} = require('../../src/backends/bluez/bluez-backend-provider')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
  BLUEZ_GATT_DESCRIPTOR_INTERFACE,
  BLUEZ_GATT_SERVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../test-support/bluez/in-memory-bluez-object-manager')

const bluezAdapterPath = '/org/bluez/hci0'
const bluezDevicePath = `${bluezAdapterPath}/dev_AA_BB_CC_DD_EE_FF`
const bluezServiceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const bluezCharacteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const bluezDescriptorUuid = '00002902-0000-1000-8000-00805f9b34fb'

function bluezServiceObject(path) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_SERVICE_INTERFACE,
        properties: {
          Device: { signature: 'o', value: bluezDevicePath },
          UUID: { signature: 's', value: bluezServiceUuid },
          Primary: { signature: 'b', value: true }
        }
      }
    ]
  }
}

function bluezCharacteristicObject(path, service) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        properties: {
          Service: { signature: 'o', value: service },
          UUID: { signature: 's', value: bluezCharacteristicUuid },
          Flags: { signature: 'as', value: ['read', 'write', 'notify'] },
          Value: { signature: 'ay', value: new Uint8Array([1]) },
          Notifying: { signature: 'b', value: false }
        }
      }
    ]
  }
}

function bluezManagedObjects() {
  const service0 = `${bluezDevicePath}/service0001`
  const service1 = `${bluezDevicePath}/service0002`
  const characteristic0 = `${service0}/char0001`
  const characteristic1 = `${service1}/char0001`
  return [
    {
      path: bluezAdapterPath,
      interfaces: [
        {
          name: BLUEZ_ADAPTER_INTERFACE,
          properties: {
            Address: { signature: 's', value: '00:11:22:33:44:55' },
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true },
            Discovering: { signature: 'b', value: false }
          }
        }
      ]
    },
    {
      path: bluezDevicePath,
      interfaces: [
        {
          name: BLUEZ_DEVICE_INTERFACE,
          properties: {
            Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
            AddressType: { signature: 's', value: 'random' },
            Alias: { signature: 's', value: 'Polar H10' },
            RSSI: { signature: 'n', value: -48 },
            UUIDs: { signature: 'as', value: [bluezServiceUuid] },
            Connected: { signature: 'b', value: true },
            ServicesResolved: { signature: 'b', value: true }
          }
        }
      ]
    },
    bluezServiceObject(service0),
    bluezServiceObject(service1),
    bluezCharacteristicObject(characteristic0, service0),
    bluezCharacteristicObject(characteristic1, service1),
    {
      path: `${characteristic0}/desc0001`,
      interfaces: [
        {
          name: BLUEZ_GATT_DESCRIPTOR_INTERFACE,
          properties: {
            Characteristic: { signature: 'o', value: characteristic0 },
            UUID: { signature: 's', value: bluezDescriptorUuid }
          }
        }
      ]
    }
  ]
}

async function bluezLeg() {
  const boundary = new InMemoryBluezBoundary({ objects: bluezManagedObjects() })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 20
  })
  const backend = await provider.create({ selectedAdapterId: bluezAdapterPath })
  await attachBackend(backend, compatibility())
  const peerId = await observeFirstPeer(backend, async () => flushMicrotasks())
  return {
    label: 'bluez',
    backend,
    peerId,
    connect: (clientId, options) => backend.connections.connect(peerId, clientId, options),
    release: lease => lease.release(),
    discover: connection => backend.gatt.discover(connection, operation()),
    dropLink: async () => {
      boundary.objectManager.emitPropertiesChanged(bluezDevicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: false }
      })
      await flushMicrotasks()
    },
    settle: () => flushMicrotasks(),
    destroy: () => backend.destroy()
  }
}

// --- Web leg ---

const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')

const webServiceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const webCharacteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'

function webBoundaryFixture() {
  const disconnectListeners = new Set()
  const characteristic = {
    uuid: webCharacteristicUuid,
    properties: { read: true, write: false, writeWithoutResponse: false, notify: false, indicate: false },
    getDescriptors: async () => [],
    readValue: async () => new Uint8Array([0, 70])
  }
  const service = {
    uuid: webServiceUuid,
    getCharacteristics: async () => [characteristic]
  }
  const gatt = {
    connected: false,
    connectCalls: 0,
    disconnectCalls: 0,
    connect: async () => {
      gatt.connectCalls += 1
      gatt.connected = true
    },
    disconnect: () => {
      gatt.disconnectCalls += 1
      gatt.connected = false
    },
    getPrimaryServices: async () => [service]
  }
  const device = {
    id: 'browser-secret-device',
    gatt,
    addDisconnectListener: listener => disconnectListeners.add(listener),
    removeDisconnectListener: listener => disconnectListeners.delete(listener)
  }
  const selection = { device, grantedServices: [webServiceUuid] }
  return {
    device,
    gatt,
    disconnectListeners,
    selection,
    boundary: {
      implementationVersion: 'same-peer-join-test',
      browserEngine: 'test',
      isSecureContext: () => true,
      hasTransientUserActivation: () => true,
      bluetoothAvailable: async () => true,
      requestDevice: async () => selection,
      now: () => 10,
      setTimer: callback => ({ callback }),
      clearTimer: () => {},
      addPageLifecycleListener: () => () => {}
    }
  }
}

async function webLeg() {
  const testFixture = webBoundaryFixture()
  const provider = createWebBluetoothProvider(testFixture.boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const selected = await backend.choose(
    {
      filters: [{ serviceUuids: [webServiceUuid], manufacturerData: [], localNamePrefix: null }],
      acceptAllDevices: false,
      optionalServices: [webServiceUuid]
    },
    client('same-peer-join-chooser', 'web:chooser'),
    operation()
  )
  return {
    label: 'web',
    backend,
    testFixture,
    peerId: selected.peerId,
    connect: (clientId, options) => backend.connections.connect(selected.peerId, clientId, options),
    release: lease => lease.release(),
    discover: connection => backend.gatt.discover(connection, operation()),
    dropLink: async () => {
      // The browser fires gattserverdisconnected on a dropped link.
      for (const listener of [...testFixture.disconnectListeners]) listener()
      await flushMicrotasks()
    },
    settle: () => flushMicrotasks(),
    destroy: () => backend.destroy()
  }
}

// --- WinRT leg (compact deterministic boundary) ---

const { createWinRtBackendProvider } = require('../../src/backends/winrt/winrt-provider')

const winrtServiceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const winrtCharacteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const winrtNativePeerId = 'C0FFEE000001'

function winrtCompleted(value) {
  return { completion: Promise.resolve(value), cancel: async () => 'already-terminal' }
}

class CompactWinRtBoundary {
  constructor() {
    this.connected = new Set()
    this.connectionGenerations = new Map()
    this.disconnectCalls = 0
    this.destroyed = false
    this.scanToken = null
    this.scanHandler = null
    this.connectionListeners = new Set()
    this.databaseListeners = new Set()
    this.adapterListeners = new Set()
    this.scanTerminalListeners = new Set()
    this.state = { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
  }

  listAdapters() {
    return winrtCompleted([
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
      throw new Error('Unknown WinRT adapter')
    }
    return winrtCompleted(undefined)
  }

  adapterSnapshot() {
    return this.state
  }

  startScan(scanToken, _serviceUuids, handler) {
    this.scanToken = scanToken
    this.scanHandler = handler
    return winrtCompleted(undefined)
  }

  stopScan(scanToken) {
    if (this.scanToken !== scanToken) {
      throw new Error('WinRT scan token mismatch during stop')
    }
    this.scanHandler = null
    this.scanToken = null
    for (const listener of this.scanTerminalListeners) {
      listener({ scanToken, status: 'stopped', error: 'success' })
    }
    return winrtCompleted(undefined)
  }

  onScanTerminal(listener) {
    this.scanTerminalListeners.add(listener)
    return () => this.scanTerminalListeners.delete(listener)
  }

  emitAdvertisement() {
    if (this.scanHandler === null) {
      throw new Error('WinRT advertisement emitted with no scan')
    }
    this.scanHandler({
      scanToken: this.scanToken,
      nativePeerId: winrtNativePeerId,
      localName: 'Polar H10',
      rssi: -47,
      serviceUuids: [winrtServiceUuid],
      connectable: true
    })
  }

  connect(nativePeerId, connectionGeneration) {
    if (nativePeerId !== winrtNativePeerId) {
      throw new Error('Unknown deterministic WinRT peer')
    }
    this.connected.add(nativePeerId)
    this.connectionGenerations.set(nativePeerId, connectionGeneration)
    return winrtCompleted(undefined)
  }

  disconnect(nativePeerId) {
    this.disconnectCalls += 1
    this.connected.delete(nativePeerId)
    return winrtCompleted(undefined)
  }

  discover(nativePeerId) {
    if (!this.connected.has(nativePeerId)) {
      return { completion: Promise.reject(new Error('WinRT discovery requires an active connection')), cancel: async () => 'not-cancellable' }
    }
    return winrtCompleted({
      cacheMode: 'uncached',
      services: [
        {
          uuid: winrtServiceUuid,
          occurrence: 0,
          characteristics: [
            {
              uuid: winrtCharacteristicUuid,
              occurrence: 0,
              readable: true,
              writableWithResponse: true,
              writableWithoutResponse: false,
              notifiable: false,
              indicatable: false,
              descriptors: []
            }
          ]
        }
      ]
    })
  }

  onConnectionLost(listener) {
    this.connectionListeners.add(listener)
    return () => this.connectionListeners.delete(listener)
  }

  onDatabaseChanged(listener) {
    this.databaseListeners.add(listener)
    return () => this.databaseListeners.delete(listener)
  }

  onAdapterState(listener) {
    this.adapterListeners.add(listener)
    return () => this.adapterListeners.delete(listener)
  }

  emitConnectionLoss(connectionGeneration) {
    for (const listener of this.connectionListeners) {
      listener({ nativePeerId: winrtNativePeerId, connectionGeneration, safeReason: null })
    }
  }

  ingressTelemetry() {
    return { pendingOperations: 0, retainedBytes: 0 }
  }

  destroy() {
    this.destroyed = true
    return winrtCompleted(undefined)
  }
}

async function winrtLeg() {
  let boundary = null
  const provider = createWinRtBackendProvider({
    boundaryFactory: () => {
      boundary = new CompactWinRtBoundary()
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('winrt-deterministic-adapter', 'adapter', 'winrt')
  })
  await attachBackend(backend, compatibility())
  const peerId = await observeFirstPeer(backend, async () => boundary.emitAdvertisement())
  return {
    label: 'winrt',
    backend,
    boundary,
    peerId,
    connect: (clientId, options) => backend.connections.connect(peerId, clientId, options),
    release: lease => lease.release(),
    discover: connection => backend.gatt.discover(connection, operation()),
    dropLink: async generation => {
      boundary.emitConnectionLoss(generation)
      await flushMicrotasks()
    },
    settle: () => flushMicrotasks(),
    destroy: () => backend.destroy()
  }
}

// --- React Native/Android leg (legacy boundary + deterministic native) ---

const {
  createReactNativeAndroidBackendProvider,
  reactNativeAndroidDefaultAdapterId
} = require('../../src/backends/reactnative/react-native-android-provider')
const {
  DeterministicNativeControl,
  DeterministicReactNativeProtocolRuntime
} = require('../../test-support/react-native/deterministic-legacy-native-protocol')

async function reactNativeAndroidLeg() {
  const control = new DeterministicNativeControl(true)
  const runtime = new DeterministicReactNativeProtocolRuntime(control, false)
  globalThis.__unifiedBleNativeProtocolV2 = runtime
  try {
    const provider = createReactNativeAndroidBackendProvider({ control, now: () => 1 })
    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    await attachBackend(backend, compatibility())
    const peerId = await observeFirstPeer(backend, async () => runtime.emitAdvertisement())
    return {
      label: 'react-native-android',
      backend,
      peerId,
      connect: (clientId, options) => backend.connections.connect(peerId, clientId, options),
      release: lease => lease.release(),
      discover: connection => backend.gatt.discover(connection, operation()),
      dropLink: async () => {
        if (runtime.connection === null) {
          throw new Error('same-peer-join has no native Android connection to lose')
        }
        runtime.emitEvent('connectionLost', [{ id: 7, value: runtime.connection }])
        await flushMicrotasks()
      },
      settle: () => flushMicrotasks(),
      destroy: async () => {
        const cleanup = await backend.destroy()
        delete globalThis.__unifiedBleNativeProtocolV2
        return cleanup
      }
    }
  } catch (error) {
    delete globalThis.__unifiedBleNativeProtocolV2
    throw error
  }
}

// --- shared scenario: one answer on every backend ---

async function assertSamePeerJoin(leg) {
  const { backend } = leg
  const scope = `same-peer-join:${leg.label}`
  const owner = await leg.connect(client('owner-client', scope), operation())
  const joiner = await leg.connect(client('joiner-client', scope), operation())
  expect(String(joiner.connection.connectionGeneration)).not.toBe(
    String(owner.connection.connectionGeneration)
  )
  expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 2, physicalLinks: 1 })
  await expect(leg.release(joiner)).resolves.toMatchObject({ state: 'released', failures: [] })
  expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 1, physicalLinks: 1 })
  // The first lease keeps working after the joiner releases.
  const database = await leg.discover(owner.connection)
  expect((await database.snapshot()).characteristics.length).toBeGreaterThan(0)
  // A link drop ends every remaining lease.
  const joiner2 = await leg.connect(client('joiner-2-client', scope), operation())
  expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 2, physicalLinks: 1 })
  await leg.dropLink(String(owner.connection.connectionGeneration))
  await leg.settle()
  await flushMicrotasks()
  expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 0, physicalLinks: 0 })
  await expect(leg.release(owner)).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(leg.release(joiner2)).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(leg.destroy()).resolves.toMatchObject({ state: 'released', failures: [] })
  expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 0, physicalLinks: 0 })
}

describe('same-peer connect joins on every backend (FX1B)', () => {
  test.each([
    ['deterministic', deterministicLeg],
    ['corebluetooth', coreBluetoothLeg],
    ['bluez', bluezLeg],
    ['web', webLeg],
    ['winrt', winrtLeg],
    ['react-native-android', reactNativeAndroidLeg]
  ])('%s: second connect leases the live link', async (_label, buildLeg) => {
    const leg = await buildLeg()
    try {
      await assertSamePeerJoin(leg)
    } finally {
      delete globalThis.__unifiedBleNativeProtocolV2
    }
  })
})


