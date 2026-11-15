// Run in an isolated browser profile. This fixture uses IndexedDB only.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'

const name = 'ubm-ordered-recordings'
const legacy = [
  ['recordings', 'id'], ['records', ['recordingId', 'ordinal']],
  ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
  ['readerReceipts', 'recordingId'], ['authorityReservations', 'id']
]
const v4 = [
  ['markerLedger', ['recordingId', 'producerIncarnation', 'markerId']],
  ['markerOperations', ['recordingId', 'producerIncarnation', 'operationId']],
  ['recoveryOperations', ['recordingId', 'producerIncarnation', 'operationId']],
  ['abandonments', ['recordingId', 'producerIncarnation']]
]
const assert = (condition, message) => { if (!condition) throw new Error(message) }
const request = req => new Promise((resolve, reject) => {
  req.onsuccess = () => resolve(req.result)
  req.onerror = () => reject(req.error)
})
async function clear() { await request(indexedDB.deleteDatabase(name)) }
async function seed(version) {
  const req = indexedDB.open(name, version)
  req.onupgradeneeded = () => {
    for (const [store, keyPath] of legacy) req.result.createObjectStore(store, { keyPath })
    if (version === 4)
      for (const [store, keyPath] of v4) req.result.createObjectStore(store, { keyPath })
    req.transaction.objectStore('recordings').put({ id: 'same-id', evidence: ['untouched', version] })
    req.transaction.objectStore('records').put({ recordingId: 'same-id', ordinal: 1, payload: 'historical' })
    req.transaction.objectStore('authorityReservations').put({ id: 'same-id', phase: 'released', payload: 'reservation-historical' })
    if (version === 4) {
      req.transaction.objectStore('markerLedger').put({ recordingId: 'same-id', producerIncarnation: 'producer', markerId: 'marker', payload: 'v4-historical' })
      req.transaction.objectStore('markerOperations').put({ recordingId: 'same-id', producerIncarnation: 'producer', operationId: 'marker-operation', payload: 'v4-marker-operation' })
      req.transaction.objectStore('recoveryOperations').put({ recordingId: 'same-id', producerIncarnation: 'producer', operationId: 'recovery-operation', payload: 'v4-recovery-operation' })
      req.transaction.objectStore('abandonments').put({ recordingId: 'same-id', producerIncarnation: 'producer', payload: 'v4-abandonment' })
    }
  }
  return request(req)
}
async function run() {
  for (const version of [2, 3, 4]) {
    await clear()
    const old = await seed(version)
    let changed = false
    old.onversionchange = () => { changed = true; old.close() }
    const db = await openWebRecordingDatabase(indexedDB)
    assert(changed, `v${version} did not receive versionchange`)
    assert(db.version === 5, 'wrong database version')
    const tx = db.transaction(
      version === 4
        ? ['recordings', 'records', 'authorityReservations', ...v4.map(([store]) => store)]
        : ['recordings', 'records', 'authorityReservations'],
      'readonly'
    )
    const [recording, record] = await Promise.all([
      request(tx.objectStore('recordings').get('same-id')),
      request(tx.objectStore('records').get(['same-id', 1]))
    ])
    assert(JSON.stringify(recording.evidence) === JSON.stringify(['untouched', version]), 'recording changed')
    assert(record.payload === 'historical', 'record changed')
    const reservation = await request(tx.objectStore('authorityReservations').get('same-id'))
    assert(reservation.payload === 'reservation-historical', 'reservation changed')
    if (version === 4) {
      const v4Rows = [
        ['markerLedger', ['same-id', 'producer', 'marker'], 'v4-historical'],
        ['markerOperations', ['same-id', 'producer', 'marker-operation'], 'v4-marker-operation'],
        ['recoveryOperations', ['same-id', 'producer', 'recovery-operation'], 'v4-recovery-operation'],
        ['abandonments', ['same-id', 'producer'], 'v4-abandonment']
      ]
      for (const [store, key, payload] of v4Rows) {
        const row = await request(tx.objectStore(store).get(key))
        assert(row.payload === payload, `${store} changed`)
      }
    }
    for (const store of ['markerLedger', 'markerOperations', 'recoveryOperations', 'abandonments'])
      assert(db.objectStoreNames.contains(store), `missing ${store}`)
    for (const store of ['terminalEvidence', 'terminalEvidenceState'])
      assert(db.objectStoreNames.contains(store), `missing ${store}`)
    let oldError
    try { await request(indexedDB.open(name, version)) } catch (error) { oldError = error }
    assert(oldError?.name === 'VersionError', 'old writer reopened')
    db.close()
  }
  await clear()
  const old = await seed(3)
  let blocked = false
  try { await openWebRecordingDatabase(indexedDB) } catch (error) {
    blocked = error.normalized?.code === 'capability.unavailable'
  }
  assert(blocked, 'blocked upgrade did not fail closed')
  old.close()
  // The abandoned upgrade must abort, leaving the legacy database intact.
  await new Promise(resolve => setTimeout(resolve, 100))
  const db = await request(indexedDB.open(name, 3))
  assert(db.version === 3 && !db.objectStoreNames.contains('markerLedger'), 'abandoned upgrade committed')
  db.close()
  await clear()
  document.body.textContent = 'PASS real IndexedDB v2/v3/v4 upgrades preserve rows, versionchange, old writer fencing, blocked upgrade'
}
run().catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
