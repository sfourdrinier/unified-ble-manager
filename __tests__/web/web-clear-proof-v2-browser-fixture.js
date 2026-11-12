// Local real IndexedDB fixture; this does not open a BLE adapter or use the network.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebAuthorityRecordingJournalBridge } from '../../src/web/web-authority-recording-journal-bridge'
import { createWebRecordingPeerIdentityStore } from '../../src/web/web-recording-peer-identity'
import { createWebContinuationRecordingController } from '../../src/web/web-continuation-recording-controller'

const assert = (condition, message) => { if (!condition) throw new Error(message) }
const request = value => new Promise((resolve, reject) => {
  value.onsuccess = () => resolve(value.result)
  value.onerror = () => reject(value.error)
})
const databaseName = 'ubm-v2-clear-idb-fixture'
const factory = { open: (_name, version) => indexedDB.open(databaseName, version) }
const context = {
  kind: 'ordered-recording', version: 1, peerId: 'device', connectionGeneration: 'c', databaseGeneration: 'd',
  maxItems: 8, maxBytes: 1024, maxControls: 1,
  members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }]
}

async function run() {
  await request(indexedDB.deleteDatabase(databaseName))
  const first = await openWebRecordingDatabase(factory)
  const second = await openWebRecordingDatabase(factory)
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database: first, locks: navigator.locks, digest: async () => 'ab'.repeat(32) },
    { id: 'sample', maxBytes: 1048576, maxRecords: 10, context, collectionId: '1', sourceGeneration: 'source' },
    crypto,
    { store: createWebRecordingPeerIdentityStore(first), reference: { version: 1, backendId: 'web', scope: 'origin', opaqueId: 'device' } }
  )
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
  await bridge.retire('1')
  await bridge.seal()
  await bridge.observeTerminal({ result: { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } } })
  await bridge.releaseAuthority()
  const a = await createWebContinuationRecordingController({ database: first, locks: navigator.locks })
  const b = await createWebContinuationRecordingController({ database: second, locks: navigator.locks })
  const batch = await a.prepareOrdered('sample', { maxItems: 8, maxBytes: 10000 })
  await a.acknowledge('sample', batch.token)
  const source = await a.prepareClearAttemptV1('sample')
  const input = { recordingId: source.recordingId, producerIncarnation: source.producerIncarnation, requestId: 'b'.repeat(64) }
  const [attempt, same] = await Promise.all([a.admitClearAttemptV2(input), b.admitClearAttemptV2(input)])
  assert(JSON.stringify(attempt) === JSON.stringify(same), 'admission retry changed identity')
  await a.commitClearAttemptV2(attempt)
  const outcome = await b.inspectClearAttemptV2(attempt)
  assert(outcome.state === 'committed' && outcome.result.records === 0, 'committed outcome missing')
  assert((await b.commitClearAttemptV2(attempt)).cleared, 'committed retry failed')
  await b.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })
  first.close()
  second.close()
  const reopened = await openWebRecordingDatabase(factory)
  const after = await createWebContinuationRecordingController({ database: reopened, locks: navigator.locks })
  assert((await after.inspectClearAttemptV2(attempt)).state === 'retired-or-unknown', 'retirement not persisted')
  assert(JSON.stringify(await after.admitClearAttemptV2(input)) === JSON.stringify(attempt), 'retired identity reused')
  const removeRow = key => new Promise((resolve, reject) => {
    const tx = reopened.transaction(['readerReceipts'], 'readwrite')
    tx.oncomplete = resolve
    tx.onabort = () => reject(tx.error)
    tx.objectStore('readerReceipts').delete(key)
  })
  await removeRow(`:ubm-proof-request-v2:${input.requestId}`)
  let rejected = false
  try { await after.admitClearAttemptV2(input) } catch (error) { rejected = error.code === 'protocol.malformed' }
  assert(rejected, 'missing request fence was readmitted')
  await removeRow(':ubm-staged-proof-ledger-v1')
  rejected = false
  try { await after.clearProofUsageV2() } catch (error) { rejected = error.code === 'protocol.malformed' }
  assert(rejected, 'missing metadata was reinitialized')
  reopened.close()
  await request(indexedDB.deleteDatabase(databaseName))
  document.body.textContent = 'PASS V2 clear real IndexedDB two connections reopen and corruption'
}
run().catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
