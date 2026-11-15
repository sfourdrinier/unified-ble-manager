const { createWebRecordingMarkerLedger } = require('../../src/web/web-recording-marker-ledger')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { recordingDatabase } = require('./web-marker-ledger-database-522-helper')

const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
const declaration = { id: 'sample', maxBytes: 1048576, maxRecords: 20, context, producerIncarnation: 'incarnation', collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }] }
const op = n => n.toString(16).padStart(64, '0')
async function setup(maxBytes = 1048576) {
  const db = recordingDatabase()
  const d = { ...declaration, maxBytes }
  const { owner } = await createWebAuthorityReservations(db).acquire({ declaration: d, ownerToken: 'a'.repeat(64), operationId: op(1), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration: null })
  const journal = createWebOrderedRecordingJournal(db, { ...d, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.open(op(2))
  const ledger = createWebRecordingMarkerLedger(db, owner, d.producerIncarnation)
  return { db, owner, ledger, journal }
}
const request = (id = '0') => ({ markerId: id, target: { serviceUuid: '180d', characteristicUuid: '2a37' }, mode: 'with-response', bytes: Uint8Array.from([0, 255]) })

test('reservation retains exact owned bytes and idempotent operation receipt', async () => {
  const { ledger, db } = await setup()
  const input = request()
  const pending = ledger.reserve(op(3), input)
  input.bytes[0] = 7
  await pending
  const row = await ledger.inspect('0')
  expect(row.request.bytes).toEqual([0, 255])
  expect((await ledger.reconcile(op(3))).outcome).toBe('committed')
  await ledger.reserve(op(3), request())
  expect(db.rows('markerLedger')).toHaveLength(1)
  await expect(ledger.reserve(op(3), request('1'))).rejects.toThrow()
})

test('records distinct facts and requires journal identity before source acknowledgement or retirement', async () => {
  const { ledger, journal } = await setup()
  await ledger.reserve(op(3), request())
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  await ledger.dispatchIntent(op(5), '0')
  await expect(ledger.retire(op(9), '0')).rejects.toThrow()
  await ledger.settle(op(6), '0', { state: 'settled', outcome: 'success' })
  await expect(ledger.acknowledge(op(7), '0', { deliveryId: '1', journalOrdinal: 1 })).rejects.toThrow()
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '5' } })
  await ledger.acknowledge(op(7), '0', { deliveryId: '1', journalOrdinal: 1 })
  await ledger.retire(op(9), '0')
  const row = await ledger.inspect('0')
  expect(row).toMatchObject({ retired: true, admission: { state: 'admitted', ordinal: '5' }, dispatchIntent: true, settlement: { state: 'settled', outcome: 'success' }, sourceAck: { deliveryId: '1', journalOrdinal: 1 } })
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
})

test('rejects stale owners, changed incarnation, conflicting request and simultaneous same marker', async () => {
  const { db, owner, ledger } = await setup()
  const stale = createWebRecordingMarkerLedger(db, { ...owner, generation: '2' }, 'incarnation')
  const wrong = createWebRecordingMarkerLedger(db, owner, 'other')
  await expect(stale.reserve(op(3), request())).rejects.toThrow()
  await expect(wrong.reserve(op(3), request())).rejects.toThrow()
  const results = await Promise.allSettled([ledger.reserve(op(4), request()), ledger.reserve(op(5), request())])
  expect(results.filter(result => result.status === 'fulfilled')).toHaveLength(1)
  await expect(ledger.reserve(op(6), { ...request(), bytes: Uint8Array.of(1) })).rejects.toThrow()
})

test('reconciles a lost completion and preserves dispatched-but-unsettled uncertainty', async () => {
  const { db, ledger } = await setup()
  await ledger.reserve(op(3), request())
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  db.suppressNextCompletion()
  void ledger.dispatchIntent(op(5), '0')
  expect((await ledger.reconcile(op(5))).outcome).toBe('committed')
  await ledger.dispatchIntent(op(5), '0')
  expect(await ledger.inspect('0')).toMatchObject({ dispatchIntent: true, settlement: null })
  await expect(ledger.retire(op(6), '0')).rejects.toThrow()
  expect((await ledger.reconcile(op(99))).outcome).toBe('unknown')
})

test('reserves and charges bounded evidence against journal quota atomically', async () => {
  const { ledger, journal, db } = await setup()
  await ledger.reserve(op(3), { ...request(), bytes: new Uint8Array(110000).fill(255) })
  const charge = db.rows('recordings')[0].markerLedgerBytes
  expect(charge).toBeGreaterThan(0)
  await expect(ledger.reserve(op(4), { ...request('1'), bytes: new Uint8Array(40000).fill(255) })).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  const bytes = new Uint8Array(100000).fill(255)
  const record = { kind: 'value', ordinal: '1', member: '0', membership: { member: '0', epoch: '1' }, bytes, delivery: 'notification', provenance: 'read-or-notification' }
  await expect(journal.commit({ id: '1', record })).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  expect(db.rows('markerLedger')).toHaveLength(1)
})

test('every exact transition retry returns its original receipt after later transitions', async () => {
  const { ledger, journal } = await setup()
  const reserve = await ledger.reserve(op(3), request())
  const admission = await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  const intent = await ledger.dispatchIntent(op(5), '0')
  const settled = await ledger.settle(op(6), '0', { state: 'settled', outcome: 'failure', evidence: 'platform failure' })
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '5' } })
  const ack = await ledger.acknowledge(op(7), '0', { deliveryId: '1', journalOrdinal: 1 })
  const retired = await ledger.retire(op(8), '0')
  for (const [id, expected] of [[3, reserve], [4, admission], [5, intent], [6, settled], [7, ack], [8, retired]]) {
    expect(await ledger.reconcile(op(id))).toEqual({ outcome: 'committed', result: expected })
  }
  expect(await ledger.reserve(op(3), request())).toEqual(reserve)
  expect(await ledger.dispatchIntent(op(5), '0')).toEqual(intent)
  await expect(ledger.settle(op(6), '0', { state: 'settled', outcome: 'success' })).rejects.toThrow()
})

test('unrecognized durable state is retained and never used to authorize a transition', async () => {
  const { db, ledger } = await setup()
  await ledger.reserve(op(3), request())
  db.mutate('markerLedger', row => { row.admission = { state: 'future-state' } })
  await expect(ledger.inspect('0')).rejects.toThrow()
  await expect(ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })).rejects.toThrow()
  expect(db.rows('markerLedger')[0].admission).toEqual({ state: 'future-state' })
})

test('read-only inspection survives authority loss but a stale owner cannot mutate', async () => {
  const { db, ledger } = await setup()
  await ledger.reserve(op(3), request())
  db.mutate('authorityReservations', row => { row.generation = '2'; row.ownerToken = 'b'.repeat(64) })
  expect((await ledger.inspect('0')).request.bytes).toEqual([0, 255])
  expect((await ledger.reconcile(op(3))).outcome).toBe('committed')
  await expect(ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })).rejects.toThrow()
})

test('a storage abort rolls back ledger rows and byte charge', async () => {
  const { db, ledger } = await setup()
  db.abortNext(new Error('quota exceeded'))
  await expect(ledger.reserve(op(3), request())).rejects.toThrow('quota exceeded')
  expect(db.rows('markerLedger')).toHaveLength(0)
  expect(db.rows('markerOperations')).toHaveLength(0)
  expect(db.rows('recordings')[0].markerLedgerBytes).toBeUndefined()
  await ledger.reserve(op(3), request())
})

test('source acknowledgement rejects a journal identity whose retained record is missing or altered', async () => {
  const { db, ledger, journal } = await setup()
  await ledger.reserve(op(3), request())
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '5' } })
  db.mutate('records', row => { row.record.ordinal = '6' })
  await expect(ledger.acknowledge(op(7), '0', { deliveryId: '1', journalOrdinal: 1 })).rejects.toThrow()
  expect((await ledger.inspect('0')).sourceAck).toBeNull()
})

test.each(['reserve', 'admit', 'dispatch', 'settle', 'ack', 'retire'])('reconciles and retries a lost %s transaction completion', async lostStage => {
  const { db, ledger, journal } = await setup()
  const steps = [
    ['reserve', 3, () => ledger.reserve(op(3), request())],
    ['admit', 4, () => ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })],
    ['dispatch', 5, () => ledger.dispatchIntent(op(5), '0')],
    ['settle', 6, () => ledger.settle(op(6), '0', { state: 'settled', outcome: 'success' })],
    ['ack', 7, () => ledger.acknowledge(op(7), '0', { deliveryId: '1', journalOrdinal: 1 })],
    ['retire', 8, () => ledger.retire(op(8), '0')]
  ]
  for (const [stage, id, run] of steps) {
    if (stage === 'ack') await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '5' } })
    if (stage === lostStage) {
      db.suppressNextCompletion()
      void run()
      const receipt = await ledger.reconcile(op(id))
      expect(receipt.outcome).toBe('committed')
      expect(await run()).toEqual(receipt.result)
    } else await run()
  }
})

test('retirement of a rejected marker requires explicit proof no source delivery exists', async () => {
  const { ledger } = await setup()
  await ledger.reserve(op(3), request())
  await ledger.admit(op(4), '0', { state: 'rejected', evidence: 'source refused', delivery: 'unknown' })
  await expect(ledger.retire(op(5), '0')).rejects.toThrow()
  await ledger.reserve(op(6), request('1'))
  await ledger.admit(op(7), '1', { state: 'rejected', evidence: 'source refused without delivery', delivery: 'none' })
  expect((await ledger.retire(op(8), '1')).retired).toBe(true)
})

test('copies marker identity and target before the transaction runs', async () => {
  const { ledger } = await setup()
  const input = request()
  const pending = ledger.reserve(op(3), input)
  input.markerId = '1'
  input.target.characteristicUuid = 'changed'
  await pending
  expect(await ledger.inspect('1')).toBeNull()
  expect(await ledger.inspect('0')).toMatchObject({ markerId: '0', request: { target: { characteristicUuid: '2a37' } } })
})
