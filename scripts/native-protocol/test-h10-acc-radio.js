'use strict'

// Explicit physical-radio / simulated-peripheral qualification, never real-H10 proof.
// UBM_NAPI_ADDON=/absolute/current.node UBM_RADIO_PLATFORM=corebluetooth|bluez|winrt
// UBM_RADIO_ADAPTER=<optional> node scripts/native-protocol/test-h10-acc-radio.js
// BlueZ also requires UBM_BLUEZ_DAEMON_OWNER=<verified current unique owner>.
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { execFileSync } = require('node:child_process')
const { radioProbeOptions } = require('./radio-probe-options')

function bounded(promise, timeoutMs, operation) {
  let timer
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${operation} timed out`)), timeoutMs)
    })
  ]).finally(() => clearTimeout(timer))
}

// A watchdog ends a wait, not the owned operation. Share that pending work
// through final cleanup; only a settled failure may admit a retry.
function singleFlight(action) {
  let pending
  return () => {
    if (!pending) {
      const tracked = Promise.resolve()
        .then(action)
        .finally(() => {
          if (pending === tracked) pending = undefined
        })
      pending = tracked
    }
    return pending
  }
}

// One pump owns each iterator. No polling, discarded stream errors, or unbounded history.
function inbox(stream, label, parse = value => value) {
  const iterator = stream[Symbol.asyncIterator]()
  const queued = [],
    pending = []
  let failure,
    stopped = false
  function fail(error) {
    failure = error
    for (const waiter of pending.splice(0)) waiter.reject(error)
  }
  const pump = (async () => {
    try {
      while (!stopped) {
        const item = await iterator.next()
        if (stopped && item.done) break
        if (item.done) throw new Error(`${label} ended unexpectedly`)
        assert.equal(item.value.kind, 'value', `${label} loss or terminal: ${JSON.stringify(item.value)}`)
        const value = parse(item.value.value.value)
        if (stopped) {
          queued.push(value)
          break
        }
        const waiter = pending.shift()
        if (waiter) waiter.resolve(value)
        else {
          assert.ok(queued.length < 128, `${label} probe queue overflow`)
          queued.push(value)
        }
      }
    } catch (error) {
      fail(error)
    }
  })()
  return {
    next() {
      if (failure) return Promise.reject(failure)
      if (queued.length) return Promise.resolve(queued.shift())
      return new Promise((resolve, reject) => pending.push({ resolve, reject }))
    },
    clear() {
      if (failure) throw failure
      const count = queued.length
      queued.length = 0
      return count
    },
    async close() {
      stopped = true
      for (const waiter of pending.splice(0)) waiter.reject(new Error(`${label} probe closed`))
      if (iterator.return) await iterator.return()
      await pump
      if (failure) throw failure
    }
  }
}

async function defaultManager(env) {
  const radioOptions = radioProbeOptions(env)
  const entry = require(`../../lib/commonjs/node-${env.UBM_RADIO_PLATFORM}`)
  const factories = {
    corebluetooth: 'createCoreBluetoothBleManager',
    bluez: 'createBluezBleManager',
    winrt: 'createWinRtBleManager'
  }
  // These public factories verify UBM_NAPI_ADDON identity before radio effects.
  return entry[factories[env.UBM_RADIO_PLATFORM]]({
    owner: 'h10-acc-radio-probe',
    ...radioOptions,
    ...(env.UBM_RADIO_ADAPTER ? { adapterId: env.UBM_RADIO_ADAPTER } : {})
  })
}

async function run(options, pmd) {
  const env = options.env || process.env
  assert.ok(env.UBM_NAPI_ADDON && path.isAbsolute(env.UBM_NAPI_ADDON), 'explicit absolute UBM_NAPI_ADDON required')
  assert.ok(['corebluetooth', 'bluez', 'winrt'].includes(env.UBM_RADIO_PLATFORM), 'explicit radio platform required')
  const timeoutMs = options.timeoutMs || 15000
  const log = options.log || (entry => console.log(JSON.stringify(entry)))
  const manager = await (options.createManager || defaultManager)(env)
  const cleanup = [],
    errors = [],
    abort = new AbortController()
  const operation = { timeoutMs, signal: abort.signal }
  let completed = false
  const own = (name, action, receipt = true) => cleanup.push({ name, action, receipt })
  try {
    const peer = await manager.find({
      timeoutMs: 20000,
      signal: abort.signal,
      select: candidate => candidate.name === 'SIM Polar H10 0001'
    })
    assert.equal(peer.name, 'SIM Polar H10 0001', 'refusing unrelated sensor')
    const connection = await manager.connect(peer, operation)
    own('connection', () => connection.disconnect())
    const gatt = await connection.discover(operation)
    const cp = gatt.characteristic(pmd.PMD_SERVICE, pmd.PMD_CONTROL_POINT)
    const data = gatt.characteristic(pmd.PMD_SERVICE, pmd.PMD_DATA)
    const features = pmd.parsePmdFeatures(await cp.read(operation))
    assert.ok(features.acc && features.ecg, 'simulator must advertise ECG and ACC')
    const cpSub = await cp.subscribe({ ...operation, delivery: 'prefer-indication', stream: 'lossless-bounded' })
    own('control subscription', () => cpSub.remove())
    const cpInbox = inbox(cpSub.values, 'control point')
    own('control iterator', () => cpInbox.close(), false)
    const parseData = bytes => {
      const measurement = bytes[0] % 64
      if (measurement === 2) return { measurement, frame: pmd.parseAccFrame(bytes) }
      if (measurement === 0) return { measurement, frame: pmd.parseEcgFrame(bytes) }
      throw new Error(`unexpected PMD measurement ${measurement}`)
    }
    let dataInbox, retireData
    async function openDataPhase() {
      const subscription = await data.subscribe({
        ...operation,
        delivery: 'prefer-notification',
        stream: 'lossless-bounded'
      })
      let released = false,
        closed = false
      const remove = singleFlight(async () => {
        if (released) return { state: 'released', failures: [] }
        const receipt = await subscription.remove()
        released = receipt.state === 'released' && receipt.failures.length === 0
        return receipt
      })
      own('data subscription', remove)
      const current = inbox(subscription.values, 'PMD data', parseData)
      const close = singleFlight(async () => {
        if (!closed) {
          await current.close()
          closed = true
        }
      })
      own('data iterator', close, false)
      dataInbox = current
      retireData = async () => {
        // End the old local consumer before awaiting authoritative removal.
        // Independent CP/data queues make a STOP response insufficient as a
        // data-generation fence. Never admit the next consumer on failed release.
        await bounded(close(), timeoutMs, 'phase data iterator cleanup')
        const count = current.clear()
        log({
          phase: 'phase-boundary',
          validatedFramesExcluded: count,
          scope: 'locally-buffered',
          iteratorClosed: true
        })
        const receipt = await bounded(remove(), timeoutMs, 'phase data subscription cleanup')
        assert.equal(receipt.state, 'released', 'phase data subscription cleanup remained unresolved')
        assert.deepEqual(receipt.failures, [])
      }
    }
    await openDataPhase()
    const phaseBoundary = () => {
      const count = dataInbox.clear()
      if (count) log({ phase: 'phase-boundary', validatedFramesExcluded: count })
    }
    async function command(bytes) {
      const receipt = await cp.write(bytes, { ...operation, response: 'required' })
      assert.equal(receipt.commitState, 'confirmed', 'PMD write was not confirmed')
      const response = pmd.parseControlPointMessage(await bounded(cpInbox.next(), timeoutMs, 'PMD response'))
      assert.equal(response.kind, 'response', 'unexpected PMD control message')
      assert.equal(response.opCode, bytes[0], 'PMD response opcode mismatch')
      assert.equal(response.measurementType, bytes[1], 'PMD response measurement mismatch')
      assert.equal(response.status, 0, `PMD command refused: ${response.statusName} (${response.status})`)
      assert.equal(response.more, false, 'unexpected fragmented simulator response')
      return response
    }
    async function frames(type, count = 2) {
      const result = []
      await bounded(
        (async () => {
          while (result.length < count) {
            const { measurement, frame } = await dataInbox.next()
            if (measurement === type) result.push(frame)
          }
        })(),
        timeoutMs,
        'positive PMD samples'
      )
      if (type === 0)
        assert.ok(
          result.some(frame => frame.samplesMicroVolts.some(sample => sample !== 0)),
          'ECG all-zero payload'
        )
      return result
    }
    const settings = pmd.parsePmdSettings((await command(pmd.buildGetAccSettingsCommand())).parameters)
    assert.deepEqual(settings.SAMPLE_RATE, [...pmd.H10_ACC_SAMPLE_RATES_HZ])
    assert.deepEqual(settings.RESOLUTION, [16])
    assert.deepEqual(settings.RANGE, [...pmd.H10_ACC_RANGES_G])
    for (const sampleRateHz of pmd.H10_ACC_SAMPLE_RATES_HZ)
      for (const rangeG of pmd.H10_ACC_RANGES_G) {
        phaseBoundary()
        await command(pmd.buildStartAccCommand({ sampleRateHz, resolutionBits: 16, rangeG }))
        const [first, second] = await frames(2)
        // Exactly two metadata-only records per setting: retain the evidence
        // even when the following strict timing assertion fails. No raw values.
        log({
          phase: 'acc-frame-pair',
          sampleRateHz,
          rangeG,
          frames: [first, second].map(frame => ({
            timestampNs: frame.timestampNs.toString(),
            samples: frame.samplesMilliG.length
          }))
        })
        for (const frame of [first, second]) {
          assert.equal(frame.frameType, 1, 'H10 selected16-bit ACC must use raw type1')
          assert.ok(
            frame.samplesMilliG.some(sample => sample.x !== 0 || sample.y !== 0 || sample.z !== 0),
            'ACC all-zero payload'
          )
          for (const sample of frame.samplesMilliG)
            for (const axis of [sample.x, sample.y, sample.z])
              assert.ok(Math.abs(axis) <= rangeG * 1000, 'ACC exceeds selected physical range')
        }
        assert.equal(
          second.timestampNs - first.timestampNs,
          (BigInt(second.samplesMilliG.length) * 1000000000n) / BigInt(sampleRateHz),
          'ACC timestamp/sample-rate mismatch'
        )
        await command(pmd.buildStopAccCommand())
        await retireData()
        log({
          phase: 'acc-setting',
          sampleRateHz,
          rangeG,
          samples: first.samplesMilliG.length + second.samplesMilliG.length
        })
        await openDataPhase()
      }
    const startAcc = () => command(pmd.buildStartAccCommand({ sampleRateHz: 200, resolutionBits: 16, rangeG: 8 }))
    phaseBoundary()
    await command(pmd.buildStartEcgCommand())
    await startAcc()
    await frames(2)
    await frames(0)
    await command(pmd.buildStopAccCommand())
    phaseBoundary()
    await frames(0)
    await startAcc()
    await command(pmd.buildStopEcgCommand())
    phaseBoundary()
    await frames(2)
    await command(pmd.buildStopAccCommand())
    completed = true
  } catch (error) {
    errors.push(error)
    abort.abort()
  } finally {
    for (const resource of cleanup.reverse()) {
      try {
        const receipt = await bounded(Promise.resolve().then(resource.action), timeoutMs, `${resource.name} cleanup`)
        if (resource.receipt) {
          assert.equal(receipt.state, 'released', `${resource.name} cleanup remained unresolved`)
          assert.deepEqual(receipt.failures, [])
        }
      } catch (error) {
        errors.push(error)
      }
    }
    try {
      const receipt = await bounded(manager.destroy(), timeoutMs, 'manager cleanup')
      assert.equal(receipt.state, 'released', 'manager cleanup remained unresolved')
      assert.deepEqual(receipt.failures, [])
    } catch (error) {
      errors.push(error)
    }
  }
  if (errors.length === 1) throw errors[0]
  if (errors.length) throw new AggregateError(errors, errors.map(error => error.message).join('; '))
  assert.ok(completed)
  log({
    phase: 'passed',
    settings: 12,
    interleavedStopOrders: 2,
    evidence: 'physical-radio-simulated-peripheral; not real-H10 equivalence'
  })
}

async function main(options = {}) {
  if (options.pmd) return run(options, options.pmd)
  const root = path.resolve(__dirname, '../..')
  const dist = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-acc-probe-'))
  try {
    execFileSync(
      process.execPath,
      [
        require.resolve('typescript/bin/tsc'),
        'examples-shared/driver/polar-pmd.ts',
        '--outDir',
        dist,
        '--module',
        'commonjs',
        '--target',
        'es2022',
        '--moduleResolution',
        'node',
        '--skipLibCheck',
        '--declaration',
        'false'
      ],
      { cwd: root, stdio: 'pipe' }
    )
    return await run(options, require(path.join(dist, 'polar-pmd.js')))
  } finally {
    fs.rmSync(dist, { recursive: true, force: true })
  }
}

module.exports = { main }
if (require.main === module)
  main().catch(error => {
    console.error(error)
    process.exitCode = 1
  })
