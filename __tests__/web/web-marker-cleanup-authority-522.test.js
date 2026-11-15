const { createWebRecordingMarkerLedger } = require('../../src/web/web-recording-marker-ledger')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { recordingDatabase } = require('./web-marker-ledger-database-522-helper')

const op = n => n.toString(16).padStart(64, '0')
const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
const declaration = { id: 'cleanup-marker', maxBytes: 1048576, maxRecords: 20, context, producerIncarnation: 'incarnation', collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }] }

async function setup() {
  const db = recordingDatabase()
  const reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({ declaration, ownerToken: 'a'.repeat(64), operationId: op(1), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration: null })
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.open(op(2))
  const ledger = createWebRecordingMarkerLedger(db, owner, declaration.producerIncarnation)
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(1) })
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '1' })
  await ledger.dispatchIntent(op(5), '0')
  return { db, reservations, owner, ledger }
}

test('an exact in-flight write can settle after stop starts cleanup', async () => {
  const { db, reservations, owner, ledger } = await setup()
  await reservations.cleanupPending(owner, op(6))
  await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })
  const result = await ledger.reconcile(op(7))
  expect(result).toMatchObject({ outcome: 'committed', result: { dispatchIntent: true, settlement: { state: 'settled', outcome: 'success' } } })
  expect(await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })).toEqual(result.result)
  await expect(ledger.dispatchIntent(op(8), '0')).rejects.toThrow()
  await expect(ledger.retire(op(9), '0')).rejects.toThrow()
})


test('reconciles a lost settlement reply during cleanup and fences a migrated owner', async () => {
  const { db, reservations, owner, ledger } = await setup()
  await reservations.cleanupPending(owner, op(6))
  db.suppressNextCompletion()
  void ledger.settle(op(7), '0', { state: 'settled', outcome: 'failure', evidence: 'browser rejected write' })
  const receipt = await ledger.reconcile(op(7))
  expect(receipt).toMatchObject({ outcome: 'committed', result: { settlement: { state: 'settled', outcome: 'failure' } } })
  expect(await ledger.settle(op(7), '0', { state: 'settled', outcome: 'failure', evidence: 'browser rejected write' })).toEqual(receipt.result)
  db.mutate('authorityReservations', row => { row.generation = '2'; row.ownerToken = 'b'.repeat(64) })
  await expect(ledger.settle(op(7), '0', { state: 'settled', outcome: 'failure', evidence: 'browser rejected write' })).rejects.toThrow()
  expect(await ledger.reconcile(op(7))).toEqual(receipt)
})

test('cleanup does not authorize a new marker or dispatch', async () => {
  const { reservations, owner, ledger } = await setup()
  await reservations.cleanupPending(owner, op(6))
  await expect(ledger.reserve(op(8), { markerId: '1', target: {}, mode: 'with-response', bytes: Uint8Array.of(2) })).rejects.toThrow()
  await expect(ledger.dispatchIntent(op(9), '0')).rejects.toThrow()
})

test('an aborted settlement transaction retains the exact debt for retry during cleanup', async () => {
  const { db, reservations, owner, ledger } = await setup()
  await reservations.cleanupPending(owner, op(6))
  db.abortNext(new Error('storage unavailable'))
  await expect(ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })).rejects.toThrow('storage unavailable')
  expect(await ledger.inspect('0')).toMatchObject({ dispatchIntent: true, settlement: null })
  expect((await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })).settlement).toEqual({ state: 'settled', outcome: 'success' })
})

test('terminal cannot commit across an unsettled durable write, even after cleanup starts', async () => {
  const { db, reservations, owner, ledger } = await setup()
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '1' } })
  await ledger.acknowledge(op(10), '0', { deliveryId: '1', journalOrdinal: 1 })
  await journal.retire('1')
  await reservations.cleanupPending(owner, op(6))
  const terminal = { id: '2', record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
    secondaryFailure: null, additionalFailureCount: '0' } }
  await expect(journal.commit(terminal)).rejects.toThrow()
  expect(db.rows('recordings')[0].terminal).toBe(false)
  db.abortNext(new Error('settlement failed'))
  await expect(ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })).rejects.toThrow('settlement failed')
  await expect(journal.commit(terminal)).rejects.toThrow()
  await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })
  await journal.commit(terminal)
  await journal.retire('2')
  await journal.seal()
  await reservations.release(owner, op(11))
})

test('a reservation can be cancelled durably after cleanup begins without dispatch', async () => {
  const { db, reservations, owner, ledger } = await setup()
  await ledger.reserve(op(12), { markerId: '1', target: {}, mode: 'with-response', bytes: Uint8Array.of(2) })
  await reservations.cleanupPending(owner, op(6))
  await expect(ledger.admit(op(13), '1', { state: 'not-admitted', evidence: 'cancelled before admission' })).resolves.toMatchObject({ admission: { state: 'not-admitted' } })
  expect(db.rows('markerLedger').find(row => row.markerId === '1').dispatchIntent).toBe(false)
})

test('a terminal that wins the transaction race fences new reservations', async () => {
  const { db, owner, ledger } = await setup()
  await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '1' } })
  await ledger.acknowledge(op(10), '0', { deliveryId: '1', journalOrdinal: 1 })
  const terminal = { id: '2', record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
    secondaryFailure: null, additionalFailureCount: '0' } }
  await journal.commit(terminal)
  await expect(ledger.reserve(op(12), { markerId: '1', target: {}, mode: 'with-response', bytes: Uint8Array.of(2) })).rejects.toThrow()
  expect(db.rows('markerLedger')).toHaveLength(1)
})

test('settlement alone cannot place terminal ahead of the durable marker acknowledgement', async () => {
  const { db, owner, ledger } = await setup()
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '1' } })
  await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })
  const terminal = { id: '2', record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
    secondaryFailure: null, additionalFailureCount: '0' } }
  await expect(journal.commit(terminal)).rejects.toThrow()
  await ledger.acknowledge(op(10), '0', { deliveryId: '1', journalOrdinal: 1 })
  await journal.commit(terminal)
})

test('cleanup permits exact lost-reply retries but no new reserve or dispatch', async () => {
  const { reservations, owner, ledger } = await setup()
  await reservations.cleanupPending(owner, op(6))
  expect(await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(1) })).toMatchObject({ admission: null })
  expect(await ledger.dispatchIntent(op(5), '0')).toMatchObject({ dispatchIntent: true })
  await expect(ledger.reserve(op(12), { markerId: '1', target: {}, mode: 'with-response', bytes: Uint8Array.of(2) })).rejects.toThrow()
})

test('legacy marker evidence with unknown inventory cannot authorize terminal or release', async () => {
  const { db, reservations, owner } = await setup()
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  db.mutate('recordings', row => { delete row.markerDebt })
  await reservations.cleanupPending(owner, op(6))
  const terminal = { id: '1', record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
    secondaryFailure: null, additionalFailureCount: '0' } }
  await expect(journal.commit(terminal)).rejects.toThrow()
  db.mutate('recordings', row => { row.ownerState = 'interrupted' })
  await expect(reservations.release(owner, op(11))).rejects.toThrow()
  expect(db.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
})
