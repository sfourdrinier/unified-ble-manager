'use strict'

const path = require('node:path')

const root = path.resolve(__dirname, '../..')
const { capacity } = require(path.join(root, 'lib/commonjs/backend-contract/primitives.js'))
const { CoreBoundedStream } = require(path.join(root, 'lib/commonjs/core/bounded-stream.js'))
const { normalizeScanObservation, normalizeScanQuery, observationMatchesScanQuery } = require(
  path.join(root, 'lib/commonjs/public/scan-query.js')
)
const { DeterministicVirtualClock } = require(path.join(root, 'lib/commonjs/testing/deterministic/virtual-clock.js'))
const { planBluezScan } = require(path.join(root, 'lib/commonjs/backends/bluez/bluez-scan-planner.js'))

const serviceHeartRate = '0000180d-0000-1000-8000-00805f9b34fb'
const serviceBattery = '0000180f-0000-1000-8000-00805f9b34fb'
const defaultEvents = 96
const defaultSamples = 9

function positiveInteger(value, name, minimum, maximum) {
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed) || parsed < minimum || parsed > maximum) {
    throw new Error(`${name} must be a safe integer between ${String(minimum)} and ${String(maximum)}`)
  }
  return parsed
}

function createWorkload(events) {
  const query = normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] })
  const plan = planBluezScan(query)
  const observations = Array.from({ length: events }, (_, index) =>
    normalizeScanObservation({
      peerId: `scan-path-peer-${String(index)}`,
      localName: 'Heart Strap',
      rssi: -42,
      txPowerLevel: null,
      serviceUuids: index % 3 === 2 ? [serviceBattery] : [serviceHeartRate],
      manufacturerData: [],
      serviceData: []
    })
  )
  return { query, nativeFilter: plan.nativeFilter, observations }
}

function nativeAccepts(filter, observation) {
  return (
    filter.serviceUuids.length === 0 ||
    (observation.serviceUuids !== null && filter.serviceUuids.every(uuid => observation.serviceUuids.includes(uuid)))
  )
}

function runBatch(events) {
  const workload = createWorkload(events)
  const clock = new DeterministicVirtualClock()
  const stream = new CoreBoundedStream(
    {
      itemCapacity: capacity(8),
      byteCapacity: capacity(9),
      reservedControlCapacity: capacity(1)
    },
    'drop-oldest'
  )
  let callbacks = 0
  let residualMatcherEvaluations = 0

  workload.observations.forEach((observation, index) => {
    clock.scheduleAfter(index, () => {
      if (!nativeAccepts(workload.nativeFilter, observation)) return
      callbacks += 1
      residualMatcherEvaluations += 1
      if (observationMatchesScanQuery(workload.query, observation)) {
        stream.emit(observation, 1, null, 1)
      }
    })
  })
  clock.runUntilIdle()
  const overflow = stream.overflowCounters()
  const result = {
    callbacks,
    residualMatcherEvaluations,
    overflow: {
      droppedItems: Number(overflow.droppedItems),
      droppedBytes: Number(overflow.droppedBytes),
      replacedItems: Number(overflow.replacedItems)
    }
  }
  stream.closeWithExactZeroCounters('closed')
  return result
}

function percentile(values, percentileValue) {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.max(0, Math.ceil((percentileValue / 100) * sorted.length) - 1)
  return sorted[index]
}

function runScanPathHarness(options = {}) {
  const samples = positiveInteger(options.samples ?? defaultSamples, 'samples', 3, 31)
  const events = positiveInteger(options.events ?? defaultEvents, 'events', 9, 4096)
  const durationsNanoseconds = []
  let firstCounters = null
  for (let index = 0; index < samples; index += 1) {
    const started = process.hrtime.bigint()
    const counters = runBatch(events)
    const elapsed = process.hrtime.bigint() - started
    if (elapsed <= 0n) throw new Error('scan-path performance clock did not advance')
    durationsNanoseconds.push(Number(elapsed) / events)
    if (firstCounters === null) firstCounters = counters
    if (JSON.stringify(counters) !== JSON.stringify(firstCounters)) {
      throw new Error('scan-path deterministic counters changed between samples')
    }
  }
  if (firstCounters === null) throw new Error('scan-path harness produced no samples')
  return Object.freeze({
    schema: 'unified-ble-scan-path-performance/v1',
    proof: Object.freeze({ scope: 'deterministic-scan-path-model', claim: 'model-only' }),
    schedule: Object.freeze({ clock: 'deterministic-virtual-time', eventCount: events }),
    metrics: Object.freeze({
      callbacks: Object.freeze({ status: 'measured', evidence: 'deterministic-model', count: firstCounters.callbacks }),
      residualMatcherEvaluations: Object.freeze({
        status: 'measured',
        evidence: 'canonical-residual-matcher-invocations',
        count: firstCounters.residualMatcherEvaluations
      }),
      overflow: Object.freeze({
        status: 'measured',
        evidence: 'bounded-stream-counters',
        droppedItems: firstCounters.overflow.droppedItems,
        droppedBytes: firstCounters.overflow.droppedBytes,
        replacedItems: firstCounters.overflow.replacedItems
      })
    }),
    latency: Object.freeze({
      status: 'measured',
      evidence: 'javascript-path-wall-clock',
      p50NanosecondsPerEvent: percentile(durationsNanoseconds, 50),
      p95NanosecondsPerEvent: percentile(durationsNanoseconds, 95),
      samples
    }),
    evidence: Object.freeze([
      Object.freeze({
        metric: 'native-radio-callback-latency',
        status: 'blocked',
        reason: 'The deterministic harness models the native callback boundary and does not open a radio host.'
      })
    ])
  })
}

function parseArguments(argumentsValue) {
  const options = {}
  for (let index = 0; index < argumentsValue.length; index += 1) {
    const argument = argumentsValue[index]
    const value = argumentsValue[index + 1]
    if (value === undefined || value.startsWith('--')) throw new Error(`${argument} requires a value`)
    if (argument === '--samples') options.samples = positiveInteger(value, '--samples', 3, 31)
    else if (argument === '--events') options.events = positiveInteger(value, '--events', 9, 4096)
    else if (argument === '--output') options.output = path.resolve(process.cwd(), value)
    else throw new Error(`unknown scan-path harness option: ${argument}`)
    index += 1
  }
  return options
}

if (require.main === module) {
  try {
    const options = parseArguments(process.argv.slice(2))
    const serialized = `${JSON.stringify(runScanPathHarness(options), null, 2)}\n`
    if (options.output === undefined) process.stdout.write(serialized)
    else require('node:fs').writeFileSync(options.output, serialized)
  } catch (error) {
    const message = error instanceof Error ? error.message : 'scan-path harness failed with a non-Error value'
    process.stderr.write(`[scan-path-harness] ${message}\n`)
    process.exitCode = 1
  }
}

module.exports = { runScanPathHarness }
