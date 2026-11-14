const { createWebClearProofLedger } = require('../../src/web/web-clear-proof-ledger')
const { recordingDatabase } = require('./web-marker-ledger-database-522-helper')
beforeAll(() => { global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) } })
afterAll(() => { delete global.IDBKeyRange })

const epoch = 'a'.repeat(64)
const identity = n => ({ recordingId: 'recording', producerIncarnation: 'incarnation', requestId: String(n) })
const make = db => createWebClearProofLedger(db, () => epoch)

test('retryable admission recovers one identity across instances and after retirement', async () => {
  const db = recordingDatabase(), first = make(db), second = make(db)
  const requested = { ...identity(1), requestId: 'b'.repeat(64) }
  const [a, b] = await Promise.all([first.admitRetryable(requested), second.admitRetryable(requested)])
  expect(b).toEqual(a)
  expect((await first.usage()).retained).toBe(1)
  const outcome = await first.cancel(a)
  await second.retire({ outcome, acknowledgeDurableCopy: true })
  expect(await second.admitRetryable(requested)).toEqual(a)
  expect(await first.inspect(a)).toEqual({ state: 'retired-or-unknown' })
  await expect(first.admitRetryable({ ...requested, recordingId: 'another' })).rejects.toThrow()
})

test('retirement compares the outcome values rather than object property order', async () => {
  const ledger = make(recordingDatabase())
  const attempt = await ledger.admitRetryable({ ...identity(1), requestId: 'c'.repeat(64) })
  await ledger.cancel(attempt)
  const reordered = { attempt: { requestId: attempt.requestId, producerIncarnation: attempt.producerIncarnation, recordingId: attempt.recordingId, sequence: attempt.sequence, epoch: attempt.epoch }, state: 'cancelled' }
  expect(await ledger.retire({ outcome: reordered, acknowledgeDurableCopy: true })).toEqual({ state: 'retired' })
})

test('retryable admission recovers a lost reply without using a second slot', async () => {
  const db = recordingDatabase(), first = make(db), second = make(db)
  const initial = await first.usage()
  await first.configure({ limit: 1, expectedRevision: initial.revision })
  const requested = { ...identity(1), requestId: 'd'.repeat(64) }
  db.suppressNextCompletion()
  void first.admitRetryable(requested)
  const attempt = await second.admitRetryable(requested)
  expect(attempt.sequence).toBe('1')
  expect((await second.usage()).retained).toBe(1)
})

test('shared admission atomically enforces an explicitly configured budget', async () => {
  const db = recordingDatabase(),
    first = make(db),
    second = make(db)
  const initial = await first.usage()
  expect(initial.limit).toBeNull()
  const configured = await first.configure({ limit: 1, expectedRevision: initial.revision })
  const results = await Promise.allSettled([first.admit(identity(1)), second.admit(identity(2))])
  expect(results.filter(result => result.status === 'fulfilled')).toHaveLength(1)
  expect(results.filter(result => result.status === 'rejected')[0].reason.normalized.code).toBe('stream.quota')
  expect(await second.usage()).toMatchObject({ limit: 1, retained: 1, revision: configured.revision })
  await expect(second.configure({ limit: 2, expectedRevision: initial.revision })).rejects.toThrow()
})

test('retirement requires an exact cancelled outcome and leaves the issued identity fenced', async () => {
  const db = recordingDatabase(),
    first = make(db),
    second = make(db)
  const attempt = await first.admit(identity(1))
  await expect(
    second.retire({ outcome: { state: 'cancelled', attempt }, acknowledgeDurableCopy: true })
  ).rejects.toThrow()
  const outcome = await second.cancel(attempt)
  await expect(
    first.retire({
      outcome: { ...outcome, attempt: { ...attempt, requestId: 'different' } },
      acknowledgeDurableCopy: true
    })
  ).rejects.toThrow()
  expect(await first.retire({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired' })
  expect(await second.inspect(attempt)).toEqual({ state: 'retired-or-unknown' })
  expect(await second.cancel(attempt)).toEqual({ state: 'retired-or-unknown' })
  expect(await second.retire({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired-or-unknown' })
  const next = await second.admit(identity(2))
  expect(next.sequence).not.toBe(attempt.sequence)
  expect(next.epoch).toBe(attempt.epoch)
})

test('an aborted admission does not charge the ledger or allocate an identity', async () => {
  const db = recordingDatabase(),
    ledger = make(db)
  await ledger.usage()
  db.abortNext(new Error('quota exceeded'))
  await expect(ledger.admit(identity(1))).rejects.toThrow('quota exceeded')
  expect((await ledger.usage()).retained).toBe(0)
  expect((await ledger.admit(identity(2))).sequence).toBe('1')
})

test('a lost admission response can be recovered by paging the durable ledger', async () => {
  const db = recordingDatabase(),
    ledger = make(db)
  await ledger.admit(identity(1))
  const second = await ledger.admit(identity(2))
  db.suppressNextCompletion()
  void ledger.admit(identity(3))
  const page1 = await make(db).list({ after: '0', limit: 2 })
  expect(page1.entries.map(entry => entry.attempt.requestId)).toEqual(['1', '2'])
  expect(page1.next).toBe('2')
  const page2 = await make(db).list({ after: page1.next, limit: 2 })
  expect(page2.entries.map(entry => entry.attempt.requestId)).toEqual(['3'])
  expect(page2.next).toBeNull()
  await ledger.cancel(second)
  await ledger.retire({ outcome: { state: 'cancelled', attempt: second }, acknowledgeDurableCopy: true })
  expect((await make(db).list({ after: '0', limit: 2 })).entries.map(entry => entry.attempt.requestId)).toEqual(['1'])
})

test('pre-existing V1 and archived proof rows are never modified by staged operations', async () => {
  const db = recordingDatabase()
  const legacy = { recordingId: ':ubm-clear-attempt-v1:legacy', unexpected: { raw: 'keep' } }
  const archived = { recordingId: ':ubm-clear-retired-v1:legacy', evidence: 'keep' }
  db.seed('readerReceipts', legacy)
  db.seed('readerReceipts', archived)
  const ledger = make(db),
    attempt = await ledger.admit(identity(1))
  const outcome = await ledger.cancel(attempt)
  await ledger.retire({ outcome, acknowledgeDurableCopy: true })
  expect(db.rows('readerReceipts')).toEqual(expect.arrayContaining([legacy, archived]))
})

test('lowering a budget preserves existing admissions and permits new work after retirement', async () => {
  const db = recordingDatabase(),
    ledger = make(db)
  const first = await ledger.admit(identity(1))
  const second = await ledger.admit(identity(2))
  const initial = await ledger.usage()
  await ledger.configure({ limit: 1, expectedRevision: initial.revision })
  await expect(ledger.admit(identity(3))).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  const outcome1 = await ledger.cancel(first)
  await ledger.retire({ outcome: outcome1, acknowledgeDurableCopy: true })
  await expect(ledger.admit(identity(3))).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  const outcome2 = await ledger.cancel(second)
  await ledger.retire({ outcome: outcome2, acknowledgeDurableCopy: true })
  expect((await ledger.admit(identity(3))).requestId).toBe('3')
})

test('malformed retained proof is preserved and cannot be cancelled or retired', async () => {
  const db = recordingDatabase(),
    ledger = make(db)
  const attempt = await ledger.admit(identity(1))
  db.mutate('readerReceipts', row => {
    if (row.attempt) row.state = 'future-state'
  })
  await expect(ledger.cancel(attempt)).rejects.toThrow()
  await expect(ledger.list({ after: '0', limit: 10 })).rejects.toThrow()
  expect(db.rows('readerReceipts').find(row => row.attempt).state).toBe('future-state')
  expect((await ledger.usage()).retained).toBe(1)
})

test('admission snapshots the requested identity before dispatch', async () => {
  const ledger = make(recordingDatabase())
  const input = identity('before')
  const pending = ledger.admit(input)
  input.requestId = 'after'
  expect((await pending).requestId).toBe('before')
})
