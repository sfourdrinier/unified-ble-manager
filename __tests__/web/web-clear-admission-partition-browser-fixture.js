// Bundle into a local HTML page. Three independent frames share real IndexedDB.
// No Bluetooth, server, or network access is required.
import { createWebContinuationRecordingController } from '../../src/web/web-continuation-recording-controller'

const name = 'ubm-522-admission-partition-fixture'
const request = value =>
  new Promise((resolve, reject) => {
    value.onsuccess = () => resolve(value.result)
    value.onerror = () => reject(value.error)
  })
const open = async () => {
  const opening = indexedDB.open(name, 1)
  opening.onupgradeneeded = () => opening.result.createObjectStore('readerReceipts', { keyPath: 'recordingId' })
  return request(opening)
}
const delay = ms => new Promise(resolve => setTimeout(resolve, ms))

async function peer() {
  const db = await open()
  const database = {
    name,
    transaction(stores, mode, options) {
      const tx = db.transaction(stores, mode, options)
      parent.postMessage({ kind: 'dispatch' }, '*')
      return tx
    }
  }
  const controller = await createWebContinuationRecordingController({ database, locks: navigator.locks })
  window.addEventListener('message', event => {
    if (event.data !== 'start') return
    void controller
      .admitClearAttemptV2({
        recordingId: 'sample',
        producerIncarnation: 'producer',
        requestId: location.search.slice(1).repeat(64)
      })
      .then(
        value => parent.postMessage({ kind: 'result', value }, '*'),
        error => parent.postMessage({ kind: 'error', code: error.code, message: error.message }, '*')
      )
  })
  parent.postMessage({ kind: 'ready' }, '*')
}

async function parentFixture() {
  await request(indexedDB.deleteDatabase(name))
  const db = await open()
  // Keep an actual readwrite transaction active, so admission transactions
  // stay queued and their completion cannot free a slot during the check.
  const blocker = db.transaction(['readerReceipts'], 'readwrite')
  let hold = true
  const pump = () => {
    if (hold) blocker.objectStore('readerReceipts').get('blocker').onsuccess = pump
  }
  pump()
  let ready = 0,
    dispatched = 0
  window.addEventListener('message', event => {
    if (event.data?.kind === 'ready') ready++
    if (event.data?.kind === 'dispatch') dispatched++
  })
  const frames = Array.from({ length: 3 }, (_, index) => {
    const frame = document.createElement('iframe')
    frame.src = `${location.href}?${index + 1}`
    document.body.appendChild(frame)
    return frame
  })
  for (let i = 0; i < 100 && ready < 3; i++) await delay(20)
  if (ready !== 3) throw new Error(`only ${ready} realms became ready`)
  for (let i = 0; i < 2; i++) {
    frames[i].contentWindow.postMessage('start', '*')
    for (let retry = 0; retry < 100 && dispatched < i + 1; retry++) await delay(20)
    if (dispatched !== i + 1) throw new Error(`only ${dispatched} fence transactions dispatched`)
  }
  frames[2].contentWindow.postMessage('start', '*')
  await delay(500)
  if (dispatched !== 2) throw new Error(`${dispatched} transactions dispatched with default limit 2`)
  hold = false
  document.body.textContent = 'PASS partition admission across browser realms'
  db.close()
}

;(location.search ? peer() : parentFixture()).catch(error => {
  document.body.textContent = `FAIL ${error.stack || error}`
})
