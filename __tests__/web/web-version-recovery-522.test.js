// The current Web host must not pretend that an in-memory stream is a journal.
const {
  createWebBleManager,
  createWebBleManagerWithEnvironment,
  createWebRecordingController
} = require('unified-ble-manager/web')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { webcrypto } = require('crypto')

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
  const open = jest.fn(() => {
    const failure = openError
    let closed = false
    const db = {
      objectStoreNames: { length: Object.keys(keys).length, contains: name => Object.hasOwn(keys, name) },
      close: () => { closed = true },
      transaction(...args) {
        if (closed) throw Object.assign(new Error('closed'), { name: 'InvalidStateError' })
        const tx = storage.transaction(...args)
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
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  try { await run({ storage, databases, held, open, fail: error => { openError = error } }) }
  finally {
    for (const db of databases) db.onversionchange?.()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
}

test('existing controller recovers an active owner and its pending receipt after versionchange', async () => {
  await withStorage(async ({ databases, held, open }) => {
    const f = await fixture()
    try {
      const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
      await f.database.reserveOrderedRecordingStart().start(members, {
        recording: { id: 'recover-owner', maxBytes: 1048576, maxRecords: 20 }
      })
      const controller = await createWebRecordingController()
      f.emit([8])
      let batch
      for (let i = 0; i < 100; i++) {
        batch = await controller.prepareOrdered('recover-owner', { maxItems: 8, maxBytes: 10000 })
        if (batch.token !== null) break
      }
      expect(batch.token).not.toBeNull()
      databases[0].onversionchange()
      expect(await controller.prepareOrdered('recover-owner', { maxItems: 8, maxBytes: 10000 })).toEqual(batch)
      expect(await controller.stopOrdered('recover-owner')).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
      expect(await controller.acknowledge('recover-owner', batch.token)).toMatchObject({ acknowledged: true })
      expect(held.size).toBe(0)
      expect(open).toHaveBeenCalledTimes(2)
    } finally { await f.manager.destroy() }
  })
})

test('existing controller reports incompatible newer database and can retry after storage becomes compatible', async () => {
  await withStorage(async ({ databases, fail }) => {
    const controller = await createWebRecordingController()
    databases[0].onversionchange()
    fail(Object.assign(new Error('newer schema'), { name: 'VersionError' }))
    await expect(controller.status('missing')).rejects.toMatchObject({ code: 'protocol.incompatible' })
    fail(null)
    await expect(controller.status('missing')).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(databases).toHaveLength(2)
  })
})

test('an already started transaction reports its own completion across versionchange', async () => {
  await withStorage(async ({ storage, databases }) => {
    const f = await fixture()
    try {
      const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
      await f.database.reserveOrderedRecordingStart().start(members, {
        recording: { id: 'in-flight', maxBytes: 1048576, maxRecords: 20 }
      })
      const controller = await createWebRecordingController()
      await controller.stopOrdered('in-flight')
      storage.pauseNextCompletion()
      const status = controller.contextOrdered('in-flight')
      await storage.requestsProcessed()
      databases[0].onversionchange()
      storage.complete()
      await expect(status).resolves.toMatchObject({ kind: 'ordered-recording' })
      await expect(controller.contextOrdered('in-flight')).resolves.toMatchObject({ kind: 'ordered-recording' })
    } finally { await f.manager.destroy() }
  })
})

test('active work reopens storage after it closes before a notification', async () => {
  await withStorage(async ({ databases, held }) => {
    const f = await fixture()
    try {
      const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
      await f.database.reserveOrderedRecordingStart().start(members, {
        recording: { id: 'retained-work', maxBytes: 1048576, maxRecords: 20 }
      })
      const controller = await createWebRecordingController()
      databases[0].onversionchange()
      f.emit([9])
      for (let i = 0; i < 20; i++) await Promise.resolve()
      let result
      for (let i = 0; i < 4; i++) {
        result = await controller.stopOrdered('retained-work')
        if (result.hostCleanup.state === 'released') break
      }
      expect(result).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
      const batch = await controller.prepareOrdered('retained-work', { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.some(row => row.record.kind === 'value' && row.record.bytes[0] === 9)).toBe(true)
      expect(held.size).toBe(0)
    } finally { await f.manager.destroy() }
  })
})

test('an aborted transaction is not replayed or reported as committed after versionchange', async () => {
  await withStorage(async ({ storage, databases }) => {
    const f = await fixture()
    try {
      const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
      await f.database.reserveOrderedRecordingStart().start(members, {
        recording: { id: 'aborted-in-flight', maxBytes: 1048576, maxRecords: 20 }
      })
      const controller = await createWebRecordingController()
      storage.pauseNextCompletion()
      storage.abortNext(Object.assign(new Error('aborted'), { name: 'AbortError' }))
      const context = controller.contextOrdered('aborted-in-flight')
      await storage.requestsProcessed()
      databases[0].onversionchange()
      storage.complete()
      await expect(context).rejects.toMatchObject({ code: 'capability.unavailable', platform: { code: 'AbortError' } })
      await expect(controller.contextOrdered('aborted-in-flight')).resolves.toMatchObject({ kind: 'ordered-recording' })
      await controller.stopOrdered('aborted-in-flight')
    } finally { await f.manager.destroy() }
  })
})
