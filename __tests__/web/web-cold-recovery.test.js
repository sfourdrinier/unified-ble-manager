const { webcrypto } = require('crypto')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const token = c => c.repeat(64)
const coordinator = { kind: 'locks', name: 'lock' }
const declaration = { id: 'cold', maxBytes: 1048576, maxRecords: 4, producerIncarnation: 'producer', collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }], context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c', databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] } }
const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
const opts = database => ({ database, locks, digest: async () => 'ab'.repeat(32), crypto: webcrypto })
async function sealed(database) {
  const reservations = createWebAuthorityReservations(database)
  const { owner } = await reservations.acquire({ declaration, ownerToken: token('a'), operationId: token('b'), expectedGeneration: null, coordinator })
  const journal = createWebOrderedRecordingJournal(database, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.open(token('c'))
  await journal.commit({ id: '1', outcome: { version: 1, state: 'finished', error: null }, status: { recordingId: 'cold', phase: 'stopped', accepting: false, records: 1, bytes: 1, lostRecords: 0, maxBytes: declaration.maxBytes, maxRecords: declaration.maxRecords, encrypted: false, orderedOwner: { version: 1, producerIncarnation: 'producer', collectionId: '1', sourceGeneration: { kind: 'opaque', value: 'source' }, state: 'sealed', uncertainTail: null }, runtimeFailure: null, collectionFailure: null }, record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0' } })
  await journal.retire('1')
  await reservations.cleanupPending(owner, token('d'))
  await journal.seal()
  await reservations.release(owner, token('e'))
  return { reservations, owner, journal }
}

test('released sealed recovery fences the original owner without rewriting its receipts', async () => {
  const db = recordingDatabase(), { journal, owner } = await sealed(db)
  const before = db.rows('recordings')[0]
  const recovery = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  expect(await recovery.recover()).toMatchObject({ state: 'sealed', ownerEpoch: owner.ownerEpoch, uncertainTail: null })
  expect(recovery.reservationOwner()).toMatchObject({ generation: '2', ownerEpoch: owner.ownerEpoch })
  expect(db.rows('recordings')[0]).toEqual(before)
  await expect(journal.seal()).rejects.toThrow()
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  expect(await controller.status('cold')).toMatchObject({ phase: 'stopped', orderedOwner: { state: 'sealed' } })
  expect(await controller.stopOrdered('cold')).toMatchObject({ recording: { state: 'finished' } })
  await recovery.release()
  expect(db.rows('authorityReservations')[0].phase).toBe('released')
  expect(db.rows('recordings')[0]).toEqual(before)
})

test('a new coordinator cannot take an armed owner or infer interruption', async () => {
  const db = recordingDatabase(), reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({ declaration, ownerToken: token('a'), operationId: token('b'), expectedGeneration: null, coordinator })
  await reservations.arm(owner, token('c'))
  const before = db.rows('recordings')
  expect(await createWebRecordingAuthority(opts(db), 'cold')).toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
  expect(db.rows('recordings')).toEqual(before)
})


test('subsequent recovery keeps the producer receipt across multiple generations', async () => {
  const db = recordingDatabase()
  await sealed(db)
  const original = db.rows('recordings')[0]
  for (const generation of ['2', '3']) {
    const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
    await authority.recover()
    expect(authority.reservationOwner().generation).toBe(generation)
    await authority.release()
  }
  expect(db.rows('recordings')[0]).toEqual(original)
})

test('released interrupted journal is recovered without manufacturing a terminal', async () => {
  const db = recordingDatabase(), reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({ declaration, ownerToken: token('a'), operationId: token('b'), expectedGeneration: null, coordinator })
  await reservations.arm(owner, token('c'))
  await reservations.cleanupPending(owner, token('d'))
  db.mutate('recordings', row => { row.ownerState = 'interrupted' })
  await reservations.release(owner, token('e'))
  const before = db.rows('recordings')[0]
  const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  expect(await authority.recover()).toMatchObject({ state: 'interrupted', ownerEpoch: '1', uncertainTail: true })
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  expect(await controller.stopOrdered('cold')).toMatchObject({ recording: { state: 'interrupted' } })
  await authority.release()
  expect(db.rows('recordings')[0]).toEqual(before)
})

test('a lost recovery response is reconciled by the same operation before cleanup', async () => {
  jest.useFakeTimers()
  try {
    const db = recordingDatabase()
    await sealed(db)
    const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
    db.suppressNextCompletion()
    const recovering = authority.recover()
    await jest.advanceTimersByTimeAsync(5000)
    expect(await recovering).toMatchObject({ state: 'sealed', ownerEpoch: '1' })
    const acquired = db.rows('authorityReservations')[0]
    expect(acquired).toMatchObject({ generation: '2', phase: 'cleanup-pending' })
    await authority.release()
    expect(db.rows('authorityReservations')[0]).toMatchObject({ generation: '2', phase: 'released', recoveryReceipt: acquired.recoveryReceipt })
  } finally { jest.useRealTimers() }
})

test('recovery abort leaves the old released receipt intact and allows retry', async () => {
  const db = recordingDatabase()
  await sealed(db)
  const before = db.rows('authorityReservations')[0]
  const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  db.abortNext(new Error('recovery aborted'))
  await expect(authority.recover()).rejects.toThrow('recovery aborted')
  expect(db.rows('authorityReservations')[0]).toEqual(before)
  expect(await authority.recover()).toMatchObject({ state: 'sealed' })
  await authority.release()
})

test('close waits for the atomic recovery before discharging the new owner', async () => {
  const db = recordingDatabase()
  await sealed(db)
  const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  db.pauseNext()
  const recovering = authority.recover()
  await db.requestsProcessed()
  expect(authority.current()).toBe(false)
  const closing = authority.release()
  db.complete()
  expect(await recovering).toMatchObject({ state: 'sealed' })
  await closing
  expect(db.rows('authorityReservations')[0]).toMatchObject({ generation: '2', phase: 'released' })
})

test('failed recovery cleanup retains ownership and succeeds on retry', async () => {
  const db = recordingDatabase()
  await sealed(db)
  const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  await authority.recover()
  await expect(authority.close(async () => { throw new Error('cleanup failed') })).rejects.toThrow('cleanup failed')
  expect(db.rows('authorityReservations')[0]).toMatchObject({ generation: '2', phase: 'cleanup-pending' })
  await authority.release()
  expect(db.rows('authorityReservations')[0]).toMatchObject({ generation: '2', phase: 'released' })
})

test('recovery receipt accepts only the exact operation and never reacquires after release', async () => {
  const db = recordingDatabase(), { reservations } = await sealed(db)
  const input = { id: 'cold', ownerToken: token('f'), operationId: token('1'), expectedGeneration: '1', coordinator }
  const owner = await reservations.recover(input)
  expect(await reservations.recover(input)).toEqual(owner)
  expect(await reservations.reconcileRecovery(input)).toEqual({ outcome: 'committed', owner })
  await expect(reservations.recover({ ...input, coordinator: { kind: 'locks', name: 'different' } })).rejects.toThrow()
  expect(await reservations.reconcileRecovery({ ...input, operationId: token('2') })).toEqual({ outcome: 'unknown' })
  await reservations.release(owner, token('3'))
  expect(await reservations.recover(input)).toEqual(owner)
  expect(db.rows('authorityReservations')[0]).toMatchObject({ generation: '2', phase: 'released' })
})

test('invalid sealed receipt cannot authorize recovery or mutate the reservation', async () => {
  const db = recordingDatabase()
  await sealed(db)
  db.mutate('recordings', row => { row.sealReceipt.ownerToken = token('f') })
  const before = db.rows('authorityReservations')
  const authority = (await createWebRecordingAuthority(opts(db), 'cold')).authority
  await expect(authority.recover()).rejects.toThrow()
  expect(db.rows('authorityReservations')).toEqual(before)
  await authority.release()
})

test('a fresh worker probe does not prove an armed owner is quiescent', async () => {
  const db = recordingDatabase(), reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({ declaration, ownerToken: token('a'), operationId: token('b'), expectedGeneration: null, coordinator })
  await reservations.arm(owner, token('c'))
  const before = { reservations: db.rows('authorityReservations'), recordings: db.rows('recordings') }
  const original = global.SharedWorker
  global.SharedWorker = class {
    constructor() {
      this.onerror = null
      this.port = { onmessage: null, onmessageerror: null, start() {}, close() {},
        postMessage(message) { queueMicrotask(() => this.onmessage({ data: message })) } }
    }
  }
  try {
    expect(await createWebRecordingAuthority({ database: db, workerUrl: 'https://example.test/worker.js', digest: async () => 'ab'.repeat(32), crypto: webcrypto }, 'cold'))
      .toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
    expect(db.rows('authorityReservations')).toEqual(before.reservations)
    expect(db.rows('recordings')).toEqual(before.recordings)
  } finally { global.SharedWorker = original }
})
