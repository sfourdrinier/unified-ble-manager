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
    liveOwners,
    locks: lockManager,
    randomToken: () => `token-${++token}`
  })
  return {
    boundary,
    storage,
    liveOwners,
    locks: lockManager,
    manager,
    connection,
    database,
    characteristic,
    controller
  }
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

test('a separate context cannot clear while a stopped owner retains a pending marker write', async () => {
  const f = await publicFixture({ writable: true })
  let resolveWrite
  f.boundary.characteristic.writeValueWithResponse = () =>
    new Promise(resolve => {
      resolveWrite = resolve
    })
  try {
    const start = await startRecording(f, 'cross-context')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await f.controller.stopOrdered('cross-context')
    const batch = await f.controller.prepareOrdered('cross-context', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('cross-context', batch.token)
    const other = await createWebContinuationRecordingController({
      database: f.storage,
      locks: f.locks,
      liveOwners: createWebRecordingLiveOwners()
    })
    await expect(other.clear('cross-context')).rejects.toMatchObject({ code: 'capability.unavailable' })
    resolveWrite()
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    await waitFor(async () => (await f.controller.stopOrdered('cross-context')).hostCleanup.state === 'released')
    const terminal = await f.controller.prepareOrdered('cross-context', { maxItems: 8, maxBytes: 10000 })
    if (terminal.token !== null) await f.controller.acknowledge('cross-context', terminal.token)
    await marker.forget()
    expect(await other.clear('cross-context')).toEqual({ cleared: true, records: 0 })
  } finally {
    await f.manager.destroy()
  }
})

test('an allocation completion that is lost is bounded and never dispatches clear', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'allocation-lost')
    await f.controller.stopOrdered('allocation-lost')
    const batch = await f.controller.prepareOrdered('allocation-lost', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('allocation-lost', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let lost = false
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let allocation = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') allocation = true
            return put(value)
          }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          if (allocation && !lost) lost = true
          else complete?.()
        }
      })
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const clear = f.controller.clear('allocation-lost')
    const result = clear.then(
      () => 'resolved',
      () => 'rejected'
    )
    for (let i = 0; i < 200 && !lost; i++) await jest.advanceTimersByTimeAsync(0)
    expect(lost).toBe(true)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await Promise.race([result, Promise.resolve('pending')])).toBe('rejected')
    expect(f.storage.rows('recordings')).toHaveLength(1)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a transient bind failure can be cleaned up and retried with the same public ID', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const actual = peerModule.createWebRecordingPeerIdentityStore
  let fail = true
  jest.spyOn(peerModule, 'createWebRecordingPeerIdentityStore').mockImplementation(db => {
    const store = actual(db)
    return {
      ...store,
      bindOwned: async (...args) => {
        if (fail) throw new Error('bind aborted')
        return store.bindOwned(...args)
      }
    }
  })
  const f = await publicFixture()
  try {
    const first = f.database.reserveOrderedRecordingStart()
    const failure = await first.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('retry-id')
    })
    expect(failure.kind).toBe('prepare-failed')
    fail = false
    const second = f.database.reserveOrderedRecordingStart()
    const retry = await second.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('retry-id')
    })
    expect(retry.kind).toBe('started')
    await f.controller.stopOrdered('retry-id')
  } finally {
    await f.manager.destroy()
  }
})

test('a forgotten marker is retired after capacity acknowledgement finishes cleanup', async () => {
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'capacity-retire', 2)
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
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(2)
    })
    await waitFor(() => f.storage.rows('recordings')[0].recordCount === 2)
    const result = await f.controller.stopOrdered('capacity-retire')
    expect(result.recording.state).toBe('waiting-capacity')
    for (let i = 0; i < 8; i++) {
      const batch = await f.controller.prepareOrdered('capacity-retire', { maxItems: 8, maxBytes: 10000 })
      if (batch.token) await f.controller.acknowledge('capacity-retire', batch.token)
      if (f.storage.rows('recordings')[0].ownerState === 'sealed') break
    }
    const row = f.storage.rows('recordings')[0]
    expect(
      f.liveOwners.has({
        recordingId: row.id,
        producerIncarnation: row.producerIncarnation,
        ownerEpoch: row.ownerEpoch
      })
    ).toBe(false)
  } finally {
    await f.manager.destroy()
  }
})

test('a later clear waits for an earlier unresolved clear completion', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'two-clears')
    await f.controller.stopOrdered('two-clears')
    let batch = await f.controller.prepareOrdered('two-clears', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('two-clears', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let lost = false,
      deliverCompletion
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let clearing = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            clearing = true
            return remove(key)
          }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          if (clearing && !lost) {
            lost = true
            deliverCompletion = complete
          } else complete?.()
        }
      })
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const first = f.controller.clear('two-clears')
    first.catch(() => {})
    for (let i = 0; i < 200 && !lost; i++) await jest.advanceTimersByTimeAsync(0)
    expect(lost).toBe(true)
    await startRecording(f, 'two-clears')
    await f.controller.stopOrdered('two-clears')
    batch = await f.controller.prepareOrdered('two-clears', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('two-clears', batch.token)
    await expect(f.controller.clear('two-clears')).rejects.toMatchObject({ code: 'capability.unavailable' })
    await jest.advanceTimersByTimeAsync(5000)
    expect(await first).toEqual({ cleared: true, records: 0 })
    deliverCompletion()
    await jest.advanceTimersByTimeAsync(0)
    expect(await f.controller.clear('two-clears')).toEqual({ cleared: true, records: 0 })
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a delayed clear excludes a newer allocation for the same recording', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'clear-allocation-fence')
    await f.controller.stopOrdered('clear-allocation-fence')
    const batch = await f.controller.prepareOrdered('clear-allocation-fence', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('clear-allocation-fence', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let holdNext = false
    const releases = []
    f.storage.ready = () => {
      if (!holdNext) return Promise.resolve()
      holdNext = false
      return new Promise(resolve => releases.push(resolve))
    }
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdNext = true
            return put(value)
          }
        }
        return store
      }
      return tx
    }
    const first = f.controller.clear('clear-allocation-fence').then(
      () => 'cleared',
      () => 'rejected'
    )
    await waitFor(() => releases.length === 1)
    const second = f.controller.clear('clear-allocation-fence').then(
      value => value,
      () => 'rejected'
    )
    expect(await second).toBe('rejected')
    expect(releases).toHaveLength(1)
    releases[0]()
    const firstResult = await first
    expect(firstResult).toBe('cleared')
    expect(f.storage.rows('recordings')).toHaveLength(0)
  } finally {
    await f.manager.destroy()
  }
})

test('a lost explicit retirement reply leaves the clear retired and a retry reports absent', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const f = await publicFixture()
  const previousRange = global.IDBKeyRange
  try {
    await startRecording(f, 'receipt-retirement-lost')
    await f.controller.stopOrdered('receipt-retirement-lost')
    const batch = await f.controller.prepareOrdered('receipt-retirement-lost', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('receipt-retirement-lost', batch.token)
    await f.controller.clear('receipt-retirement-lost')
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    const inspector = createWebRecordingRecovery(f.storage, { retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' }) })
    const { clearReceipts: [receipt] } = await inspector.listIncarnations('receipt-retirement-lost', { after: null, limit: 1, includeClearReceipts: true })
    const transaction = f.storage.transaction.bind(f.storage)
    let lost = false
    f.storage.transaction = (...args) => {
      const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
      let retiring = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const remove = store.delete
          store.delete = key => { if (key === receipt.recordingId) retiring = true; return remove(key) }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', { set: fn => { complete = fn }, get: () => () => {
        if (retiring && !lost) lost = true
        else complete?.()
      } })
      return tx
    }
    const abandoned = inspector.disposeRetained(receipt.targetRecordingId, receipt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt })
    abandoned.catch(() => {})
    await waitFor(() => lost)
    f.storage.transaction = transaction
    expect(await inspector.disposeRetained(receipt.targetRecordingId, receipt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).toEqual({ state: 'absent', deleted: 0 })
  } finally { global.IDBKeyRange = previousRange; await f.manager.destroy() }
})

test('an aborted explicit receipt acknowledgement preserves the outcome for retry', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const f = await publicFixture()
  const previousRange = global.IDBKeyRange
  try {
    await startRecording(f, 'retirement-aborted')
    await f.controller.stopOrdered('retirement-aborted')
    const batch = await f.controller.prepareOrdered('retirement-aborted', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('retirement-aborted', batch.token)
    await f.controller.clear('retirement-aborted')
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    const inspector = createWebRecordingRecovery(f.storage, { retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' }) })
    const { clearReceipts: [receipt] } = await inspector.listIncarnations('retirement-aborted', { after: null, limit: 1, includeClearReceipts: true })
    const transaction = f.storage.transaction.bind(f.storage)
    f.storage.transaction = (...args) => {
      const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const remove = store.delete
          store.delete = key => { if (key === receipt.recordingId) tx.abort(); return remove(key) }
        }
        return store
      }
      return tx
    }
    const options = { acknowledgeExport: true, limit: 1, clearReceipt: receipt }
    await expect(inspector.disposeRetained(receipt.targetRecordingId, receipt.producerIncarnation, options)).rejects.toThrow()
    f.storage.transaction = transaction
    expect(await inspector.disposeRetained(receipt.targetRecordingId, receipt.producerIncarnation, options)).toEqual({ state: 'more', deleted: 1 })
  } finally { global.IDBKeyRange = previousRange; await f.manager.destroy() }
})

test('clearing distinct recording IDs does not create a sequence row for each ID', async () => {
  const f = await publicFixture()
  try {
    for (const id of ['sequence-one', 'sequence-two', 'sequence-three']) {
      await startRecording(f, id)
      await f.controller.stopOrdered(id)
      const batch = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
      await f.controller.acknowledge(id, batch.token)
      await f.controller.clear(id)
    }
    expect(
      f.storage.rows('readerReceipts').filter(row => row.recordingId.startsWith(':ubm-clear-sequence-v1:'))
    ).toEqual([])
  } finally {
    await f.manager.destroy()
  }
})

test('inspection exposes legacy per-ID sequence fences without resetting them', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const storage = recordingDatabase()
  storage.seed('readerReceipts', { recordingId: ':ubm-clear-sequence-v1:legacy', version: 1, sequence: '42' })
  storage.seed('readerReceipts', {
    recordingId: ':ubm-clear-v2:legacy:42',
    version: 1,
    operationId: '42',
    producerIncarnation: 'old',
    ownerEpoch: '1',
    generation: '1'
  })
  const inspector = createWebRecordingRecovery(storage, {
    retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' })
  })
  const result = await inspector.listIncarnations('legacy', { after: null, limit: 1, includeClearReceipts: true })
  expect(result.legacyClearSequence).toBe('42')
  expect(result.clearReceipts).toEqual([expect.objectContaining({ operationId: '42', targetRecordingId: 'legacy' })])
  await inspector.disposeRetained('legacy', 'old', {
    acknowledgeExport: true,
    limit: 1,
    clearReceipt: result.clearReceipts[0]
  })
  expect(
    (await inspector.listIncarnations('legacy', { after: null, limit: 1, includeClearReceipts: true })).clearReceipts
  ).toEqual([])
  expect(storage.rows('readerReceipts').some(row => row.recordingId === ':ubm-clear-sequence-v1:legacy')).toBe(true)
})

test('retained clear receipts across IDs are discoverable through one bounded partition scan', async () => {
  const web = require('../../src/web')
  const f = await publicFixture()
  const previousRange = global.IDBKeyRange
  try {
    const transaction = f.storage.transaction.bind(f.storage)
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const remove = store.delete
          store.delete = key => {
            if (typeof key === 'string' && /^:ubm-clear-v[23]:/.test(key)) tx.abort()
            return remove(key)
          }
        }
        return store
      }
      return tx
    }
    for (const id of ['orphan-one', 'orphan-two']) {
      await startRecording(f, id)
      await f.controller.stopOrdered(id)
      const batch = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
      await f.controller.acknowledge(id, batch.token)
      await f.controller.clear(id)
    }
    f.storage.transaction = transaction
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    const partition = web.createWebRecordingPartition({
      database: f.storage,
      crypto: webcrypto,
      digest: async () => 'x'
    })
    const inspector = await web.createWebRecordingRecoveryInspector(partition)
    const { clearReceipts } = await inspector.listIncarnations('unrelated-id', {
      after: null,
      limit: 2,
      includeClearReceipts: true
    })
    expect(clearReceipts.map(receipt => receipt.targetRecordingId).sort()).toEqual(['orphan-one', 'orphan-two'])
    for (const receipt of clearReceipts) {
      await inspector.disposeRetained(receipt.targetRecordingId, receipt.producerIncarnation, {
        acknowledgeExport: true,
        limit: 1,
        clearReceipt: receipt
      })
    }
    expect(
      (
        await inspector.listIncarnations('unrelated-id', {
          after: null,
          limit: 1,
          includeClearReceipts: true
        })
      ).clearReceipts
    ).toEqual([])
  } finally {
    global.IDBKeyRange = previousRange
    await f.manager.destroy()
  }
})

test('a legacy occupied partition slot is preserved while new clears proceed', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'slot-retry')
    await f.controller.stopOrdered('slot-retry')
    const batch = await f.controller.prepareOrdered('slot-retry', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('slot-retry', batch.token)
    const receipt = {
      recordingId: ':ubm-clear-v3:1',
      version: 2,
      targetRecordingId: 'older',
      operationId: 'g1',
      producerIncarnation: 'old',
      ownerEpoch: '1',
      generation: '1'
    }
    await new Promise((resolve, reject) => {
      const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
      tx.oncomplete = resolve
      tx.onabort = reject
      tx.objectStore('readerReceipts').put(receipt)
      tx.objectStore('readerReceipts').put({ recordingId: ':ubm-clear-global-sequence-v1', version: 1, sequence: '64' })
    })
    expect(await f.controller.clear('slot-retry')).toEqual({ cleared: true, records: 0 })
    expect(f.storage.rows('readerReceipts')).toContainEqual(receipt)
  } finally {
    await f.manager.destroy()
  }
})

test('a retained clear receipt can be inspected and retired only by its exact identity', async () => {
  const web = require('../../src/web')
  const f = await publicFixture({ writable: true })
  const previousRange = global.IDBKeyRange
  try {
    const start = await startRecording(f, 'retirement-recovery')
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
    await f.controller.stopOrdered('retirement-recovery')
    const batch = await f.controller.prepareOrdered('retirement-recovery', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('retirement-recovery', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const remove = store.delete
          store.delete = key => {
            if (typeof key === 'string' && key.startsWith(':ubm-clear-v3:')) tx.abort()
            return remove(key)
          }
        }
        return store
      }
      return tx
    }
    expect(await f.controller.clear('retirement-recovery')).toEqual({ cleared: true, records: 0 })
    f.storage.transaction = transaction
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    const partition = web.createWebRecordingPartition({
      database: f.storage,
      crypto: webcrypto,
      digest: async () => 'x'
    })
    const inspector = await web.createWebRecordingRecoveryInspector(partition)
    const list = async () =>
      (
        await inspector.listIncarnations('retirement-recovery', {
          after: null,
          limit: 1,
          includeClearReceipts: true
        })
      ).clearReceipts
    const [receipt] = await list()
    expect(receipt).toBeDefined()
    const markerEvidence = f.storage.rows('markerLedger')
    const operations = f.storage.rows('markerOperations')
    expect(markerEvidence.length).toBeGreaterThan(0)
    const dispose = clearReceipt =>
      inspector.disposeRetained('retirement-recovery', receipt.producerIncarnation, {
        acknowledgeExport: true,
        limit: 1,
        clearReceipt
      })
    await expect(dispose({ ...receipt, generation: receipt.generation === '1' ? '2' : '1' })).rejects.toThrow()
    expect(await list()).toEqual([receipt])
    expect(await dispose(receipt)).toEqual({ state: 'more', deleted: 1 })
    expect(await list()).toEqual([])
    expect(await dispose(receipt)).toEqual({ state: 'absent', deleted: 0 })
    const replacement = { ...receipt, generation: receipt.generation === '1' ? '2' : '1' }
    await new Promise((resolve, reject) => {
      const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
      tx.oncomplete = resolve
      tx.onabort = reject
      tx.objectStore('readerReceipts').put(replacement)
    })
    await expect(dispose(receipt)).rejects.toThrow()
    expect(await list()).toEqual([replacement])
    // The same operation identity cannot be replaced with conflicting proof.
    await expect(dispose(replacement)).rejects.toThrow()
    expect(await list()).toEqual([replacement])
    expect(f.storage.rows('markerLedger')).toEqual(markerEvidence)
    expect(f.storage.rows('markerOperations')).toEqual(operations)
  } finally {
    global.IDBKeyRange = previousRange
    await f.manager.destroy()
  }
})

test('retiring an unresolved clear receipt cannot make its late reconciliation clear a successor', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const f = await publicFixture()
  try {
    await startRecording(f, 'retired-late')
    await f.controller.stopOrdered('retired-late')
    const batch = await f.controller.prepareOrdered('retired-late', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('retired-late', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let lost = false
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let clearing = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            clearing = true
            return remove(key)
          }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          if (clearing && !lost) lost = true
          else complete?.()
        }
      })
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const first = f.controller.clear('retired-late').then(
      () => 'cleared',
      () => 'uncertain'
    )
    for (let i = 0; i < 200 && !lost; i++) await jest.advanceTimersByTimeAsync(0)
    expect(lost).toBe(true)
    const inspector = createWebRecordingRecovery(f.storage, {
      retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' })
    })
    const { clearReceipts } = await inspector.listIncarnations('retired-late', {
      after: null,
      limit: 1,
      includeClearReceipts: true
    })
    const receipt = clearReceipts[0]
    await inspector.disposeRetained('retired-late', receipt.producerIncarnation, {
      acknowledgeExport: true,
      limit: 1,
      clearReceipt: receipt
    })
    await startRecording(f, 'retired-late')
    const successor = f.storage.rows('recordings')[0].producerIncarnation
    await jest.advanceTimersByTimeAsync(5000)
    expect(await first).toBe('cleared')
    expect(f.storage.rows('recordings')[0].producerIncarnation).toBe(successor)
    await f.controller.stopOrdered('retired-late')
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a committed clear remains discoverable when both completion and reconciliation replies are lost', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const f = await publicFixture()
  try {
    await startRecording(f, 'both-replies-lost')
    await f.controller.stopOrdered('both-replies-lost')
    const batch = await f.controller.prepareOrdered('both-replies-lost', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('both-replies-lost', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let clearLost = false,
      barrierLost = false
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let clearing = false,
        checking = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            clearing = true
            return remove(key)
          }
        }
        if (name === 'readerReceipts') {
          const get = store.get
          store.get = key => {
            if (clearLost && typeof key === 'string' && key.startsWith(':ubm-clear-v4:')) checking = true
            return get(key)
          }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          if (clearing && !clearLost) clearLost = true
          else if (checking && !barrierLost) barrierLost = true
          else complete?.()
        }
      })
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const clear = f.controller.clear('both-replies-lost').then(
      () => 'cleared',
      () => 'uncertain'
    )
    for (let i = 0; i < 200 && !clearLost; i++) await jest.advanceTimersByTimeAsync(0)
    expect(clearLost).toBe(true)
    await jest.advanceTimersByTimeAsync(5000)
    expect(barrierLost).toBe(true)
    await jest.advanceTimersByTimeAsync(5000)
    expect(await clear).toBe('uncertain')
    f.storage.transaction = transaction
    const inspector = createWebRecordingRecovery(f.storage, {
      retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' })
    })
    const { clearReceipts } = await inspector.listIncarnations('both-replies-lost', {
      after: null,
      limit: 1,
      includeClearReceipts: true
    })
    expect(clearReceipts).toHaveLength(1)
    const receipt = clearReceipts[0]
    await inspector.disposeRetained('both-replies-lost', receipt.producerIncarnation, {
      acknowledgeExport: true,
      limit: 1,
      clearReceipt: receipt
    })
    expect(f.storage.rows('readerReceipts').filter(row => row.recordingId.startsWith(':ubm-clear-v3:'))).toEqual([])
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a clear that commits after its caller gives up leaves a discoverable exact receipt', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const f = await publicFixture()
  try {
    await startRecording(f, 'late-clear')
    await f.controller.stopOrdered('late-clear')
    const batch = await f.controller.prepareOrdered('late-clear', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('late-clear', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let holdNext = false,
      release
    f.storage.ready = () => {
      if (!holdNext) return Promise.resolve()
      holdNext = false
      return new Promise(resolve => {
        release = resolve
      })
    }
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdNext = true
            return put(value)
          }
        }
        return store
      }
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const clear = f.controller.clear('late-clear').then(
      () => 'cleared',
      () => 'uncertain'
    )
    for (let i = 0; i < 200 && !release; i++) await jest.advanceTimersByTimeAsync(0)
    expect(release).toBeDefined()
    await jest.advanceTimersByTimeAsync(5000)
    expect(await clear).toBe('uncertain')
    release()
    for (let i = 0; i < 200 && f.storage.rows('recordings').length; i++) await jest.advanceTimersByTimeAsync(0)
    expect(f.storage.rows('recordings')).toHaveLength(0)
    const inspector = createWebRecordingRecovery(f.storage, {
      retained: id => ({ lower: `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`, upper: '\uffff' })
    })
    const { clearReceipts } = await inspector.listIncarnations('late-clear', {
      after: null,
      limit: 1,
      includeClearReceipts: true
    })
    expect(clearReceipts).toHaveLength(1)
    const receipt = clearReceipts[0]
    await inspector.disposeRetained('late-clear', receipt.producerIncarnation, {
      acknowledgeExport: true,
      limit: 1,
      clearReceipt: receipt
    })
    expect(f.storage.rows('readerReceipts').filter(row => row.recordingId.startsWith(':ubm-clear-v3:'))).toEqual([])
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('unbound cleanup preserves a declaration whose reader evidence was altered', async () => {
  const peerModule = require('../../src/web/web-recording-peer-identity')
  const actual = peerModule.createWebRecordingPeerIdentityStore
  jest.spyOn(peerModule, 'createWebRecordingPeerIdentityStore').mockImplementation(db => {
    const store = actual(db)
    return {
      ...store,
      bindOwned: async () => {
        db.mutate('recordings', row => {
          row.readerIntegrity.accountedBytes = 42
        })
        throw new Error('bind aborted')
      }
    }
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('corrupt-prepared')
    })
    expect(result.kind).toBe('prepare-failed')
    expect(f.storage.rows('recordings')[0].readerIntegrity.accountedBytes).toBe(42)
  } finally {
    await f.manager.destroy()
  }
})

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
    await waitFor(
      async () =>
        !f.liveOwners.has(identity) ||
        f.liveOwners.use(identity, async owner => owner.supervisor.status().authorityReleased)
    )
    expect(f.liveOwners.has(identity)).toBe(false)
  } finally {
    await f.manager.destroy()
  }
})

test('a malformed partition clear receipt is reported without hiding valid receipts', async () => {
  const web = require('../../src/web')
  const f = await publicFixture()
  const previousRange = global.IDBKeyRange
  try {
    await new Promise((resolve, reject) => {
      const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
      tx.oncomplete = resolve
      tx.onabort = reject
      tx.objectStore('readerReceipts').put({ recordingId: ':ubm-clear-v3:1', version: 2, broken: true })
      tx.objectStore('readerReceipts').put({
        recordingId: ':ubm-clear-v3:2',
        version: 2,
        targetRecordingId: 'valid',
        operationId: 'g2',
        producerIncarnation: 'old',
        ownerEpoch: '1',
        generation: '1'
      })
    })
    global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
    const partition = web.createWebRecordingPartition({
      database: f.storage,
      crypto: webcrypto,
      digest: async () => 'x'
    })
    const inspector = await web.createWebRecordingRecoveryInspector(partition)
    const page = await inspector.listIncarnations('valid', { after: null, limit: 1, includeClearReceipts: true })
    expect(page.malformedClearReceipts).toEqual([{ recordingId: ':ubm-clear-v3:1', error: 'malformed-clear-receipt' }])
    expect(page.clearReceipts).toHaveLength(1)
    await inspector.disposeRetained('valid', 'old', {
      acknowledgeExport: true,
      limit: 1,
      clearReceipt: page.clearReceipts[0]
    })
    expect(f.storage.rows('readerReceipts')).toContainEqual({
      recordingId: ':ubm-clear-v3:1',
      version: 2,
      broken: true
    })
  } finally {
    global.IDBKeyRange = previousRange
    await f.manager.destroy()
  }
})

test('stalled storage readiness bounds clear calls and prevents late allocation', async () => {
  const f = await publicFixture()
  const releases = []
  try {
    await startRecording(f, 'stalled-ready')
    await f.controller.stopOrdered('stalled-ready')
    const batch = await f.controller.prepareOrdered('stalled-ready', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('stalled-ready', batch.token)
    let readinessCalls = 0
    f.storage.ready = () => {
      readinessCalls++
      return new Promise(resolve => {
        releases.push(resolve)
      })
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    let settled = 0
    const results = Array.from({ length: 80 }, () =>
      f.controller.clear('stalled-ready').then(
        () => {
          settled++
        },
        () => {
          settled++
        }
      )
    )
    await jest.advanceTimersByTimeAsync(5001)
    expect(settled).toBe(80)
    expect(readinessCalls).toBeLessThanOrEqual(64)
    f.storage.ready = () => Promise.resolve()
    releases.forEach(resolve => resolve())
    await jest.advanceTimersByTimeAsync(0)
    await Promise.all(results)
    expect(f.storage.rows('recordings')).toHaveLength(1)
    expect(f.storage.rows('readerReceipts').some(row => row.recordingId === ':ubm-clear-global-sequence-v1')).toBe(
      false
    )
  } finally {
    f.storage.ready = () => Promise.resolve()
    releases.forEach(resolve => resolve())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('an abandoned allocation must settle before a newer clear can start', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'allocation-delayed')
    await f.controller.stopOrdered('allocation-delayed')
    const batch = await f.controller.prepareOrdered('allocation-delayed', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('allocation-delayed', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let holdNext = false,
      holdDelete = false,
      release,
      releaseDelete
    f.storage.ready = () => {
      if (holdNext) {
        holdNext = false
        return new Promise(resolve => {
          release = resolve
        })
      }
      if (holdDelete) {
        holdDelete = false
        return new Promise(resolve => {
          releaseDelete = resolve
        })
      }
      return Promise.resolve()
    }
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdDelete = true
            return put(value)
          }
        }
        if (name === 'authorityReservations' && !release) {
          const get = store.get
          store.get = key => {
            const request = get(key)
            holdNext = true
            return request
          }
        }
        return store
      }
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const first = f.controller.clear('allocation-delayed').then(
      () => 'cleared',
      () => 'uncertain'
    )
    for (let i = 0; i < 200 && !release; i++) await jest.advanceTimersByTimeAsync(0)
    expect(release).toBeDefined()
    await jest.advanceTimersByTimeAsync(5001)
    expect(await first).toBe('uncertain')
    await expect(f.controller.clear('allocation-delayed')).rejects.toMatchObject({ code: 'capability.unavailable' })
    release()
    await jest.advanceTimersByTimeAsync(0)
    const second = f.controller.clear('allocation-delayed')
    for (let i = 0; i < 200 && !releaseDelete; i++) await jest.advanceTimersByTimeAsync(0)
    expect(releaseDelete).toBeDefined()
    expect(
      f.storage.rows('readerReceipts').find(row => row.recordingId === ':ubm-clear-global-sequence-v1').sequence
    ).toBe('1')
    releaseDelete()
    await jest.advanceTimersByTimeAsync(0)
    expect(await second).toEqual({ cleared: true, records: 0 })
  } finally {
    f.storage.ready = () => Promise.resolve()
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('retained marker evidence applies partition backpressure and is discoverable across IDs', async () => {
  const web = require('../../src/web')
  const f = await publicFixture({ writable: true })
  const previousRange = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
  try {
    const start = await startRecording(f, 'retention-current')
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
    await f.controller.stopOrdered('retention-current')
    const batch = await f.controller.prepareOrdered('retention-current', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('retention-current', batch.token)
    await new Promise((resolve, reject) => {
      const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
      tx.oncomplete = resolve
      tx.onabort = reject
      for (let i = 0; i < 64; i++)
        tx.objectStore('readerReceipts').put({
          recordingId: `:ubm-marker-retained-v1:${JSON.stringify([`old-${i}`, 'inc'])}`,
          version: 1,
          disposition: 'cleared-marker-evidence-retained',
          markerLedgerBytes: 65536
        })
    })
    await expect(f.controller.clear('retention-current')).rejects.toMatchObject({ code: 'stream.quota' })
    expect(f.storage.rows('recordings')).toHaveLength(1)
    const partition = web.createWebRecordingPartition({
      database: f.storage,
      crypto: webcrypto,
      digest: async () => 'x'
    })
    const inspector = await web.createWebRecordingRecoveryInspector(partition)
    const page = await inspector.listIncarnations('unrelated', {
      after: null,
      limit: 1,
      includePartitionRetained: true
    })
    expect(page.partitionRetained).toHaveLength(32)
    expect(page.partitionRetainedNext).toBeTruthy()
    const entry = page.partitionRetained[0]
    expect(
      await inspector.disposeRetained(entry.recordingId, entry.producerIncarnation, {
        acknowledgeExport: true,
        limit: 32
      })
    ).toEqual({ state: 'disposed', deleted: 0 })
    const next = await inspector.listIncarnations('unrelated', {
      after: null,
      limit: 1,
      includePartitionRetained: true,
      partitionRetainedAfter: page.partitionRetainedNext
    })
    expect(next.partitionRetained).toHaveLength(32)
    const largeKey = `:ubm-marker-retained-v1:${JSON.stringify(['old-1', 'inc'])}`
    f.storage.mutate('readerReceipts', row => {
      if (row.recordingId === largeKey) row.markerLedgerBytes = 1_073_741_824
    })
    await expect(f.controller.clear('retention-current')).rejects.toMatchObject({ code: 'stream.quota' })
    f.storage.mutate('readerReceipts', row => {
      if (row.recordingId === largeKey) row.markerLedgerBytes = 65536
    })
    expect(await f.controller.clear('retention-current')).toEqual({ cleared: true, records: 0 })
  } finally {
    global.IDBKeyRange = previousRange
    await f.manager.destroy()
  }
})

test('lost allocation replies across controllers retain bounded admissions', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'lost-many')
    await f.controller.stopOrdered('lost-many')
    const batch = await f.controller.prepareOrdered('lost-many', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('lost-many', batch.token)
    const other = await createWebContinuationRecordingController({
      database: f.storage,
      liveOwners: f.liveOwners,
      locks: f.locks
    })
    const transaction = f.storage.transaction.bind(f.storage)
    let lost = 0
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let allocation = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') allocation = true
            return put(value)
          }
        }
        return store
      }
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          if (allocation) lost++
          else complete?.()
        }
      })
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const calls = Array.from({ length: 80 }, (_, i) =>
      (i % 2 ? other : f.controller).clear('lost-many').then(
        () => 'cleared',
        () => 'unavailable'
      )
    )
    for (let i = 0; i < 200 && lost < 1; i++) await jest.advanceTimersByTimeAsync(0)
    expect(lost).toBe(1)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await Promise.all(calls)).toEqual(Array(80).fill('unavailable'))
    expect(f.storage.rows('recordings')).toHaveLength(1)
    const before = lost
    await expect(other.clear('lost-many')).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(lost).toBe(before)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('unresolved clear deletion dispatches retain admission after callers time out', async () => {
  const f = await publicFixture()
  const releases = []
  try {
    await startRecording(f, 'lost-clear-work')
    await f.controller.stopOrdered('lost-clear-work')
    const batch = await f.controller.prepareOrdered('lost-clear-work', { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge('lost-clear-work', batch.token)
    const transaction = f.storage.transaction.bind(f.storage)
    let holdNext = false
    f.storage.ready = () => {
      if (!holdNext) return Promise.resolve()
      holdNext = false
      return new Promise(resolve => releases.push(resolve))
    }
    f.storage.transaction = (...args) => {
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdNext = true
            return put(value)
          }
        }
        return store
      }
      return tx
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    for (let i = 0; i < 65; i++) {
      const outcome = f.controller.clear('lost-clear-work').then(
        () => 'cleared',
        () => 'unavailable'
      )
      for (let j = 0; j < 200 && releases.length < 1; j++) await jest.advanceTimersByTimeAsync(0)
      await jest.advanceTimersByTimeAsync(5001)
      expect(await outcome).toBe('unavailable')
    }
    expect(releases).toHaveLength(1)
    expect(f.storage.rows('recordings')).toHaveLength(1)
  } finally {
    f.storage.ready = () => Promise.resolve()
    releases.forEach(resolve => resolve())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('corrupt anchors and legacy sequence remain accessible without hiding valid evidence', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = transactionalDatabase()
  const corrupt = { recordingId: ':ubm-marker-retained-v1:["mixed","bad"]', version: 1, bytes: [0, 255] }
  const valid = {
    recordingId: ':ubm-marker-retained-v1:["mixed","good"]',
    version: 1,
    disposition: 'cleared-marker-evidence-retained',
    markerLedgerBytes: 8
  }
  const legacy = { recordingId: ':ubm-clear-sequence-v1:mixed', version: 1, sequence: 'bad', bytes: [255, 0] }
  await new Promise((resolve, reject) => {
    const tx = db.transaction(['readerReceipts'], 'readwrite')
    tx.oncomplete = resolve
    tx.onabort = reject
    for (const row of [corrupt, valid, legacy]) tx.objectStore('readerReceipts').put(row)
  })
  const page = await createWebRecordingRecovery(db).listIncarnations('mixed', {
    after: null,
    limit: 2,
    includePartitionRetained: true,
    includeClearReceipts: true
  })
  expect(page.retained).toEqual(['good'])
  expect(page.partitionRetained).toEqual([{ recordingId: 'mixed', producerIncarnation: 'good', markerLedgerBytes: 8 }])
  expect(page.malformedRetained).toEqual([{ recordingId: corrupt.recordingId, value: corrupt }])
  expect(page.malformedPartitionRetained).toEqual([{ recordingId: corrupt.recordingId, value: corrupt }])
  expect(page.malformedLegacyClearSequence).toEqual({ recordingId: legacy.recordingId, value: legacy })
  expect(db.rows('readerReceipts')).toEqual(expect.arrayContaining([corrupt, valid, legacy]))
})

test('corrupt look-ahead anchor does not prevent returning the preceding partition page', async () => {
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = transactionalDatabase()
  await new Promise((resolve, reject) => {
    const tx = db.transaction(['readerReceipts'], 'readwrite')
    tx.oncomplete = resolve
    tx.onabort = reject
    for (let i = 0; i < 32; i++)
      tx.objectStore('readerReceipts').put({
        recordingId: `:ubm-marker-retained-v1:${JSON.stringify([`id${String(i).padStart(2, '0')}`, 'inc'])}`,
        version: 1,
        disposition: 'cleared-marker-evidence-retained',
        markerLedgerBytes: 1
      })
    tx.objectStore('readerReceipts').put({ recordingId: ':ubm-marker-retained-v1:["zz","bad"]', bytes: [0, 255] })
  })
  const recovery = createWebRecordingRecovery(db)
  const first = await recovery.listIncarnations('unused', { after: null, limit: 1, includePartitionRetained: true })
  expect(first.partitionRetained).toHaveLength(32)
  expect(first.partitionRetainedNext).toBeTruthy()
  const next = await recovery.listIncarnations('unused', {
    after: null,
    limit: 1,
    includePartitionRetained: true,
    partitionRetainedAfter: first.partitionRetainedNext
  })
  expect(next.malformedPartitionRetained).toEqual([
    {
      recordingId: ':ubm-marker-retained-v1:["zz","bad"]',
      value: { recordingId: ':ubm-marker-retained-v1:["zz","bad"]', bytes: [0, 255] }
    }
  ])
})
