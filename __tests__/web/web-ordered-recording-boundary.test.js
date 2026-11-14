// The current Web host must not pretend that an in-memory stream is a journal.
const {
  createWebBleManager,
  createWebBleManagerWithEnvironment,
  createWebRecordingController,
  createWebRecordingPartition
} = require('unified-ble-manager/web')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { webcrypto } = require('crypto')
const { BleError } = require('../../src/public/errors')

const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'

function expectUnsupported(action) {
  try {
    action()
    throw new Error('Expected an unsupported recording operation')
  } catch (error) {
    expect(error).toBeInstanceOf(BleError)
    expect(error.code).toBe('capability.unsupported')
  }
}

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

test.each([
  [false, 'throw'],
  [true, 'throw'],
  [false, 'error'],
  [true, 'error'],
  [false, 'getter'],
  [true, 'getter']
])(
  'factory (default=%s, open=%s) defers unavailable recording storage and preserves BLE operations',
  async (useDefaultFactory, mode) => {
    const keys = ['indexedDB', 'navigator', 'crypto', 'isSecureContext', 'window']
    const originals = Object.fromEntries(keys.map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
    const failure = new Error('storage denied')
    const open = jest.fn(() => {
      if (mode === 'throw') throw failure
      const request = { error: failure }
      queueMicrotask(() => request.onerror())
      return request
    })
    const storageGetter = jest.fn(() => {
      throw failure
    })
    Object.defineProperty(
      globalThis,
      'indexedDB',
      mode === 'getter' ? { configurable: true, get: storageGetter } : { configurable: true, value: { open } }
    )
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { locks: { request: jest.fn() } }
    })
    Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
    if (useDefaultFactory)
      Object.defineProperty(globalThis, 'window', {
        configurable: true,
        value: { addEventListener() {}, removeEventListener() {} }
      })
    let f
    try {
      f = await fixture(undefined, useDefaultFactory)
      expect(open).not.toHaveBeenCalled()
      const characteristic = f.database.characteristic(SERVICE, CHARACTERISTIC)
      await characteristic.write(Uint8Array.from([7]))
      expect(f.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
      const handle = f.database.reserveOrderedRecordingStart()
      const result = await handle.start([{ memberId: 'heart', characteristic }], {
        recording: { id: 'storage-denied', maxBytes: 1048576, maxRecords: 20 }
      })
      expect(result).toMatchObject({
        kind: 'prepare-failed',
        receipt: {
          sourceStarted: false,
          error: {
            kind: 'source',
            source: {
              code: 'capability.unavailable',
              domain: 'restoration',
              platformDomain: 'indexeddb',
              platformCode: 'Error'
            }
          }
        }
      })
      if (mode === 'getter') {
        expect(storageGetter).toHaveBeenCalled()
        expect(open).not.toHaveBeenCalled()
      } else expect(open).toHaveBeenCalledTimes(1)
      expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
      await handle.retire()
      expect(await handle.status()).toEqual({ state: 'retired' })
      await expect(createWebRecordingController()).rejects.toMatchObject({
        code: 'capability.unavailable',
        domain: 'restoration',
        platform: { domain: 'indexeddb', code: 'Error' }
      })
      if (mode !== 'getter') expect(open).toHaveBeenCalledTimes(2)
      await characteristic.write(Uint8Array.from([8]))
      expect(f.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(2)
    } finally {
      if (f) {
        await f.connection.release()
        await f.manager.destroy()
      }
      for (const [key, descriptor] of Object.entries(originals)) {
        if (descriptor) Object.defineProperty(globalThis, key, descriptor)
        else delete globalThis[key]
      }
    }
  }
)

test.each([
  ['VersionError', 'protocol.incompatible'],
  ['QuotaExceededError', 'stream.quota'],
  ['SecurityError', 'capability.unavailable']
])('default storage %s retains its public classification', async (name, code) => {
  const originals = Object.fromEntries(
    ['indexedDB', 'navigator', 'crypto', 'window', 'isSecureContext'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)])
  )
  const failure = Object.assign(new Error('storage denied'), { name })
  Object.defineProperty(globalThis, 'indexedDB', {
    configurable: true,
    value: {
      open: () => {
        throw failure
      }
    }
  })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: { request: jest.fn() } } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  let f
  try {
    f = await fixture()
    const handle = f.database.reserveOrderedRecordingStart()
    const result = await handle.start(
      [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }],
      { recording: { id: 'classified-storage', maxBytes: 1048576, maxRecords: 20 } }
    )
    expect(result).toMatchObject({
      kind: 'prepare-failed',
      receipt: {
        error: {
          kind: 'source',
          source: { code, domain: 'restoration', platformDomain: 'indexeddb', platformCode: name }
        }
      }
    })
    await expect(createWebRecordingController()).rejects.toMatchObject({
      code,
      domain: 'restoration',
      platform: { domain: 'indexeddb', code: name }
    })
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
    await handle.retire()
  } finally {
    if (f) await f.manager.destroy()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test('cancelling while storage is opening leaves no radio source and closes an abandoned late open', async () => {
  const keys = ['indexedDB', 'navigator', 'crypto']
  const originals = Object.fromEntries(keys.map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  const close = jest.fn()
  const keysByStore = {
    recordings: 'id',
    records: ['recordingId', 'ordinal'],
    pendingIdentities: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'],
    readerReceipts: 'recordingId',
    authorityReservations: 'id',
    markerLedger: ['recordingId', 'producerIncarnation', 'markerId'],
    markerOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    recoveryOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    abandonments: ['recordingId', 'producerIncarnation'],
    terminalEvidence: 'ref',
    terminalEvidenceState: 'key'
  }
  const request = {
    result: {
      close,
      objectStoreNames: { length: Object.keys(keysByStore).length, contains: name => Object.hasOwn(keysByStore, name) },
      transaction: () => ({ objectStore: name => ({ keyPath: keysByStore[name] }) })
    },
    error: null,
    transaction: null
  }
  const open = jest.fn(() => request)
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: { request: jest.fn() } } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  let f
  try {
    f = await fixture()
    expect(open).not.toHaveBeenCalled()
    const handle = f.database.reserveOrderedRecordingStart()
    const abort = new AbortController()
    const start = handle.start(
      [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }],
      { recording: { id: 'cancel-opening', maxBytes: 1048576, maxRecords: 20 }, signal: abort.signal }
    )
    for (let i = 0; i < 50 && open.mock.calls.length === 0; i++) await new Promise(resolve => setTimeout(resolve, 0))
    expect(open).toHaveBeenCalledTimes(1)
    abort.abort()
    await expect(start).rejects.toMatchObject({ code: 'operation.aborted' })
    for (let i = 0; i < 50 && (await handle.status()).state === 'pending'; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(await handle.status()).toMatchObject({ state: 'result', result: { kind: 'prepare-failed' } })
    await handle.retire()
    request.onsuccess()
    expect(close).toHaveBeenCalledTimes(1)
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
  } finally {
    if (f) await f.manager.destroy()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test('cancelling a recording does not abandon a controller sharing its pending open', async () => {
  const originals = Object.fromEntries(
    ['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)])
  )
  const keys = {
    recordings: 'id',
    records: ['recordingId', 'ordinal'],
    pendingIdentities: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'],
    readerReceipts: 'recordingId',
    authorityReservations: 'id',
    markerLedger: ['recordingId', 'producerIncarnation', 'markerId'],
    markerOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    recoveryOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    abandonments: ['recordingId', 'producerIncarnation'],
    terminalEvidence: 'ref',
    terminalEvidenceState: 'key'
  }
  const db = {
    close: jest.fn(),
    objectStoreNames: { length: Object.keys(keys).length, contains: name => Object.hasOwn(keys, name) },
    transaction: () => ({ objectStore: name => ({ keyPath: keys[name] }) })
  }
  const request = { result: db, error: null, transaction: null }
  const open = jest.fn(() => request)
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: { request: jest.fn() } } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  let f
  try {
    f = await fixture()
    const handle = f.database.reserveOrderedRecordingStart()
    const abort = new AbortController()
    const start = handle.start(
      [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }],
      { recording: { id: 'shared-opening', maxBytes: 1048576, maxRecords: 20 }, signal: abort.signal }
    )
    for (let i = 0; i < 50 && open.mock.calls.length === 0; i++) await new Promise(resolve => setTimeout(resolve, 0))
    const controller = createWebRecordingController()
    abort.abort()
    await expect(start).rejects.toMatchObject({ code: 'operation.aborted' })
    request.onsuccess()
    await expect(controller).resolves.toBeDefined()
    expect(open).toHaveBeenCalledTimes(1)
    expect(db.close).not.toHaveBeenCalled()
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
    await handle.retire()
  } finally {
    if (f) await f.manager.destroy()
    if (db.onversionchange) db.onversionchange()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test('recording reservations fail closed without acquiring or writing to the radio', async () => {
  const f = await fixture()
  try {
    expectUnsupported(() => f.database.reserveOrderedRecordingStart())
    expectUnsupported(() => f.database.reserveOrderedRecordingMarker('recording'))
    expectUnsupported(() => f.database.reserveOrderedRecordingReclaim({ kind: 'ordered-recording-recovery' }))
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
    expect(f.characteristic.writeValueWithResponse).not.toHaveBeenCalled()
  } finally {
    await f.connection.release()
    await f.manager.destroy()
  }
})

test('configured recording marker reports missing recording as invalid state', async () => {
  const partition = createWebRecordingPartition({
    database: transactionalDatabase(),
    locks: { request: jest.fn() },
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32)
  })
  const f = await fixture(partition)
  try {
    expect(() => f.database.reserveOrderedRecordingMarker('not-started')).toThrow(
      expect.objectContaining({ code: 'lifecycle.invalid-state' })
    )
    expect(f.characteristic.writeValueWithResponse).not.toHaveBeenCalled()
  } finally {
    await f.manager.destroy()
  }
})

test('a refused recording does not disturb a live ordered subscriber while hidden', async () => {
  const f = await fixture()
  try {
    const characteristic = f.database.characteristic(SERVICE, CHARACTERISTIC)
    const setup = await f.database.subscribeOrdered([{ memberId: 'heart', characteristic }])
    expect(setup.state).toBe('ready')
    const iterator = setup.stream[Symbol.asyncIterator]()
    f.emit([1])
    expectUnsupported(() => f.database.reserveOrderedRecordingStart())
    f.emit([2])
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([1]) })
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '2', value: new Uint8Array([2]) })
    f.hide()
    f.emit([3])
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '3', value: new Uint8Array([3]) })
    expect(await setup.stream.close()).toMatchObject({ group: { state: 'released' } })
  } finally {
    await f.manager.destroy()
  }
})

test('public factory shares recording dependencies with its public controller', async () => {
  const originalIndexedDb = Object.getOwnPropertyDescriptor(globalThis, 'indexedDB')
  const open = jest.fn(() => {
    throw new Error('default storage must not be opened')
  })
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  const held = new Set()
  const locks = {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
  const partition = createWebRecordingPartition({
    database: transactionalDatabase(),
    locks,
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32)
  })
  const f = await fixture(partition)
  try {
    const characteristic = f.database.characteristic(SERVICE, CHARACTERISTIC)
    const handle = f.database.reserveOrderedRecordingStart()
    expect(
      await handle.start([{ memberId: 'heart', characteristic }], {
        recording: { id: 'package-public', maxBytes: 1048576, maxRecords: 20 }
      })
    ).toMatchObject({ kind: 'started', receipt: { state: 'active' } })
    f.emit([8])
    const controller = await partition.createController()
    expect(await controller.stopOrdered('package-public')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const batch = await controller.prepareOrdered('package-public', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.some(row => row.record.kind === 'value' && row.record.bytes[0] === 8)).toBe(true)
    expect(open).not.toHaveBeenCalled()
  } finally {
    await f.manager.destroy()
    if (originalIndexedDb) Object.defineProperty(globalThis, 'indexedDB', originalIndexedDb)
    else delete globalThis.indexedDB
  }
})

test('public factory retains a redacted member cleanup failure after retry', async () => {
  const held = new Set()
  const locks = { request(name, _options, callback) {
    if (held.has(name)) return Promise.resolve(callback(null))
    held.add(name)
    return Promise.resolve(callback({})).finally(() => held.delete(name))
  } }
  const partition = createWebRecordingPartition({ database: transactionalDatabase(), locks, crypto: webcrypto, digest: async () => 'ab'.repeat(32) })
  const f = await fixture(partition)
  try {
    const characteristic = f.database.characteristic(SERVICE, CHARACTERISTIC)
    const handle = f.database.reserveOrderedRecordingStart()
    expect(await handle.start([{ memberId: 'heart', characteristic }], { recording: { id: 'factory-cleanup', maxBytes: 1048576, maxRecords: 20 } })).toMatchObject({ kind: 'started' })
    f.characteristic.stopNotifications.mockRejectedValueOnce(new Error('private-radio-message'))
    const controller = await partition.createController()
    expect(await controller.stopOrdered('factory-cleanup')).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    const diagnostic = await controller.runtimeDiagnostic('factory-cleanup')
    expect(diagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'web-gatt.unsubscribe' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-radio-message')
    expect(await controller.stopOrdered('factory-cleanup')).toMatchObject({ hostCleanup: { state: 'released' } })
    await expect(controller.runtimeDiagnostic('factory-cleanup')).rejects.toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.live-owner' })
  } finally { await f.manager.destroy() }
})

test('released recording can be acknowledged, cleared from a fresh controller, and reused', async () => {
  const held = new Set()
  const locks = {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
  const storage = transactionalDatabase()
  const options = { database: storage, locks, crypto: webcrypto, digest: async () => 'ab'.repeat(32) }
  const partition = createWebRecordingPartition(options)
  const f = await fixture(partition)
  try {
    const member = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
    const config = { recording: { id: 'reusable', maxBytes: 1048576, maxRecords: 20 } }
    await f.database.reserveOrderedRecordingStart().start(member, config)
    const controller = await partition.createController()
    expect(await controller.stopOrdered('reusable')).toMatchObject({ hostCleanup: { state: 'released' } })
    const batch = await controller.prepareOrdered('reusable', { maxItems: 8, maxBytes: 10000 })
    await controller.acknowledge('reusable', batch.token)
    expect(await (await createWebRecordingPartition(options).createController()).clear('reusable')).toEqual({
      cleared: true,
      records: 0
    })
    expect(await f.database.reserveOrderedRecordingStart().start(member, config)).toMatchObject({
      kind: 'started',
      receipt: { state: 'active' }
    })
    await controller.stopOrdered('reusable')
  } finally {
    await f.manager.destroy()
  }
})

test('default Web factory and controller share browser storage dependencies', async () => {
  const storage = transactionalDatabase()
  const keys = {
    recordings: 'id',
    records: ['recordingId', 'ordinal'],
    pendingIdentities: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'],
    readerReceipts: 'recordingId',
    authorityReservations: 'id',
    markerLedger: ['recordingId', 'producerIncarnation', 'markerId'],
    markerOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    recoveryOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    abandonments: ['recordingId', 'producerIncarnation'],
    terminalEvidence: 'ref',
    terminalEvidenceState: 'key'
  }
  storage.objectStoreNames = { length: Object.keys(keys).length, contains: name => Object.hasOwn(keys, name) }
  storage.close = () => {}
  const transaction = storage.transaction.bind(storage)
  storage.transaction = (...args) => {
    const tx = transaction(...args)
    const store = tx.objectStore.bind(tx)
    tx.objectStore = name => Object.assign(store(name), { keyPath: keys[name] })
    return tx
  }
  const locksHeld = new Set()
  const locks = {
    request(name, _options, callback) {
      if (locksHeld.has(name)) return Promise.resolve(callback(null))
      locksHeld.add(name)
      return Promise.resolve(callback({})).finally(() => locksHeld.delete(name))
    }
  }
  const originals = Object.fromEntries(
    ['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)])
  )
  const open = jest.fn(() => {
    const request = { result: storage, transaction: null, error: null }
    queueMicrotask(() => request.onsuccess())
    return request
  })
  Object.defineProperty(globalThis, 'indexedDB', {
    configurable: true,
    value: { open }
  })
  const obsoleteRequest = jest.fn(() => {
    throw new Error('obsolete coordinator')
  })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: { request: obsoleteRequest } } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  Object.defineProperty(globalThis, 'window', { configurable: true, value: { addEventListener() {}, removeEventListener() {} } })
  const unopened = await fixture(undefined, true)
  await unopened.manager.destroy()
  expect(open).not.toHaveBeenCalled()
  const f = await fixture(undefined, true)
  const other = await fixture(undefined, true)
  try {
    expect(open).not.toHaveBeenCalled()
    Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks } })
    const member = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
    expect(
      await f.database
        .reserveOrderedRecordingStart()
        .start(member, { recording: { id: 'default-partition', maxBytes: 1048576, maxRecords: 20 } })
    ).toMatchObject({ kind: 'started' })
    const controller = await createWebRecordingController()
    f.characteristic.stopNotifications.mockRejectedValueOnce(new Error('private-default-cleanup'))
    expect(await controller.stopOrdered('default-partition')).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    const diagnostic = await controller.runtimeDiagnostic('default-partition')
    expect(diagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'web-gatt.unsubscribe' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-default-cleanup')
    expect(await controller.stopOrdered('default-partition')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    const otherMember = [{ memberId: 'heart', characteristic: other.database.characteristic(SERVICE, CHARACTERISTIC) }]
    expect(
      await other.database.reserveOrderedRecordingStart().start(otherMember, {
        recording: { id: 'second-manager', maxBytes: 1048576, maxRecords: 20 }
      })
    ).toMatchObject({ kind: 'started' })
    expect(await (await createWebRecordingController()).stopOrdered('second-manager')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    expect(open).toHaveBeenCalledTimes(1)
    expect(obsoleteRequest).not.toHaveBeenCalled()
  } finally {
    await other.manager.destroy()
    await f.manager.destroy()
    storage.onversionchange()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test('version change reopens for new recordings while an existing live owner remains observable', async () => {
  const storage = transactionalDatabase()
  const keys = {
    recordings: 'id',
    records: ['recordingId', 'ordinal'],
    pendingIdentities: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'],
    readerReceipts: 'recordingId',
    authorityReservations: 'id',
    markerLedger: ['recordingId', 'producerIncarnation', 'markerId'],
    markerOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    recoveryOperations: ['recordingId', 'producerIncarnation', 'operationId'],
    abandonments: ['recordingId', 'producerIncarnation'],
    terminalEvidence: 'ref',
    terminalEvidenceState: 'key'
  }
  const databases = []
  const open = jest.fn(() => {
    let closed = false
    const db = {
      objectStoreNames: { length: Object.keys(keys).length, contains: name => Object.hasOwn(keys, name) },
      close: jest.fn(() => {
        closed = true
      }),
      transaction(...args) {
        if (closed) throw Object.assign(new Error('connection closed'), { name: 'InvalidStateError' })
        const tx = storage.transaction(...args)
        const getStore = tx.objectStore.bind(tx)
        tx.objectStore = name => Object.assign(getStore(name), { keyPath: keys[name] })
        return tx
      }
    }
    databases.push(db)
    const request = { result: db, error: null, transaction: null }
    queueMicrotask(() => request.onsuccess())
    return request
  })
  const held = new Set()
  const locks = {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
  const originals = Object.fromEntries(
    ['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)])
  )
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  let f
  try {
    f = await fixture()
    const members = [{ memberId: 'heart', characteristic: f.database.characteristic(SERVICE, CHARACTERISTIC) }]
    const config = id => ({ recording: { id, maxBytes: 1048576, maxRecords: 20 } })
    expect(
      await f.database.reserveOrderedRecordingStart().start(members, config('before-version-change'))
    ).toMatchObject({ kind: 'started' })
    databases[0].onversionchange()
    const controller = await createWebRecordingController()
    expect(open).toHaveBeenCalledTimes(2)
    const stopped = await controller.stopOrdered('before-version-change')
    expect(stopped).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    expect(
      await f.database.reserveOrderedRecordingStart().start(members, config('after-version-change'))
    ).toMatchObject({ kind: 'started' })
    expect(await controller.stopOrdered('after-version-change')).toMatchObject({ recording: { state: 'finished' } })
  } finally {
    if (f) await f.manager.destroy()
    for (const db of databases) if (db.onversionchange) db.onversionchange()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})
