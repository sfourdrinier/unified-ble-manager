// scripts/ci/tauri-ordered-public-bridge.js

import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { strict as assert } from 'node:assert'
import { createTauriBleManagerWithEnvironment } from '../../src/tauri'

const child = spawn(
  'cargo',
  [
    'test',
    '--manifest-path',
    'native/tauri/Cargo.toml',
    '--lib',
    'ordered_public_tauri_bridge',
    '--',
    '--ignored',
    '--nocapture'
  ],
  { env: { ...process.env, UBM_TAURI_BRIDGE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] }
)
const pending = new Map()
let sequence = 1
let readyResolve = () => undefined
const ready = new Promise(resolve => {
  readyResolve = resolve
})
let receiver = null
const terminalEvidenceEvents = []
createInterface({ input: child.stdout }).on('line', line => {
  const index = line.indexOf('UBM_BRIDGE ')
  if (index < 0) return
  const message = JSON.parse(line.slice(index + 11))
  if (message.ready) readyResolve()
  else if (message.event !== undefined) {
    if (message.event?.item?.kind === 'terminal' && message.event.item.evidence?.durability === 'committed') terminalEvidenceEvents.push(message.event.item.evidence)
    receiver?.(message.event)
  }
  else if (typeof message.id === 'number') {
    const item = pending.get(message.id)
    pending.delete(message.id)
    item?.resolve(message.result)
  }
})
child.on('exit', code => {
  for (const item of pending.values()) item.reject(new Error(`bridge exited ${code}`))
})
function command(kind, fields = {}) {
  const id = sequence++
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject })
    child.stdin.write(`${JSON.stringify({ id, kind, ...fields })}\n`)
  })
}
class Channel {
  onmessage = null
  constructor() {
    receiver = value => this.onmessage?.(value)
  }
}
let loseFirst = true
let loseMarker = true
let loseRecording = true
let loseTerminalEvidenceRead = false
const loseReclaim = new Set(['reserve', 'commit', 'retire', 'ack-retirement', 'forget-retirement', 'confirm-forgotten'])
let holdMarkerReservation = false
let signalReservationHeld = () => undefined
let releaseMarkerReservation = () => undefined
const markerReservationHeld = new Promise(resolve => { signalReservationHeld = resolve })
await ready
try {
  const manager = await createTauriBleManagerWithEnvironment({
    Channel,
    invoke: async (_command, args) => {
      const request = args?.request
      const operation = typeof request === 'object' && request !== null ? Reflect.get(Reflect.get(request, 'envelope') ?? {}, 'command') : null
      if (loseTerminalEvidenceRead && operation === 'terminalEvidence.read') {
        loseTerminalEvidenceRead = false
        throw new Error('synthetic terminal evidence read failure')
      }
      const result = await command('invoke', { request })
      if (holdMarkerReservation && operation === 'gatt.ordered.marker.reserve') {
        signalReservationHeld()
        await new Promise(resolve => { releaseMarkerReservation = resolve })
        holdMarkerReservation = false
      }
      if (typeof operation === 'string' && operation.startsWith('gatt.ordered-recording.reclaim.')) {
        const action = operation.slice('gatt.ordered-recording.reclaim.'.length)
        if (loseReclaim.delete(action)) throw new Error(`synthetic lost reclaim ${action} response`)
      }
      if (loseMarker && operation === 'gatt.ordered.marker.start') {
        loseMarker = false
        throw new Error('synthetic lost marker response')
      }
      if (loseRecording && operation === 'gatt.ordered-recording.start') {
        loseRecording = false
        throw new Error('synthetic lost recording response')
      }
      if (
        loseFirst &&
        typeof request === 'object' &&
        request !== null &&
        Reflect.get(Reflect.get(request, 'envelope') ?? {}, 'command') === 'gatt.ordered.next' &&
        typeof result === 'object' &&
        result !== null &&
        Reflect.get(Reflect.get(result, 'payload') ?? {}, 'state') === 'item'
      ) {
        loseFirst = false
        throw new Error('synthetic lost response')
      }
      return result
    }
  }, { terminalEvidence: { maximumCanonicalBytes: 4096, maximumDurableBytes: 8192 } })
  const connection = await manager.connect('bridge-peer')
  const database = await connection.discover()
  const a = database.characteristic('180d', '2a37')
  const b = database.characteristic('180d', '2a38')
  const controlPoint = database.characteristic('180d', '2a39')
  const setup = await database.subscribeOrdered([
    { memberId: 'A', characteristic: a },
    { memberId: 'B', characteristic: b }
  ])
  assert.equal(setup.state, 'ready')
  if (setup.state !== 'ready') throw new Error('group setup failed')
  const iterator = setup.stream[Symbol.asyncIterator]()
  for (const [uuid, bytes] of [
    ['00002a37-0000-1000-8000-00805f9b34fb', [1]],
    ['00002a38-0000-1000-8000-00805f9b34fb', [2]],
    ['00002a37-0000-1000-8000-00805f9b34fb', [3]]
  ])
    await command('notify', { uuid, bytes })
  await assert.rejects(iterator.next(), /synthetic lost response/)
  const items = [await iterator.next(), await iterator.next(), await iterator.next()]
  assert.deepEqual(
    items.map(item => (item.value?.kind === 'value' ? [item.value.memberId, [...item.value.value]] : null)),
    [
      ['A', [1]],
      ['B', [2]],
      ['A', [3]]
    ]
  )
  const cancelledMarker = setup.stream.reserveMarkerWrite()
  assert.equal((await cancelledMarker.cancel()).state, 'not-admitted')
  assert.equal((await cancelledMarker.start(controlPoint, new Uint8Array([99]), 'with-response')).state, 'not-admitted')
  await cancelledMarker.forget()
  holdMarkerReservation = true
  const racingMarker = setup.stream.reserveMarkerWrite()
  const racingStart = racingMarker.start(controlPoint, new Uint8Array([98]), 'with-response')
  await markerReservationHeld
  const racingCancel = racingMarker.cancel()
  releaseMarkerReservation()
  assert.equal((await racingStart).state, 'not-admitted')
  assert.equal((await racingCancel).state, 'not-admitted')
  await racingMarker.forget()
  const marker = setup.stream.reserveMarkerWrite()
  await assert.rejects(marker.start(controlPoint, new Uint8Array([1]), 'with-response'), /synthetic lost marker response/)
  let markerStatus = await marker.status()
  for (let retry = 0; retry < 100 && markerStatus.state === 'pending'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    markerStatus = await marker.status()
  }
  assert.equal(markerStatus.state, 'admitted')
  const markerItem = await iterator.next()
  assert.equal(markerItem.value?.kind, 'marker')
  const afterMarker = iterator.next()
  await command('notify', { uuid: '00002a37-0000-1000-8000-00805f9b34fb', bytes: [4] })
  assert.equal((await afterMarker).value?.kind, 'value')
  await command('settle-marker', { collectionId: markerItem.value.collectionId, markerId: markerItem.value.markerId })
  markerStatus = await marker.status()
  assert.equal(markerStatus.write.state, 'settled')
  await marker.forget()
  await command('loss', { count: 2 })
  const loss = await iterator.next()
  assert.equal(loss.value?.kind, 'loss')
  let cleanup = await setup.stream.close()
  for (let retry = 0; retry < 20 && cleanup.group.state !== 'released'; retry += 1) {
    const item = await iterator.next()
    if (item.done) break
    cleanup = await setup.stream.close()
  }
  await iterator.next()
  cleanup = await setup.stream.close()
  assert.equal(cleanup.group.state, 'released')
  const abandoned = await database.subscribeOrdered([
    { memberId: 'A', characteristic: a },
    { memberId: 'B', characteristic: b }
  ])
  assert.equal(abandoned.state, 'ready')
  await command('notify', { uuid: '00002a37-0000-1000-8000-00805f9b34fb', bytes: [8] })
  await command('notify', { uuid: '00002a38-0000-1000-8000-00805f9b34fb', bytes: [9] })
  let discarded = await abandoned.stream.discard()
  for (let retry = 0; retry < 100 && discarded.group.state !== 'released'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    discarded = await abandoned.stream.discard()
  }
  assert.equal(discarded.group.state, 'released')
  assert.equal(discarded.discarded.values, '2')
  assert.equal(discarded.discarded.bytes, '2')
  // This is deliberately a public-manager journey, not a Rust parser check:
  // subscription admission reserves host-owned evidence, the real channel
  // delivers a terminal only after native commit, and a failed read leaves
  // the retained terminal available to a late public reader.
  const terminalSubscription = await a.subscribe()
  const terminalIterator = terminalSubscription.values[Symbol.asyncIterator]()
  await command('notify', { uuid: '00002a37-0000-1000-8000-00805f9b34fb', bytes: [55] })
  assert.deepEqual((await terminalIterator.next()).value?.value?.value, new Uint8Array([55]))
  await command('fail-notification-source', { uuid: '00002a37-0000-1000-8000-00805f9b34fb' })
  for (let retry = 0; retry < 100 && terminalEvidenceEvents.length === 0; retry += 1)
    await new Promise(resolve => setTimeout(resolve, 10))
  assert.equal(terminalEvidenceEvents.length, 1)
  assert.equal(terminalEvidenceEvents[0].durability, 'committed')
  loseTerminalEvidenceRead = true
  await assert.rejects(terminalIterator.next(), /synthetic terminal evidence read failure/)
  const lateTerminal = terminalSubscription.values[Symbol.asyncIterator]()
  const terminal = await lateTerminal.next()
  assert.equal(terminal.done, false)
  assert.equal(terminal.value?.kind, 'terminal')
  assert.equal(terminal.value?.reason, 'source-failed')
  assert.equal(terminal.value?.error?.code, 'platform.failure')
  assert.equal(terminal.value?.error?.operation, 'gatt.notification')
  const terminalCleanup = await terminalSubscription.remove()
  assert.equal(terminalCleanup.state, 'released')
  const recording = database.reserveOrderedRecordingStart()
  await assert.rejects(recording.start([{ memberId: 'A', characteristic: a }], {
    recording: { id: 'bridge-recording', maxBytes: 1048576, maxRecords: 128 }
  }), /synthetic lost recording response/)
  const recoveredStart = await recording.status()
  assert.equal(recoveredStart.state, 'result')
  const started = recoveredStart.result
  assert.equal(started.kind, 'started')
  assert.equal(started.receipt.state, 'active')
  assert.equal((await recording.status()).state, 'result')
  const recoveryHandle = recording.recoveryHandle()
  const recordedMarker = recording.reserveMarkerWrite()
  let recordedMarkerStatus = await recordedMarker.start(controlPoint, new Uint8Array([2]), 'with-response')
  for (let retry = 0; retry < 100 && recordedMarkerStatus.state === 'pending'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    recordedMarkerStatus = await recordedMarker.status()
  }
  assert.equal(recordedMarkerStatus.state, 'admitted')
  await command('settle-marker', { collectionId: started.receipt.collectionId, markerId: '0' })
  assert.equal((await recordedMarker.status()).write.state, 'settled')
  await command('notify', { uuid: '00002a37-0000-1000-8000-00805f9b34fb', bytes: [42] })
  for (let retry = 0; retry < 100; retry += 1) {
    const status = await command('recording-status', { recordingId: 'bridge-recording' })
    if (status.records >= 2) break
    if (retry === 99) throw new Error('recording did not capture notification')
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  await recordedMarker.forget()
  await connection.release()
  const replacementConnection = await manager.connect('bridge-peer')
  const replacementDatabase = await replacementConnection.discover()
  const replacementControl = replacementDatabase.characteristic('180d', '2a39')
  const reclaim = replacementDatabase.reserveOrderedRecordingReclaim(recoveryHandle)
  await assert.rejects(reclaim.start(), /synthetic lost reclaim reserve response/)
  let reclaimStatus = await reclaim.start()
  for (let retry = 0; retry < 100 && reclaimStatus.state === 'pending'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    reclaimStatus = await reclaim.status()
  }
  assert.equal(reclaimStatus.state, 'ready')
  await assert.rejects(reclaim.commit(), /synthetic lost reclaim commit response/)
  assert.equal((await reclaim.status()).mappingCommitted, true)
  let releaseStatus = await reclaim.release()
  for (let retry = 0; retry < 100 && releaseStatus.state === 'releasing'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    releaseStatus = await reclaim.status()
  }
  assert.equal(releaseStatus.state, 'released')
  for (let retry = 0; retry < 5; retry += 1) {
    try {
      await reclaim.retire()
      break
    } catch (error) {
      if (retry === 4 || !String(error).includes('synthetic lost reclaim')) throw error
    }
  }
  assert.equal(loseReclaim.size, 0)
  const replacementMarker = replacementDatabase.reserveOrderedRecordingMarker('bridge-recording')
  let replacementStatus = await replacementMarker.start(replacementControl, new Uint8Array([3]), 'with-response')
  for (let retry = 0; retry < 100 && replacementStatus.state === 'pending'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    replacementStatus = await replacementMarker.status()
  }
  assert.equal(replacementStatus.state, 'admitted')
  await command('settle-marker', { collectionId: started.receipt.collectionId, markerId: '1' })
  assert.equal((await replacementMarker.status()).write.state, 'settled')
  for (let retry = 0; retry < 100; retry += 1) {
    const status = await command('recording-status', { recordingId: 'bridge-recording' })
    if (status.records >= 3) break
    if (retry === 99) throw new Error('replacement marker was not journaled')
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  await replacementMarker.forget()
  await replacementConnection.release()
  let stopped = await command('recording-stop', { recordingId: 'bridge-recording' })
  for (let retry = 0; retry < 100 && stopped.hostCleanup.state !== 'released'; retry += 1) {
    await new Promise(resolve => setTimeout(resolve, 10))
    stopped = await command('recording-stop', { recordingId: 'bridge-recording' })
  }
  assert.equal(stopped.recording.state, 'finished')
  assert.equal(stopped.hostCleanup.state, 'released')
  const batch = await command('recording-prepare', { recordingId: 'bridge-recording' })
  assert.ok(batch.records.some(entry => entry.record.kind === 'value' && entry.record.bytes[0] === 42))
  await recording.retire()
  await manager.destroy()
  console.log('Tauri public ordered bridge passed')
} finally {
  await command('finish')
  child.stdin.end()
}
