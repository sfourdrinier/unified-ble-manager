// Bundle for a local browser page with Bun:
// bun build __tests__/web/web-cold-recovery-browser-fixture.js --target browser --outfile /tmp/ubm-cold-recovery-fixture.js
// Load the bundle from an isolated local page; its body reports PASS or FAIL.
// This uses real IndexedDB and no Bluetooth.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebAuthorityReservations } from '../../src/web/web-authority-reservation'
import { createWebOrderedRecordingJournal } from '../../src/web/web-ordered-recording-journal'
import { createWebRecordingAuthority } from '../../src/web/web-recording-authority'
import { createWebContinuationRecordingController } from '../../src/web/web-continuation-recording-controller'

const assert = (value, message) => { if (!value) throw new Error(message) }
const token = c => c.repeat(64)
const declaration = { id: 'browser-cold', maxBytes: 1048576, maxRecords: 4, producerIncarnation: 'producer', collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }], context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c', databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] } }
const terminal = { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0' }
const status = { recordingId: declaration.id, phase: 'stopped', accepting: false, records: 1, bytes: 1, lostRecords: 0, maxBytes: declaration.maxBytes, maxRecords: declaration.maxRecords, encrypted: false, orderedOwner: { version: 1, producerIncarnation: 'producer', collectionId: '1', sourceGeneration: { kind: 'opaque', value: 'source' }, state: 'sealed', uncertainTail: null }, runtimeFailure: null, collectionFailure: null }
const coordinator = { kind: 'locks', name: 'browser-fixture' }
const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
const read = (db, store, key) => new Promise((resolve, reject) => { const tx = db.transaction([store], 'readonly'); const req = tx.objectStore(store).get(key); tx.oncomplete = () => resolve(req.result); tx.onabort = () => reject(tx.error) })

async function main() {
  const db = await openWebRecordingDatabase(indexedDB)
  const reservations = createWebAuthorityReservations(db)
  const { owner } = await reservations.acquire({ declaration, ownerToken: token('a'), operationId: token('b'), expectedGeneration: null, coordinator })
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.open(token('c'))
  const options = { database: db, locks, digest: async () => 'ab'.repeat(32), crypto }
  const busy = await createWebRecordingAuthority(options, declaration.id)
  assert(busy.state === 'busy' && busy.lastCommitted.state === 'live', 'armed owner must remain busy')
  await reservations.cleanupPending(owner, token('d'))
  await journal.commit({ id: '1', record: terminal, outcome: { version: 1, state: 'finished', error: null }, status })
  await journal.retire('1')
  await journal.seal()
  await reservations.release(owner, token('e'))
  const original = JSON.stringify(await read(db, 'recordings', declaration.id))
  // Suppress delivery of one completion while preserving the real committed
  // IndexedDB transaction. Recovery must reconcile its exact receipt.
  let suppress = false
  const lossyDatabase = { transaction(...args) {
    const tx = db.transaction(...args)
    if (!suppress) return tx
    suppress = false
    let completion = null
    tx.oncomplete = () => {}
    return { objectStore: tx.objectStore.bind(tx), abort: tx.abort.bind(tx),
      get error() { return tx.error }, get oncomplete() { return completion },
      set oncomplete(value) { completion = value },
      get onabort() { return tx.onabort }, set onabort(value) { tx.onabort = value } }
  } }
  const recovered = await createWebRecordingAuthority({ ...options, database: lossyDatabase }, declaration.id)
  assert(recovered.state === 'acquired', 'recovery coordinator')
  suppress = true
  const result = await recovered.authority.recover()
  assert(result.state === 'sealed' && result.ownerEpoch === '1', 'recovery status')
  assert(recovered.authority.reservationOwner().generation === '2', 'new generation')
  const reader = await createWebContinuationRecordingController({ database: db, randomToken: () => 'batch' })
  const publicStatus = await reader.status(declaration.id)
  assert(publicStatus.phase === 'stopped' && publicStatus.orderedOwner.state === 'sealed', 'public status')
  await recovered.authority.release()
  assert(JSON.stringify(await read(db, 'recordings', declaration.id)) === original, 'receipts and epoch changed')
  const batch = await reader.prepareOrdered(declaration.id, { maxItems: 4, maxBytes: 10000 })
  assert(batch.records.length === 1 && batch.records[0].record.kind === 'terminal', 'terminal replay')
  const again = await createWebRecordingAuthority(options, declaration.id)
  assert((await again.authority.recover()).state === 'sealed', 'second recovery')
  await again.authority.release()
  const interruptedDeclaration = { ...declaration, id: 'browser-interrupted' }
  const interrupted = await reservations.acquire({ declaration: interruptedDeclaration, ownerToken: token('2'), operationId: token('3'), expectedGeneration: null, coordinator })
  await reservations.arm(interrupted.owner, token('4'))
  await reservations.cleanupPending(interrupted.owner, token('5'))
  // The journal owner normally supplies this state. This fixture represents an
  // already interrupted, explicitly released journal; it does not infer death.
  await new Promise((resolve, reject) => {
    const tx = db.transaction(['recordings'], 'readwrite')
    const store = tx.objectStore('recordings'), request = store.get(interruptedDeclaration.id)
    request.onsuccess = () => { const row = request.result; row.ownerState = 'interrupted'; store.put(row) }
    tx.oncomplete = resolve; tx.onabort = () => reject(tx.error)
  })
  await reservations.release(interrupted.owner, token('6'))
  const interruptedBefore = JSON.stringify(await read(db, 'recordings', interruptedDeclaration.id))
  const interruptedRecovery = await createWebRecordingAuthority(options, interruptedDeclaration.id)
  const interruptedStatus = await interruptedRecovery.authority.recover()
  assert(interruptedStatus.state === 'interrupted' && interruptedStatus.uncertainTail === true, 'interrupted status')
  assert((await reader.stopOrdered(interruptedDeclaration.id)).recording.state === 'interrupted', 'public interrupted result')
  await interruptedRecovery.authority.release()
  assert(JSON.stringify(await read(db, 'recordings', interruptedDeclaration.id)) === interruptedBefore, 'interrupted journal changed')
  db.close()
}
main().then(() => { document.body.textContent = 'PASS real IndexedDB cold recovery' }, error => { document.body.textContent = `FAIL ${error.stack || error}` })
