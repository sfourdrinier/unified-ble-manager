const { webcrypto } = require('crypto')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const digest = async () => 'ab'.repeat(32)
const input = {
  id: 'review', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'source',
  context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c', databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
}
function locks() {
  let held = false
  return { isHeld: () => held, request(_name, _options, callback) {
    if (held) return Promise.resolve(callback(null))
    held = true
    return Promise.resolve(callback({})).finally(() => { held = false })
  } }
}
function owner(epoch = '1') {
  const ingress = new WebNotificationIngress(() => {})
  const characteristic = {}
  let group
  return {
    reserve: () => ({ memberships: () => [{ member: '0', epoch }] }),
    install: wake => {
      group = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
      return { source: group, leases: [{ id: 'x', enable: async () => {}, release: async () => {} }] }
    },
    release: async () => {}, releaseRetainingDeliveries: async () => {}, isCurrent: () => true,
    head: () => group?.peek()
  }
}
const bridgeFor = (database, manager, extra = {}) => createWebAuthorityRecordingJournalBridge({ database, locks: manager, digest, crypto: webcrypto, ...extra }, input, webcrypto)
const flush = async () => { for (let i = 0; i < 100; i++) await Promise.resolve() }

test('a revoked prepared recording is rebound to the next supervisor source identity', async () => {
  const db = recordingDatabase(), manager = locks()
  const first = bridgeFor(db, manager)
  const open = first.open
  first.open = (reservation) => open(reservation, () => false)
  const supervisor = new WebOrderedRecordingSupervisor(owner('1'), first)
  await supervisor.start(); await supervisor.retry()
  expect(db.rows('authorityReservations')[0].phase).toBe('released')
  const prior = db.rows('recordings')[0]
  const next = new WebOrderedRecordingSupervisor(owner('2'), bridgeFor(db, manager))
  expect((await next.start()).state).toBe('active')
  const rebound = db.rows('recordings')[0]
  expect(rebound.producerIncarnation).not.toBe(prior.producerIncarnation)
  expect(rebound.memberEpochs).toEqual([{ member: '0', epoch: '2' }])
  expect(rebound.ownerEpoch).toBe('2')
  next.stop(); await next.retry()
  expect(next.status().terminalCommitted).toBe(true)
})

test('busy acquisition has no authority cleanup obligation', async () => {
  const db = recordingDatabase(), manager = locks()
  const held = await createWebRecordingAuthority({ database: db, locks: manager, digest, crypto: webcrypto }, input.id)
  const supervisor = new WebOrderedRecordingSupervisor(owner(), bridgeFor(db, manager))
  await supervisor.start(); await supervisor.retry()
  expect(supervisor.status().cleanupFailure).toBeNull()
  expect(supervisor.status().setupFailure).not.toBeNull()
  await held.authority.release()
})

test('aborted acquisition is reconciled and coordinator released through the fenced supervisor', async () => {
  const db = recordingDatabase(), manager = locks(), abort = new AbortController()
  const bridge = bridgeFor(db, manager)
  const prepare = bridge.prepare
  bridge.prepare = () => prepare(abort.signal)
  const open = bridge.open
  let opening
  const opened = new Promise(resolve => { opening = resolve })
  bridge.open = (...args) => { db.pauseNext(); opening(); return open(...args) }
  const supervisor = new WebOrderedRecordingSupervisor(owner(), bridge)
  const starting = supervisor.start()
  await opened; await db.requestsProcessed()
  abort.abort()
  await starting
  db.abortNext(new Error('acquisition aborted'))
  db.complete()
  await flush()
  await supervisor.retry(); await flush(); await supervisor.retry()
  expect(db.rows('authorityReservations')).toHaveLength(0)
  expect(manager.isHeld()).toBe(false)
  expect(supervisor.status().cleanupFailure).toBeNull()
})

test('terminal commit evidence survives coordinator loss without acknowledging its source', async () => {
  const db = recordingDatabase()
  let lose
  const manager = { request: (_name, _options, callback) => { callback({}); return new Promise((_resolve, reject) => { lose = reject }) } }
  const bridge = bridgeFor(db, manager), sourceOwner = owner()
  const supervisor = new WebOrderedRecordingSupervisor(sourceOwner, bridge)
  await supervisor.start()
  const commit = bridge.commit
  let committing
  const committingStarted = new Promise(resolve => { committing = resolve })
  bridge.commit = delivery => { db.pauseNext(); committing(); return commit(delivery) }
  supervisor.stop()
  await committingStarted; await db.requestsProcessed()
  lose(new Error('coordinator lost'))
  await flush()
  db.complete()
  await flush(); await supervisor.retry()
  expect(db.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  expect(supervisor.status().terminalCommitted).toBe(true)
  expect(sourceOwner.head().record.kind).toBe('terminal')
})

test('repeated timed-out revocation reuses its outstanding reconciliation barrier', async () => {
  jest.useFakeTimers()
  try {
    const db = recordingDatabase(), manager = locks()
    const result = await createWebRecordingAuthority({ database: db, locks: manager, digest, crypto: webcrypto }, input.id)
    const declaration = { ...input, producerIncarnation: result.authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] }
    await result.authority.declare(declaration)
    let transactions = 0
    const transaction = db.transaction.bind(db)
    db.transaction = (...args) => { transactions++; return transaction(...args) }
    db.pauseNext()
    let attempt = result.authority.release().catch(error => error)
    await db.requestsProcessed()
    await jest.advanceTimersByTimeAsync(10000); await attempt
    for (let i = 0; i < 3; i++) {
      attempt = result.authority.release().catch(error => error)
      await jest.advanceTimersByTimeAsync(10000); await attempt
    }
    expect(transactions).toBe(2)
    db.complete()
    await result.authority.release()
    expect(manager.isHeld()).toBe(false)
  } finally { jest.useRealTimers() }
})

test('an aborted re-acquisition recognizes the unchanged released generation', async () => {
  const db = recordingDatabase(), manager = locks()
  const prior = await createWebRecordingAuthority({ database: db, locks: manager, digest, crypto: webcrypto }, input.id)
  await prior.authority.declare({ ...input, producerIncarnation: prior.authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] })
  await prior.authority.release()
  const abort = new AbortController(), bridge = bridgeFor(db, manager)
  const prepare = bridge.prepare
  bridge.prepare = () => prepare(abort.signal)
  const open = bridge.open
  let opening
  const opened = new Promise(resolve => { opening = resolve })
  bridge.open = (...args) => { db.pauseNext(); opening(); return open(...args) }
  const supervisor = new WebOrderedRecordingSupervisor(owner('2'), bridge)
  const starting = supervisor.start()
  await opened; await db.requestsProcessed()
  abort.abort(); await starting
  db.abortNext(new Error('acquisition aborted')); db.complete()
  await flush(); await supervisor.retry(); await flush(); await supervisor.retry()
  expect(db.rows('authorityReservations')[0]).toMatchObject({ phase: 'released', generation: '1' })
  expect(manager.isHeld()).toBe(false)
})

test('fenced supervisor retries a failed acquisition reconciliation without abandoning its coordinator', async () => {
  const db = recordingDatabase(), manager = locks(), abort = new AbortController()
  const transaction = db.transaction.bind(db)
  let fail = false, failedTransactions = 0
  db.transaction = (...args) => {
    const tx = transaction(...args)
    if (fail) { failedTransactions++; tx.error = new Error('barrier unavailable'); tx.abort() }
    return tx
  }
  const bridge = bridgeFor(db, manager)
  const prepare = bridge.prepare
  bridge.prepare = () => prepare(abort.signal)
  const open = bridge.open
  let opening
  const opened = new Promise(resolve => { opening = resolve })
  bridge.open = (...args) => { db.pauseNext(); opening(); return open(...args) }
  const supervisor = new WebOrderedRecordingSupervisor(owner(), bridge)
  const starting = supervisor.start()
  await opened; await db.requestsProcessed()
  fail = true
  abort.abort(); await starting; db.complete(); await flush()
  await supervisor.retry()
  expect(failedTransactions).toBeGreaterThan(0)
  expect(manager.isHeld()).toBe(true)
  expect(db.rows('authorityReservations')[0].phase).toBe('provisional')
  fail = false
  const competitor = await createWebRecordingAuthority({ database: db, locks: locks(), digest, crypto: webcrypto }, input.id)
  expect(competitor.state).toBe('busy')
  await supervisor.retry(); await flush(); await supervisor.retry()
  expect(db.rows('authorityReservations')[0].phase).toBe('released')
  expect(manager.isHeld()).toBe(false)
})

test('repeated cleanup retries reuse an unresolved transition barrier', async () => {
  jest.useFakeTimers()
  try {
    const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
    const db = recordingDatabase(), manager = locks()
    const result = await createWebRecordingAuthority({ database: db, locks: manager, digest, crypto: webcrypto }, input.id)
    const declaration = { ...input, producerIncarnation: result.authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] }
    const prepared = await result.authority.declare(declaration)
    const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: prepared.ownerEpoch }, result.authority.reservationOwner())
    await journal.open('ef'.repeat(32)); result.authority.noteOpenCommitted()
    let transactions = 0
    const transaction = db.transaction.bind(db)
    db.transaction = (...args) => { if (++transactions === 2) db.pauseNext(); return transaction(...args) }
    db.abortNext(new Error('cleanup aborted'))
    let attempt = result.authority.beginCleanup().catch(error => error)
    await flush(); await db.requestsProcessed()
    await jest.advanceTimersByTimeAsync(5000); await attempt
    for (let i = 0; i < 3; i++) {
      attempt = result.authority.beginCleanup().catch(error => error)
      await jest.advanceTimersByTimeAsync(5000); await attempt
    }
    expect(transactions).toBe(2)
    db.complete(); await flush()
    await result.authority.beginCleanup()
    expect(db.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
  } finally { jest.useRealTimers() }
})

test('rebind preserves the recording contract and fences the previous token', async () => {
  const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
  const db = recordingDatabase(), reservations = createWebAuthorityReservations(db)
  const oldDeclaration = { ...input, producerIncarnation: 'old', memberEpochs: [{ member: '0', epoch: '1' }] }
  const request = (declaration, token, expectedGeneration) => ({ declaration, ownerToken: token.repeat(64), operationId: token.repeat(64), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration })
  const first = await reservations.acquire(request(oldDeclaration, 'a', null))
  await reservations.revokeProvisional(first.owner, 'b'.repeat(64))
  const changed = { ...oldDeclaration, maxRecords: 11, producerIncarnation: 'new', memberEpochs: [{ member: '0', epoch: '2' }] }
  await expect(reservations.acquire(request(changed, 'c', '1'))).rejects.toThrow()
  expect(db.rows('recordings')[0]).toMatchObject({ maxRecords: 10, producerIncarnation: 'old', ownerEpoch: '1' })
  const next = await reservations.acquire(request({ ...changed, maxRecords: 10 }, 'd', '1'))
  await expect(reservations.arm(first.owner, 'e'.repeat(64))).rejects.toThrow()
  await reservations.arm(next.owner, 'f'.repeat(64))
  expect(db.rows('recordings')[0]).toMatchObject({ ownerEpoch: '2', producerIncarnation: 'new', memberEpochs: [{ member: '0', epoch: '2' }], ownerState: 'live' })
})

test('a bridge with ambiguous acquisition failure retains the cleanup obligation', async () => {
  const { createWebRecordingJournalBridge } = require('../../src/web/web-recording-journal-bridge')
  const db = recordingDatabase()
  const declaration = { ...input, producerIncarnation: 'source', ownerEpoch: '1' }
  const bridge = createWebRecordingJournalBridge(db, declaration, async () => { throw new Error('ambiguous') })
  await expect(bridge.prepare()).rejects.toThrow('ambiguous')
  await expect(bridge.releaseAuthority()).rejects.toMatchObject({ normalized: { operation: 'web-recording.authority-release-unproven' } })
})

test('concurrent cleanup retries dispatch only one transition after the shared barrier', async () => {
  const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
  const db = recordingDatabase(), manager = locks()
  const result = await createWebRecordingAuthority({ database: db, locks: manager, digest, crypto: webcrypto }, input.id)
  const declaration = { ...input, producerIncarnation: result.authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] }
  const prepared = await result.authority.declare(declaration)
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: prepared.ownerEpoch }, result.authority.reservationOwner())
  await journal.open('ef'.repeat(32)); result.authority.noteOpenCommitted()
  let transactions = 0
  const transaction = db.transaction.bind(db)
  db.transaction = (...args) => { if (++transactions === 2) db.pauseNext(); return transaction(...args) }
  db.abortNext(new Error('cleanup aborted'))
  const first = result.authority.beginCleanup().catch(error => error)
  await flush(); await db.requestsProcessed()
  const retries = Array.from({ length: 3 }, () => result.authority.beginCleanup().catch(error => error))
  db.complete()
  await first; await Promise.all(retries)
  expect(transactions).toBe(2)
  await result.authority.beginCleanup()
  expect(transactions).toBe(4)
  expect(db.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
})

test('rebind rejects a released receipt that does not prove provisional revocation', async () => {
  const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
  const db = recordingDatabase(), reservations = createWebAuthorityReservations(db)
  const declaration = { ...input, producerIncarnation: 'old', memberEpochs: [{ member: '0', epoch: '1' }] }
  const request = (token, generation) => ({ declaration, ownerToken: token.repeat(64), operationId: token.repeat(64), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration: generation })
  const first = await reservations.acquire(request('a', null))
  await reservations.revokeProvisional(first.owner, 'b'.repeat(64))
  db.mutate('authorityReservations', row => { row.receipt.transition = 'release' })
  await expect(reservations.acquire(request('c', '1'))).rejects.toThrow()
  expect(db.rows('recordings')[0].ownerEpoch).toBe('1')
})
