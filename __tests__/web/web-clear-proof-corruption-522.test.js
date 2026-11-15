const { createWebClearProofLedger } = require('../../src/web/web-clear-proof-ledger')
const { recordingDatabase } = require('./web-recovery-522-helper')

const metadataKey = ':ubm-staged-proof-ledger-v1'
const identity = n => ({ recordingId: 'sample', producerIncarnation: 'producer', requestId: n.repeat(64) })
const ledger = db => createWebClearProofLedger(db, () => 'a'.repeat(64))

async function remove(db, key) {
  await new Promise((resolve, reject) => {
    const tx = db.transaction(['readerReceipts'], 'readwrite')
    tx.oncomplete = resolve
    tx.onabort = () => reject(tx.error)
    tx.objectStore('readerReceipts').delete(key)
  })
}

beforeEach(() => { global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) } })
afterEach(() => { delete global.IDBKeyRange })

test('missing established metadata fails closed and preserves surviving outcomes and fences', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  const first = await proofs.admitRetryable(identity('b'))
  const cancelled = await proofs.cancel(first, true)
  await proofs.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  const second = await proofs.admitRetryable(identity('c'))
  const before = db.rows('readerReceipts').filter(row => row.recordingId !== metadataKey)
  await remove(db, metadataKey)
  await expect(ledger(db).usage()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  await expect(ledger(db).inspect(second, true)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(db.rows('readerReceipts')).toEqual(before)
})

test('a lone retired request fence prevents metadata reinitialization', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  const attempt = await proofs.admitRetryable(identity('d'))
  const cancelled = await proofs.cancel(attempt, true)
  await proofs.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  await remove(db, metadataKey)
  await expect(ledger(db).admitRetryable(identity('e'))).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(db.rows('readerReceipts').find(row => row.recordingId.startsWith(':ubm-proof-request-v2:'))).toBeDefined()
})

test('a missing retired request fence cannot be readmitted as a new deletion', async () => {
  const db = recordingDatabase(), proofs = ledger(db), input = identity('f')
  const attempt = await proofs.admitRetryable(input)
  const cancelled = await proofs.cancel(attempt, true)
  await proofs.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  await remove(db, `:ubm-proof-request-v2:${input.requestId}`)
  await expect(proofs.admitRetryable(input)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  await expect(proofs.usage()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('missing retired fence is corruption when inspecting the original attempt', async () => {
  const db = recordingDatabase(), proofs = ledger(db), input = identity('7')
  const attempt = await proofs.admitRetryable(input)
  const cancelled = await proofs.cancel(attempt, true)
  await proofs.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  await remove(db, `:ubm-proof-request-v2:${input.requestId}`)
  await expect(proofs.inspect(attempt, true)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('a forged epoch or sequence for a known request is not reported as retired or unknown', async () => {
  const proofs = ledger(recordingDatabase())
  const attempt = await proofs.admitRetryable(identity('6'))
  await expect(proofs.inspect({ ...attempt, epoch: 'c'.repeat(64) }, true)).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  await expect(proofs.inspect({ ...attempt, sequence: '999' }, true)).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
})

test('a request fence whose stored identity disagrees with its key is corrupt', async () => {
  const db = recordingDatabase(), proofs = ledger(db), input = identity('4')
  const attempt = await proofs.admitRetryable(input)
  db.mutate('readerReceipts', row => {
    if (row.recordingId === `:ubm-proof-request-v2:${input.requestId}`) row.attempt.requestId = '5'.repeat(64)
  })
  await expect(proofs.admitRetryable(input)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  await expect(proofs.inspect(attempt, true)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('usage accounts for retained request fences separately from outcomes', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  const attempt = await proofs.admitRetryable(identity('8'))
  const cancelled = await proofs.cancel(attempt, true)
  await proofs.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  expect(await proofs.usage()).toMatchObject({ retained: 0, requestFences: { count: 1, integrity: 'count-verified' } })
  db.mutate('readerReceipts', row => { if (row.recordingId === metadataKey) delete row.requestCount })
  expect(await proofs.usage()).toMatchObject({ requestFences: { count: 1, integrity: 'legacy-unverified' } })
})

test('an explicit fence quota bounds permanent V2 identities without blocking exact retries', async () => {
  const db = recordingDatabase(), first = ledger(db), second = ledger(db)
  const initial = await first.usage()
  await first.configure({ limit: null, fenceLimit: 1, expectedRevision: initial.revision })
  const input = identity('9')
  const attempt = await first.admitRetryable(input)
  const cancelled = await first.cancel(attempt, true)
  await first.retire({ outcome: cancelled, acknowledgeDurableCopy: true }, true)
  expect(await second.admitRetryable(input)).toEqual(attempt)
  await expect(second.admitRetryable(identity('a'))).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  expect(await second.usage()).toMatchObject({ retained: 0, requestFences: { count: 1, limit: 1 } })
})

test('usage includes legacy clear namespace rows without modifying their contents', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  const rows = [
    { recordingId: ':ubm-clear-attempt-v1:legacy', raw: 'preserve' },
    { recordingId: ':ubm-clear-retired-v1:legacy', raw: 'preserve' },
    { recordingId: ':ubm-clear-global-sequence-v1', raw: 'preserve' }
  ]
  rows.forEach(row => db.seed('readerReceipts', row))
  expect(await proofs.usage()).toMatchObject({ clearNamespaceRows: 3 })
  expect(db.rows('readerReceipts')).toEqual(expect.arrayContaining(rows))
})

test('configured empty ledger cannot silently reset its policy after metadata loss', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  const initial = await proofs.usage()
  await proofs.configure({ limit: 1, expectedRevision: initial.revision })
  await remove(db, metadataKey)
  await expect(ledger(db).usage()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('new metadata cannot be used after its independent marker is lost', async () => {
  const db = recordingDatabase(), proofs = ledger(db)
  await proofs.usage()
  await remove(db, ':ubm-staged-proof-ledger-marker-v1')
  await expect(proofs.usage()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(db.rows('readerReceipts').find(row => row.recordingId === metadataKey)).toBeDefined()
})

test('legacy metadata permits an exact retry but cannot classify an absent request as new', async () => {
  const db = recordingDatabase(), proofs = ledger(db), input = identity('1')
  const attempt = await proofs.admitRetryable(input)
  db.mutate('readerReceipts', row => { if (row.recordingId === metadataKey) delete row.requestCount })
  expect(await proofs.admitRetryable(input)).toEqual(attempt)
  await expect(proofs.admitRetryable(identity('2'))).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})
