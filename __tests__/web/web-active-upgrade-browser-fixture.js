// Run in a fresh, isolated browser profile: this fixture commits an incompatible
// version 3 of the UBM database. No Bluetooth hardware or external network is used.
// bun build __tests__/web/web-active-upgrade-browser-fixture.js --target=browser --outfile=/tmp/ubm-active-upgrade/fixture.js
// Serve /tmp/ubm-active-upgrade on localhost with an index.html containing
// <script type="module" src="/fixture.js"></script>. The body reports PASS/FAIL.
import { createWebBleManagerWithEnvironment, createWebRecordingController } from '../../src/web'

const DB = 'ubm-ordered-recordings'
const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const realIndexedDB = globalThis.indexedDB
const assert = (condition, message) => { if (!condition) throw new Error(message) }
const delay = ms => new Promise(resolve => setTimeout(resolve, ms))
const json = value => JSON.stringify(value, (_key, v) => v instanceof Uint8Array ? Array.from(v) : v)
async function waitFor(description, predicate) {
  const end = Date.now() + 5000
  while (Date.now() < end) { if (await predicate()) return; await delay(10) }
  throw new Error(`Timed out: ${description}`)
}
function openRaw(version) {
  return new Promise((resolve, reject) => {
    const request = version === undefined ? realIndexedDB.open(DB) : realIndexedDB.open(DB, version)
    request.onerror = () => reject(request.error)
    request.onblocked = () => reject(new Error('raw IndexedDB open blocked'))
    request.onsuccess = () => resolve(request.result)
  })
}
async function snapshot() {
  const db = await openRaw()
  try {
    const names = Array.from(db.objectStoreNames)
    const rows = await new Promise((resolve, reject) => {
      const tx = db.transaction(names, 'readonly')
      const result = {}
      for (const name of names) {
        const request = tx.objectStore(name).getAll()
        request.onsuccess = () => { result[name] = request.result }
      }
      tx.oncomplete = () => resolve(result)
      tx.onabort = () => reject(tx.error)
    })
    return { version: db.version, names, rows }
  } finally { db.close() }
}
function upgrade(abort) {
  return new Promise((resolve, reject) => {
    const request = realIndexedDB.open(DB, 3)
    let upgraded = false
    request.onblocked = () => reject(new Error('version 3 upgrade blocked'))
    request.onupgradeneeded = () => {
      upgraded = true
      request.result.createObjectStore('fixtureUpgradeEvidence')
      if (abort) request.transaction.abort()
    }
    request.onerror = () => abort && upgraded && request.error?.name === 'AbortError'
      ? resolve() : reject(request.error || new Error('upgrade failed'))
    request.onsuccess = () => {
      request.result.close()
      if (abort) reject(new Error('aborted upgrade unexpectedly succeeded'))
      else if (!upgraded) reject(new Error('upgrade event was not observed'))
      else resolve()
    }
  })
}
async function lockName(id) {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(id))
  return `ubm-ordered-recording:${Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('')}`
}
async function lockHeld(id) {
  const name = await lockName(id)
  return (await navigator.locks.query()).held.some(lock => lock.name === name)
}
async function radio() {
  const listeners = new Set(), disconnectListeners = new Set()
  let starts = 0, stops = 0
  const characteristic = {
    uuid: CHARACTERISTIC, properties: { notify: true }, value: null,
    getDescriptors: async () => [],
    startNotifications: async () => { starts++; return characteristic },
    stopNotifications: async () => { stops++; return characteristic },
    addEventListener: (_type, listener) => listeners.add(listener),
    removeEventListener: (_type, listener) => listeners.delete(listener)
  }
  const service = { uuid: SERVICE, getCharacteristics: async () => [characteristic] }
  const gatt = {
    connected: false,
    connect: async () => { gatt.connected = true; return gatt },
    disconnect: () => { gatt.connected = false; for (const listener of disconnectListeners) listener() },
    getPrimaryServices: async () => [service]
  }
  const device = {
    id: 'synthetic-upgrade-device', gatt,
    addEventListener: (_type, listener) => disconnectListeners.add(listener),
    removeEventListener: (_type, listener) => disconnectListeners.delete(listener)
  }
  const environment = {
    implementationVersion: 'fixture', browserEngine: 'fixture',
    bluetooth: { getAvailability: async () => true, requestDevice: async () => device },
    isSecureContext: () => true, hasTransientUserActivation: () => true,
    now: () => performance.now(), setTimer: (callback, ms) => setTimeout(callback, ms),
    clearTimer: handle => clearTimeout(handle), addPageLifecycleListener: () => () => {}
  }
  const manager = await createWebBleManagerWithEnvironment({ environment })
  const peer = await manager.choose({ acceptAllDevices: true, optionalServices: [SERVICE] })
  const connection = await manager.connect(peer)
  const database = await connection.discover()
  return {
    manager, connection, database,
    get starts() { return starts }, get stops() { return stops },
    emit(bytes) {
      characteristic.value = new DataView(Uint8Array.from(bytes).buffer)
      for (const listener of listeners) listener({ target: characteristic })
    },
    start(id) {
      return database.reserveOrderedRecordingStart().start(
        [{ memberId: 'heart', characteristic: database.characteristic(SERVICE, CHARACTERISTIC) }],
        { recording: { id, maxBytes: 1048576, maxRecords: 20 } }
      )
    }
  }
}
async function records(id) {
  const state = await snapshot()
  return state.rows.records.filter(row => row.recordingId === id)
}
async function valueRows(id, count) {
  await waitFor(`${id} has ${count} values`, async () =>
    (await records(id)).filter(row => row.record.kind === 'value').length === count)
}
async function verifyValues(controller, id, expected) {
  const batch = await controller.prepareOrdered(id, { maxItems: 20, maxBytes: 10000 })
  const values = batch.records.filter(row => row.record.kind === 'value').map(row => Array.from(row.record.bytes))
  assert(json(values) === json(expected), `${id} values: ${json(values)}`)
  assert(json(await controller.prepareOrdered(id, { maxItems: 20, maxBytes: 10000 })) === json(batch), 'prepared batch changed')
  return batch
}
async function stopAndCheck(controller, id, source) {
  const stopped = await controller.stopOrdered(id)
  assert(stopped.recording.state === 'finished' && stopped.hostCleanup.state === 'released', `${id} stop: ${json(stopped)}`)
  await waitFor(`${id} lock released`, async () => !(await lockHeld(id)))
  assert(source.stops === 1, `${id} synthetic notification cleanup count ${source.stops}`)
}
// Hold the package's next open before issuing a real browser request, then
// forward its real events and result. No database or lock implementation is mocked.
function holdNextOpen() {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'indexedDB')
  let pending, seen = false
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: {
    open(name, version) {
      assert(!pending, 'more than one held open')
      seen = true
      const facade = { result: undefined, error: null, transaction: null, onupgradeneeded: null, onsuccess: null, onerror: null, onblocked: null }
      pending = () => {
        const request = realIndexedDB.open(name, version)
        for (const event of ['upgradeneeded', 'success', 'error', 'blocked']) request[`on${event}`] = e => {
          facade.result = request.result
          facade.error = request.error
          facade.transaction = request.transaction
          facade[`on${event}`]?.(e)
        }
      }
      return facade
    }
  } })
  return {
    get seen() { return seen },
    release() { assert(pending, 'no open to release'); const run = pending; pending = null; run() },
    restore() { if (original) Object.defineProperty(globalThis, 'indexedDB', original); else delete globalThis.indexedDB }
  }
}
async function main() {
  assert(isSecureContext && realIndexedDB && navigator.locks && crypto.subtle, 'secure localhost, IndexedDB, Web Locks and Web Crypto required')
  assert(!(await indexedDB.databases()).some(db => db.name === DB), 'use a fresh isolated browser storage partition')
  const id = 'browser-active-upgrade', source = await radio()
  const started = await source.start(id)
  assert(started.kind === 'started' && started.receipt.state === 'active', `start: ${json(started)}`)
  const controller = await createWebRecordingController()
  source.emit([1, 2, 255])
  await valueRows(id, 1)
  const before = await snapshot()
  const owner = before.rows.recordings.find(row => row.id === id)
  assert(owner && owner.ownerState === 'live' && await lockHeld(id), 'active owner and Web Lock required')
  await upgrade(true)
  source.emit([3, 4])
  await valueRows(id, 2)
  const after = await snapshot()
  const reopenedOwner = after.rows.recordings.find(row => row.id === id)
  const priorValues = before.rows.records.filter(row => row.recordingId === id && row.record.kind === 'value')
  const reopenedValues = after.rows.records.filter(row => row.recordingId === id && row.record.kind === 'value')
  assert(json(reopenedValues.slice(0, priorValues.length)) === json(priorValues), 'existing record changed across reopen')
  for (const key of ['ownerEpoch', 'producerIncarnation', 'collectionId', 'sourceGeneration'])
    assert(owner[key] === reopenedOwner[key], `owner changed: ${key}`)
  assert(await lockHeld(id), 'reopen lost real Web Lock')
  assert(source.starts === 1, 'reopen restarted producer')
  await verifyValues(controller, id, [[1, 2, 255], [3, 4]])
  await stopAndCheck(controller, id, source)
  await source.connection.release()
  await source.manager.destroy()

  const pendingId = 'browser-pending-reopen', pendingSource = await radio()
  const pendingStart = await pendingSource.start(pendingId)
  assert(pendingStart.kind === 'started', `pending start: ${json(pendingStart)}`)
  const pendingController = await createWebRecordingController()
  await upgrade(true)
  const gate = holdNextOpen()
  try {
    pendingSource.emit([9, 8])
    await waitFor('held reopen', () => gate.seen)
    let stopped = false, destroyed = false
    const stopping = pendingController.stopOrdered(pendingId).then(value => { stopped = true; return value })
    const destroying = pendingSource.manager.destroy().then(value => { destroyed = true; return value })
    await delay(50)
    assert(!stopped, 'stop falsely succeeded while storage open held')
    assert(await lockHeld(pendingId), 'pending reopen lost authority')
    gate.release()
    gate.restore()
    const outcome = await stopping
    await destroying
    assert(outcome.recording.state === 'interrupted' && outcome.hostCleanup.state === 'released', `pending stop/destroy: ${json(outcome)}`)
    await waitFor('pending lock release', async () => !(await lockHeld(pendingId)))
    await verifyValues(pendingController, pendingId, [[9, 8]])
    assert(destroyed && pendingSource.stops === 1, 'destroy or source cleanup incomplete')
  } finally { gate.restore() }

  const incompatibleId = 'browser-incompatible-upgrade', incompatibleSource = await radio()
  const incompatibleStart = await incompatibleSource.start(incompatibleId)
  assert(incompatibleStart.kind === 'started', `incompatible start: ${json(incompatibleStart)}`)
  const existingController = await createWebRecordingController()
  incompatibleSource.emit([7, 6])
  await valueRows(incompatibleId, 1)
  const preserved = await snapshot()
  await upgrade(false)
  const upgraded = await snapshot()
  assert(upgraded.version === 3 && upgraded.names.includes('fixtureUpgradeEvidence'), 'real upgrade did not commit')
  for (const name of preserved.names) {
    assert(upgraded.names.includes(name), `lost store ${name}`)
    assert(json(upgraded.rows[name]) === json(preserved.rows[name]), `changed rows in ${name}`)
  }
  assert(await lockHeld(incompatibleId), 'incompatible upgrade lost live owner')
  for (const operation of [() => existingController.contextOrdered(incompatibleId), () => createWebRecordingController()]) {
    let failure
    try { await operation() } catch (error) { failure = error }
    assert(failure?.code === 'protocol.incompatible' && failure?.domain === 'restoration' &&
      failure?.platform?.domain === 'indexeddb' && failure?.platform?.code === 'VersionError',
      `incompatible classification: ${json(failure)}`)
  }
  // A newer schema cannot be downgraded in-place; keep the active owner visible
  // and do not claim release or retry success in this intentionally incompatible partition.
  const afterRejectedReopen = await snapshot()
  for (const name of preserved.names)
    assert(json(afterRejectedReopen.rows[name]) === json(preserved.rows[name]), `rejected reopen changed ${name}`)
  assert(await lockHeld(incompatibleId), 'incompatible reopen falsely released owner')
  document.body.textContent = 'PASS active synthetic Web Bluetooth; real IndexedDB compatible reopen and incompatible upgrade; real Web Locks; held reopen stop/destroy'
}
main().catch(error => { document.body.textContent = `FAIL ${error?.stack || error}` })
