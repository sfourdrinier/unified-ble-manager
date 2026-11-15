// Standalone file:// fixture; synthetic IndexedDB data only, no Bluetooth or network.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebRecordingPartition, createWebRecordingRecoveryInspector } from '../../src/web'

const assert = (condition, message) => { if (!condition) throw new Error(message) }
const complete = tx => new Promise((resolve, reject) => { tx.oncomplete = resolve; tx.onabort = () => reject(tx.error) })

async function run() {
  const db = await openWebRecordingDatabase(indexedDB)
  const other = await openWebRecordingDatabase(indexedDB)
  const tx = db.transaction(['recordings', 'markerLedger'], 'readwrite')
  const done = complete(tx)
  tx.objectStore('recordings').put({ id: 'sample', producerIncarnation: 'incarnation' })
  for (let i = 0; i < 300; i++) tx.objectStore('markerLedger').put({
    version: 1, recordingId: 'sample', producerIncarnation: 'incarnation', markerId: String(i),
    owner: { ownerToken: 'private' }, request: { bytes: [i % 256] }, dispatchIntent: i === 150, settlement: null
  })
  await done
  const partition = createWebRecordingPartition({ database: other, crypto, digest: async () => 'unused' })
  const inspector = await createWebRecordingRecoveryInspector(partition)
  let after = null
  const markers = []
  do {
    const page = await inspector.listMarkers('sample', 'incarnation', { after, limit: 17 })
    assert(page.markers.length <= 17, 'page exceeded bound')
    markers.push(...page.markers)
    after = page.next
  } while (after !== null)
  assert(markers.length === 300 && new Set(markers.map(row => row.markerId)).size === 300, 'inventory lost or duplicated rows')
  const dispatched = markers.find(row => row.markerId === '150')
  assert(dispatched.dispatchIntent === true && dispatched.settlement === null && dispatched.request.bytes[0] === 150, 'uncertain dispatch changed')
  assert(!('owner' in dispatched), 'owner token exposed')
  let rejected = false
  try { await inspector.listMarkers('sample', 'other', { after: null, limit: 17 }) } catch { rejected = true }
  assert(rejected, 'wrong incarnation accepted')
  db.close(); other.close()
  document.body.textContent = 'PASS real IndexedDB paged marker evidence'
}
run().catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
