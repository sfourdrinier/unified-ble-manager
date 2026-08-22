// scripts/performance/run-performance-suite.js

'use strict'

const fs = require('node:fs')
const path = require('node:path')

const root = path.resolve(__dirname, '../..')
const packageManifest = require(path.join(root, 'package.json'))
const { capacity, opaqueId } = require(path.join(root, 'lib/commonjs/backend-contract/primitives.js'))
const { snapshotSerializableRecord } = require(path.join(root, 'lib/commonjs/backend-contract/serializable.js'))
const { CoreBoundedStream } = require(path.join(root, 'lib/commonjs/core/bounded-stream.js'))
const { CoreOperationCoordinator } = require(path.join(root, 'lib/commonjs/core/operation-coordinator.js'))
const { ResourceLedger } = require(path.join(root, 'lib/commonjs/core/resource-ledger.js'))
const { CoreTraceRecorder } = require(path.join(root, 'lib/commonjs/core/trace-recorder.js'))
const { planLongWrite } = require(path.join(root, 'lib/commonjs/core/core-capabilities.js'))
const { createDeterministicTestBackend } = require(
  path.join(root, 'lib/commonjs/testing/deterministic/deterministic-test-backend.js')
)
const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require(
  path.join(root, 'lib/commonjs/native-protocol/v2-codec.js')
)

const maximumIterations = 65_536
const minimumSamples = 3
const maximumSamples = 31
const minimumTargetMilliseconds = 1
const maximumTargetMilliseconds = 5_000
const maximumPayloadBytes = 1024 * 1024
const performanceBaselineId = 'unified-ble-pr8-deterministic-performance-v1'
const deterministicProofLevel = 'deterministic-core'

function fail(message) {
  throw new Error(message)
}

function positiveInteger(value, name, minimum, maximum) {
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed) || parsed < minimum || parsed > maximum) {
    fail(`${name} must be a safe integer between ${String(minimum)} and ${String(maximum)}`)
  }
  return parsed
}

function parseArguments(argumentsValue) {
  const options = {
    samples: 9,
    targetMilliseconds: 25,
    payloadSizes: [0, 20, 244, 4096, 65536],
    outputPath: null,
    nativeHostReportPath: null,
    requireNativeHost: false
  }
  for (let index = 0; index < argumentsValue.length; index += 1) {
    const argument = argumentsValue[index]
    if (argument === '--require-native-host') {
      options.requireNativeHost = true
      continue
    }
    const value = argumentsValue[index + 1]
    if (value === undefined || value.startsWith('--')) fail(`${argument} requires a value`)
    if (argument === '--samples') {
      options.samples = positiveInteger(value, '--samples', minimumSamples, maximumSamples)
    } else if (argument === '--target-ms') {
      options.targetMilliseconds = positiveInteger(
        value,
        '--target-ms',
        minimumTargetMilliseconds,
        maximumTargetMilliseconds
      )
    } else if (argument === '--payload-sizes') {
      const values = value.split(',').map(item => positiveInteger(item, '--payload-sizes', 0, maximumPayloadBytes))
      if (values.length === 0 || new Set(values).size !== values.length) {
        fail('--payload-sizes must contain unique comma-separated byte lengths')
      }
      options.payloadSizes = values
    } else if (argument === '--output') {
      options.outputPath = path.resolve(process.cwd(), value)
    } else if (argument === '--native-host-report') {
      options.nativeHostReportPath = path.resolve(process.cwd(), value)
    } else {
      fail(`unknown performance option: ${argument}`)
    }
    index += 1
  }
  return options
}

function nativeHostSection(options) {
  if (options.nativeHostReportPath === null) {
    if (options.requireNativeHost) fail('required native-host benchmark report was not supplied')
    return Object.freeze({
      status: 'not-run',
      proofLevel: 'none',
      reason: 'No native-host report was supplied; JavaScript timings are not native transport proof.'
    })
  }
  const report = JSON.parse(fs.readFileSync(options.nativeHostReportPath, 'utf8'))
  validateNativeHostReport(report)
  return Object.freeze({
    status: 'measured',
    proofLevel: 'host-native',
    report
  })
}

function validateNativeHostReport(report) {
  if (
    report === null ||
    typeof report !== 'object' ||
    Array.isArray(report) ||
    report.schema !== 'unified-ble-native-host-performance/v1' ||
    !Array.isArray(report.measurements) ||
    report.measurements.length === 0
  ) {
    fail('native-host benchmark report does not match unified-ble-native-host-performance/v1')
  }
  for (const measurement of report.measurements) {
    if (
      typeof measurement.id !== 'string' ||
      measurement.id.length === 0 ||
      !Number.isFinite(measurement.nanosecondsPerOperation) ||
      measurement.nanosecondsPerOperation <= 0 ||
      !Number.isFinite(measurement.p95NanosecondsPerOperation) ||
      measurement.p95NanosecondsPerOperation < measurement.nanosecondsPerOperation
    ) {
      fail('native-host benchmark report contains an invalid measurement')
    }
  }
}

async function elapsedNanoseconds(runBatch, iterations) {
  const started = process.hrtime.bigint()
  await runBatch(iterations)
  const elapsed = process.hrtime.bigint() - started
  if (elapsed <= 0n) fail('performance clock did not advance')
  return Number(elapsed)
}

async function calibratedIterations(runBatch, targetNanoseconds, definitionMaximumIterations = maximumIterations) {
  const maximumDefinitionIterations = Math.min(maximumIterations, definitionMaximumIterations)
  let iterations = 1
  while (iterations < maximumDefinitionIterations) {
    const elapsed = await elapsedNanoseconds(runBatch, iterations)
    if (elapsed >= targetNanoseconds) return iterations
    iterations = Math.min(maximumDefinitionIterations, iterations * 2)
  }
  return iterations
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right)
  return sorted[Math.floor(sorted.length / 2)]
}

function percentile(values, percentileValue) {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.max(0, Math.ceil((percentileValue / 100) * sorted.length) - 1)
  return sorted[index]
}

async function measure(definition, options) {
  const targetNanoseconds = options.targetMilliseconds * 1_000_000
  const iterations = await calibratedIterations(definition.runBatch, targetNanoseconds, definition.maximumIterations)
  const samples = []
  for (let index = 0; index < options.samples; index += 1) {
    const durationNanoseconds = await elapsedNanoseconds(definition.runBatch, iterations)
    const nanosecondsPerOperation = durationNanoseconds / iterations
    samples.push(
      Object.freeze({
        durationNanoseconds,
        operationsPerSecond: 1_000_000_000 / nanosecondsPerOperation,
        nanosecondsPerOperation,
        bytesPerSecond:
          definition.payloadBytes === 0 ? null : (definition.payloadBytes * 1_000_000_000) / nanosecondsPerOperation
      })
    )
  }
  return Object.freeze({
    id: definition.id,
    category: definition.category,
    payloadBytes: definition.payloadBytes,
    ownership: definition.ownership,
    ownershipMetadata: Object.freeze({
      scope: deterministicProofLevel,
      input: definition.ownership,
      retained: definition.retainedOwnership ?? 'no retained benchmark-owned resource beyond the operation',
      cleanup: 'before-return'
    }),
    baselineId: performanceBaselineId,
    proofLevel: deterministicProofLevel,
    iterationsPerSample: iterations,
    samples: Object.freeze(samples),
    summary: Object.freeze({
      p50OperationsPerSecond: median(samples.map(sample => sample.operationsPerSecond)),
      p50NanosecondsPerOperation: median(samples.map(sample => sample.nanosecondsPerOperation)),
      p95NanosecondsPerOperation: percentile(
        samples.map(sample => sample.nanosecondsPerOperation),
        95
      ),
      minimumNanosecondsPerOperation: Math.min(...samples.map(sample => sample.nanosecondsPerOperation)),
      maximumNanosecondsPerOperation: Math.max(...samples.map(sample => sample.nanosecondsPerOperation)),
      p50BytesPerSecond: definition.payloadBytes === 0 ? null : median(samples.map(sample => sample.bytesPerSecond))
    })
  })
}

function attachmentRecord(payloadBytes) {
  const text = 'a'.repeat(Math.max(1, payloadBytes))
  return Object.freeze({
    kind: 'attachment',
    fields: Object.freeze([
      Object.freeze({ id: 1, value: text }),
      Object.freeze({ id: 2, value: 'backend-instance' }),
      Object.freeze({ id: 3, value: 'backend-generation' }),
      Object.freeze({ id: 4, value: 'adapter' }),
      Object.freeze({ id: 5, value: 'adapter-generation' })
    ])
  })
}

function createCoordinator(maximumQueuedOperationsPerConnection = 64) {
  const resourceLedger = new ResourceLedger()
  const trace = new CoreTraceRecorder(4_096, 512 * 1024)
  let nextCorrelation = 1
  const coordinator = new CoreOperationCoordinator({
    now: () => 0,
    createCorrelation: () => {
      const value = `operation-${String(nextCorrelation)}`
      nextCorrelation += 1
      return opaqueId(value, 'core-operation', `attachment:${value}`)
    },
    resourceLedger,
    trace,
    maximumQueuedOperationsPerConnection
  })
  return { coordinator, resourceLedger, trace }
}

function coreExecution(value, options = {}) {
  return {
    queueKey: options.queueKey ?? 'connection-1',
    fairnessKey: options.fairnessKey,
    options: { signal: options.signal ?? null, deadline: null },
    mayCommit: options.mayCommit ?? false,
    retainedPayloadBytes: options.retainedPayloadBytes ?? 0,
    dispatch: () => {
      options.onDispatch?.(value)
      return {
        completion: Promise.resolve(value),
        requestCancellation: async () => {}
      }
    }
  }
}

async function runSuccessfulCoordinatorScenario(executions, maximumQueuedOperationsPerConnection = 64) {
  const { coordinator, resourceLedger, trace } = createCoordinator(maximumQueuedOperationsPerConnection)
  try {
    const results = await Promise.all(executions.map(execution => coordinator.run(execution)))
    if (results.some(result => result.outcome !== 'succeeded')) {
      fail('deterministic coordinator benchmark did not settle successfully')
    }
    if (trace.snapshot().length === 0) fail('deterministic coordinator benchmark recorded no trace')
    return results
  } finally {
    coordinator.destroy()
    await coordinator.waitForQuarantineDrain()
    if (!resourceLedger.isZero()) fail('deterministic coordinator benchmark leaked resources')
  }
}

async function benchmarkPerConnectionQueueScheduling(iterations) {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const executions = []
    const dispatchOrder = []
    for (let operation = 0; operation < 8; operation += 1) {
      executions.push(
        coreExecution(operation, {
          queueKey: 'connection-1',
          fairnessKey: operation % 2 === 0 ? 'read' : 'write',
          onDispatch: value => dispatchOrder.push(value)
        })
      )
    }
    await runSuccessfulCoordinatorScenario(executions, 16)
    if (dispatchOrder.join(',') !== '0,1,2,3,4,5,6,7') {
      fail('per-connection fairness benchmark observed an unstable dispatch order')
    }
  }
}

async function benchmarkCrossConnectionDispatch(iterations) {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const executions = []
    for (let operation = 0; operation < 10; operation += 1) {
      executions.push(
        coreExecution(operation, {
          queueKey: operation % 2 === 0 ? 'connection-1' : 'connection-2',
          fairnessKey: 'default'
        })
      )
    }
    await runSuccessfulCoordinatorScenario(executions, 4)
  }
}

async function benchmarkWriteOperation(iterations, mode) {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const executions = []
    const readinessStream =
      mode === 'without-response'
        ? new CoreBoundedStream(
            { itemCapacity: capacity(4), byteCapacity: capacity(1024), reservedControlCapacity: capacity(128) },
            'error'
          )
        : null
    try {
      if (readinessStream !== null) {
        const readiness = readinessStream.emit({ kind: 'write-ready', mode }, 20, 'write-readiness', 20)
        if (!readiness.accepted) fail('without-response readiness control was not admitted')
      }
      for (let operation = 0; operation < 4; operation += 1) {
        executions.push(
          coreExecution(
            { mode, operation },
            {
              fairnessKey: mode,
              mayCommit: mode === 'with-response',
              retainedPayloadBytes: 20
            }
          )
        )
      }
      await runSuccessfulCoordinatorScenario(executions, 8)
    } finally {
      if (readinessStream !== null) await readinessStream.close()
    }
  }
}

async function benchmarkNotificationIngress(iterations) {
  const payload = new Uint8Array(20)
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const stream = new CoreBoundedStream(
      {
        itemCapacity: capacity(16),
        byteCapacity: capacity(16 * payload.byteLength + 128),
        reservedControlCapacity: capacity(128)
      },
      'drop-oldest'
    )
    try {
      for (let event = 0; event < 64; event += 1) {
        stream.emit(payload, payload.byteLength, null, payload.byteLength)
      }
      if (stream.overflowCounters().droppedItems <= 0) fail('notification stream did not exercise bounded overflow')
      if (stream.retainedBytes() > stream.limits.byteCapacity) fail('notification stream exceeded byte capacity')
    } finally {
      await stream.close()
    }
  }
}

async function benchmarkLongWritePlanning(iterations) {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const fixture = createDeterministicTestBackend({ maximumWriteLength: 20 })
    try {
      for (const byteLength of [0, 1, 20, 21, 244, 4096]) {
        const plan = await planLongWrite(
          fixture.backend.features,
          'connection-1',
          'generation-1',
          'with-response',
          byteLength,
          20
        )
        const expectedChunks = Math.max(1, Math.ceil(byteLength / 20))
        if (plan.totalChunks !== expectedChunks || plan.maximumWriteLength !== 20) {
          fail('long-write planner returned an invalid deterministic plan')
        }
      }
    } finally {
      await fixture.backend.destroy()
    }
  }
}

async function benchmarkServiceChangeClose(iterations) {
  const payload = new Uint8Array(20)
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const stream = new CoreBoundedStream(
      { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(128) },
      'drop-oldest'
    )
    const iterator = stream[Symbol.asyncIterator]()
    try {
      stream.emit(payload, payload.byteLength, null, payload.byteLength)
      stream.closeWithReason('service-changed')
      const terminal = await iterator.next()
      if (terminal.done || terminal.value.kind !== 'terminal' || terminal.value.reason !== 'service-changed') {
        fail('service-change stream did not publish its terminal reason')
      }
      await iterator.return()
    } finally {
      await stream.close()
    }
  }
}

async function benchmarkCleanupTenThousand(iterations) {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const { coordinator, resourceLedger, trace } = createCoordinator(10_000)
    let resolveActive
    const activeCompletion = new Promise(resolve => {
      resolveActive = resolve
    })
    const activeResult = coordinator.run({
      ...coreExecution('active'),
      dispatch: () => ({ completion: activeCompletion, requestCancellation: async () => {} })
    })
    const cancelledResults = []
    const controllers = []
    for (let operation = 0; operation < 9_999; operation += 1) {
      const controller = new AbortController()
      controllers.push(controller)
      cancelledResults.push(
        coordinator.run(coreExecution('queued', { signal: controller.signal, retainedPayloadBytes: 1 }))
      )
    }
    for (const controller of controllers) controller.abort()
    resolveActive('active')
    const results = await Promise.all([activeResult, ...cancelledResults])
    if (
      results.length !== 10_000 ||
      results.some(result => result.outcome !== 'succeeded' && result.outcome !== 'aborted')
    ) {
      fail('10,000-operation cleanup benchmark did not settle all operations')
    }
    coordinator.destroy()
    await coordinator.waitForQuarantineDrain()
    if (!resourceLedger.isZero() || trace.snapshot().length === 0) {
      fail('10,000-operation cleanup benchmark leaked resources')
    }
  }
}

function benchmarkDefinitions(payloadSizes) {
  const definitions = []
  for (const payloadBytes of payloadSizes) {
    const payload = new Uint8Array(payloadBytes)
    payload.fill(0x5a)
    const controlRecord = attachmentRecord(payloadBytes)
    const encodedControlRecord = encodeNativeProtocolRecord(controlRecord)
    definitions.push({
      id: `binary-control-codec-${String(payloadBytes)}`,
      category: 'binary-control-codec',
      payloadBytes,
      ownership: 'fresh encoded and decoded control-record copies',
      runBatch: iterations => {
        for (let index = 0; index < iterations; index += 1) {
          decodeNativeProtocolRecord(encodeNativeProtocolRecord(controlRecord))
        }
      }
    })
    definitions.push({
      id: `byte-ownership-copy-${String(payloadBytes)}`,
      category: 'byte-ownership-copy',
      payloadBytes,
      ownership: 'one fresh Uint8Array copy per operation',
      runBatch: iterations => {
        for (let index = 0; index < iterations; index += 1) new Uint8Array(payload)
      }
    })
    definitions.push({
      id: `bounded-stream-ingress-${String(payloadBytes)}`,
      category: 'bounded-stream',
      payloadBytes,
      ownership: 'pre-owned payload; stream accounts bounded queue retention without copying bytes',
      runBatch: async iterations => {
        const reservedControlCapacity = 128
        const byteCapacity = Math.max(reservedControlCapacity + 1, iterations * Math.max(payloadBytes, 1) + 128)
        const stream = new CoreBoundedStream(
          {
            itemCapacity: capacity(iterations + 1),
            byteCapacity: capacity(byteCapacity),
            reservedControlCapacity: capacity(reservedControlCapacity)
          },
          'error'
        )
        try {
          for (let index = 0; index < iterations; index += 1) {
            const result = stream.emit(payload, payloadBytes, null, payloadBytes)
            if (!result.accepted) fail('bounded stream benchmark unexpectedly rejected an in-budget value')
          }
        } finally {
          await stream.close()
        }
      }
    })
    definitions.push({
      id: `ipc-copy-${String(payloadBytes)}`,
      category: 'ipc-copy',
      payloadBytes,
      ownership: 'deep serializable snapshot with one owned binary payload copy',
      runBatch: iterations => {
        for (let index = 0; index < iterations; index += 1) {
          snapshotSerializableRecord({ kind: 'notification', payload })
        }
      }
    })
    if (encodedControlRecord.byteLength <= 0) fail('binary control codec produced an empty record')
  }
  const pr8Payload = new Uint8Array(244)
  pr8Payload.fill(0x5a)
  definitions.push({
    id: 'pr8-per-connection-queue-scheduling-fairness',
    category: 'per-connection-queue-scheduling-fairness',
    payloadBytes: 0,
    maximumIterations: 256,
    ownership: 'coordinator-owned queued operation records; no radio or payload retention',
    retainedOwnership: 'coordinator queue entries released after deterministic settlement',
    runBatch: benchmarkPerConnectionQueueScheduling
  })
  definitions.push({
    id: 'pr8-cross-connection-dispatch-admission',
    category: 'cross-connection-dispatch-admission',
    payloadBytes: 0,
    maximumIterations: 256,
    ownership: 'coordinator-owned admission records across two deterministic connection keys',
    retainedOwnership: 'per-connection queue entries released after deterministic settlement',
    runBatch: benchmarkCrossConnectionDispatch
  })
  definitions.push({
    id: 'pr8-write-with-response-operation',
    category: 'write-with-response-operation',
    payloadBytes: 20,
    maximumIterations: 512,
    ownership: 'coordinator-retained 20-byte operation payload until acknowledged',
    retainedOwnership: 'operation payload ownership released by coordinator acknowledgement',
    runBatch: iterations => benchmarkWriteOperation(iterations, 'with-response')
  })
  definitions.push({
    id: 'pr8-write-without-response-readiness',
    category: 'write-without-response-readiness',
    payloadBytes: 20,
    maximumIterations: 512,
    ownership: 'coordinator-owned control admission with deterministic without-response mode metadata',
    retainedOwnership: 'operation payload ownership released by coordinator acknowledgement',
    runBatch: iterations => benchmarkWriteOperation(iterations, 'without-response')
  })
  definitions.push({
    id: 'pr8-notification-sustained-ingress',
    category: 'notification-sustained-ingress',
    payloadBytes: pr8Payload.byteLength,
    maximumIterations: 128,
    ownership: 'stream-owned bounded notification values with drop-oldest overflow accounting',
    retainedOwnership: 'bounded stream retention released on close',
    runBatch: benchmarkNotificationIngress
  })
  definitions.push({
    id: 'pr8-long-write-chunk-planning',
    category: 'long-write-chunk-planning',
    payloadBytes: 4096,
    maximumIterations: 64,
    ownership: 'deterministic feature-registry plan inputs; no write bytes or radio resource',
    retainedOwnership: 'temporary deterministic backend released after planning',
    runBatch: benchmarkLongWritePlanning
  })
  definitions.push({
    id: 'pr8-control-ipc-snapshot',
    category: 'control-ipc-snapshot',
    payloadBytes: pr8Payload.byteLength,
    maximumIterations: 2_048,
    ownership: 'serializable snapshot owns an independent structured control record and byte copy',
    retainedOwnership: 'snapshot result becomes unreachable before the next operation',
    runBatch: iterations => {
      for (let index = 0; index < iterations; index += 1) {
        snapshotSerializableRecord({
          kind: 'connection-control',
          version: 2,
          generation: 'connection-generation-1',
          admission: { state: 'ready', queued: 3, dispatched: 1 },
          readiness: ['write-without-response', 'notifications'],
          payload: pr8Payload
        })
      }
    }
  })
  definitions.push({
    id: 'pr8-bounded-queue-memory-overflow',
    category: 'bounded-queue-memory-overflow',
    payloadBytes: 20,
    maximumIterations: 1_024,
    ownership: 'bounded queue owns four control-budgeted values and one deterministic overflow terminal',
    retainedOwnership: 'queue values and terminal state released on close',
    runBatch: async iterations => {
      const payload = new Uint8Array(20)
      for (let index = 0; index < iterations; index += 1) {
        const stream = new CoreBoundedStream(
          {
            itemCapacity: capacity(4),
            byteCapacity: capacity(4 * payload.byteLength + 128),
            reservedControlCapacity: capacity(128)
          },
          'error'
        )
        try {
          for (let value = 0; value < 5; value += 1) stream.emit(payload, payload.byteLength, null, payload.byteLength)
          const counters = stream.overflowCounters()
          if (counters.droppedItems !== 1 || stream.retainedBytes() > stream.limits.byteCapacity) {
            fail('bounded queue overflow benchmark violated its deterministic budget')
          }
        } finally {
          await stream.close()
        }
      }
    }
  })
  definitions.push({
    id: 'pr8-service-change-invalidation-stream-close',
    category: 'service-change-invalidation-stream-close',
    payloadBytes: 20,
    maximumIterations: 1_024,
    ownership: 'subscription-like bounded stream owns a service-change terminal and its iterator',
    retainedOwnership: 'stream and iterator ownership released after terminal delivery',
    runBatch: benchmarkServiceChangeClose
  })
  definitions.push({
    id: 'pr8-cleanup-10000-operations',
    category: 'cleanup-10000-operations',
    payloadBytes: 1,
    maximumIterations: 1,
    ownership: 'coordinator owns one active and 9,999 queued cancellable operation records',
    retainedOwnership: 'queued payloads and cancellation quarantine released before return',
    runBatch: benchmarkCleanupTenThousand
  })
  definitions.push({
    id: 'trace-redacted-record',
    category: 'trace',
    payloadBytes: 0,
    ownership: 'bounded redacted trace metadata only; no peer, path, or payload retention',
    runBatch: iterations => {
      const recorder = new CoreTraceRecorder(Math.min(iterations + 1, 10_000), 512 * 1024)
      for (let index = 0; index < iterations; index += 1) {
        recorder.record({
          timestamp: index,
          resource: 'operation',
          transition: 'completed',
          operation: 'operation-1',
          cause: null,
          queuedOperations: 0,
          dispatchedOperations: 0,
          quarantinedOperations: 0
        })
      }
      recorder.snapshotDocument()
    }
  })
  return definitions
}

async function createReport(options) {
  const nativeHost = nativeHostSection(options)
  const measurements = []
  for (const definition of benchmarkDefinitions(options.payloadSizes)) {
    measurements.push(await measure(definition, options))
  }
  const report = Object.freeze({
    schema: 'unified-ble-performance-report/v1',
    package: Object.freeze({ name: packageManifest.name, version: packageManifest.version }),
    runtime: Object.freeze({ node: process.version, platform: process.platform, architecture: process.arch }),
    baseline: Object.freeze({
      id: performanceBaselineId,
      scope: deterministicProofLevel,
      proofLevel: deterministicProofLevel,
      nativeHostSeparate: true
    }),
    methodology: Object.freeze({
      clock: 'process.hrtime.bigint',
      samples: options.samples,
      targetMillisecondsPerSample: options.targetMilliseconds,
      maximumIterations,
      payloadSizes: Object.freeze([...options.payloadSizes])
    }),
    nativeHost,
    measurements: Object.freeze(measurements)
  })
  validateReport(report)
  return report
}

function validateReport(report) {
  if (report.measurements.length === 0) fail('performance report has no measurements')
  if (
    report.baseline?.id !== performanceBaselineId ||
    report.baseline.scope !== deterministicProofLevel ||
    report.baseline.proofLevel !== deterministicProofLevel ||
    report.baseline.nativeHostSeparate !== true
  ) {
    fail('performance report has invalid deterministic baseline metadata')
  }
  const ids = new Set()
  for (const measurement of report.measurements) {
    if (ids.has(measurement.id)) fail(`duplicate performance measurement: ${measurement.id}`)
    ids.add(measurement.id)
    if (
      measurement.baselineId !== performanceBaselineId ||
      measurement.proofLevel !== deterministicProofLevel ||
      measurement.ownershipMetadata?.scope !== deterministicProofLevel ||
      measurement.ownershipMetadata?.cleanup !== 'before-return'
    ) {
      fail(`${measurement.id} has invalid deterministic ownership metadata`)
    }
    if (measurement.samples.length !== report.methodology.samples)
      fail(`${measurement.id} has an incomplete sample set`)
    if (
      !Number.isFinite(measurement.summary.p50OperationsPerSecond) ||
      measurement.summary.p50OperationsPerSecond <= 0 ||
      !Number.isFinite(measurement.summary.p50NanosecondsPerOperation) ||
      measurement.summary.p50NanosecondsPerOperation <= 0 ||
      !Number.isFinite(measurement.summary.p95NanosecondsPerOperation) ||
      measurement.summary.p95NanosecondsPerOperation < measurement.summary.p50NanosecondsPerOperation
    ) {
      fail(`${measurement.id} has an invalid median`)
    }
  }
}

async function main() {
  const options = parseArguments(process.argv.slice(2))
  const serialized = `${JSON.stringify(await createReport(options), null, 2)}\n`
  if (options.outputPath === null) {
    process.stdout.write(serialized)
    return
  }
  fs.mkdirSync(path.dirname(options.outputPath), { recursive: true })
  fs.writeFileSync(options.outputPath, serialized)
}

main().catch(error => {
  const message = error instanceof Error ? error.message : 'performance suite failed with a non-Error value'
  process.stderr.write(`[run-performance-suite] ${message}\n`)
  process.exitCode = 1
})
