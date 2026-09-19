// scripts/performance/w6-rn-throughput.js
//
// W6 React Native throughput benchmark. Measures, for 1, 3 and 6
// simultaneous notification streams in the deterministic RN harness (no
// radio): native calls/sec, notifications/sec, drain backlog age, retained
// bytes, and lifecycle-delivery latency. Read-only measurement: the drain
// design is unchanged. Deterministic: fixed sizes and counts, no randomness.
//
// Runs in plain node (`node scripts/performance/w6-rn-throughput.js`) AND
// under jest via scripts/performance/w6-rn-throughput.measure.test.js (a
// thin wrapper over runThroughputMeasurement below): one implementation, so
// both runtimes pin the same numbers. W8: the previous entry spawned the
// measurement under jest because the harness "stalled" in plain node. That
// was a module-resolution artifact, not a pump race — plain node could not
// resolve `react-native` (Flow source) or src/*.ts (jest transform) — fixed
// here with the f01-factory-routing-proof loader pattern (stub +
// src→lib redirect). The wake-to-drain chain itself is runtime-independent:
// wakes and drains owe nothing to JS timers.
//
// Drain fairness note (W8): each emit fans out to every consumer and the
// router delivers in ordinal order, so live readers share the drain fairly
// (pinned by rust-core-drain-no-lost-wake.test.js). drainAll below still
// reads subscriptions sequentially, like W6: streams that wait their turn
// keep their drop-oldest cap (8), so 3/6-stream reports show [N,8,8…].
// That is per-stream policy, not drain starvation.
//
// Usage: node scripts/performance/w6-rn-throughput.js [--output <path>]
// (run with the repo toolchain on PATH, as for the other gates).

'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { performance } = require('node:perf_hooks')

const root = path.resolve(__dirname, '..', '..')

// The native module currently being measured. Plain node resolves
// `react-native` through the stub below, which answers with this holder:
// journeys run sequentially, so one slot is exact.
const nativeHolder = { native: null }

/**
 * Plain-node loader (no-op under jest): stub `react-native` (its published
 * entry is Flow source, unparseable outside the RN preset) and resolve the
 * test-support src/*.ts requires from the compiled lib/ instead. Production
 * code is untouched: the injected deterministic module means the stubbed
 * TurboModuleRegistry is never consulted for BLE work.
 */
function installPlainNodeLoader() {
  const Module = require('node:module')
  const { createRequire } = require('node:module')
  const originalLoad = Module._load
  const srcRoot = path.join(root, 'src')
  const libRoot = path.join(root, 'lib', 'commonjs')
  Module._load = function hooked(request, parent, isMain) {
    if (request === 'react-native') {
      return {
        Platform: { OS: 'android', Version: 35, select: options => options.android },
        TurboModuleRegistry: {
          get: name => (name === 'UnifiedBleRustCore' ? nativeHolder.native : null),
          getEnforcing: name => {
            if (name === 'UnifiedBleRustCore' && nativeHolder.native !== null) return nativeHolder.native
            throw new Error(`TurboModule ${name} is not installed`)
          }
        },
        NativeModules: {}
      }
    }
    if (typeof request === 'string' && parent !== undefined && parent.filename.startsWith(root)) {
      const req = createRequire(parent.filename)
      for (const candidate of [request, `${request}.ts`]) {
        let resolved = null
        try {
          resolved = req.resolve(candidate)
        } catch {
          resolved = null
        }
        if (resolved !== null && resolved.startsWith(`${srcRoot}${path.sep}`)) {
          const rel = path.relative(srcRoot, resolved).replace(/\.ts$/, '')
          return originalLoad.call(this, path.join(libRoot, `${rel}.js`), parent, isMain)
        }
        if (resolved !== null) break
      }
    }
    return originalLoad.call(this, request, parent, isMain)
  }
}

if (typeof jest === 'undefined') installPlainNodeLoader()

const {
  rustCoreHarness,
  environment,
  scanOptions,
  subscribeOptions
} = require('../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../lib/commonjs/react-native-manager')

const STREAM_COUNTS = [1, 3, 6]
const FLOOD_VALUES = 2000
const VALUE_BYTES = 20
const LIFECYCLE_BACKLOG = 100
const HANG_GUARD_MS = 15000

function harnessCallCount(native) {
  // Every native-module crossing the double records (invoke/drain/session
  // ops; synchronous queue appends do not cross). The delta over a phase is
  // the harness-native call volume for that phase.
  if (!Array.isArray(native.calls)) return null
  return native.calls.length
}

function withHangGuard(promise, label) {
  // A parked pump fails loudly instead of stalling the benchmark: a lost
  // wake would surface here, never as a silent hang or an exit 0.
  let timer = null
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`w6 benchmark hung at ${label}`)), HANG_GUARD_MS)
  })
  return Promise.race([promise, guard]).finally(() => {
    if (timer !== null) clearTimeout(timer)
  })
}

async function idleNext(iterator, idleMs = 2000) {
  let timer = null
  const idle = new Promise(resolve => {
    timer = setTimeout(() => resolve(null), idleMs)
  })
  try {
    return await Promise.race([iterator.next(), idle])
  } finally {
    if (timer !== null) clearTimeout(timer)
  }
}

async function openJourney() {
  const harness = rustCoreHarness({ platform: 'android' })
  nativeHolder.native = harness.native
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const scan = await manager.scan(scanOptions())
  const seen = scan.observations[Symbol.asyncIterator]().next()
  harness.native.emitAdvertisement()
  const observed = await withHangGuard(seen, 'benchmark observe')
  if (observed.done || observed.value.kind !== 'value') throw new Error('benchmark setup observed no peer')
  await scan.stop()
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await withHangGuard(
    manager.connect(peerId, { signal: null, deadline: null }),
    'benchmark connect'
  )
  const database = await withHangGuard(
    connection.discover({ signal: null, deadline: null }),
    'benchmark discover'
  )
  const snapshot = await database.snapshot()
  const target = snapshot.characteristics[0].path
  return { harness, manager, backend, connection, database, target }
}

async function drainAll(subscriptions) {
  const counts = []
  // lastItemAtMs stops at the final delivered item: the 2s idle tail that
  // terminates each stream is quiescence detection, not drain work, and is
  // excluded from the drain/backlog metrics.
  let lastItemAtMs = 0
  for (const subscription of subscriptions) {
    let values = 0
    let overflows = 0
    const iterator = subscription.values[Symbol.asyncIterator]()
    for (;;) {
      const item = await idleNext(iterator)
      if (item === null || item.done) break
      if (item.value.kind === 'value') values += 1
      else if (item.value.kind === 'overflow') overflows += 1
      else break
      lastItemAtMs = performance.now()
    }
    counts.push({ values, overflows })
  }
  return { counts, lastItemAtMs }
}

async function proveLive(subscriptions, emit, label) {
  // One emit fans out to every consumer, so a single emit proves every
  // stream live with nothing left buffered. (A per-stream emit loop leaves
  // strays in every stream — the W6 [2002,8,8] +2.)
  const iterators = subscriptions.map(subscription => subscription.values[Symbol.asyncIterator]())
  emit()
  for (let index = 0; index < iterators.length; index += 1) {
    const warmup = await withHangGuard(iterators[index].next(), `${label} ${index}`)
    if (warmup.done || warmup.value.kind !== 'value') {
      throw new Error(`benchmark ${label} did not deliver a value`)
    }
  }
}

async function measureStreamCount(streamCount) {
  const { harness, manager, backend, connection, database, target } = await openJourney()
  const subscriptions = []
  for (let index = 0; index < streamCount; index += 1) {
    subscriptions.push(
      await withHangGuard(database.subscribe(target, subscribeOptions()), `benchmark subscribe ${index}`)
    )
  }
  const payload = new Uint8Array(VALUE_BYTES)

  await proveLive(
    subscriptions,
    () => harness.native.emitNotification(new Uint8Array([0xff])),
    'benchmark warmup'
  )

  const heapBefore = process.memoryUsage().heapUsed
  const callsBefore = harnessCallCount(harness.native)
  let lastEmitAt = 0
  const floodStart = performance.now()
  for (let index = 0; index < FLOOD_VALUES; index += 1) {
    payload[0] = index & 0xff
    payload[1] = (index >> 8) & 0xff
    if (index === FLOOD_VALUES - 1) lastEmitAt = performance.now()
    harness.native.emitNotification(payload)
  }
  const floodWallMs = performance.now() - floodStart
  const retainedByteBuffers = Number(backend.resourceCounters().retainedByteBuffers)
  const heapDeltaBytes = process.memoryUsage().heapUsed - heapBefore

  const drainStart = performance.now()
  const drained = await withHangGuard(drainAll(subscriptions), `benchmark drain ${streamCount}`)
  const drainWallMs = drained.lastItemAtMs > 0 ? drained.lastItemAtMs - drainStart : 0
  const backlogAgeMs = drained.lastItemAtMs > 0 ? drained.lastItemAtMs - lastEmitAt : 0
  // Native crossings happen in the drain pump (microtasks after the
  // synchronous flood loop), so the call volume is measured over
  // flood+drain, not the flood alone.
  const callsAfterDrain = harnessCallCount(harness.native)
  const nativeCallsPerSec =
    callsBefore === null || callsAfterDrain === null
      ? null
      : Math.round((callsAfterDrain - callsBefore) / ((floodWallMs + drainWallMs) / 1000))

  for (const subscription of subscriptions) {
    await subscription.remove()
  }

  const resubscribed = []
  for (let index = 0; index < streamCount; index += 1) {
    resubscribed.push(await database.subscribe(target, subscribeOptions()))
  }
  await proveLive(
    resubscribed,
    () => harness.native.emitNotification(new Uint8Array([0xfe])),
    'benchmark lifecycle warmup'
  )
  for (let index = 0; index < LIFECYCLE_BACKLOG; index += 1) {
    harness.native.emitNotification(payload)
  }
  const lifecycleStart = performance.now()
  await withHangGuard(connection.disconnect(), `benchmark disconnect ${streamCount}`)
  const terminals = await withHangGuard(
    Promise.all(
      resubscribed.map(async subscription => {
        const iterator = subscription.values[Symbol.asyncIterator]()
        for (;;) {
          const item = await iterator.next()
          if (item.done || item.value.kind === 'terminal') return item.done ? 'done' : item.value.reason
        }
      })
    ),
    `benchmark lifecycle ${streamCount}`
  )
  const lifecycleLatencyMs = performance.now() - lifecycleStart
  for (const subscription of resubscribed) {
    await subscription.remove().catch(() => ({ state: 'released', failures: [] }))
  }
  await connection.release()
  await manager.destroy()

  const floodWallSec = floodWallMs / 1000
  return {
    streams: streamCount,
    floodValues: FLOOD_VALUES,
    valueBytes: VALUE_BYTES,
    floodWallMs: Number(floodWallMs.toFixed(2)),
    notificationsPerSec: Math.round(FLOOD_VALUES / floodWallSec),
    nativeCallsPerSec,
    drainedValuesPerStream: drained.counts.map(entry => entry.values),
    drainWallMs: Number(drainWallMs.toFixed(2)),
    backlogAgeMs: Number(backlogAgeMs.toFixed(2)),
    retainedByteBuffers,
    heapDeltaBytes,
    lifecycleBacklog: LIFECYCLE_BACKLOG,
    lifecycleLatencyMs: Number(lifecycleLatencyMs.toFixed(2)),
    lifecycleTerminals: terminals
  }
}

async function runThroughputMeasurement() {
  const measurements = []
  for (const streamCount of STREAM_COUNTS) {
    measurements.push(await measureStreamCount(streamCount))
  }
  return {
    schema: 'w6-rn-throughput/v1',
    startedAt: new Date().toISOString(),
    proofLevel: 'deterministic-rn-harness',
    note: 'Deterministic harness timing, no radio; compares stream counts, not devices. Runs in plain node and under jest from one implementation.',
    measurements
  }
}

function summaryLines(report) {
  return report.measurements.map(
    measurement =>
      `streams=${measurement.streams} notif/sec=${measurement.notificationsPerSec} ` +
      `native-calls/sec=${measurement.nativeCallsPerSec ?? 'n/a'} ` +
      `drainMs=${measurement.drainWallMs} backlogAgeMs=${measurement.backlogAgeMs} ` +
      `retainedBuffers=${measurement.retainedByteBuffers} heapDelta=${measurement.heapDeltaBytes} ` +
      `lifecycleMs=${measurement.lifecycleLatencyMs}`
  )
}

function parseArguments(raw) {
  let outputPath = null
  for (let index = 0; index < raw.length; index += 1) {
    if (raw[index] === '--output') {
      const value = raw[index + 1]
      if (value === undefined || value.length === 0) throw new Error('usage: w6-rn-throughput.js [--output <path>]')
      outputPath = path.resolve(process.cwd(), value)
      index += 1
    } else {
      throw new Error(`unknown option: ${raw[index]} (usage: w6-rn-throughput.js [--output <path>])`)
    }
  }
  return { outputPath }
}

async function main() {
  const { outputPath } = parseArguments(process.argv.slice(2))
  const report = await runThroughputMeasurement()
  for (const line of summaryLines(report)) console.log(line)
  const text = JSON.stringify(report, null, 2)
  if (outputPath !== null) fs.writeFileSync(outputPath, `${text}\n`)
  console.log(text)
}

if (require.main === module) {
  main().then(
    () => {},
    error => {
      console.error(`w6-rn-throughput failed: ${error?.stack ?? String(error)}`)
      process.exitCode = 1
    }
  )
}

module.exports = { runThroughputMeasurement, summaryLines }
