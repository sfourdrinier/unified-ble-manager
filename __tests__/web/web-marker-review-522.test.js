const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createPublicGattDatabase } = require('../../src/public/gatt')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { webcrypto } = require('crypto')
const priorKeyRange = global.IDBKeyRange
beforeEach(() => {
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
})
afterEach(() => {
  global.IDBKeyRange = priorKeyRange
})

const { parseOrderedRecordingStartResult } = require('../../src/core/ordered-recording-start')

function locks() {
  const held = new Set()
  return {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
}

async function publicFixture(overrides = {}) {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  if (overrides.writable) {
    boundary.characteristic.properties = { ...boundary.characteristic.properties, write: true }
    delete overrides.writable
  }
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners,
    ...overrides
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  const attachedBackend = await attachBleBackend(backend, provider.descriptor.compatibility)
  const manager = await createBleManager(
    {
      attachedBackend,
      clientId: opaqueId('public-recording-client', 'client', 'web'),
      managerId: opaqueId('public-recording-manager', 'manager', 'web'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attachedBackend),
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => boundary.now() }
  )
  const choosing = backend.choose(
    { filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] },
    { signal: null, deadline: null }
  )
  await boundary.flush()
  boundary.resolveChooser()
  const peer = await choosing
  const connection = await manager.connect(peer.peerId, { signal: null, deadline: null })
  const database = await createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const characteristic = database.characteristic(boundary.serviceUuid, boundary.characteristicUuid)
  let token = 0
  const controller = await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return { boundary, storage, liveOwners, manager, connection, database, characteristic, controller }
}
const recording = id => ({ id, maxBytes: 1048576, maxRecords: 20 })

const bridgeModule = require('../../src/web/web-authority-recording-journal-bridge')

afterEach(() => jest.restoreAllMocks())

async function startRecording(f, id, maxRecords = 20) {
  const start = f.database.reserveOrderedRecordingStart()
  await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
    recording: { id, maxBytes: 1048576, maxRecords }
  })
  return start
}

async function waitFor(predicate) {
  for (let i = 0; i < 200; i++) {
    if (await predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error('condition did not settle')
}

test('capacity reports waiting, then the worker cause after retained work drains', async () => {
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'capacity-522', 1)
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    await waitFor(() =>
      f.liveOwners.use(identity, async owner => owner.supervisor.status().state === 'waiting-capacity')
    )
    const stopped = await f.controller.stopOrdered('capacity-522')
    expect(stopped.recording).toMatchObject({ state: 'waiting-capacity', error: null })
    let outcome = stopped
    let counter = 0
    while (outcome.recording.state === 'waiting-capacity' && counter++ < 10) {
      const batch = await f.controller.prepareOrdered('capacity-522', { maxItems: 8, maxBytes: 10000 })
      if (batch.token !== null) await f.controller.acknowledge('capacity-522', batch.token)
      outcome = await f.controller.stopOrdered('capacity-522')
    }
    expect(outcome.recording).toMatchObject({ state: 'failed', error: { kind: 'worker-failed' } })
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test.each(['observeTerminal', 'releaseAuthority'])('stop exposes %s failure and retries cleanup', async method => {
  const original = bridgeModule.createWebAuthorityRecordingJournalBridge
  let fail = true
  jest.spyOn(bridgeModule, 'createWebAuthorityRecordingJournalBridge').mockImplementation((...args) => {
    const bridge = original(...args)
    return {
      ...bridge,
      [method]: async (...params) => {
        if (fail) throw new Error('injected finalization failure')
        return bridge[method](...params)
      }
    }
  })
  const f = await publicFixture()
  try {
    await startRecording(f, `cleanup-${method}`)
    const stopped = await f.controller.stopOrdered(`cleanup-${method}`)
    expect(stopped).toMatchObject({
      recording: method === 'observeTerminal'
        ? { state: 'failed', error: { kind: 'storage', storage: { kind: 'reconciliation-unknown' } } }
        : { state: 'finished', error: null },
      hostCleanup: { state: 'release-failed' }
    })
    fail = false
    expect(await f.controller.stopOrdered(`cleanup-${method}`)).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
  } finally {
    await f.manager.destroy()
  }
})

test('forget after a pending write and stop retires the owner and permits clear', async () => {
  const f = await publicFixture({ writable: true })
  let resolveWrite
  f.boundary.characteristic.writeValueWithResponse = jest.fn(
    () =>
      new Promise(resolve => {
        resolveWrite = resolve
      })
  )
  try {
    const start = await startRecording(f, 'forget-522')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    expect(await f.controller.stopOrdered('forget-522')).toMatchObject({ hostCleanup: { state: 'pending' } })
    await expect(marker.forget()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    resolveWrite()
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    await waitFor(async () => (await f.controller.stopOrdered('forget-522')).hostCleanup.state === 'released')
    const batch = await f.controller.prepareOrdered('forget-522', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.some(row => row.record.kind === 'marker')).toBe(true)
    await f.controller.acknowledge('forget-522', batch.token)
    await expect(f.controller.clear('forget-522')).rejects.toMatchObject({ code: 'capability.unavailable' })
    await marker.forget()
    const before = f.storage.rows('recordings')[0]
    expect(await f.controller.clear('forget-522')).toEqual({ cleared: true, records: 0 })
    expect(f.storage.rows('markerLedger')[0].request.bytes).toEqual([1])
    const recovery = require('../../src/web/web-recording-recovery').createWebRecordingRecovery(f.storage, {
      bound: (id, incarnation) => ({ id, incarnation })
    })
    const evidence = await recovery.inspect('forget-522', before.producerIncarnation)
    expect(evidence).toMatchObject({
      recordingDisposition: 'cleared-marker-evidence-retained',
      markerLedgerBytes: before.markerLedgerBytes,
      markers: [{ retired: true, request: { bytes: [1] } }]
    })
    const web = require('../../src/web')
    const previousRange = global.IDBKeyRange
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    try {
      const partition = web.createWebRecordingPartition({
        database: f.storage,
        crypto: webcrypto,
        digest: async () => 'x'
      })
      const inspector = await web.createWebRecordingRecoveryInspector(partition)
      const identities = await inspector.listIncarnations('forget-522', { after: null, limit: 4 })
      expect(identities.active).toBeNull()
      expect(identities.retained).toEqual([before.producerIncarnation])
      const incarnation = identities.retained[0]
      const operations = await inspector.listMarkerOperations('forget-522', incarnation, { after: null, limit: 8 })
      expect(new Set(operations.operations.map(item => item.transition.kind))).toEqual(
        new Set(['reserve', 'admit', 'dispatch', 'settle', 'ack', 'retire'])
      )
      expect(
        operations.operations.every(item => item.signature === 'unverified' && item.result.request.bytes[0] === 1)
      ).toBe(true)
      let disposed
      do {
        disposed = await inspector.disposeRetained('forget-522', incarnation, { acknowledgeExport: true, limit: 2 })
      } while (disposed.state === 'more')
      expect(disposed.state).toBe('disposed')
      expect(f.storage.rows('markerLedger')).toHaveLength(0)
      expect(f.storage.rows('markerOperations')).toHaveLength(0)
    } finally {
      global.IDBKeyRange = previousRange
    }
  } finally {
    await f.manager.destroy()
  }
})

test('forget racing final observation waits for stop and retires without a second stop', async () => {
  const original = bridgeModule.createWebAuthorityRecordingJournalBridge
  let releaseObservation
  let entered = false
  jest.spyOn(bridgeModule, 'createWebAuthorityRecordingJournalBridge').mockImplementation((...args) => {
    const bridge = original(...args)
    return {
      ...bridge,
      observeTerminal: async observation => {
        entered = true
        await new Promise(resolve => {
          releaseObservation = resolve
        })
        return bridge.observeTerminal(observation)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'race-522')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    const stop = f.controller.stopOrdered('race-522')
    await waitFor(() => entered)
    const forgotten = marker.forget()
    await expect(f.controller.clear('race-522')).rejects.toMatchObject({ code: 'capability.unavailable' })
    releaseObservation()
    expect(await stop).toMatchObject({ hostCleanup: { state: 'released' } })
    await forgotten
    const batch = await f.controller.prepareOrdered('race-522', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('race-522', batch.token)
    expect(await f.controller.clear('race-522')).toEqual({ cleared: true, records: 0 })
  } finally {
    await f.manager.destroy()
  }
})

const {
  createWebBleManager,
  createWebBleManagerWithEnvironment,
  createWebRecordingPartition
} = require('unified-ble-manager/web')
const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'

async function fixture(recordingPartition, useDefaultFactory = false) {
  const listeners = new Set()
  const disconnectListeners = new Set()
  let lifecycle
  const characteristic = {
    uuid: CHARACTERISTIC,
    properties: { notify: true, write: true },
    value: null,
    getDescriptors: async () => [],
    startNotifications: jest.fn(async () => characteristic),
    stopNotifications: jest.fn(async () => characteristic),
    writeValueWithResponse: jest.fn(async () => {}),
    addEventListener: (_type, listener) => listeners.add(listener),
    removeEventListener: (_type, listener) => listeners.delete(listener)
  }
  const service = { uuid: SERVICE, getCharacteristics: async () => [characteristic] }
  const gatt = {
    connected: false,
    connect: async () => {
      gatt.connected = true
      return gatt
    },
    disconnect: () => {
      gatt.connected = false
      for (const listener of disconnectListeners) listener()
    },
    getPrimaryServices: async () => [service]
  }
  const device = {
    id: 'recording-boundary-device',
    gatt,
    addEventListener: (_type, listener) => disconnectListeners.add(listener),
    removeEventListener: (_type, listener) => disconnectListeners.delete(listener)
  }
  const environment = {
    implementationVersion: 'test',
    browserEngine: 'test',
    bluetooth: { getAvailability: async () => true, requestDevice: async () => device },
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    now: () => 10,
    setTimer: callback => ({ callback }),
    clearTimer: () => {},
    addPageLifecycleListener: listener => {
      lifecycle = listener
      return () => {
        lifecycle = undefined
      }
    }
  }
  if (useDefaultFactory) {
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { ...globalThis.navigator, bluetooth: environment.bluetooth, userActivation: { isActive: true } }
    })
    Object.defineProperty(globalThis, 'isSecureContext', { configurable: true, value: true })
  }
  const manager = useDefaultFactory
    ? await createWebBleManager()
    : await createWebBleManagerWithEnvironment({ recordingPartition, environment })
  const peer = await manager.choose({ acceptAllDevices: true, optionalServices: [SERVICE] })
  const connection = await manager.connect(peer)
  const database = await connection.discover()
  return {
    manager,
    connection,
    database,
    characteristic,
    emit: bytes => {
      characteristic.value = new DataView(Uint8Array.from(bytes).buffer)
      for (const listener of listeners) listener({ target: characteristic })
    },
    hide: () => lifecycle('page-hidden')
  }
}

test('shipped Web factory records a marker and clears after forget', async () => {
  const storage = transactionalDatabase()
  const partition = createWebRecordingPartition({
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32)
  })
  const f = await fixture(partition)
  try {
    const characteristic = f.database.characteristic(SERVICE, CHARACTERISTIC)
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic }], { recording: recording('factory-522') })
    const marker = f.database.reserveOrderedRecordingMarker('factory-522')
    expect(await marker.start(characteristic, Uint8Array.of(7), 'with-response')).toMatchObject({ state: 'admitted' })
    const controller = await partition.createController()
    expect(await controller.stopOrdered('factory-522')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const batch = await controller.prepareOrdered('factory-522', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.map(row => row.record.kind)).toEqual(['marker', 'terminal'])
    await controller.acknowledge('factory-522', batch.token)
    await marker.forget()
    expect(await controller.clear('factory-522')).toEqual({ cleared: true, records: 0 })
  } finally {
    await f.manager.destroy()
  }
})

test('a settled write cannot be forgotten until its pending marker commit is acknowledged', async () => {
  const original = bridgeModule.createWebAuthorityRecordingJournalBridge
  let releaseCommit
  let entered = false
  jest.spyOn(bridgeModule, 'createWebAuthorityRecordingJournalBridge').mockImplementation((...args) => {
    const bridge = original(...args)
    return {
      ...bridge,
      commit: async delivery => {
        if (delivery.record.kind === 'marker') {
          entered = true
          await new Promise(resolve => {
            releaseCommit = resolve
          })
        }
        return bridge.commit(delivery)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'commit-race-522')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await waitFor(() => entered)
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    await expect(marker.forget()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    const stop = f.controller.stopOrdered('commit-race-522')
    releaseCommit()
    expect(await stop).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    await marker.forget()
    const batch = await f.controller.prepareOrdered('commit-race-522', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.some(row => row.record.kind === 'marker')).toBe(true)
    await f.controller.acknowledge('commit-race-522', batch.token)
    expect(await f.controller.clear('commit-race-522')).toEqual({ cleared: true, records: 0 })
  } finally {
    await f.manager.destroy()
  }
})

test('capacity reached after an explicit stop does not replace the original closed cause', async () => {
  const original = bridgeModule.createWebAuthorityRecordingJournalBridge
  let releaseCommit
  let entered = false
  jest.spyOn(bridgeModule, 'createWebAuthorityRecordingJournalBridge').mockImplementation((...args) => {
    const bridge = original(...args)
    return {
      ...bridge,
      commit: async delivery => {
        if (delivery.record.kind === 'marker' && !entered) {
          entered = true
          await new Promise(resolve => {
            releaseCommit = resolve
          })
        }
        return bridge.commit(delivery)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'closed-capacity-522', 1)
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await waitFor(() => entered)
    const stop = f.controller.stopOrdered('closed-capacity-522')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    await waitFor(() =>
      f.liveOwners.use(identity, async owner => owner.supervisor.status().terminalReason === 'closed')
    )
    releaseCommit()
    let outcome = await stop
    expect(outcome.recording.state).toBe('waiting-capacity')
    for (let i = 0; outcome.recording.state === 'waiting-capacity' && i < 10; i++) {
      const batch = await f.controller.prepareOrdered('closed-capacity-522', { maxItems: 8, maxBytes: 10000 })
      if (batch.token !== null) await f.controller.acknowledge('closed-capacity-522', batch.token)
      outcome = await f.controller.stopOrdered('closed-capacity-522')
    }
    expect(outcome.recording).toMatchObject({ state: 'finished', error: null })
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})
