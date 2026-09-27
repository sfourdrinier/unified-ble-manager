'use strict'

// Physical transport, simulated H10: never evidence of phone OS background life.
// Explicit opt-in only; the caller supplies an identity-verified checkout addon.
// UBM_NAPI_ADDON=/absolute/file.node UBM_RADIO_PLATFORM=corebluetooth|bluez|winrt
// UBM_RADIO_ADAPTER=<optional exact adapter> UBM_SIM_CONTROL_PORT=<loopback port>
// UBM_CONTINUATION_SECONDS=600 node scripts/native-protocol/test-continuation-radio.js
// The simulator must be in adversarial mode; this test drops only its BLE link.
// Optional UBM_RECORDING_DIRECTORY must be an absolute test-only directory.
// The probe creates a unique recording, validates/acknowledges it, then clears it.
const assert = require('node:assert/strict')
const { randomUUID } = require('node:crypto')
const fs = require('node:fs')
const net = require('node:net')
const path = require('node:path')
const { setTimeout: delay } = require('node:timers/promises')
const hrSelector = Object.freeze({
  serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
  serviceOccurrence: 1,
  characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
  characteristicOccurrence: 1
})

function value(text) {
  const envelope = JSON.parse(text)
  assert.equal(envelope.ok, true, text)
  return envelope.value
}

async function control(port, command) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port })
    let response = ''
    socket.setTimeout(10000, () => socket.destroy(new Error('simulator control timeout')))
    socket.on('error', reject)
    socket.on('connect', () => socket.write(`${JSON.stringify(command)}\n`))
    socket.on('data', chunk => {
      response += chunk.toString()
      if (!response.includes('\n')) return
      socket.end()
      try {
        resolve(JSON.parse(response.split('\n')[0]))
      } catch (error) {
        reject(error)
      }
    })
    socket.on('end', () => {
      if (!response.includes('\n')) reject(new Error('simulator closed without response'))
    })
  })
}

async function verifyRecording(store, id, { minimumSamples, archive }) {
  const status = await store.status(id)
  assert.equal(status.lostRecords, 0, 'recording lost records')
  assert.equal(status.collectionFailure, null, 'recording collection failed')
  assert.equal(status.runtimeFailure, null, 'recording storage operation failed')
  const stopped = await store.stop(id)
  assert.equal(stopped.phase, 'stopped')
  assert.equal(stopped.accepting, false)
  assert.equal(stopped.radioRelease, 'not-requested')
  const samples = []
  const generations = new Set()
  const databases = new Set()
  const registrations = new Map()
  const registeredDatabases = new Set()
  let previousOrdinal = 0
  for (let pass = 0; pass < 64; pass++) {
    const limits = { maxItems: 256, maxBytes: 1048576 }
    const batch = await store.prepare(id, limits)
    if (batch.token === null) {
      assert.equal(batch.records.length, 0)
      assert.ok(samples.length >= minimumSamples, `too few recorded HR samples: ${samples.length}`)
      assert.equal(generations.size, 2, 'expected exactly two connection generations for one controlled outage')
      assert.equal(databases.size, 2, 'unexpected database generation churn during one controlled outage')
      assert.equal(registrations.size, 2, 'expected exactly two registered consumers for one controlled outage')
      assert.equal(registeredDatabases.size, 2, 'unexpected registered database generation churn')
      assert.equal((await store.status(id)).records, 0, 'acknowledged recording remains nonempty')
      assert.equal((await store.clear(id)).cleared, true)
      return { samples, connectionGenerations: generations.size, databaseGenerations: databases.size }
    }
    assert.deepEqual(await store.prepare(id, limits), batch, 'recording replay changed before acknowledgement')
    for (const entry of batch.records) {
      assert.equal(entry.ordinal, previousOrdinal + 1, 'recording ordinal sequence is not contiguous')
      previousOrdinal = entry.ordinal
      if (['consumer-registration', 'value', 'stream-end'].includes(entry.record.t)) {
        const context = entry.metadata
        const consumer = context.consumer
        assert.ok(consumer, 'record has no consumer registration metadata')
        assert.equal(entry.record.consumer, consumer.consumer, 'record differs from consumer registration identity')
        const identity = JSON.stringify([context.session.sessionEpoch, consumer.consumer])
        if (entry.record.t === 'consumer-registration') {
          assert.ok(!registrations.has(identity), 'duplicate consumer registration')
          assert.deepEqual(consumer.selector, hrSelector, 'consumer registration differs from the declared HR selector')
          registrations.set(identity, context)
          registeredDatabases.add(
            JSON.stringify([context.session.sessionEpoch, consumer.connectionGeneration, consumer.databaseGeneration])
          )
        } else {
          assert.ok(registrations.has(identity), 'record precedes its consumer registration')
          assert.deepEqual(
            context,
            registrations.get(identity),
            'record metadata differs from its consumer registration'
          )
        }
      }
      if (entry.record.t === 'value') {
        assert.ok(entry.metadata.consumer, 'recorded value has no generation identity')
        assert.ok(entry.record.value.length >= 2 && entry.record.value[1] > 0, 'invalid recorded HR')
        generations.add(entry.metadata.consumer.connectionGeneration)
        databases.add(
          JSON.stringify([entry.metadata.consumer.connectionGeneration, entry.metadata.consumer.databaseGeneration])
        )
        samples.push(entry.record)
      } else if (entry.record.t === 'stream-end') {
        assert.equal(entry.record.droppedItems, 0, 'recorded stream lost items')
        assert.equal(entry.record.droppedBytes, 0, 'recorded stream lost bytes')
      }
    }
    // Durable evidence must survive consumption of the native cursor, including
    // a later qualification failure. The caller flushes the archive before ACK.
    await archive(batch.records)
    const receipt = await store.acknowledge(id, batch.token)
    assert.equal(receipt.acknowledged, true)
    assert.equal(receipt.token, batch.token)
    assert.equal(receipt.records, batch.records.length)
  }
  throw new Error('recording drain exceeded bounded qualification budget')
}

async function main(options = {}) {
  const env = options.env || process.env
  const wait = options.wait || delay
  const sendControl = options.sendControl || control
  const log = options.log || console.log
  const addonPath = env.UBM_NAPI_ADDON
  assert.ok(addonPath && path.isAbsolute(addonPath), 'UBM_NAPI_ADDON must identify the tested addon')
  const platform = env.UBM_RADIO_PLATFORM
  assert.ok(['corebluetooth', 'bluez', 'winrt'].includes(platform), 'explicit platform required')
  const duration = Number(env.UBM_CONTINUATION_SECONDS || 600)
  const port = Number(env.UBM_SIM_CONTROL_PORT)
  assert.ok(Number.isInteger(duration) && duration >= 30 && duration <= 1200)
  assert.ok(Number.isInteger(port) && port > 0 && port <= 65535)
  const recordingDirectory = env.UBM_RECORDING_DIRECTORY
  if (recordingDirectory !== undefined)
    assert.ok(path.isAbsolute(recordingDirectory), 'absolute recording directory required')
  const recordingId = recordingDirectory === undefined ? null : `radio-${randomUUID()}`
  const api = options.api || require('../../lib/commonjs/desktop-rust-core-exports')
  const binding = await api.loadDesktopCoreBinding({ platform, operationPrefix: 'continuation-radio' })
  const central = await binding.openProduction({
    owner: 'native-continuation-radio-test',
    platform,
    adapterId: env.UBM_RADIO_ADAPTER || null
  })
  const continuation = api.createNativeContinuationController(central)
  const started = Date.now()
  let passed
  let operationError
  try {
    const recordings = recordingId === null ? null : await continuation.recordings(recordingDirectory)
    const scan = await central.startScan({
      owner: 'native-continuation-radio-test',
      serviceUuids: [hrSelector.serviceUuid],
      timeoutMs: 20000
    })
    let peer
    try {
      const deadline = Date.now() + 20000
      while (Date.now() < deadline && !peer) {
        const observation = (await central.takeScanObservation())?.advertisement
        if (observation?.localName === 'SIM Polar H10 0001') peer = observation.peerId
        if (!peer) await wait(50)
      }
      assert.ok(peer, 'named simulator was not observed; do not connect an unrelated sensor')
    } finally {
      await central.stopScan(scan.operationId, { timeoutMs: 10000 })
    }
    const declaration = {
      onAppearance: 'native',
      peerId: peer,
      ...(recordingId === null ? {} : { recording: { id: recordingId, maxBytes: 16777216, maxRecords: 10000 } }),
      resubscribe: [hrSelector]
    }
    const execution = await continuation.execute(declaration)
    assert.equal(execution.resubscribed, 1)
    log(JSON.stringify({ phase: 'collecting-native', platform, peer, duration, execution }))
    // Do not call a drain/pump while collecting: Rust must own notification intake.
    await wait(10000)
    const before = await continuation.status()
    const beforeRecords = recordings === null ? before?.queuedData : (await recordings.status(recordingId)).records
    assert.ok(beforeRecords > 0, 'no native data before disruption')
    const disruption = await sendControl(port, { cmd: 'drop-link' })
    assert.equal(disruption.ok, true, JSON.stringify(disruption))
    assert.ok(
      Array.isArray(disruption.state?.dropped) && disruption.state.dropped.length > 0,
      'simulator did not report a dropped client link'
    )
    log(JSON.stringify({ phase: 'link-disruption', disruption, before }))
    await wait((duration - 10) * 1000)
    const after = await continuation.status()
    assert.equal(after.continuationOutcome?.event, 'continuation.completed', JSON.stringify(after))
    const afterRecords = recordings === null ? after.queuedData : (await recordings.status(recordingId)).records
    assert.ok(afterRecords > beforeRecords, 'no data after recovery')
    const samples = []
    let released = false
    for (let attempt = 0; attempt < 32 && !released; attempt++) {
      const claim = value(await central.continuationPrepareClaim(256, 1048576))
      const replay = value(await central.continuationPrepareClaim(256, 1048576))
      assert.deepEqual(replay, claim, 'prepared handoff must replay unchanged before acknowledgement')
      assert.ok(claim.claimToken, 'owned backlog needs acknowledgement token')
      // Public controller validates every batch before acknowledging the same
      // prepared token. Malformed bytes must never authorize native disposal.
      const backlog = await continuation.claim({ maxItems: 256, maxBytes: 1048576 })
      if (recordingId !== null) {
        assert.equal(backlog.recording?.id, recordingId, 'claim lost independent recording identity')
        assert.equal(backlog.values.length, 0, 'durable values duplicated into volatile claim')
      }
      samples.push(...backlog.values)
      assert.equal(backlog.controlLost, 0, 'control records were lost')
      assert.deepEqual(backlog.afterCutoffLoss, { items: 0, bytes: 0 }, 'handoff lost records after cutoff')
      assert.equal(backlog.streamEnds.filter(end => end.reason === 'overflow').length, 0, 'native backlog overflowed')
      released = backlog.disposed
    }
    assert.ok(released, 'native owner cleanup remained unresolved')
    if (recordingId === null)
      assert.ok(samples.length >= Math.floor(duration / 3), `too few HR samples: ${samples.length}`)
    for (const sample of samples) {
      const bytes = sample.value
      assert.ok(bytes.length >= 2 && bytes[1] > 0, 'invalid/zero HR measurement')
    }
    const consumers = new Set(samples.map(record => record.consumer))
    if (recordingId === null)
      assert.equal(consumers.size, 2, 'expected exactly two subscription generations for one controlled outage')
    passed = {
      phase: 'passed',
      platform,
      elapsedMs: Date.now() - started,
      sampleCount: samples.length,
      consumerGenerations: consumers.size,
      before,
      after,
      evidence: 'physical-radio-simulated-peripheral; not phone background qualification'
    }
  } catch (error) {
    operationError = error
  } finally {
    try {
      const report = await central.close()
      log(JSON.stringify({ phase: 'central-close', report }))
      assert.equal(report.state, 'released', 'central cleanup remained unresolved')
    } catch (cleanupError) {
      if (operationError) throw new AggregateError([operationError, cleanupError], 'probe and cleanup failed')
      throw cleanupError
    }
  }
  if (operationError) throw operationError
  if (recordingId !== null) {
    // This is deliberately after successful central close. Offline retrieval
    // must not initialize a replacement radio or consume a native claim.
    const offline = await api.openNativeContinuationRecordings(binding, recordingDirectory)
    const archivePath = path.join(recordingDirectory, `${recordingId}-evidence.jsonl`)
    const archiveFd = fs.openSync(archivePath, 'wx', 0o600)
    try {
      const verified = await verifyRecording(offline, recordingId, {
        minimumSamples: Math.floor(duration / 3),
        archive: async records => {
          const text =
            records
              .map(record =>
                JSON.stringify(record, (_key, fieldValue) =>
                  fieldValue instanceof Uint8Array ? [...fieldValue] : fieldValue
                )
              )
              .join('\n') + '\n'
          fs.writeFileSync(archiveFd, text)
          fs.fsyncSync(archiveFd)
        }
      })
      passed.sampleCount = verified.samples.length
      passed.connectionGenerations = verified.connectionGenerations
      passed.databaseGenerations = verified.databaseGenerations
      delete passed.consumerGenerations
      passed.recording = {
        id: recordingId,
        archivePath,
        offlineAfterRadioClose: true,
        acknowledged: true,
        cleared: true
      }
    } finally {
      fs.closeSync(archiveFd)
    }
  }
  log(JSON.stringify(passed))
}

module.exports = { main, verifyRecording }
if (require.main === module)
  main().catch(error => {
    console.error(error)
    process.exitCode = 1
  })
