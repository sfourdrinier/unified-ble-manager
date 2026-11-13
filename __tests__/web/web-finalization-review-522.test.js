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
  const lockManager = locks()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: lockManager,
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
    locks: lockManager,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return { boundary, storage, liveOwners, lockManager, manager, connection, database, characteristic, controller }
}
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
test('a forgotten marker retires after unsolicited connection loss completes cleanup', async () => {
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'loss-retire')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    await waitFor(async () => {
      try {
        await marker.forget()
        return true
      } catch {
        return false
      }
    })
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    f.boundary.gatt.disconnect()
    await waitFor(() => !f.liveOwners.has(identity))
    expect(f.liveOwners.has(identity)).toBe(false)
  } finally {
    await f.manager.destroy()
  }
})

test('authority loss during storage readiness prevents a late producer transaction', async () => {
  let rejectLock
  let lost = false
  const locks = {
    request(_name, _options, callback) {
      callback({})
      return new Promise((_resolve, reject) => { rejectLock = reject })
    }
  }
  let hold = false
  let readyEntered = false
  const resumes = []
  const preparePartition = () => {
    if (!hold) return Promise.resolve()
    readyEntered = true
    return new Promise(resolve => { resumes.push(resolve) })
  }
  const f = await publicFixture({ locks, preparePartition, onLoss: () => { lost = true } })
  try {
    let transactions = 0
    const transaction = f.storage.transaction.bind(f.storage)
    f.storage.transaction = (...args) => {
      transactions++
      return transaction(...args)
    }
    await startRecording(f, 'lost-before-dispatch')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    hold = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(7)
    })
    await waitFor(() => readyEntered)
    rejectLock(new Error('authority lost'))
    await waitFor(() => lost)
    const before = transactions
    resumes.shift()?.()
    await new Promise(resolve => setTimeout(resolve, 10))
    expect(transactions).toBe(before)
    expect(f.storage.rows('records').some(row => row.record.kind === 'value' && row.record.bytes[0] === 7)).toBe(false)
  } finally {
    hold = false
    resumes.splice(0).forEach(resolve => resolve())
    await f.manager.destroy()
  }
})

test('authority loss while source-open readiness waits prevents source-open dispatch', async () => {
  const markerModule = require('../../src/web/web-recording-markers')
  const register = markerModule.registerWebRecordingMarkers
  let registered = false
  jest.spyOn(markerModule, 'registerWebRecordingMarkers').mockImplementation((...args) => {
    register(...args)
    registered = true
  })
  let rejectLock
  let lost = false
  const locks = { request(_name, _options, callback) {
    callback({})
    return new Promise((_resolve, reject) => { rejectLock = reject })
  } }
  let afterBinding = 0
  let entered = false
  let resume
  const preparePartition = () => {
    if (!registered || ++afterBinding !== 2) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resume = resolve })
  }
  const f = await publicFixture({ locks, preparePartition, onLoss: () => { lost = true } })
  let transactions = 0
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => { transactions++; return transaction(...args) }
  try {
    const pending = startRecording(f, 'open-lost')
    await waitFor(() => entered)
    rejectLock(new Error('authority lost'))
    await waitFor(() => lost)
    const before = transactions
    resume()
    await pending
    expect(transactions).toBe(before)
    expect(f.storage.rows('recordings')[0].ownerState).toBe('prepared')
  } finally {
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('authority loss while declaration readiness waits prevents stale declaration dispatch', async () => {
  const { WebRecordingAuthority } = require('../../src/web/web-recording-authority')
  const declare = WebRecordingAuthority.prototype.declare
  let hold = false
  let entered = false
  let resume
  jest.spyOn(WebRecordingAuthority.prototype, 'declare').mockImplementation(function (...args) {
    hold = true
    return declare.apply(this, args)
  })
  let rejectLock
  let lost = false
  const locks = { request(_name, _options, callback) {
    callback({})
    return new Promise((_resolve, reject) => { rejectLock = reject })
  } }
  const f = await publicFixture({ locks, onLoss: () => { lost = true }, preparePartition: () => {
    if (!hold) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resume ??= resolve })
  } })
  let transactions = 0
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => { transactions++; return transaction(...args) }
  try {
    const pending = startRecording(f, 'declaration-lost')
    pending.catch(() => {})
    await waitFor(() => entered)
    rejectLock(new Error('authority lost'))
    await waitFor(() => lost)
    const before = transactions
    resume()
    await new Promise(resolve => setTimeout(resolve, 10))
    expect(transactions).toBe(before)
    expect(f.storage.rows('recordings')).toHaveLength(0)
  } finally {
    hold = false
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('authority loss while peer binding waits prevents stale binding dispatch', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const actual = peerModule.createWebRecordingPeerIdentityStore
  let hold = false
  let entered = false
  let resume
  jest.spyOn(peerModule, 'createWebRecordingPeerIdentityStore').mockImplementation(db => {
    const store = actual(db)
    return { ...store, bindOwned: (...args) => { hold = true; return store.bindOwned(...args) } }
  })
  let rejectLock
  let lost = false
  const locks = { request(_name, _options, callback) {
    callback({})
    return new Promise((_resolve, reject) => { rejectLock = reject })
  } }
  const f = await publicFixture({ locks, onLoss: () => { lost = true }, preparePartition: () => {
    if (!hold) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resume ??= resolve })
  } })
  let transactions = 0
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => { transactions++; return transaction(...args) }
  try {
    const pending = startRecording(f, 'binding-lost')
    pending.catch(() => {})
    await waitFor(() => entered)
    rejectLock(new Error('authority lost'))
    await waitFor(() => lost)
    const before = transactions
    resume()
    await new Promise(resolve => setTimeout(resolve, 10))
    expect(transactions).toBe(before)
    expect(f.storage.rows('readerReceipts').some(row => row.reference)).toBe(false)
  } finally {
    hold = false
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('authority loss while owner-fence readiness waits prevents stale fence dispatch', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const fence = peerModule.webRecordingOwnerFence
  let hold = false
  let entered = false
  const resumes = []
  jest.spyOn(peerModule, 'webRecordingOwnerFence').mockImplementation((db, owner, incarnation, action) => {
    if (action === 'hold') hold = true
    return fence(db, owner, incarnation, action)
  })
  let rejectLock
  let lost = false
  const locks = { request(_name, _options, callback) {
    callback({})
    return new Promise((_resolve, reject) => { rejectLock = reject })
  } }
  const f = await publicFixture({ locks, onLoss: () => { lost = true }, preparePartition: () => {
    if (!hold) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resumes.push(resolve) })
  } })
  let transactions = 0
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => { transactions++; return transaction(...args) }
  try {
    const pending = startRecording(f, 'fence-lost')
    pending.catch(() => {})
    await waitFor(() => entered)
    rejectLock(new Error('authority lost'))
    await waitFor(() => lost)
    const before = transactions
    resumes.shift()?.()
    await new Promise(resolve => setTimeout(resolve, 10))
    expect(transactions).toBe(before)
    expect(f.storage.rows('readerReceipts').some(row => row.recordingId === ':ubm-owner-fence-v1:fence-lost')).toBe(false)
  } finally {
    hold = false
    resumes.splice(0).forEach(resolve => resolve())
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('a stalled default partition open bounds callers and closes late success', async () => {
  const { defaultWebRecordingPartition } = require('../../src/web/web-recording-partition')
  const originals = Object.fromEntries(['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  let request
  const close = jest.fn()
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open: () => {
    request = { result: { close }, error: null, transaction: null }
    return request
  } } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: locks() } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  jest.useFakeTimers()
  try {
    const callers = Array.from({ length: 100 }, () => defaultWebRecordingPartition().then(() => 'resolved', () => 'rejected'))
    await jest.advanceTimersByTimeAsync(5001)
    const results = await Promise.all(callers)
    expect(results).toEqual(Array(100).fill('rejected'))
    request.onsuccess()
    expect(close).toHaveBeenCalledTimes(1)
  } finally {
    jest.useRealTimers()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test('a transient readiness failure drains and releases without a controller call', async () => {
  let fail = false
  let failures = 0
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) {
      failures++
      throw new Error('storage temporarily unavailable')
    }
  } })
  try {
    await startRecording(f, 'autonomous-transient')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    fail = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(9)
    })
    await waitFor(() => failures > 0)
    fail = false
    await waitFor(() => !f.liveOwners.has(identity))
    expect(f.storage.rows('recordings')[0].ownerState).toBe('sealed')
    expect(f.storage.rows('records').some(entry => entry.record.kind === 'value' && entry.record.bytes[0] === 9)).toBe(true)
  } finally {
    fail = false
    await f.manager.destroy()
  }
})

test('public runtime diagnostic retains redacted readiness evidence through recovery', async () => {
  let fail = false
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) throw new DOMException('private readiness detail', 'InvalidStateError')
  } })
  const stop = f.boundary.characteristic.stopNotifications
  let releaseStop
  const gate = new Promise(resolve => { releaseStop = resolve })
  f.boundary.characteristic.stopNotifications = async () => { await gate; return stop() }
  try {
    await startRecording(f, 'runtime-diagnostic')
    expect(await f.controller.runtimeDiagnostic('runtime-diagnostic')).toMatchObject({ lastFailure: null })
    fail = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(3) })
    await waitFor(async () => (await f.controller.runtimeDiagnostic('runtime-diagnostic')).lastFailure !== null)
    const before = await f.controller.runtimeDiagnostic('runtime-diagnostic')
    expect(before.lastFailure).toMatchObject({ persisted: false, error: {
      code: 'capability.unavailable', domain: 'restoration',
      operation: 'web-recording.indexeddb-transaction', platformCode: 'InvalidStateError'
    } })
    expect(JSON.stringify(before)).not.toContain('private readiness detail')
    fail = false
    await waitFor(() => f.storage.rows('records').some(row => row.record.kind === 'value'))
    const after = await f.controller.runtimeDiagnostic('runtime-diagnostic')
    expect(after.lastFailure).toEqual(before.lastFailure)
  } finally {
    fail = false
    releaseStop()
    await f.controller.stopOrdered('runtime-diagnostic')
    await f.manager.destroy()
  }
})

test('an absent reconciliation resumes the retained delivery in the same autonomous retry', async () => {
  let fail = false
  let failures = 0
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) {
      failures++
      throw new Error('storage temporarily unavailable')
    }
  } })
  try {
    await startRecording(f, 'absent-retry')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    fail = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(19)
    })
    for (let i = 0; i < 100 && failures < 2; i++) await jest.advanceTimersByTimeAsync(0)
    expect(failures).toBeGreaterThanOrEqual(2)
    // Let the failed commit and failed reconciliation finish before restoring storage.
    await jest.advanceTimersByTimeAsync(0)
    expect(f.liveOwners.has(identity)).toBe(true)
    fail = false
    await jest.advanceTimersByTimeAsync(100)
    // Jest advances a timer scheduled by another timer on the next tick.
    await jest.advanceTimersByTimeAsync(1)
    expect(f.storage.rows('records').some(entry => entry.record.kind === 'value' && entry.record.bytes[0] === 19)).toBe(true)
    expect(f.storage.rows('recordings')[0].ownerState).toBe('sealed')
    expect(f.liveOwners.has(identity)).toBe(false)
  } finally {
    fail = false
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a new delivery readiness failure backs off after an uncertain retirement is resolved', async () => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const make = journal.createWebOrderedRecordingJournal
  let retireFailures = 2, secondAttempts = 0, failSecond = true
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return { ...producer,
      retire: async id => {
        if (id === '1' && retireFailures-- > 0) throw new WebRecordingReadinessError(new Error('retire unavailable'))
        return producer.retire(id)
      },
      commitWithDurability: async delivery => {
        if (delivery.id === '2') {
          secondAttempts++
          if (failSecond) throw new WebRecordingReadinessError(new Error('new outage'))
        }
        return producer.commitWithDurability(delivery)
      }
    }
  })
  const f = await publicFixture()
  try {
    await startRecording(f, 'mixed-retry')
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    for (const value of [1, 2]) f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0, characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0,
      value: Uint8Array.of(value) })
    for (let i = 0; i < 100 && retireFailures > 0; i++) await jest.advanceTimersByTimeAsync(0)
    expect(retireFailures).toBe(0)
    await jest.advanceTimersByTimeAsync(100)
    expect(secondAttempts).toBe(1)
    await jest.advanceTimersByTimeAsync(1)
    expect(secondAttempts).toBe(1)
    failSecond = false
    await jest.advanceTimersByTimeAsync(500)
    expect(secondAttempts).toBeGreaterThan(1)
  } finally {
    failSecond = false
    jest.useRealTimers()
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('a readiness timeout cannot dispatch its abandoned transaction on late success', async () => {
  let hold = false
  let entered = false
  let resume
  const f = await publicFixture({ preparePartition: () => {
    if (!hold) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resume ??= resolve })
  } })
  try {
    await startRecording(f, 'late-ready-timeout')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    hold = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(12)
    })
    for (let i = 0; i < 100 && !entered; i++) await jest.advanceTimersByTimeAsync(0)
    expect(entered).toBe(true)
    await jest.advanceTimersByTimeAsync(5001)
    resume()
    await jest.advanceTimersByTimeAsync(0)
    expect(f.storage.rows('records').some(row => row.record.kind === 'value' && row.record.bytes[0] === 12)).toBe(false)
  } finally {
    hold = false
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('an injected database readiness timeout cannot dispatch after late success', async () => {
  const f = await publicFixture()
  let hold = false
  let entered = false
  let resume
  f.storage.ready = () => {
    if (!hold) return Promise.resolve()
    entered = true
    return new Promise(resolve => { resume ??= resolve })
  }
  try {
    await startRecording(f, 'late-injected-ready')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    hold = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(13)
    })
    for (let i = 0; i < 100 && !entered; i++) await jest.advanceTimersByTimeAsync(0)
    expect(entered).toBe(true)
    await jest.advanceTimersByTimeAsync(5001)
    resume()
    await jest.advanceTimersByTimeAsync(0)
    expect(f.storage.rows('records').some(row => row.record.kind === 'value' && row.record.bytes[0] === 13)).toBe(false)
  } finally {
    hold = false
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a lost commit completion reconciles once before a late callback', async () => {
  const f = await publicFixture()
  let lose = false
  let lost = false
  let late
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => {
    const tx = transaction(...args)
    const objectStore = tx.objectStore.bind(tx)
    let valueWrite = false
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'records') {
        const put = store.put
        store.put = row => {
          if (lose && row.record?.kind === 'value' && row.record.bytes[0] === 11) valueWrite = true
          return put(row)
        }
      }
      return store
    }
    let completion
    Object.defineProperty(tx, 'oncomplete', {
      set: callback => { completion = callback },
      get: () => () => {
        if (valueWrite && !lost) {
          lost = true
          late = completion
        } else completion?.()
      }
    })
    return tx
  }
  try {
    await startRecording(f, 'lost-real-completion')
    await waitFor(() => f.storage.rows('pendingIdentities').every(row => row.retired === true))
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    lose = true
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(11)
    })
    for (let i = 0; i < 200 && !lost; i++) await jest.advanceTimersByTimeAsync(0)
    expect(lost).toBe(true)
    await jest.advanceTimersByTimeAsync(5001)
    for (let i = 0; i < 200 && !f.storage.rows('pendingIdentities').some(row => row.deliveryId === '2' && row.retired); i++)
      await jest.advanceTimersByTimeAsync(0)
    expect(f.storage.rows('pendingIdentities').some(row => row.deliveryId === '2' && row.retired)).toBe(true)
    late()
    await jest.advanceTimersByTimeAsync(0)
    expect(f.storage.rows('records').filter(row => row.record.kind === 'value' && row.record.bytes[0] === 11)).toHaveLength(1)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a failed provisional bind keeps a cleanup-only owner when release fails', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const { WebRecordingAuthority } = require('../../src/web/web-recording-authority')
  const actual = peerModule.createWebRecordingPeerIdentityStore
  jest.spyOn(peerModule, 'createWebRecordingPeerIdentityStore').mockImplementation(db => ({
    ...actual(db),
    bindOwned: async () => { throw new Error('binding failed') }
  }))
  const release = WebRecordingAuthority.prototype.release
  let fail = true
  jest.spyOn(WebRecordingAuthority.prototype, 'release').mockImplementation(function () {
    if (fail) return Promise.reject(new Error('cleanup failed'))
    return release.call(this)
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: { id: 'provisional-cleanup', maxBytes: 1048576, maxRecords: 20 }
    })
    expect(result.kind).toBe('prepare-failed')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    expect(f.liveOwners.has(identity)).toBe(true)
    expect(() => start.reserveMarkerWrite()).toThrow()
    fail = false
    await f.controller.stopOrdered('provisional-cleanup')
    await waitFor(() => !f.liveOwners.has(identity))
    expect(f.storage.rows('recordings')).toHaveLength(0)
  } finally {
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('a cleanup-only owner retries an initially unreadable binding reconciliation', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const actual = peerModule.createWebRecordingPeerIdentityStore
  let unavailable = true
  jest.spyOn(peerModule, 'createWebRecordingPeerIdentityStore').mockImplementation(db => {
    const store = actual(db)
    return {
      ...store,
      bindOwned: async () => { throw new Error('binding failed') },
      readOwned: (...args) => unavailable ? Promise.reject(new Error('read failed')) : store.readOwned(...args)
    }
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: { id: 'unreadable-binding', maxBytes: 1048576, maxRecords: 20 }
    })
    expect(result.kind).toBe('prepare-failed')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    expect(f.liveOwners.has(identity)).toBe(true)
    unavailable = false
    await f.controller.stopOrdered('unreadable-binding')
    await waitFor(() => !f.liveOwners.has(identity))
    expect(f.storage.rows('recordings')).toHaveLength(0)
  } finally {
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('an absent legacy fence alone cannot prove that another context has no marker receipt', async () => {
  const f = await publicFixture({ writable: true })
  let resolveWrite
  f.boundary.characteristic.writeValueWithResponse = () => new Promise(resolve => { resolveWrite = resolve })
  try {
    const start = await startRecording(f, 'legacy-with-marker')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await f.controller.stopOrdered('legacy-with-marker')
    const batch = await f.controller.prepareOrdered('legacy-with-marker', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('legacy-with-marker', batch.token)
    await new Promise((resolve, reject) => {
      const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
      tx.objectStore('readerReceipts').delete(':ubm-owner-fence-v1:legacy-with-marker')
      tx.oncomplete = resolve
      tx.onabort = reject
    })
    const other = await createWebContinuationRecordingController({
      database: f.storage,
      locks: f.lockManager,
      liveOwners: createWebRecordingLiveOwners()
    })
    await expect(other.clear('legacy-with-marker')).rejects.toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.cleanup-evidence' })
  } finally {
    resolveWrite?.()
    await f.manager.destroy()
  }
})
