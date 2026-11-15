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
  return { boundary, storage, liveOwners, backend, manager, connection, database, characteristic, controller }
}
async function startRecording(f, id, maxRecords = 20) {
  const start = f.database.reserveOrderedRecordingStart()
  await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
    recording: { id, maxBytes: 1048576, maxRecords }
  })
  return start
}

function identity(row) {
  return { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
}
async function advanceUntil(predicate, steps = 100) {
  for (let i = 0; i < steps && !(await predicate()); i++) await jest.advanceTimersByTimeAsync(1000)
  expect(await predicate()).toBe(true)
}
// Abort the first actual observation transaction but withhold only its event.
// Optionally abort its first ordered reconciliation read, immediately or late.
function abortObservationWithheld(storage, readFailure) {
  const original = storage.transaction.bind(storage)
  const held = []
  let writes = 0, reads = 0, observationAborted = false, attemptedObservation
  storage.transaction = (...args) => {
    const tx = original(...args)
    const objectStore = tx.objectStore.bind(tx)
    let selectedAbort = false, holdAbort = false
    tx.objectStore = name => {
      const store = objectStore(name)
      const put = store.put, get = store.get
      store.put = value => {
        if (name === 'recordings' && value.terminalObservation !== undefined) {
          writes++
          if (writes === 1) {
            attemptedObservation = value.terminalObservation
            selectedAbort = true
            holdAbort = true
            observationAborted = true
            storage.abortNext(new DOMException('private write abort', 'AbortError'))
          }
        }
        return put(value)
      }
      store.get = value => {
        if (observationAborted && name === 'recordings' && args[1] === 'readonly' && readFailure) {
          reads++
          selectedAbort = true
          holdAbort = readFailure === 'late'
          storage.abortNext(new DOMException('private read failure', 'DataError'))
        }
        return get(value)
      }
      return store
    }
    let onabort
    Object.defineProperty(tx, 'onabort', {
      set: fn => { onabort = fn },
      get: () => () => {
        if (selectedAbort && holdAbort) held.push(() => onabort?.())
        else onabort?.()
      }
    })
    return tx
  }
  return { held, counts: () => ({ writes, reads }), attemptedObservation: () => attemptedObservation }
}

test('an exact absent observation read permits one retry after an abort event is withheld', async () => {
  const f = await publicFixture()
  let fault
  try {
    await startRecording(f, 'absent-observation')
    const owner = identity(f.storage.rows('recordings')[0])
    fault = abortObservationWithheld(f.storage)
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.gatt.disconnect()
    await advanceUntil(() => fault.held.length === 1, 2)
    expect(fault.counts().writes).toBe(1)
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeUndefined()
    await jest.advanceTimersByTimeAsync(3000)
    expect(fault.counts().writes).toBe(1)
    await advanceUntil(() => !f.liveOwners.has(owner), 70)
    expect(fault.counts().writes).toBe(2)
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeDefined()
    expect((await f.controller.stopOrdered('absent-observation')).hostCleanup.state).toBe('released')
  } finally {
    fault?.held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a conflicting observation receipt never authorizes another write', async () => {
  const f = await publicFixture()
  let fault
  try {
    await startRecording(f, 'conflicting-observation')
    const owner = identity(f.storage.rows('recordings')[0])
    fault = abortObservationWithheld(f.storage)
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.gatt.disconnect()
    await advanceUntil(() => fault.held.length === 1, 2)
    f.storage.mutate('recordings', row => {
      row.terminalObservation = { ...fault.attemptedObservation(), operationId: 'f'.repeat(64) }
    })
    await jest.advanceTimersByTimeAsync(40000)
    const state = await f.liveOwners.use(owner, async live => live.supervisor.status())
    expect(state.storageOutcomeUnknown).toBe(true)
    expect(fault.counts().writes).toBe(1)
    const result = await f.controller.stopOrdered('conflicting-observation')
    expect(result.recording.error.storage.kind).toBe('reconciliation-unknown')
    expect(result.recording.error.storage.detail).toContain('WebRecordingOutcomeUnknown')
  } finally {
    fault?.held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test.each(['prompt', 'late'])('public uncertainty retains a %s production reconciliation read failure after authority loss', async timing => {
  const f = await publicFixture()
  let fault
  try {
    await startRecording(f, `observation-read-${timing}`)
    const owner = identity(f.storage.rows('recordings')[0])
    fault = abortObservationWithheld(f.storage, timing)
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.gatt.disconnect()
    await advanceUntil(() => fault.counts().reads >= 1, 40)
    if (timing === 'late') await jest.advanceTimersByTimeAsync(6000)
    const live = await f.liveOwners.use(owner, async entry => entry)
    live.supervisor.loseAuthority()
    live.supervisor.noteRetirementFailure(new Error('private cleanup failure'))
    if (timing === 'late') {
      expect(fault.held).toHaveLength(2)
      fault.held[1]()
    }
    await jest.advanceTimersByTimeAsync(1000)
    const result = await f.controller.stopOrdered(`observation-read-${timing}`)
    expect(result.recording.error.storage.kind).toBe('reconciliation-unknown')
    expect(result.recording.error.storage.detail).toContain('DataError')
    expect(result.recording.error.storage.detail).toContain('WebRecordingAuthorityLost')
    expect(result.recording.error.storage.detail).not.toContain('private')
    expect(fault.counts().writes).toBe(1)
  } finally {
    fault?.held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})


test.each(['commit', 'abort'])('public retry fences observation readiness and unresolved transaction through %s', async outcome => {
  const journalModule = require('../../src/web/web-ordered-recording-journal')
  const make = journalModule.createWebOrderedRecordingJournal
  let enteringObservation = false, enteringRead = false, observationCalls = 0, reconcileCalls = 0
  jest.spyOn(journalModule, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return {
      ...producer,
      observeTerminal: (...input) => {
        observationCalls++
        enteringObservation = true
        try { return producer.observeTerminal(...input) } finally { enteringObservation = false }
      },
      reconcileTerminal: (...input) => {
        reconcileCalls++
        enteringRead = true
        try { return producer.reconcileTerminal(...input) } finally { enteringRead = false }
      }
    }
  })
  const f = await publicFixture()
  let releaseReady, releaseCallback, observationReady = false, firstDispatch = false
  let readReady = 0, readTransactions = 0, observationTransactions = 0, writes = 0
  const attempted = []
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.ready = () => {
    if (enteringObservation && !observationReady) {
      observationReady = true
      // The outer observation timeout is already armed; readiness arms its
      // own five-second timeout after this call returns.
      jest.advanceTimersByTime(1)
      return new Promise(resolve => { releaseReady = () => { firstDispatch = true; resolve() } })
    }
    if (enteringRead) readReady++
    if (enteringObservation) return Promise.resolve().then(() => { firstDispatch = true })
    return Promise.resolve()
  }
  f.storage.transaction = (...args) => {
    const isObservation = firstDispatch && args[1] === 'readwrite'
    const first = isObservation && observationTransactions === 0
    if (isObservation) { firstDispatch = false; observationTransactions++ }
    const tx = transaction(...args)
    const objectStore = tx.objectStore.bind(tx)
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'recordings') {
        const get = store.get, put = store.put
        store.get = key => {
          if (observationReady && args[1] === 'readonly') readTransactions++
          return get(key)
        }
        store.put = row => {
          if (row.terminalObservation !== undefined) { writes++; attempted.push(row.terminalObservation) }
          return put(row)
        }
      }
      return store
    }
    if (first) {
      let complete, abort
      Object.defineProperty(tx, 'oncomplete', { set: fn => { complete = fn }, get: () => () => { releaseCallback = () => complete?.() } })
      Object.defineProperty(tx, 'onabort', { set: fn => { abort = fn }, get: () => () => { releaseCallback = () => abort?.() } })
    }
    return tx
  }
  try {
    await startRecording(f, `readiness-fence-${outcome}`)
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.gatt.disconnect()
    for (let i = 0; i < 200 && !observationReady; i++) await jest.advanceTimersByTimeAsync(0)
    expect(observationReady).toBe(true)
    await jest.advanceTimersByTimeAsync(4999)
    const retry = f.controller.stopOrdered(`readiness-fence-${outcome}`)
    for (let i = 0; i < 50; i++) await jest.advanceTimersByTimeAsync(0)
    expect({ observationTransactions, writes, observationCalls, reconcileCalls, readReady }).toEqual({ observationTransactions: 0, writes: 0, observationCalls: 1, reconcileCalls: 0, readReady: 0 })
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeUndefined()
    const controllerReads = readTransactions
    f.storage.pauseNextCompletion()
    releaseReady()
    await f.storage.requestsProcessed()
    await jest.advanceTimersByTimeAsync(0)
    expect(observationTransactions).toBe(1)
    expect(writes).toBe(1)
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeUndefined()
    await jest.advanceTimersByTimeAsync(6000)
    expect(readReady).toBeGreaterThan(0)
    expect(readTransactions).toBeGreaterThan(controllerReads)
    expect(writes).toBe(1)
    if (outcome === 'abort') f.storage.abortNext(new DOMException('original abort', 'AbortError'))
    f.storage.complete()
    await advanceUntil(() => !f.liveOwners.has(owner), 90)
    expect(readReady).toBeGreaterThan(0)
    expect(readTransactions).toBeGreaterThan(0)
    expect(writes).toBe(outcome === 'commit' ? 1 : 2)
    expect(observationTransactions).toBe(outcome === 'commit' ? 1 : 2)
    expect(f.storage.rows('recordings')[0].terminalObservation).toEqual(attempted[0])
    if (outcome === 'abort') expect(attempted[1]).toEqual(attempted[0])
    await retry
    expect((await f.controller.stopOrdered(`readiness-fence-${outcome}`)).hostCleanup.state).toBe('released')
  } finally {
    releaseReady?.()
    releaseCallback?.()
    jest.restoreAllMocks()
    jest.useRealTimers()
    await f.manager.destroy()
  }
})
