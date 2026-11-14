const { openWebRecordingDatabase } = require('../../src/web/web-recording-database')
const { defaultWebRecordingPartition, webRecordingDependencies } = require('../../src/web/web-recording-partition')
const { webcrypto } = require('crypto')

function openRequest() {
  return {
    result: null,
    error: null,
    transaction: null,
    onupgradeneeded: null,
    onsuccess: null,
    onerror: null,
    onblocked: null
  }
}

function database() {
  const names = new Map()
  const stores = new Map()
  return {
    stores,
    objectStoreNames: {
      contains: name => names.has(name),
      get length() {
        return names.size
      }
    },
    createObjectStore: jest.fn((name, options) => {
      names.set(name, options.keyPath)
      const store = { put: jest.fn() }
      stores.set(name, store)
      return store
    }),
    transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }),
    close: jest.fn(),
    onversionchange: null
  }
}

test('creates the versioned partition journal and closes on version change', async () => {
  const request = openRequest()
  const factory = { open: jest.fn(() => request) }
  const db = database()
  request.result = db
  const opening = openWebRecordingDatabase(factory)
  request.onupgradeneeded({ oldVersion: 0 })
  request.onsuccess()
  expect(await opening).toBe(db)
  expect(factory.open).toHaveBeenCalledWith('ubm-ordered-recordings', 5)
  expect(db.createObjectStore.mock.calls).toEqual([
    ['recordings', { keyPath: 'id' }],
    ['records', { keyPath: ['recordingId', 'ordinal'] }],
    [
      'pendingIdentities',
      { keyPath: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId'] }
    ],
    ['readerReceipts', { keyPath: 'recordingId' }],
    ['authorityReservations', { keyPath: 'id' }],
    ['markerLedger', { keyPath: ['recordingId', 'producerIncarnation', 'markerId'] }],
    ['markerOperations', { keyPath: ['recordingId', 'producerIncarnation', 'operationId'] }],
    ['recoveryOperations', { keyPath: ['recordingId', 'producerIncarnation', 'operationId'] }],
    ['abandonments', { keyPath: ['recordingId', 'producerIncarnation'] }],
    ['terminalEvidence', { keyPath: 'ref' }],
    ['terminalEvidenceState', { keyPath: 'key' }]
  ])
  expect(db.stores.get('readerReceipts').put).toHaveBeenCalledWith({
    recordingId: ':ubm-authority-policy-v1',
    version: 2,
    protocol: 'ubm-authority-reservations/1'
  })
  db.onversionchange()
  expect(db.close).toHaveBeenCalledTimes(1)
})

test('blocked opens cannot later create or return a journal', async () => {
  const request = openRequest()
  const db = database()
  const transaction = { abort: jest.fn() }
  request.result = db
  request.transaction = transaction
  const opening = openWebRecordingDatabase({ open: () => request })
  request.onblocked()
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  request.onupgradeneeded({ oldVersion: 0 })
  expect(transaction.abort).toHaveBeenCalledTimes(1)
  expect(db.createObjectStore).not.toHaveBeenCalled()
  request.onsuccess()
  expect(db.close).toHaveBeenCalledTimes(1)
})

test('rejects malformed schemas without modifying them', async () => {
  const request = openRequest()
  const db = database()
  request.result = db
  const opening = openWebRecordingDatabase({ open: () => request })
  request.onsuccess()
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(db.close).toHaveBeenCalledTimes(1)
  expect(db.createObjectStore).not.toHaveBeenCalled()
})

function validDatabase(legacy = false) {
  const db = database()
  for (const [name, keyPath] of [
    ['recordings', 'id'],
    ['records', ['recordingId', 'ordinal']],
    ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
    ['readerReceipts', 'recordingId'],
    ['authorityReservations', 'id']
  ])
    db.createObjectStore(name, { keyPath })
  if (!legacy) {
    for (const [name, keyPath] of [
      ['markerLedger', ['recordingId', 'producerIncarnation', 'markerId']],
      ['markerOperations', ['recordingId', 'producerIncarnation', 'operationId']],
      ['recoveryOperations', ['recordingId', 'producerIncarnation', 'operationId']],
      ['abandonments', ['recordingId', 'producerIncarnation']],
      ['terminalEvidence', 'ref'],
      ['terminalEvidenceState', 'key']
    ]) db.createObjectStore(name, { keyPath })
  }
  return db
}

test('upgrades a compatible version 2 journal without rewriting any stores', async () => {
  const request = openRequest()
  const db = validDatabase(true)
  db.createObjectStore.mockClear()
  request.result = db
  request.transaction = { abort: jest.fn(), objectStore: name => db.transaction().objectStore(name) }
  const opening = openWebRecordingDatabase({ open: () => request })
  request.onupgradeneeded({ oldVersion: 2 })
  request.onsuccess()
  expect(await opening).toBe(db)
  expect(request.transaction.abort).not.toHaveBeenCalled()
  expect(db.createObjectStore.mock.calls).toEqual([
    ['markerLedger', { keyPath: ['recordingId', 'producerIncarnation', 'markerId'] }],
    ['markerOperations', { keyPath: ['recordingId', 'producerIncarnation', 'operationId'] }],
    ['recoveryOperations', { keyPath: ['recordingId', 'producerIncarnation', 'operationId'] }],
    ['abandonments', { keyPath: ['recordingId', 'producerIncarnation'] }],
    ['terminalEvidence', { keyPath: 'ref' }],
    ['terminalEvidenceState', { keyPath: 'key' }]
  ])
})

test('aborts an incompatible version 2 schema during upgrade', async () => {
  const request = openRequest()
  request.result = database()
  request.transaction = { abort: jest.fn(), objectStore: name => request.result.transaction().objectStore(name) }
  const opening = openWebRecordingDatabase({ open: () => request })
  request.onupgradeneeded({ oldVersion: 2 })
  expect(request.transaction.abort).toHaveBeenCalled()
  request.onerror()
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
})

async function withDefaultStorage(run) {
  const keys = ['indexedDB', 'navigator', 'crypto']
  const originals = Object.fromEntries(keys.map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  const requests = []
  const open = jest.fn(() => {
    const request = openRequest()
    requests.push(request)
    return request
  })
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: { request: jest.fn() } } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  try {
    await run({ requests, open })
  } finally {
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
}

test('abandoning the only pending open permits a retry and closes an ordinary late success', async () => {
  await withDefaultStorage(async ({ requests, open }) => {
    const abort = new AbortController()
    const abandoned = defaultWebRecordingPartition(abort.signal)
    abort.abort()
    await expect(abandoned).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const retry = defaultWebRecordingPartition()
    expect(open).toHaveBeenCalledTimes(2)
    const old = validDatabase()
    requests[0].result = old
    requests[0].onsuccess()
    expect(old.close).toHaveBeenCalledTimes(1)
    const current = validDatabase()
    requests[1].result = current
    requests[1].onsuccess()
    await retry
    current.onversionchange()
  })
})

test('cancelling one waiter preserves a shared pending open for another consumer', async () => {
  await withDefaultStorage(async ({ requests, open }) => {
    const abort = new AbortController()
    const cancelled = defaultWebRecordingPartition(abort.signal)
    const retained = defaultWebRecordingPartition()
    abort.abort()
    await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const db = validDatabase()
    requests[0].result = db
    requests[0].onsuccess()
    await retained
    expect(open).toHaveBeenCalledTimes(1)
    expect(db.close).not.toHaveBeenCalled()
    db.onversionchange()
  })
})

test('cancellation before the opened partition is delivered closes the connection', async () => {
  await withDefaultStorage(async ({ requests }) => {
    const abort = new AbortController()
    const pending = defaultWebRecordingPartition(abort.signal)
    const db = validDatabase()
    requests[0].result = db
    requests[0].onsuccess()
    await Promise.resolve()
    abort.abort()
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(db.close).toHaveBeenCalledTimes(1)
  })
})

test('version change before delivery cannot return the closed partition', async () => {
  await withDefaultStorage(async ({ requests }) => {
    const pending = defaultWebRecordingPartition()
    const db = validDatabase()
    requests[0].result = db
    requests[0].onsuccess()
    await Promise.resolve()
    db.onversionchange()
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
    expect(db.close).toHaveBeenCalledTimes(1)
  })
})

test('version change reopens storage while preserving the live-owner registry', async () => {
  await withDefaultStorage(async ({ requests, open }) => {
    const first = defaultWebRecordingPartition()
    const old = validDatabase()
    requests[0].result = old
    requests[0].onsuccess()
    const firstDependencies = webRecordingDependencies(await first)
    const identity = { recordingId: 'active', producerIncarnation: 'producer', ownerEpoch: '1' }
    firstDependencies.liveOwners.install(identity, {})
    old.onversionchange()
    const second = defaultWebRecordingPartition()
    expect(open).toHaveBeenCalledTimes(2)
    const current = validDatabase()
    requests[1].result = current
    requests[1].onsuccess()
    const secondDependencies = webRecordingDependencies(await second)
    expect(secondDependencies.database).toBe(current)
    expect(secondDependencies.liveOwners.has(identity)).toBe(true)
    current.onversionchange()
  })
})

test('default storage open sanitizes a typed-looking error thrown by the host', async () => {
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const spoof = Object.assign(Object.create(BackendContractError.prototype), {
    normalized: { code: 'stream.quota', domain: 'restoration', operation: 'web-recording.open-secret', retryability: 'never', platform: { domain: 'host-secret', code: 'code-secret', safeMessage: 'message-secret', metadata: {} } }
  })
  await withDefaultStorage(async ({ open }) => {
    open.mockImplementation(() => { throw spoof })
    const error = await defaultWebRecordingPartition().catch(value => value)
    expect(error.normalized).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.indexeddb-open', platform: { domain: 'indexeddb' } })
    expect(`${error.message} ${JSON.stringify(error)}`).not.toContain('secret')
  })
})
