const { openWebRecordingDatabase } = require('../../src/web/web-recording-database')

test('legacy upgrade refuses unknown ownership without deleting journal stores', async () => {
  const names = new Map([['recordings', 'id'], ['records', ['recordingId', 'ordinal']],
    ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
    ['readerReceipts', 'recordingId']])
  const db = { objectStoreNames: { length: names.size, contains: name => names.has(name) },
    createObjectStore: jest.fn(), close: jest.fn(), transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }) }
  const tx = { abort: jest.fn() }
  const req = { result: db, error: new Error('aborted'), transaction: tx }
  const factory = { open: jest.fn(() => req) }
  const opening = openWebRecordingDatabase(factory)
  req.onupgradeneeded({ oldVersion: 1 })
  req.onerror()
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(tx.abort).toHaveBeenCalledTimes(1)
  expect(db.createObjectStore).not.toHaveBeenCalled()
  expect(names.size).toBe(4)
  expect(factory.open).toHaveBeenCalledWith('ubm-ordered-recordings', 5)
})

test('new databases create a versioned reservation store alongside the journal', async () => {
  const names = new Map()
  const db = { objectStoreNames: { get length() { return names.size }, contains: name => names.has(name) },
    createObjectStore: jest.fn((name, { keyPath }) => { names.set(name, keyPath); return { put: jest.fn() } }), close: jest.fn(),
    transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }) }
  const req = { result: db, error: null, transaction: { abort: jest.fn() } }
  const opening = openWebRecordingDatabase({ open: () => req })
  req.onupgradeneeded({ oldVersion: 0 })
  req.onsuccess()
  expect(await opening).toBe(db)
  expect(names.get('authorityReservations')).toBe('id')
  expect(names.size).toBe(11)
})

test('new schema installs a sentinel that makes the old coordinator protocol refuse admission', async () => {
  const names = new Map(), rows = []
  const db = { objectStoreNames: { get length() { return names.size }, contains: name => names.has(name) },
    createObjectStore: (name, { keyPath }) => { names.set(name, keyPath); return { put: row => rows.push([name, row]) } },
    close: jest.fn(), transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }) }
  const req = { result: db, error: null, transaction: { abort: jest.fn() } }
  const opening = openWebRecordingDatabase({ open: () => req })
  req.onupgradeneeded({ oldVersion: 0 })
  req.onsuccess()
  await opening
  expect(rows).toContainEqual(['readerReceipts', { recordingId: ':ubm-authority-policy-v1', version: 2, protocol: 'ubm-authority-reservations/1' }])
})
