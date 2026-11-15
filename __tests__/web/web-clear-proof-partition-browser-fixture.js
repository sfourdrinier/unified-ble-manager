// Build with bun for a local file: URL. No radio, server or network is used.
// Both frames execute this bundle in separate realms against one IndexedDB name.
import { createWebClearProofLedger } from '../../src/web/web-clear-proof-ledger'

const name = 'ubm-522-proof-partition-fixture'
const epoch = () => 'b'.repeat(64)
const identity = digit => ({ recordingId: 'sample', producerIncarnation: 'producer', requestId: digit.repeat(64) })
const request = value => new Promise((resolve, reject) => {
  value.onsuccess = () => resolve(value.result)
  value.onerror = () => reject(value.error)
})
const assert = (condition, message) => { if (!condition) throw new Error(message) }
async function open() {
  const opening = indexedDB.open(name, 1)
  opening.onupgradeneeded = () => opening.result.createObjectStore('readerReceipts', { keyPath: 'recordingId' })
  return request(opening)
}
async function runPeer() {
  const db = await open(), ledger = createWebClearProofLedger(db, epoch)
  window.addEventListener('message', async event => {
    if (event.data?.operation !== 'admit') return
    try { event.source.postMessage({ id: event.data.id, value: await ledger.admitRetryable(event.data.identity) }, '*') }
    catch (error) { event.source.postMessage({ id: event.data.id, error: error.normalized?.code || String(error) }, '*') }
  })
  parent.postMessage({ ready: true }, '*')
}
async function runParent() {
  await request(indexedDB.deleteDatabase(name))
  const db = await open(), ledger = createWebClearProofLedger(db, epoch)
  const initial = await ledger.usage()
  await ledger.configure({ limit: null, fenceLimit: 1, expectedRevision: initial.revision })
  const peer = document.createElement('iframe')
  peer.src = `${location.href}?peer`
  const ready = new Promise(resolve => window.addEventListener('message', function listener(event) {
    if (event.data?.ready) { window.removeEventListener('message', listener); resolve() }
  }))
  document.body.appendChild(peer)
  await ready
  const call = (id, input) => new Promise(resolve => {
    const listener = event => {
      if (event.source === peer.contentWindow && event.data?.id === id) {
        window.removeEventListener('message', listener)
        resolve(event.data)
      }
    }
    window.addEventListener('message', listener)
    peer.contentWindow.postMessage({ id, operation: 'admit', identity: input }, '*')
  })
  // Suppress only delivery of the completion callback: the real transaction can commit.
  const lostDatabase = { transaction(stores, mode, options) {
    const tx = db.transaction(stores, mode, options)
    return { get error() { return tx.error }, objectStore: tx.objectStore.bind(tx), abort: tx.abort.bind(tx),
      set onabort(callback) { tx.onabort = callback }, set oncomplete(_callback) {} }
  } }
  void createWebClearProofLedger(lostDatabase, epoch).admitRetryable(identity('1'))
  // A second realm's readwrite transaction is serialized behind the first.
  const retry = await call(1, identity('1'))
  assert(retry.value?.sequence === '1', 'lost reply was not recovered exactly')
  const excess = await call(2, identity('2'))
  assert(excess.error === 'stream.quota', 'fence quota did not apply across realms')
  const usage = await ledger.usage()
  assert(usage.requestFences.count === 1 && usage.requestFences.limit === 1, 'wrong fence accounting')
  document.body.textContent = 'PASS partition proof across browser realms'
  db.close()
}
(location.search ? runPeer() : runParent()).catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
