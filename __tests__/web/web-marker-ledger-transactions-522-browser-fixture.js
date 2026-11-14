// Standalone file:// fixture. IndexedDB only; no Bluetooth or network.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebAuthorityReservations } from '../../src/web/web-authority-reservation'
import { createWebOrderedRecordingJournal } from '../../src/web/web-ordered-recording-journal'
import { createWebRecordingMarkerLedger } from '../../src/web/web-recording-marker-ledger'

const name = 'ubm-ordered-recordings'
const legacy = [ ['recordings', 'id'], ['records', ['recordingId', 'ordinal']],
  ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
  ['readerReceipts', 'recordingId'], ['authorityReservations', 'id'] ]
const request = req => new Promise((resolve, reject) => { req.onsuccess = () => resolve(req.result); req.onerror = () => reject(req.error) })
const assert = (value, message) => { if (!value) throw new Error(message) }
const rejected = async fn => { try { await fn() } catch { return } throw new Error('expected rejection') }
const op = n => n.toString(16).padStart(64, '0')
const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
const declaration = { id: 'sample', maxBytes: 1048576, maxRecords: 20, context, producerIncarnation: 'incarnation', collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }] }
const marker = id => ({ markerId: id, target: { serviceUuid: '180d', characteristicUuid: '2a37' }, mode: 'with-response', bytes: Uint8Array.of(0, 255) })
async function run() {
  for (const version of [2, 3]) {
    await request(indexedDB.deleteDatabase(name))
    const open = indexedDB.open(name, version)
    open.onupgradeneeded = () => {
      for (const [store, keyPath] of legacy) open.result.createObjectStore(store, { keyPath })
      open.transaction.objectStore('recordings').put({ id: 'historic', evidence: ['untouched', version] })
      open.transaction.objectStore('records').put({ recordingId: 'historic', ordinal: 1, payload: 'historical' })
      open.transaction.objectStore('readerReceipts').put({ recordingId: ':ubm-authority-policy-v1', version: 2, protocol: 'ubm-authority-reservations/1' })
    }
    const old = await request(open); old.close()
    const db = await openWebRecordingDatabase(indexedDB)
    const tx = db.transaction(['recordings', 'records'], 'readonly')
    const [historic, record] = await Promise.all([request(tx.objectStore('recordings').get('historic')), request(tx.objectStore('records').get(['historic', 1]))])
    assert(JSON.stringify(historic.evidence) === JSON.stringify(['untouched', version]) && record.payload === 'historical', 'legacy data changed')
    const { owner } = await createWebAuthorityReservations(db).acquire({ declaration, ownerToken: 'a'.repeat(64), operationId: op(1), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration: null })
    const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
    await journal.open(op(2))
    const db2 = await openWebRecordingDatabase(indexedDB)
    const ledger = createWebRecordingMarkerLedger(db, owner, declaration.producerIncarnation)
    const other = createWebRecordingMarkerLedger(db2, owner, declaration.producerIncarnation)
    const races = await Promise.allSettled([ledger.reserve(op(3), marker('0')), other.reserve(op(4), marker('0'))])
    assert(races.filter(result => result.status === 'fulfilled').length === 1, 'concurrent reservation')
    await rejected(() => createWebRecordingMarkerLedger(db, { ...owner, generation: '2' }, 'incarnation').reserve(op(10), marker('1')))
    await rejected(() => createWebRecordingMarkerLedger(db, owner, 'wrong').reserve(op(10), marker('1')))
    await ledger.admit(op(5), '0', { state: 'admitted', ordinal: '5' })
    let suppress = true
    const lost = { transaction(stores, mode, options) {
      const real = db.transaction(stores, mode, options)
      if (!suppress) return real
      suppress = false
      return { get error() { return real.error }, set onabort(value) { real.onabort = value }, set oncomplete(_value) {}, abort() { real.abort() }, objectStore(store) { return real.objectStore(store) } }
    } }
    const uncertain = createWebRecordingMarkerLedger(lost, owner, 'incarnation')
    void uncertain.dispatchIntent(op(6), '0')
    assert((await ledger.reconcile(op(6))).outcome === 'committed', 'lost reply not reconciled')
    await ledger.dispatchIntent(op(6), '0')
    assert((await ledger.inspect('0')).settlement === null, 'unknown settlement guessed')
    await ledger.settle(op(7), '0', { state: 'settled', outcome: 'success' })
    await journal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '5' } })
    await ledger.acknowledge(op(8), '0', { deliveryId: '1', journalOrdinal: 1 })
    await ledger.retire(op(9), '0')
    assert((await ledger.inspect('0')).retired, 'not retired')
    await ledger.reserve(op(11), { ...marker('1'), bytes: new Uint8Array(110000).fill(255) })
    await rejected(() => ledger.reserve(op(12), { ...marker('2'), bytes: new Uint8Array(40000).fill(255) }))
    const bytes = new Uint8Array(100000).fill(255)
    await rejected(() => journal.commit({ id: '2', record: { kind: 'value', ordinal: '6', member: '0', membership: { member: '0', epoch: '1' }, bytes, delivery: 'notification', provenance: 'read-or-notification' } }))

    // Exercise the stop/settlement gate with the browser's real transactions.
    const heldDeclaration = { ...declaration, id: 'held-marker' }
    const reservations = createWebAuthorityReservations(db)
    const heldOwner = (await reservations.acquire({ declaration: heldDeclaration, ownerToken: 'c'.repeat(64), operationId: op(100), coordinator: { kind: 'locks', name: 'test' }, expectedGeneration: null })).owner
    const heldJournal = createWebOrderedRecordingJournal(db, { ...heldDeclaration, ownerEpoch: heldOwner.ownerEpoch }, heldOwner)
    const heldLedger = createWebRecordingMarkerLedger(db, heldOwner, heldDeclaration.producerIncarnation)
    await heldJournal.open(op(101))
    await heldLedger.reserve(op(102), marker('0'))
    await heldLedger.admit(op(103), '0', { state: 'admitted', ordinal: '1' })
    await heldLedger.dispatchIntent(op(104), '0')
    await heldJournal.commit({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '1' } })
    await heldLedger.acknowledge(op(105), '0', { deliveryId: '1', journalOrdinal: 1 })
    await heldJournal.retire('1')
    await reservations.cleanupPending(heldOwner, op(106))
    const terminal = { id: '2', record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
      rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
      secondaryFailure: null, additionalFailureCount: '0' } }
    await rejected(() => heldJournal.commit(terminal))
    await heldLedger.settle(op(107), '0', { state: 'settled', outcome: 'success' })
    await heldJournal.commit(terminal)
    await heldJournal.retire('2')
    await heldJournal.seal()
    await reservations.release(heldOwner, op(108))
    db2.close(); db.close()
  }
  await request(indexedDB.deleteDatabase(name))
  document.body.textContent = 'PASS real IndexedDB v2/v3 preservation, durable ledger transactions, and marker cleanup gate'
}
run().catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
