'use strict'

// Physical transport, simulated H10: never evidence of phone OS background life.
// Explicit opt-in only; the caller supplies an identity-verified checkout addon.
// UBM_NAPI_ADDON=/absolute/file.node UBM_RADIO_PLATFORM=corebluetooth|bluez|winrt
// UBM_RADIO_ADAPTER=<optional exact adapter> UBM_SIM_CONTROL_PORT=<loopback port>
// UBM_CONTINUATION_SECONDS=600 node scripts/native-protocol/test-continuation-radio.js
// The simulator must be in adversarial mode; this test drops only its BLE link.
const assert = require('node:assert/strict')
const net = require('node:net')
const path = require('node:path')
const { setTimeout: delay } = require('node:timers/promises')

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
    const scan = await central.startScan({
      owner: 'native-continuation-radio-test',
      serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
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
      resubscribe: [
        {
          serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
          characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
        }
      ]
    }
    const execution = await continuation.execute(declaration)
    assert.equal(execution.resubscribed, 1)
    log(JSON.stringify({ phase: 'collecting-native', platform, peer, duration, execution }))
    // Do not call a drain/pump while collecting: Rust must own notification intake.
    await wait(10000)
    const before = await continuation.status()
    assert.ok(before && before.queuedData > 0, 'no native data before disruption')
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
    assert.ok(after.queuedData > before.queuedData, 'no data after recovery')
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
      samples.push(...backlog.values)
      assert.equal(backlog.controlLost, 0, 'control records were lost')
      assert.equal(backlog.streamEnds.filter(end => end.reason === 'overflow').length, 0, 'native backlog overflowed')
      released = backlog.disposed
    }
    assert.ok(released, 'native owner cleanup remained unresolved')
    assert.ok(samples.length >= Math.floor(duration / 3), `too few HR samples: ${samples.length}`)
    for (const sample of samples) {
      const bytes = sample.value
      assert.ok(bytes.length >= 2 && bytes[1] > 0, 'invalid/zero HR measurement')
    }
    const consumers = new Set(samples.map(record => record.consumer))
    assert.ok(consumers.size >= 2, 'no positive samples from both subscription generations')
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
  log(JSON.stringify(passed))
}

module.exports = { main }
if (require.main === module)
  main().catch(error => {
    console.error(error)
    process.exitCode = 1
  })
