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

test('public GATT recording start retains ordered radio values in the transactional journal', async () => {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners
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
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  try {
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
    const start = database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic }], {
      recording: { id: 'public-start', maxBytes: 1048576, maxRecords: 20 }
    })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'active', recordingId: 'public-start' } })
    expect(parseOrderedRecordingStartResult(result, 'public-start', 1, 2)).toEqual(result)
    expect(result.receipt.version).toBe(2)
    boundary.emitNotification({
      serviceUuid: boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.from([4, 5])
    })
    const controller = await createWebContinuationRecordingController({
      database: storage,
      liveOwners,
      randomToken: () => 'token'
    })
    expect(await controller.stopOrdered('public-start')).toMatchObject({
      recording: { version: 2, state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const batch = await controller.prepareOrdered('public-start', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.every(row => row.metadata.version === 2)).toBe(true)
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'value', 'terminal'])
    expect(batch.records[1].record.bytes).toEqual(Uint8Array.from([4, 5]))
    const row = storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    expect(liveOwners.has(identity)).toBe(false)
    expect(await controller.stopOrdered('public-start')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    await connection.release()
  } finally {
    await manager.destroy()
  }
})

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
  const controller = await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => 'token'
  })
  return { boundary, storage, liveOwners, manager, connection, database, characteristic, controller }
}
const recording = id => ({ id, maxBytes: 1048576, maxRecords: 20 })

test('public recording marker persists the exact request and dispatch intent before the browser write', async () => {
  const f = await publicFixture({ writable: true })
  const bytes = Uint8Array.of(9, 8, 7)
  let atWrite
  f.boundary.characteristic.writeValueWithResponse = jest.fn(async value => {
    atWrite = {
      bytes: Array.from(value),
      ledger: f.storage.rows('markerLedger'),
      chargedBytes: f.storage.rows('recordings')[0].markerLedgerBytes
    }
  })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-ledger-public')
    })
    const marker = start.reserveMarkerWrite()
    expect(await marker.start(f.characteristic, bytes, 'with-response')).toMatchObject({ state: 'admitted' })
    expect(f.boundary.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
    expect(atWrite).toEqual({
      bytes: [9, 8, 7],
      chargedBytes: expect.any(Number),
      ledger: [
        expect.objectContaining({
          markerId: '0',
          request: expect.objectContaining({ mode: 'with-response', bytes: [9, 8, 7] }),
          admission: expect.objectContaining({ state: 'admitted' }),
          dispatchIntent: true,
          settlement: null
        })
      ]
    })
    expect(atWrite.chargedBytes).toBeGreaterThan(0)
  } finally {
    await f.manager.destroy()
  }
})

test('public recording does not publish a terminal while physical member cleanup is pending', async () => {
  const f = await publicFixture()
  let releaseStop
  let stopEntered
  const entered = new Promise(resolve => { stopEntered = resolve })
  const originalStop = f.boundary.characteristic.stopNotifications
  f.boundary.characteristic.stopNotifications = async () => {
    stopEntered()
    await new Promise(resolve => { releaseStop = resolve })
    return originalStop()
  }
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('held-cleanup')
    })
    const stopping = f.controller.stopOrdered('held-cleanup')
    await entered
    for (const size of [3, 4, 5]) f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0,
      value: new Uint8Array(size) })
    expect(f.storage.rows('records').map(row => row.record.kind)).not.toContain('terminal')
    releaseStop()
    releaseStop = undefined
    await stopping
    const batch = await f.controller.prepareOrdered('held-cleanup', { maxItems: 8, maxBytes: 10000 })
    const failures = batch.records.filter(row => row.record.kind === 'failure-observation')
    expect(failures.map(row => row.record.sourceRejection.bytes)).toEqual(['3', '4', '5'])
    expect(new Set(failures.map(row => row.metadata.deliveryId)).size).toBe(3)
    expect(batch.records.at(-1).record.kind).toBe('terminal')
  } finally {
    releaseStop?.()
    await f.manager.destroy()
  }
})

test('public recording marker routes share one receipt and retain the ordered marker in the journal', async () => {
  const f = await publicFixture({ writable: true })
  let resolveWrite
  const emit = value =>
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(value)
    })
  f.boundary.characteristic.writeValueWithResponse = jest.fn(() => {
    emit(2)
    return new Promise(resolve => {
      resolveWrite = resolve
    })
  })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-public')
    })
    emit(1)
    const first = start.reserveMarkerWrite()
    const second = f.database.reserveOrderedRecordingMarker('marker-public')
    const receipt = await first.start(f.characteristic, Uint8Array.of(7), 'with-response')
    expect(receipt).toMatchObject({ state: 'admitted', write: { state: 'started' } })
    await expect(second.start(f.characteristic, Uint8Array.of(8), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invalid-state'
    })
    await expect(first.forget()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    expect(f.boundary.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
    await f.controller.stopOrdered('marker-public')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    expect(f.liveOwners.has(identity)).toBe(true)
    expect(await first.status()).toMatchObject({ state: 'admitted', write: { state: 'started' } })
    resolveWrite()
    for (let i = 0; i < 200 && !f.storage.rows('recordings')[0].terminal; i++) await new Promise(resolve => setTimeout(resolve, 0))
    const batch = await f.controller.prepareOrdered('marker-public', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'value', 'marker', 'value', 'terminal'])
    expect(batch.records[1].record.bytes).toEqual(Uint8Array.of(1))
    expect(batch.records[2].record).toMatchObject({ markerId: '0', ordinal: receipt.ordinal })
    expect(batch.records[3].record.bytes).toEqual(Uint8Array.of(2))
    await first.forget()
    await f.controller.stopOrdered('marker-public')
    expect(f.liveOwners.has(identity)).toBe(false)
  } finally {
    await f.manager.destroy()
  }
})

test('public marker cancellation and exact retry preserve the observed write result', async () => {
  const f = await publicFixture({ writable: true })
  const failure = new Error('browser rejected write')
  f.boundary.characteristic.writeValueWithResponse = jest.fn(() => Promise.reject(failure))
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-retry')
    })
    const cancelled = start.reserveMarkerWrite()
    expect(await cancelled.cancel()).toMatchObject({ state: 'not-admitted', error: { code: 'operation.aborted' } })
    await cancelled.forget()
    const handle = f.database.reserveOrderedRecordingMarker('marker-retry')
    const first = await handle.start(f.characteristic, Uint8Array.of(3), 'with-response')
    expect(first.state).toBe('admitted')
    await Promise.resolve()
    expect(await handle.cancel()).toMatchObject({
      state: 'admitted',
      write: { state: 'settled', error: { code: 'gatt.write-failed' } }
    })
    expect(await handle.start(f.characteristic, Uint8Array.of(3), 'with-response')).toMatchObject({
      state: 'admitted',
      ordinal: first.ordinal
    })
    await expect(handle.start(f.characteristic, Uint8Array.of(4), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invalid-state'
    })
    expect(f.boundary.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
    await f.controller.stopOrdered('marker-retry')
    await handle.forget()
    await f.controller.stopOrdered('marker-retry')
  } finally {
    await f.manager.destroy()
  }
})

test('public marker refuses a stale graph without dispatching a write', async () => {
  const f = await publicFixture({ writable: true })
  f.boundary.characteristic.writeValueWithResponse = jest.fn()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-stale')
    })
    const marker = start.reserveMarkerWrite()
    await f.connection.release()
    await expect(marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).rejects.toMatchObject({
      code: 'gatt.stale-handle'
    })
    expect(f.boundary.characteristic.writeValueWithResponse).not.toHaveBeenCalled()
    await f.controller.stopOrdered('marker-stale')
  } finally {
    await f.manager.destroy()
  }
})

test('public marker pre-abort and unsupported write mode dispatch no browser write', async () => {
  const f = await publicFixture({ writable: true })
  f.boundary.characteristic.writeValueWithResponse = jest.fn()
  f.boundary.characteristic.writeValueWithoutResponse = jest.fn()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-pre-abort')
    })
    const abort = new AbortController()
    abort.abort()
    const cancelled = start.reserveMarkerWrite()
    expect(
      await cancelled.start(f.characteristic, Uint8Array.of(1), 'with-response', { signal: abort.signal })
    ).toMatchObject({
      state: 'not-admitted',
      error: { code: 'operation.aborted' }
    })
    await cancelled.forget()
    const invalid = start.reserveMarkerWrite()
    await expect(invalid.start(f.characteristic, Uint8Array.of(1), 'without-response')).rejects.toMatchObject({
      code: 'gatt.property-not-supported'
    })
    expect(f.boundary.characteristic.writeValueWithResponse).not.toHaveBeenCalled()
    expect(f.boundary.characteristic.writeValueWithoutResponse).not.toHaveBeenCalled()
    await invalid.forget()
    await f.controller.stopOrdered('marker-pre-abort')
  } finally {
    await f.manager.destroy()
  }
})

test('public marker reconciles a committed journal delivery when its completion reply is lost', async () => {
  const f = await publicFixture({ writable: true })
  f.boundary.characteristic.writeValueWithResponse = jest.fn(async () => {})
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-lost-commit')
    })
    const original = f.storage.transaction.bind(f.storage)
    let lost = false
    f.storage.transaction = (...args) => {
      const tx = original(...args)
      const objectStore = tx.objectStore.bind(tx)
      let markerCommit = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'records') {
          const add = store.add
          store.add = row => {
            if (row.record?.kind === 'marker') markerCommit = true
            return add(row)
          }
        }
        return store
      }
      let complete = null
      Object.defineProperty(tx, 'oncomplete', {
        set: callback => {
          complete = callback
        },
        get: () => () => {
          if (markerCommit && !lost) {
            lost = true
            tx.onabort()
          } else complete?.()
        }
      })
      return tx
    }
    const marker = start.reserveMarkerWrite()
    expect(await marker.start(f.characteristic, Uint8Array.of(6), 'with-response')).toMatchObject({ state: 'admitted' })
    await f.controller.stopOrdered('marker-lost-commit')
    expect(lost).toBe(true)
    const batch = await f.controller.prepareOrdered('marker-lost-commit', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.filter(row => row.record.kind === 'marker')).toHaveLength(1)
    expect(f.boundary.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
    await marker.forget()
    await f.controller.stopOrdered('marker-lost-commit')
  } finally {
    await f.manager.destroy()
  }
})

test('loss of recording authority fences a new public marker before browser dispatch', async () => {
  let loseLock
  const f = await publicFixture({
    locks: {
      request(_name, _options, callback) {
        callback({})
        return new Promise((_resolve, reject) => {
          loseLock = reject
        })
      }
    },
    writable: true
  })
  f.boundary.characteristic.writeValueWithResponse = jest.fn()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('marker-authority-loss')
    })
    const marker = start.reserveMarkerWrite()
    loseLock(new Error('lock lost'))
    await new Promise(resolve => setTimeout(resolve, 0))
    await expect(marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invalid-state'
    })
    expect(f.boundary.characteristic.writeValueWithResponse).not.toHaveBeenCalled()
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('reports actual member cleanup failure and permits the live owner to retry', async () => {
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    expect(
      (
        await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
          recording: recording('cleanup-failure')
        })
      ).kind
    ).toBe('started')
    const failure = new Error('stop failed')
    f.boundary.nextNotificationStopFailure = failure
    expect(await f.controller.stopOrdered('cleanup-failure')).toMatchObject({
      hostCleanup: { state: 'release-failed' }
    })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    expect(await f.controller.stopOrdered('cleanup-failure')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
  } finally {
    await f.manager.destroy()
  }
})

test('public capacity-one cleanup retry journals bounded late failures and reader replay', async () => {
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('late-retry'),
      stream: { preset: 'custom', budget: { itemCapacity: 1, byteCapacity: 128, reservedControlCapacity: 1 } }
    })
    const failure = new Error('first stop failed')
    f.boundary.nextNotificationStopFailure = failure
    expect(await f.controller.stopOrdered('late-retry')).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    for (const size of [2, 3, 4]) f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0,
      value: new Uint8Array(size)
    })
    expect(f.storage.rows('records').some(row => row.record.kind === 'terminal')).toBe(false)
    expect(await f.controller.stopOrdered('late-retry')).toMatchObject({ hostCleanup: { state: 'released' } })
    const limits = { maxItems: 20, maxBytes: 20000 }
    const batch = await f.controller.prepareOrdered('late-retry', limits)
    expect(await f.controller.prepareOrdered('late-retry', limits)).toEqual(batch)
    expect(batch.records.map(row => row.record.kind)).toEqual(expect.arrayContaining(['failure-observation', 'failure-observations-omitted', 'terminal']))
    expect(batch.records.find(row => row.record.kind === 'failure-observation').record.sourceRejection.bytes).toBe('2')
    expect(batch.records.find(row => row.record.kind === 'failure-observations-omitted').record.count).toBeNull()
    expect(batch.records.at(-1).record.kind).toBe('terminal')
    const ids = batch.records.map(row => Number(row.metadata.deliveryId))
    expect(ids).toEqual(ids.map((_, i) => i + 1))
    await f.controller.acknowledge('late-retry', batch.token)
    expect(f.storage.rows('records')).toHaveLength(0)
  } finally {
    await f.manager.destroy()
  }
})

test('an unknown stored version fails before stopping the live source', async () => {
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: recording('unknown-version') })
    f.storage.mutate('recordings', row => { row.context.version = 99 })
    const before = f.boundary.notificationStops
    await expect(f.controller.stopOrdered('unknown-version')).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(f.boundary.notificationStops).toBe(before)
    f.storage.mutate('recordings', row => { row.context.version = 2 })
    expect(await f.controller.stopOrdered('unknown-version')).toMatchObject({ recording: { version: 2, state: 'finished' } })
  } finally {
    await f.manager.destroy()
  }
})

test('a failed preparation retains v2 platform metadata in its public receipt', async () => {
  const { webRecordingContractError: contractError, trustedWebRecordingError } = require('../../src/web/web-recording-errors')
  const entries = [
    { key: 'repeat', value: { kind: 'text', value: 'first' } },
    { key: 'repeat', value: { kind: 'bool', value: true } }
  ]
  const failure = contractError('capability.unavailable', 'restoration', 'prepare', {
    domain: 'test', code: 'failed', safeMessage: 'failed', metadata: { flag: true }, metadataEntries: entries
  })
  expect(trustedWebRecordingError(failure).normalized.platform.metadataEntries).toEqual(entries)
  const f = await publicFixture({ digest: async () => { throw failure } })
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    const result = await handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('metadata-prepare-failure')
    })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { version: 2,
      error: { kind: 'source', source: { platformMetadataEntries: entries } } } })
    const published = result.receipt.error.source.platformMetadataEntries
    expect(published.map(entry => entry.key)).toEqual(['repeat', 'repeat'])
    entries[0].value.value = 'changed after publication'
    expect(published).toEqual([
      { key: 'repeat', value: { kind: 'text', value: 'first' } },
      { key: 'repeat', value: { kind: 'bool', value: true } }
    ])
    expect(parseOrderedRecordingStartResult(result, 'metadata-prepare-failure', 1, 2)).toEqual(result)
    expect(f.boundary.notificationStarts).toBe(0)
    expect(f.storage.rows('recordings')).toHaveLength(0)
  } finally {
    await f.manager.destroy()
  }
})

test('a cancelled pre-admission waiter leaves no radio start and eventually resolves its reservation', async () => {
  let releaseDigest
  const digest = new Promise(resolve => {
    releaseDigest = resolve
  })
  const f = await publicFixture({ digest: () => digest })
  try {
    const abort = new AbortController()
    const handle = f.database.reserveOrderedRecordingStart()
    const waiting = handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('cancel-before'),
      signal: abort.signal
    })
    await Promise.resolve()
    abort.abort()
    await expect(waiting).rejects.toMatchObject({ code: 'operation.aborted' })
    releaseDigest('ab'.repeat(32))
    for (let i = 0; i < 100 && (await handle.status()).state === 'pending'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(f.boundary.notificationStarts).toBe(0)
    expect(f.storage.rows('recordings')).toHaveLength(0)
    expect((await handle.status()).state).toBe('result')
    await handle.retire()
    expect(await handle.status()).toEqual({ state: 'retired' })
  } finally {
    await f.manager.destroy()
  }
})

test('a cancelled dispatched start retains its eventual result and live owner', async () => {
  const f = await publicFixture()
  let releaseEnable
  const original = f.boundary.characteristic.startNotifications
  f.boundary.characteristic.startNotifications = async () => {
    await new Promise(resolve => {
      releaseEnable = resolve
    })
    await original()
  }
  try {
    const abort = new AbortController()
    const handle = f.database.reserveOrderedRecordingStart()
    const waiting = handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('lost-result'),
      signal: abort.signal
    })
    while (!releaseEnable) await new Promise(resolve => setTimeout(resolve, 0))
    abort.abort()
    await expect(waiting).rejects.toMatchObject({ code: 'operation.aborted' })
    await expect(handle.retire()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0,
      value: Uint8Array.from([77, 78]) })
    releaseEnable()
    for (let i = 0; i < 100 && (await handle.status()).state === 'pending'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(await handle.status()).toMatchObject({
      state: 'result',
      result: { kind: 'started', receipt: { state: 'active' } }
    })
    await handle.retire()
    await handle.retire()
    expect(await handle.status()).toEqual({ state: 'retired' })
    expect(await f.controller.stopOrdered('lost-result')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const batch = await f.controller.prepareOrdered('lost-result', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.some(row => row.record.kind === 'value' && row.record.bytes[0] === 77)).toBe(true)
  } finally {
    await f.manager.destroy()
  }
})

test('a second start cannot replace the live durable authority for the same recording', async () => {
  const f = await publicFixture()
  try {
    const first = f.database.reserveOrderedRecordingStart()
    const second = f.database.reserveOrderedRecordingStart()
    const members = [{ memberId: 'heart', characteristic: f.characteristic }]
    expect((await first.start(members, { recording: recording('one-owner') })).kind).toBe('started')
    expect(await second.start(members, { recording: recording('one-owner') })).toMatchObject({
      kind: 'prepare-failed',
      receipt: {
        sourceStarted: false,
        error: { kind: 'source', source: { code: 'capability.unavailable', operation: 'web-recording.authority-busy' } }
      }
    })
    expect(f.boundary.notificationStarts).toBe(1)
    expect(await f.controller.stopOrdered('one-owner')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
  } finally {
    await f.manager.destroy()
  }
})

test('a rejected enable reports its observed setup failure and retains cleanup ownership', async () => {
  const f = await publicFixture()
  const failure = new Error('enable failed')
  f.boundary.characteristic.startNotifications = async () => {
    throw failure
  }
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    const result = await handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('enable-failure')
    })
    expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'Error' }] })
    expect(result).toMatchObject({
      kind: 'started',
      receipt: {
        state: 'setup-failed',
        setupFailure: { code: 'gatt.subscribe-failed' },
        members: [{ acquisition: 'indeterminate' }]
      }
    })
    expect(await f.controller.stopOrdered('enable-failure')).toMatchObject({ recording: { state: 'failed' } })
  } finally {
    await f.manager.destroy()
  }
})

test('a graph released before stop reports interruption rather than a clean stop', async () => {
  const f = await publicFixture()
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    await handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('released-graph')
    })
    await f.connection.release()
    expect(await f.controller.stopOrdered('released-graph')).toMatchObject({ recording: { state: 'interrupted' } })
  } finally {
    await f.manager.destroy()
  }
})

test('a timed-out preparation fences source admission and exposes its eventual result', async () => {
  let releaseDigest
  const digest = new Promise(resolve => {
    releaseDigest = resolve
  })
  const f = await publicFixture({ digest: () => digest })
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    await expect(
      handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
        recording: recording('timeout-before'),
        timeoutMs: 15
      })
    ).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(await handle.status()).toEqual({ state: 'unknown' })
    releaseDigest('ab'.repeat(32))
    for (let i = 0; i < 100 && (await handle.status()).state === 'unknown'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(f.boundary.notificationStarts).toBe(0)
    expect(await handle.status()).toMatchObject({
      state: 'result',
      result: { kind: 'prepare-failed', receipt: { sourceStarted: false } }
    })
  } finally {
    await f.manager.destroy()
  }
})

test('a lost bounded start reply does not cancel the owner and remains recoverable', async () => {
  const f = await publicFixture()
  let releaseEnable
  const original = f.boundary.characteristic.startNotifications
  f.boundary.characteristic.startNotifications = async () => {
    await new Promise(resolve => {
      releaseEnable = resolve
    })
    await original()
  }
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    const waiting = handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('bounded-result')
    })
    await expect(waiting).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(await handle.status()).toEqual({ state: 'unknown' })
    expect(releaseEnable).toEqual(expect.any(Function))
    releaseEnable()
    for (let i = 0; i < 100 && (await handle.status()).state === 'unknown'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(await handle.status()).toMatchObject({
      state: 'result',
      result: { kind: 'started', receipt: { state: 'active' } }
    })
    expect(await f.controller.stopOrdered('bounded-result')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
  } finally {
    await f.manager.destroy()
  }
}, 10000)

test('hiding a page preserves an active recording and its connection', async () => {
  const f = await publicFixture()
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    await handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('hidden-survival')
    })
    f.boundary.emitPageLifecycle('page-hidden')
    await Promise.resolve()
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.from([9])
    })
    expect(await f.controller.stopOrdered('hidden-survival')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const batch = await f.controller.prepareOrdered('hidden-survival', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.some(row => row.record.kind === 'value' && row.record.bytes[0] === 9)).toBe(true)
  } finally {
    await f.manager.destroy()
  }
})

test('cancellation settles stalled pre-admission digest without waiting for it', async () => {
  const f = await publicFixture({ digest: () => new Promise(() => {}) })
  try {
    const abort = new AbortController()
    const handle = f.database.reserveOrderedRecordingStart()
    const waiting = handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('stalled-cancel'),
      signal: abort.signal
    })
    await new Promise(resolve => setTimeout(resolve, 0))
    abort.abort()
    await expect(waiting).rejects.toMatchObject({ code: 'operation.aborted' })
    for (let i = 0; i < 50 && (await handle.status()).state === 'pending'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(await handle.status()).toMatchObject({ state: 'result', result: { kind: 'prepare-failed' } })
    expect(f.boundary.notificationStarts).toBe(0)
  } finally {
    await f.manager.destroy()
  }
})

test('preparation preserves the normalized unavailable coordinator cause', async () => {
  const f = await publicFixture({ locks: undefined })
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    const result = await handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('no-coordinator')
    })
    expect(result).toMatchObject({
      kind: 'prepare-failed',
      receipt: {
        error: {
          kind: 'source',
          source: { code: 'capability.unsupported', operation: 'web-recording.no-exclusive-coordinator' }
        }
      }
    })
  } finally {
    await f.manager.destroy()
  }
})

test('a deadline settles stalled pre-admission digest without waiting for it', async () => {
  const f = await publicFixture({ digest: () => new Promise(() => {}) })
  try {
    const handle = f.database.reserveOrderedRecordingStart()
    await expect(
      handle.start([{ memberId: 'heart', characteristic: f.characteristic }], {
        recording: recording('stalled-timeout'),
        timeoutMs: 15
      })
    ).rejects.toMatchObject({ code: 'operation.timed-out' })
    for (let i = 0; i < 50 && (await handle.status()).state === 'unknown'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(await handle.status()).toMatchObject({ state: 'result', result: { kind: 'prepare-failed' } })
    expect(f.boundary.notificationStarts).toBe(0)
  } finally {
    await f.manager.destroy()
  }
})
