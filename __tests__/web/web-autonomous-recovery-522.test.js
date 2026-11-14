const { withAdmissionLocks } = require('./web-clear-admission-locks-helper')
const {
  createWebBleManagerWithEnvironment,
  createWebRecordingController
} = require('unified-ble-manager/web')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { webcrypto } = require('crypto')
beforeAll(() => { global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) } })
afterAll(() => { delete global.IDBKeyRange })

const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'

async function fixture() {
  const listeners = new Set()
  const disconnectListeners = new Set()
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
    addPageLifecycleListener: () => () => {}
  }
  const manager = await createWebBleManagerWithEnvironment({ environment })
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
    }
  }
}


async function withStorage(run) {
  const storage = transactionalDatabase()
  const keys = {
    recordings: 'id', records: ['recordingId', 'ordinal'],
    pendingIdentities: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'],
    readerReceipts: 'recordingId', authorityReservations: 'id',
    markerLedger: ['recordingId', 'producerIncarnation', 'markerId'],
    markerOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    recoveryOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    abandonments: ['recordingId', 'producerIncarnation'],
    terminalEvidence: 'ref',
    terminalEvidenceState: 'key'
  }
  const databases = []
  let openError = null
  let loseCompletion = false
  const open = jest.fn(() => {
    const failure = openError
    let closed = false
    const db = {
      objectStoreNames: { length: Object.keys(keys).length, contains: name => Object.hasOwn(keys, name) },
      close: () => { closed = true },
      transaction(...args) {
        if (closed) throw Object.assign(new Error('closed'), { name: 'InvalidStateError' })
        const tx = storage.transaction(...args)
        if (loseCompletion) {
          loseCompletion = false
          Object.defineProperty(tx, 'oncomplete', {
            configurable: true,
            set(_callback) {
              Object.defineProperty(tx, 'oncomplete', { value: () => {
                tx.error = new Error('completion receipt lost')
                tx.onabort()
              }, writable: true, configurable: true })
            }
          })
        }
        const getStore = tx.objectStore.bind(tx)
        tx.objectStore = name => Object.assign(getStore(name), { keyPath: keys[name] })
        return tx
      }
    }
    const request = { result: db, error: failure, transaction: null }
    if (!failure) databases.push(db)
    queueMicrotask(() => failure ? request.onerror() : request.onsuccess())
    return request
  })
  const held = new Set()
  const locks = { request(name, _options, callback) {
    if (held.has(name)) return Promise.resolve(callback(null))
    held.add(name)
    return Promise.resolve(callback({})).finally(() => held.delete(name))
  } }
  const originals = Object.fromEntries(['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: withAdmissionLocks(locks) } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  try { await run({ storage, databases, held, open, loseNextCompletion: () => { loseCompletion = true }, fail: error => { openError = error } }) }
  finally {
    for (const db of databases) db.onversionchange?.()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
}

async function waitFor(predicate) {
  for (let i = 0; i < 500; i++) {
    if (predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error('Timed out waiting for recording evidence')
}

async function start(f, id) {
  const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
  return f.database.reserveOrderedRecordingStart().start(members, {
    recording: { id, maxBytes: 1048576, maxRecords: 20 }
  })
}

test('the default public controller forwards V2 ledger operations and snapshots admission', async () => {
  await withStorage(async () => {
    const controller = await createWebRecordingController()
    const input = { recordingId: 'public-v2', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }
    const admission = controller.admitClearAttemptV2(input)
    input.requestId = 'b'.repeat(64)
    const attempt = await admission
    expect(attempt.requestId).toBe('a'.repeat(64))
    expect(await controller.inspectClearAttemptV2(attempt)).toEqual({ state: 'pending', attempt })
    const outcome = await controller.cancelClearAttemptV2(attempt)
    expect(await controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired' })
    expect((await controller.clearProofUsageV2()).retained).toBe(0)
  })
})

test('live notifications reopen storage and retain their exact owner before any controller opens', async () => {
  await withStorage(async ({ storage, databases, held, open }) => {
    const f = await fixture()
    try {
      const started = await start(f, 'autonomous')
      expect(started.kind).toBe('started')
      const before = storage.rows('recordings').find(row => row.id === 'autonomous')
      expect(before).toMatchObject({ ownerEpoch: expect.any(String), producerIncarnation: expect.any(String) })
      databases[0].onversionchange()
      f.emit([9])
      await waitFor(() => storage.rows('records').some(row => row.record.kind === 'value'))
      expect(open).toHaveBeenCalledTimes(2)
      const after = storage.rows('recordings').find(row => row.id === 'autonomous')
      expect(after.ownerEpoch).toBe(before.ownerEpoch)
      expect(after.producerIncarnation).toBe(before.producerIncarnation)
      expect(held.size).toBe(1)
      const controller = await createWebRecordingController()
      const batch = await controller.prepareOrdered('autonomous', { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.filter(row => row.record.kind === 'value').map(row => Array.from(row.record.bytes))).toEqual([[9]])
      expect(await controller.prepareOrdered('autonomous', { maxItems: 8, maxBytes: 10000 })).toEqual(batch)
      expect(await controller.stopOrdered('autonomous')).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
      expect(await controller.acknowledge('autonomous', batch.token)).toMatchObject({ acknowledged: true })
      expect(held.size).toBe(0)
    } finally { await f.manager.destroy() }
  })
})

test('a dispatched value commits once across versionchange before the next value reopens', async () => {
  await withStorage(async ({ storage, databases, open }) => {
    const f = await fixture()
    try {
      await start(f, 'inflight-autonomous')
      storage.pauseNextCompletion()
      f.emit([1])
      await storage.requestsProcessed()
      databases[0].onversionchange()
      f.emit([2])
      expect(open).toHaveBeenCalledTimes(1)
      storage.complete()
      await waitFor(() => storage.rows('records').filter(row => row.record.kind === 'value').length === 2)
      expect(storage.rows('records').filter(row => row.record.kind === 'value').map(row => row.record.bytes)).toEqual([[1], [2]])
      const controller = await createWebRecordingController()
      expect(await controller.stopOrdered('inflight-autonomous')).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    } finally { await f.manager.destroy() }
  })
})

test('a lost commit receipt across versionchange reconciles the exact delivery before acknowledging', async () => {
  await withStorage(async ({ storage, databases, held, loseNextCompletion }) => {
    const f = await fixture()
    try {
      await start(f, 'uncertain-autonomous')
      const before = storage.rows('recordings').find(row => row.id === 'uncertain-autonomous')
      storage.pauseNextCompletion()
      loseNextCompletion()
      f.emit([7])
      await storage.requestsProcessed()
      databases[0].onversionchange()
      storage.complete()
      await waitFor(() => storage.rows('pendingIdentities').some(row => row.recordingId === 'uncertain-autonomous' && row.deliveryId === '1' && row.retired === true))
      expect(storage.rows('records').filter(row => row.record.kind === 'value')).toHaveLength(1)
      expect(storage.rows('recordings').find(row => row.id === 'uncertain-autonomous').ownerEpoch).toBe(before.ownerEpoch)
      expect(held.size).toBe(1)
      const controller = await createWebRecordingController()
      const batch = await controller.prepareOrdered('uncertain-autonomous', { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.filter(row => row.record.kind === 'value').map(row => Array.from(row.record.bytes))).toEqual([[7]])
      expect(await controller.prepareOrdered('uncertain-autonomous', { maxItems: 8, maxBytes: 10000 })).toEqual(batch)
      expect(await controller.stopOrdered('uncertain-autonomous')).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    } finally { await f.manager.destroy() }
  })
})

test('an incompatible reopen retains owner and work without claiming a successful retry', async () => {
  await withStorage(async ({ storage, databases, held, fail, open }) => {
    const f = await fixture()
    try {
      await start(f, 'incompatible-autonomous')
      databases[0].onversionchange()
      fail(Object.assign(new Error('newer schema'), { name: 'VersionError' }))
      f.emit([5])
      await waitFor(() => open.mock.calls.length >= 2)
      for (let i = 0; i < 30; i++) await new Promise(resolve => setTimeout(resolve, 0))
      expect(storage.rows('records').filter(row => row.record.kind === 'value')).toHaveLength(0)
      expect(held.size).toBe(1)
      const attempts = open.mock.calls.length
      f.emit([8])
      for (let i = 0; i < 10; i++) await new Promise(resolve => setTimeout(resolve, 0))
      expect(open).toHaveBeenCalledTimes(attempts)
      await expect(createWebRecordingController()).rejects.toMatchObject({ code: 'protocol.incompatible' })
      expect(held.size).toBe(1)
      fail(null)
      const controller = await createWebRecordingController()
      let result = await controller.stopOrdered('incompatible-autonomous')
      for (let i = 0; i < 4 && result.hostCleanup.state !== 'released'; i++) result = await controller.stopOrdered('incompatible-autonomous')
      expect(result).toMatchObject({ recording: { state: 'failed' }, hostCleanup: { state: 'released' } })
      const batch = await controller.prepareOrdered('incompatible-autonomous', { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.filter(row => row.record.kind === 'value').map(row => Array.from(row.record.bytes))).toEqual([[5]])
    } finally { await f.manager.destroy() }
  })
})

test('an aborted in-flight write is not replayed when the connection changes', async () => {
  await withStorage(async ({ storage, databases, held }) => {
    const f = await fixture()
    try {
      await start(f, 'aborted-autonomous')
      storage.pauseNextCompletion()
      storage.abortNext(Object.assign(new Error('transaction aborted'), { name: 'AbortError' }))
      f.emit([6])
      await storage.requestsProcessed()
      databases[0].onversionchange()
      storage.complete()
      await waitFor(() => databases.length === 2)
      for (let i = 0; i < 30; i++) await new Promise(resolve => setTimeout(resolve, 0))
      expect(storage.rows('records').filter(row => row.record.kind === 'value')).toHaveLength(0)
      expect(held.size).toBe(1)
      const controller = await createWebRecordingController()
      let result = await controller.stopOrdered('aborted-autonomous')
      for (let i = 0; i < 4 && result.hostCleanup.state !== 'released'; i++) result = await controller.stopOrdered('aborted-autonomous')
      expect(result).toMatchObject({ recording: { state: 'failed' }, hostCleanup: { state: 'released' } })
      const batch = await controller.prepareOrdered('aborted-autonomous', { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.filter(row => row.record.kind === 'value').map(row => Array.from(row.record.bytes))).toEqual([[6]])
    } finally { await f.manager.destroy() }
  })
})
