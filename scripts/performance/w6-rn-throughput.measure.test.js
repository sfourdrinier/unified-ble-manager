// scripts/performance/w6-rn-throughput.measure.test.js
//
// W6 React Native throughput measurement (runs under jest: the deterministic
// RN harness is only supported in that runtime). For 1, 3 and 6 simultaneous
// notification streams through the ordinary factory
// (`createReactNativeBleManagerWithEnvironment`) over the deterministic
// `UnifiedBleRustCore` double, measures native calls/sec, notifications/sec,
// drain backlog age, retained bytes, and lifecycle-delivery latency.
// Read-only measurement: the drain design is unchanged. Deterministic: fixed
// payload sizes and counts, no randomness.
//
// Executed via scripts/performance/w6-rn-throughput.js, which spawns this
// file under jest and prints the report. W6_BENCH_OUTPUT (a writable path)
// carries the JSON report back to the spawner.

'use strict'

const fs = require('fs')
const { performance } = require('perf_hooks')
const {
  rustCoreHarness,
  environment,
  scanOptions,
  subscribeOptions
} = require('../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../lib/commonjs/react-native-manager')

jest.setTimeout(300000)

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

async function measureStreamCount(streamCount) {
  const { harness, manager, backend, connection, database, target } = await openJourney()
  const subscriptions = []
  for (let index = 0; index < streamCount; index += 1) {
    subscriptions.push(
      await withHangGuard(database.subscribe(target, subscribeOptions()), `benchmark subscribe ${index}`)
    )
  }
  const payload = new Uint8Array(VALUE_BYTES)

  // Warmup: the first emit races consumer registration, so prove every
  // stream live (and discard the warmup) before the measured flood.
  for (const subscription of subscriptions) {
    const iterator = subscription.values[Symbol.asyncIterator]()
    harness.native.emitNotification(new Uint8Array([0xff]))
    const warmup = await withHangGuard(iterator.next(), 'benchmark warmup')
    if (warmup.done || warmup.value.kind !== 'value') {
      throw new Error('benchmark warmup did not deliver a value')
    }
  }

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
  for (const subscription of resubscribed) {
    const iterator = subscription.values[Symbol.asyncIterator]()
    harness.native.emitNotification(new Uint8Array([0xfe]))
    const warmup = await withHangGuard(iterator.next(), 'benchmark lifecycle warmup')
    if (warmup.done || warmup.value.kind !== 'value') {
      throw new Error('benchmark lifecycle warmup did not deliver a value')
    }
  }
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

describe('W6 RN throughput (deterministic harness)', () => {
  test('measures 1, 3 and 6 notification streams', async () => {
    const outputPath = process.env.W6_BENCH_OUTPUT
    if (typeof outputPath !== 'string' || outputPath.length === 0) {
      throw new Error('W6_BENCH_OUTPUT must name the report file')
    }
    const measurements = []
    for (const streamCount of STREAM_COUNTS) {
      measurements.push(await measureStreamCount(streamCount))
    }
    const report = {
      schema: 'w6-rn-throughput/v1',
      startedAt: new Date().toISOString(),
      proofLevel: 'deterministic-rn-harness',
      note: 'Deterministic harness timing, no radio; compares stream counts, not devices.',
      measurements
    }
    fs.writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`)
    for (const measurement of measurements) {
      console.log(
        `streams=${measurement.streams} notif/sec=${measurement.notificationsPerSec} ` +
          `native-calls/sec=${measurement.nativeCallsPerSec ?? 'n/a'} ` +
          `drainMs=${measurement.drainWallMs} backlogAgeMs=${measurement.backlogAgeMs} ` +
          `retainedBuffers=${measurement.retainedByteBuffers} heapDelta=${measurement.heapDeltaBytes} ` +
          `lifecycleMs=${measurement.lifecycleLatencyMs}`
      )
    }
  })
})
