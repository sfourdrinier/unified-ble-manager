const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const token = c => c.repeat(64)
const declaration = {
  id: 'sample',
  maxBytes: 1048576,
  maxRecords: 2,
  producerIncarnation: 'producer',
  collectionId: '1',
  sourceGeneration: 'source',
  memberEpochs: [{ member: '0', epoch: '1' }],
  context: {
    kind: 'ordered-recording',
    version: 1,
    peerId: 'p',
    connectionGeneration: 'c',
    databaseGeneration: 'd',
    maxItems: 8,
    maxBytes: 1024,
    maxControls: 4,
    members: [
      {
        memberId: '0',
        publicMemberId: 'x',
        selector: {
          serviceUuid: '180d',
          serviceOccurrence: '0',
          characteristicUuid: '2a37',
          characteristicOccurrence: '0'
        },
        delivery: 'notification',
        preference: null
      }
    ]
  }
}
async function setup() {
  const db = recordingDatabase()
  const reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({
    declaration,
    ownerToken: token('a'),
    operationId: token('b'),
    coordinator: { kind: 'locks', name: 'lock' },
    expectedGeneration: null
  })
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  return { db, reservations, owner, journal }
}

test('reconciles the exact arm after suppressed completion, even after cleanup transition', async () => {
  const { db, reservations, owner, journal } = await setup()
  db.suppressNextCompletion()
  journal.open(token('c'))
  expect(await journal.reconcileOpen(token('c'))).toBe('committed')
  expect(await journal.reconcileOpen(token('d'))).toBe('unknown')
  await reservations.cleanupPending(owner, token('e'))
  expect(await journal.reconcileOpen(token('c'))).toBe('committed')
})

test('a revoked generation cannot open or mutate a newer reservation', async () => {
  const { db, reservations, owner, journal } = await setup()
  await reservations.revokeProvisional(owner, token('c'))
  await reservations.acquire({
    declaration,
    ownerToken: token('d'),
    operationId: token('e'),
    coordinator: { kind: 'locks', name: 'lock' },
    expectedGeneration: '1'
  })
  await expect(journal.open(token('f'))).rejects.toThrow()
  await expect(
    journal.fail({
      code: 'storage',
      domain: 'web',
      operation: 'write',
      detail: 'x',
      platformDomain: null,
      platformCode: null,
      platformMessage: null,
      platformMetadata: {}
    })
  ).rejects.toThrow()
  expect(db.rows('recordings')[0].ownerState).toBe('prepared')
})

test('open and provisional revocation serialize so only one succeeds', async () => {
  const { reservations, owner, journal } = await setup()
  const outcomes = await Promise.allSettled([
    journal.open(token('c')),
    reservations.revokeProvisional(owner, token('d'))
  ])
  expect(outcomes.filter(result => result.status === 'fulfilled')).toHaveLength(1)
})

test('same epoch with a wrong token or generation cannot arm or append', async () => {
  const { db, owner, journal } = await setup()
  for (const stale of [
    { ...owner, ownerToken: token('d') },
    { ...owner, generation: '2' }
  ]) {
    const other = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, stale)
    await expect(other.open(token('c'))).rejects.toThrow()
  }
  await journal.open(token('c'))
  db.mutate('authorityReservations', row => {
    row.ownerToken = token('d')
  })
  await expect(
    journal.fail({
      code: 'storage',
      domain: 'web',
      operation: 'write',
      detail: 'x',
      platformDomain: null,
      platformCode: null,
      platformMessage: null,
      platformMetadata: {}
    })
  ).rejects.toThrow()
  expect(db.rows('recordings')[0].failure).toBeNull()
})

test('a completed barrier proves an aborted open and a blocked barrier remains pending', async () => {
  const { db, journal } = await setup()
  const error = new Error('abort')
  db.abortNext(error)
  db.pauseNext()
  const open = journal.open(token('c'))
  await db.requestsProcessed()
  let settled = false
  const reconcile = journal.reconcileOpen(token('c')).then(result => {
    settled = true
    return result
  })
  await new Promise(resolve => setImmediate(resolve))
  expect(settled).toBe(false)
  db.complete()
  await expect(open).rejects.toBe(error)
  expect(await reconcile).toBe('aborted')
  expect(db.rows('recordings')[0].ownerState).toBe('prepared')
})

test('journal refuses missing owner and malformed open receipts', async () => {
  const { db, owner, journal } = await setup()
  const withoutOwner = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch })
  await expect(withoutOwner.open(token('c'))).rejects.toThrow()
  await journal.open(token('c'))
  db.mutate('recordings', row => {
    row.openReceipt.ownerEpoch = '9'
  })
  await expect(journal.reconcileOpen(token('c'))).rejects.toThrow()
})

test('bridge operation retains its exact identity for supervisor reconciliation', async () => {
  const { createWebRecordingOpenOperation } = require('../../src/web/web-authority-recording-journal-bridge')
  const { db, journal } = await setup()
  const operation = createWebRecordingOpenOperation(journal, token('c'))
  expect(await operation.reconcileOpen()).toBe('unknown')
  db.suppressNextCompletion()
  operation.open()
  expect(await operation.reconcileOpen()).toBe('committed')
})

test('malformed or inconsistent reservation evidence cannot authorize a journal mutation', async () => {
  const { db, journal } = await setup()
  await journal.open(token('c'))
  db.mutate('authorityReservations', row => {
    row.receipt.phase = 'provisional'
  })
  await expect(
    journal.fail({
      code: 'storage',
      domain: 'web',
      operation: 'write',
      detail: 'x',
      platformDomain: null,
      platformCode: null,
      platformMessage: null,
      platformMetadata: {}
    })
  ).rejects.toThrow()
  expect(db.rows('recordings')[0].failure).toBeNull()
})

test('valid owner appends, retires, and seals while cleanup is pending', async () => {
  const { db, journal, owner, reservations } = await setup()
  await journal.declare()
  await journal.open(token('c'))
  await reservations.cleanupPending(owner, token('d'))
  const record = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  expect(await journal.commit({ id: '1', record })).toBe(1)
  await journal.retire('1')
  await journal.seal()
  expect(db.rows('recordings')[0].ownerState).toBe('sealed')
  expect(await journal.reconcileOpen(token('c'))).toBe('committed')
})

test('a seal retry rejects conflicting durable seal evidence', async () => {
  const { db, journal, owner, reservations } = await setup()
  await journal.open(token('c'))
  await reservations.cleanupPending(owner, token('d'))
  await journal.commit({ id: '1', record: {
    kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false, rejectedValues: null,
    rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null,
    additionalFailureCount: '0'
  } })
  await journal.retire('1')
  await journal.seal()
  expect(db.rows('recordings')[0].sealReceipt).toMatchObject({ generation: owner.generation, ownerEpoch: owner.ownerEpoch, deliveryId: '1' })
  db.mutate('recordings', row => { row.sealReceipt.deliveryId = '2' })
  await expect(journal.seal()).rejects.toThrow()
})

test('a live row without this operation receipt does not prove this open committed', async () => {
  const { journal, owner, reservations } = await setup()
  await reservations.arm(owner, token('d'))
  expect(await journal.reconcileOpen(token('c'))).toBe('unknown')
})

test('cannot seal an armed reservation before its cleanup transition', async () => {
  const { db, journal } = await setup()
  await journal.open(token('c'))
  const record = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  await journal.commit({ id: '1', record })
  await journal.retire('1')
  await expect(journal.seal()).rejects.toThrow()
  expect(db.rows('recordings')[0].ownerState).toBe('live')
})

test('an open receipt inconsistent with a prepared row fails closed', async () => {
  const { db, journal } = await setup()
  await journal.open(token('c'))
  db.mutate('recordings', row => {
    row.ownerState = 'prepared'
  })
  db.mutate('authorityReservations', row => {
    row.phase = 'provisional'
    row.receipt = { operationId: token('b'), transition: 'acquire', phase: 'provisional' }
  })
  await expect(journal.reconcileOpen(token('c'))).rejects.toThrow()
})
