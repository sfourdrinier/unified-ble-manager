// Standalone local fixture: real IndexedDB only; no radio or network.
import { createWebClearProofLedger } from '../../src/web/web-clear-proof-ledger'

const name = 'ubm-staged-proof-fixture'
const token = () => 'b'.repeat(64)
const identity = requestId => ({ recordingId: 'sample', producerIncarnation: 'incarnation', requestId })
const request = value =>
  new Promise((resolve, reject) => {
    value.onsuccess = () => resolve(value.result)
    value.onerror = () => reject(value.error)
  })
const assert = (condition, message) => {
  if (!condition) throw new Error(message)
}
async function open() {
  const opening = indexedDB.open(name, 1)
  opening.onupgradeneeded = () => opening.result.createObjectStore('readerReceipts', { keyPath: 'recordingId' })
  return request(opening)
}
async function run() {
  await request(indexedDB.deleteDatabase(name))
  let first = await open(),
    second = await open()
  const a = createWebClearProofLedger(first, token),
    b = createWebClearProofLedger(second, token)
  const initial = await a.usage()
  await a.configure({ limit: 1, expectedRevision: initial.revision })
  const race = await Promise.allSettled([a.admit(identity('a')), b.admit(identity('b'))])
  assert(race.filter(item => item.status === 'fulfilled').length === 1, 'last slot was not exclusive')
  const page = await b.list({ after: '0', limit: 10 })
  assert(page.entries.length === 1, 'inventory missing admitted proof')
  const attempt = page.entries[0].attempt
  const outcome = await b.cancel(attempt)
  assert(outcome.state === 'cancelled', 'cancellation missing')
  await a.retire({ outcome, acknowledgeDurableCopy: true })
  first.close()
  second.close()
  first = await open()
  second = await open()
  const afterReload = createWebClearProofLedger(second, token)
  assert((await afterReload.inspect(attempt)).state === 'retired-or-unknown', 'retired attempt became live')
  const next = await afterReload.admit(identity('next'))
  assert(next.epoch === attempt.epoch && next.sequence !== attempt.sequence, 'identity fence changed after reopen')
  first.close()
  second.close()
  await request(indexedDB.deleteDatabase(name))
  document.body.textContent = 'PASS staged proof ledger real IndexedDB'
}
run().catch(error => {
  document.body.textContent = `FAIL ${error.stack || error}`
})
