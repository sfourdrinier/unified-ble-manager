const { openWebRecordingDatabase } = require('../../src/web/web-recording-database')

const oldStores = [
  ['recordings', 'id'], ['records', ['recordingId', 'ordinal']],
  ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
  ['readerReceipts', 'recordingId'], ['authorityReservations', 'id']
]
const v4Stores = [
  ['markerLedger', ['recordingId', 'producerIncarnation', 'markerId']],
  ['markerOperations', ['recordingId', 'producerIncarnation', 'operationId']],
  ['recoveryOperations', ['recordingId', 'producerIncarnation', 'operationId']],
  ['abandonments', ['recordingId', 'producerIncarnation']]
]

test.each([2, 3, 4])('upgrades schema version %i without rewriting old stores or rows', async version => {
  const names = new Map(version === 4 ? [...oldStores, ...v4Stores] : oldStores)
  const rows = new Map([['recordings', [{ id: 'preserved', evidence: version }]]])
  const writes = []
  const db = { objectStoreNames: { get length() { return names.size }, contains: name => names.has(name) },
    createObjectStore: (name, { keyPath }) => { names.set(name, keyPath); return { put: value => writes.push(value) } },
    close: jest.fn(), transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }) }
  const req = { result: db, error: null, transaction: { abort: jest.fn(), objectStore: name => ({ keyPath: names.get(name) }) } }
  const factory = { open: jest.fn(() => req) }
  const opening = openWebRecordingDatabase(factory)
  req.onupgradeneeded({ oldVersion: version })
  req.onsuccess()
  await expect(opening).resolves.toBe(db)
  expect(factory.open).toHaveBeenCalledWith('ubm-ordered-recordings', 5)
  expect(names.get('markerLedger')).toEqual(['recordingId', 'producerIncarnation', 'markerId'])
  expect(names.get('markerOperations')).toEqual(['recordingId', 'producerIncarnation', 'operationId'])
  expect(names.get('recoveryOperations')).toEqual(['recordingId', 'producerIncarnation', 'operationId'])
  expect(names.get('abandonments')).toEqual(['recordingId', 'producerIncarnation'])
  expect(names.get('terminalEvidence')).toBe('ref')
  expect(names.get('terminalEvidenceState')).toBe('key')
  expect(rows.get('recordings')).toEqual([{ id: 'preserved', evidence: version }])
  expect(writes).toEqual([])
})

test('rejects a malformed legacy key path before creating stores', async () => {
  const names = new Map(oldStores)
  names.set('records', 'recordingId')
  const db = { objectStoreNames: { get length() { return names.size }, contains: name => names.has(name) },
    createObjectStore: jest.fn(), close: jest.fn() }
  const tx = { abort: jest.fn(), objectStore: name => ({ keyPath: names.get(name) }) }
  const req = { result: db, error: new Error('aborted'), transaction: tx }
  const opening = openWebRecordingDatabase({ open: () => req })
  req.onupgradeneeded({ oldVersion: 3 })
  req.onerror()
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(tx.abort).toHaveBeenCalledTimes(1)
  expect(db.createObjectStore).not.toHaveBeenCalled()
})

test('closes the connection on version change', async () => {
  const names = new Map()
  const db = { objectStoreNames: { get length() { return names.size }, contains: name => names.has(name) },
    createObjectStore: (name, { keyPath }) => { names.set(name, keyPath); return { put() {} } },
    close: jest.fn(), transaction: () => ({ objectStore: name => ({ keyPath: names.get(name) }) }) }
  const req = { result: db, error: null, transaction: { abort: jest.fn() } }
  const changed = jest.fn()
  const opening = openWebRecordingDatabase({ open: () => req }, { onVersionChange: changed })
  req.onupgradeneeded({ oldVersion: 0 })
  req.onsuccess()
  await opening
  db.onversionchange()
  expect(db.close).toHaveBeenCalledTimes(1)
  expect(changed).toHaveBeenCalledTimes(1)
})
