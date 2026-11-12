// bindings/jni/js_marker_roundtrip.js
// Opt-in synthetic integration: actual JS adapter → JVM JNI → MobileSession.
const assert = require('node:assert/strict')
const { spawn } = require('node:child_process')
const readline = require('node:readline')
const fs = require('node:fs')
const ts = require('typescript')
require.extensions['.ts'] = (module, filename) => module._compile(ts.transpileModule(fs.readFileSync(filename, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText, filename)
const { createReactNativeRecordingReclaims } = require('../../src/backends/reactnative/react-native-ordered-recording-reclaim.ts')
const { createReactNativeOrderedRecordingStart } = require('../../src/backends/reactnative/react-native-ordered-recording-start.ts')
const { createReactNativeOrderedMarkerReservations } = require('../../src/backends/reactnative/react-native-ordered-marker.ts')
const { parseOpValue } = require('../../src/backends/reactnative/rust-core-wire.ts')
const { initializeReactNativeReservationKeys } = require('../../src/backends/reactnative/react-native-reservation-key.ts')
const peerId = 'AA:BB:CC:DD:EE:FF'
const selector = { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', serviceOccurrence: 0, characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb', characteristicOccurrence: 0 }
const child = spawn('java', [`-Djava.library.path=${process.env.UBM_JS_JNI_LIBRARY}`, '-cp', process.env.UBM_JS_JNI_CLASSES, 'com.ubm.core.TestJsBridge'], { stdio: ['pipe', 'pipe', 'inherit'] })
const pending = new Map(); const dropReplies = new Set(); let sequence = 0
const encode = value => Buffer.from(value).toString('base64')
let readyResolve; let readyReject; let readySeen = false
const ready = new Promise((resolve, reject) => { readyResolve = resolve; readyReject = reject })
readline.createInterface({ input: child.stdout }).on('line', line => {
  const [id, data] = line.split('\t'); if (data === undefined) { process.stderr.write(`${line}\n`); return }
  const value = Buffer.from(data, 'base64').toString()
  if (id === 'ready') { readySeen = true; readyResolve(value); return }
  const waiter = pending.get(id); if (waiter) { pending.delete(id); clearTimeout(waiter.timer); waiter.resolve(value) }
})
child.on('error', error => { if (!readySeen) readyReject(error) })
child.on('exit', code => { if (!readySeen) readyReject(new Error(`JVM exited before ready: ${code}`)); for (const waiter of pending.values()) { clearTimeout(waiter.timer); waiter.reject(new Error(`JVM exited ${code}`)) } pending.clear() })
function command(op, args = {}) {
  const id = String(++sequence)
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${op} timed out`)) }, 10000)
    pending.set(id, { resolve, reject, timer }); child.stdin.write(`${id}\t${encode(op)}\t${encode(Array.isArray(args) ? args.join('\u001f') : typeof args === 'string' ? args : JSON.stringify(args))}\n`)
  })
}
async function invoke(op, args) {
  const envelope = JSON.parse(await command(op, args))
  if (!envelope.ok) { const error = new Error(`${op}: ${envelope.error?.code ?? 'failure'}`); error.code = envelope.error?.code; error.detail = envelope.error?.detail; error.operation = envelope.error?.operation; throw error }
  const parsed = parseOpValue(op, envelope.value)
  if (!parsed.ok) throw parsed.error
  if (dropReplies.delete(op)) throw new Error('synthetic lost response')
  return parsed.value
}
async function run() {
  await initializeReactNativeReservationKeys(async length => new Uint8Array(require('node:crypto').randomBytes(length)))
  await Promise.race([ready, new Promise((_, reject) => setTimeout(() => reject(new Error('JVM startup timed out')), 10000).unref())])
  assert.equal((await invoke('gatt.ordered.marker.version', {})).wireVersion, 1)
  await invoke('scan.start', { serviceUuids: ['180D'], duplicatePolicy: 'all', operationId: 'scan' })
  await command('_advertise'); await command('_drain')
  const connection = await invoke('connection.connect', { peerId, lease: 'lease-1', operationId: 'connect' })
  const database = await invoke('gatt.discover', { peerId, lease: 'lease-1', operationId: 'discover' })
  const group = await invoke('gatt.ordered.reserve', { peerId })
  await invoke('gatt.ordered.open', { wireVersion: 2, peerId, collectionId: group.collectionId, connectionGeneration: connection.connectionGeneration, databaseGeneration: database.databaseGeneration, publicConnectionGeneration: 'public-c', publicDatabaseGeneration: 'public-d', members: [{ memberId: '0', publicMemberId: 'a', selector, requestedDelivery: 'prefer-notification' }], maxItems: 8, maxBytes: 128, maxControls: 8, operationId: 'open' })
  const make = createReactNativeOrderedMarkerReservations(invoke, peerId, group.collectionId, () => selector, () => 0, () => 'js-marker')
  const handle = make(); const target = {}; const options = { signal: null, deadline: null }
  const started = await handle.start(target, Uint8Array.from([1, 2]), 'with-response', options)
  assert.ok(['pending', 'admitted'].includes(started.state))
  let status = started
  for (let i = 0; i < 200 && status.state === 'pending'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); status = await handle.status() }
  assert.equal(status.state, 'admitted')
  let record
  for (let i = 0; i < 200; i++) { const value = await invoke('gatt.ordered.poll', { peerId, collectionId: group.collectionId }); if (value.deliveryId !== null) { record = JSON.parse(value.record); await invoke('gatt.ordered.ack-delivery', { peerId, collectionId: group.collectionId, deliveryId: value.deliveryId }); break } await new Promise(resolve => setTimeout(resolve, 5)) }
  assert.equal(record?.kind, 'marker')
  assert.equal((await handle.cancel()).state, 'admitted')
  await handle.forget()
  await command('_hold')
  const next = make()
  assert.equal((await next.start(target, Uint8Array.from([3]), 'with-response', options)).state, 'pending')
  for (let i = 0; i < 200 && await command('_held') === '0'; i++) await new Promise(resolve => setTimeout(resolve, 5))
  assert.notEqual(await command('_held'), '0')
  assert.equal((await next.cancel()).state, 'not-admitted')
  await command('_release')
  assert.equal(await command('_writes'), '1')
  await next.forget()
  const retained = await invoke('gatt.ordered.marker.reserve', { peerId, collectionId: group.collectionId, reservationId: 'reclaim-test' })
  const disposal = JSON.parse(await Promise.race([command('_replace'), new Promise((_, reject) => setTimeout(() => reject(new Error('session replacement timed out')), 5000).unref())]))
  assert.equal(disposal.ok, true)
  assert.equal(disposal.value.state, 'release-failed')
  await assert.rejects(invoke('gatt.ordered.reclaim', { peerId, collectionId: group.collectionId, reclaimCapability: 'invalid-capability' }), /ownership.denied/)
  await invoke('gatt.ordered.reclaim', { peerId, collectionId: group.collectionId, reclaimCapability: group.reclaimCapability })
  const recovered = await invoke('gatt.ordered.marker.cancel', { peerId, collectionId: group.collectionId, handle: retained.handle })
  assert.equal(recovered.state, 'not-admitted')
  await invoke('gatt.ordered.marker.forget', { peerId, collectionId: group.collectionId, handle: retained.handle })
  let closed = 'pending-source'
  for (let i = 0; i < 100 && closed !== 'released'; i++) {
    closed = await invoke('gatt.ordered.close', { peerId, collectionId: group.collectionId, operationId: `close-${i}` })
    if (closed === 'released') break
    const delivery = await invoke('gatt.ordered.poll', { peerId, collectionId: group.collectionId })
    if (delivery.deliveryId !== null) await invoke('gatt.ordered.ack-delivery', { peerId, collectionId: group.collectionId, deliveryId: delivery.deliveryId })
    if (closed !== 'released') await new Promise(resolve => setTimeout(resolve, 5))
  }
  assert.equal(closed, 'released')
  await invoke('gatt.ordered.ack-group', { peerId, collectionId: group.collectionId })
  await invoke('gatt.ordered.forget-group', { peerId, collectionId: group.collectionId, reclaimCapability: group.reclaimCapability })
  assert.equal(await command('_disables'), '1')
  await command('_reset')
  const recorderConnection = await invoke('connection.connect', { peerId, lease: 'recorder-lease', operationId: 'recorder-connect' })
  const recorderDatabase = await invoke('gatt.discover', { peerId, lease: 'recorder-lease', operationId: 'recorder-discover' })
  const recorderSession = {
    orderedRecordingVersion: async () => Number(await command('_record-version')),
    orderedRecordingReserve: id => command('_record-control', ['reserve', id]),
    orderedRecordingStartStatus: handle => command('_record-control', ['status', handle]),
    orderedRecordingRetire: handle => command('_record-control', ['retire', handle]),
    orderedRecordingStart: (handle, id, maxBytes, maxRecords, declaration, budget) => command('_record-start', [handle, id, maxBytes, maxRecords, declaration, String(budget)]),
    orderedRecordingContext: () => { throw new Error('not used') },
    orderedRecordingStop: () => { throw new Error('not used') }
  }
  let recovery = null
  const recorder = createReactNativeOrderedRecordingStart(recorderSession, (collectionId, capability) => { recovery = { collectionId, capability } })
  const declaration = { kind: 'ordered-recording', version: 1, peerId, connectionGeneration: recorderConnection.connectionGeneration, databaseGeneration: recorderDatabase.databaseGeneration, maxItems: 8, maxBytes: 128, maxControls: 8, members: [{ memberId: '0', publicMemberId: 'a', selector: { ...selector, serviceOccurrence: '0', characteristicOccurrence: '0' }, delivery: null, preference: 'notification' }] }
  const startedRecording = await recorder.start({ id: 'synthetic-recorder', maxBytes: 1048576, maxRecords: 100, declaration, budgetMs: 5000, refreshBudget: () => 5000, signal: null })
  assert.equal(startedRecording.kind, 'started')
  assert.equal(typeof recovery?.capability, 'string')
  assert.equal(JSON.stringify(startedRecording).includes(recovery.capability), false)
  await recorder.retire()
  const recorderDisposal = JSON.parse(await Promise.race([command('_replace'), new Promise((_, reject) => setTimeout(() => reject(new Error('recorder replacement timed out')), 5000).unref())]))
  assert.equal(recorderDisposal.ok, true)
  await invoke('connection.connect', { peerId, lease: 'replacement-lease', operationId: 'replacement-connect' })
  await invoke('gatt.discover', { peerId, lease: 'replacement-lease', operationId: 'replacement-discover' })
  const claim = { peerId, collectionId: recovery.collectionId, recordingId: 'synthetic-recorder', reclaimCapability: recovery.capability }
  const denied = createReactNativeRecordingReclaims(invoke, { ...claim, recordingId: 'wrong-recorder' }, () => 'js-wrong', async () => {})()
  const deniedResult = await denied.start()
  assert.equal(deniedResult.state, 'not-reserved')
  assert.equal(deniedResult.error.code, 'ownership.denied')
  await denied.retire()
  await command('_invalidate')
  const invalidated = createReactNativeRecordingReclaims(invoke, claim, () => 'js-stale', async () => {})()
  let invalidState = await invalidated.start()
  for (let i = 0; i < 200 && invalidState.state === 'pending'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); invalidState = await invalidated.status() }
  assert.equal(invalidState.state, 'failed')
  for (let i = 0; ; i++) {
    try { await invalidated.retire(); break } catch (error) {
      if (error.code !== 'lifecycle.invalid-state' || i >= 199) throw error
      await new Promise(resolve => setTimeout(resolve, 5))
    }
  }
  await command('_hold-permit')
  const heldBeforeDispose = createReactNativeRecordingReclaims(invoke, claim, () => 'js-held-dispose', async () => {})()
  let heldState
  for (let i = 0; i < 200; i++) {
    try { heldState = await heldBeforeDispose.start(); break } catch (error) {
      if (error.code !== 'lifecycle.invalid-state' || i === 199) throw error
      await new Promise(resolve => setTimeout(resolve, 5))
    }
  }
  assert.equal(heldState.state, 'pending')
  for (let i = 0; i < 200 && await command('_permit-held') === '0'; i++) await new Promise(resolve => setTimeout(resolve, 5))
  assert.notEqual(await command('_permit-held'), '0')
  const heldDisposePromise = command('_replace')
  for (let i = 0; i < 1200 && await command('_permit-fenced') !== 'true'; i++) await new Promise(resolve => setTimeout(resolve, 5))
  assert.equal(await command('_permit-fenced'), 'true')
  await command('_release-permit')
  let heldDisposal = JSON.parse(await heldDisposePromise)
  assert.equal(heldDisposal.ok, true)
  for (let i = 0; i < 200 && heldDisposal.value.state !== 'released'; i++) {
    await new Promise(resolve => setTimeout(resolve, 5))
    heldDisposal = JSON.parse(await command('_retry-old'))
  }
  assert.equal(heldDisposal.value.state, 'released')
  await invoke('connection.connect', { peerId, lease: 'held-replacement-lease', operationId: 'held-replacement-connect' })
  await invoke('gatt.discover', { peerId, lease: 'held-replacement-lease', operationId: 'held-replacement-discover' })
  const readyBeforeDispose = createReactNativeRecordingReclaims(invoke, claim, () => 'js-ready-dispose', async () => {})()
  let readyState
  for (let i = 0; i < 200; i++) {
    try { readyState = await readyBeforeDispose.start(); break } catch (error) {
      if (error.code !== 'lifecycle.invalid-state' || i === 199) throw error
      await new Promise(resolve => setTimeout(resolve, 5))
    }
  }
  for (let i = 0; i < 200 && readyState.state === 'pending'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); readyState = await readyBeforeDispose.status() }
  assert.equal(readyState.state, 'ready')
  const readyDisposal = JSON.parse(await command('_replace'))
  assert.equal(readyDisposal.ok, true)
  let oldCleanup = readyDisposal
  for (let i = 0; i < 200 && oldCleanup.value.state !== 'released'; i++) {
    await new Promise(resolve => setTimeout(resolve, 5))
    oldCleanup = JSON.parse(await command('_retry-old'))
  }
  assert.equal(oldCleanup.value.state, 'released')
  await invoke('connection.connect', { peerId, lease: 'ready-replacement-lease', operationId: 'ready-replacement-connect' })
  await invoke('gatt.discover', { peerId, lease: 'ready-replacement-lease', operationId: 'ready-replacement-discover' })
  let activated = false
  const reserveGood = createReactNativeRecordingReclaims(invoke, claim, () => 'js-reclaim', async () => { activated = true })
  const good = reserveGood()
  dropReplies.add('gatt.ordered.recording-reclaim.reserve')
  await assert.rejects(good.start(), /synthetic lost response/)
  await invoke('connection.connect', { peerId, lease: 'reserve-retry-replacement-lease', operationId: 'reserve-retry-replacement-connect' })
  await invoke('gatt.discover', { peerId, lease: 'reserve-retry-replacement-lease', operationId: 'reserve-retry-replacement-discover' })
  const retainedRequest = await invoke('gatt.ordered.recording-reclaim.next', { ...claim, callerKey: 'js-reclaim' })
  assert.match(retainedRequest.requestId, /^\d+$/)
  const competingRequest = await invoke('gatt.ordered.recording-reclaim.next', { ...claim, callerKey: 'js-competing' })
  assert.equal(competingRequest.state, 'not-reserved')
  let reclaimState = await good.start()
  for (let i = 0; i < 200 && reclaimState.state === 'pending'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); reclaimState = await good.status() }
  assert.equal(reclaimState.state, 'ready')
  dropReplies.add('gatt.ordered.recording-reclaim.commit')
  await assert.rejects(good.commit(), /synthetic lost response/)
  assert.equal((await good.status()).state, 'committed')
  let committedDisposal = JSON.parse(await command('_replace'))
  assert.equal(committedDisposal.ok, true)
  for (let i = 0; i < 200 && committedDisposal.value.state !== 'released'; i++) {
    await new Promise(resolve => setTimeout(resolve, 5))
    committedDisposal = JSON.parse(await command('_retry-old'))
  }
  assert.equal(committedDisposal.value.state, 'released')
  await invoke('connection.connect', { peerId, lease: 'committed-replacement-lease', operationId: 'committed-replacement-connect' })
  await invoke('gatt.discover', { peerId, lease: 'committed-replacement-lease', operationId: 'committed-replacement-discover' })
  const reserveFinal = createReactNativeRecordingReclaims(invoke, claim, () => 'js-final', async () => { activated = true })
  const finalHandle = reserveFinal()
  let finalState
  for (let i = 0; i < 200; i++) {
    try { finalState = await finalHandle.start(); break } catch (error) {
      if (error.code !== 'lifecycle.invalid-state' || i === 199) throw error
      await new Promise(resolve => setTimeout(resolve, 5))
    }
  }
  for (let i = 0; i < 200 && finalState.state === 'pending'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); finalState = await finalHandle.status() }
  assert.equal(finalState.state, 'ready')
  assert.equal((await finalHandle.commit()).state, 'committed')
  dropReplies.add('gatt.ordered.recording-reclaim.release')
  await assert.rejects(finalHandle.release(), /synthetic lost response/)
  let release = await finalHandle.status()
  for (let i = 0; i < 200 && release.state !== 'released'; i++) { await new Promise(resolve => setTimeout(resolve, 5)); release = await finalHandle.status() }
  assert.equal(release.state, 'released')
  assert.equal(activated, true)
  dropReplies.add('gatt.ordered.recording-reclaim.ack-retire')
  await assert.rejects(finalHandle.retire(), /synthetic lost response/)
  dropReplies.add('gatt.ordered.recording-reclaim.forget-retire')
  await assert.rejects(finalHandle.retire(), /synthetic lost response/)
  assert.throws(() => reserveFinal())
  dropReplies.add('gatt.ordered.recording-reclaim.ack-forget')
  await assert.rejects(finalHandle.retire(), /synthetic lost response/)
  assert.throws(() => reserveFinal())
  await finalHandle.retire()
  let stop
  for (let i = 0; i < 100; i++) {
    stop = JSON.parse(await command('_record-stop', 'synthetic-recorder'))
    assert.equal(stop.ok, true)
    if (stop.value.recording.state === 'finished') break
    assert.ok(['waiting-source', 'waiting-drain', 'waiting-capacity'].includes(stop.value.recording.state))
    await new Promise(resolve => setTimeout(resolve, 5))
  }
  assert.equal(stop.value.recording.state, 'finished')
  await command('_quit'); child.stdin.end()
  console.log('JS → JNI → MobileSession marker and recorder: OK')
}
run().catch(error => { console.error(error); child.kill(); process.exitCode = 1 })
