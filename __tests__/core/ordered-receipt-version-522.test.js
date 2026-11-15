const { parseOrderedRecordingStartResult } = require('../../src/core/ordered-recording-start')
const { parseOrderedRecordingStopReceipt, createContinuationRecordingController } = require('../../src/core/continuation-recording')

const entries = [
  { key: '__proto__', value: { kind: 'text', value: 'first' } },
  { key: 'same', value: { kind: 'int', value: '-2' } },
  { key: 'same', value: { kind: 'bool', value: true } }
]
const source = withEntries => ({
  code: 'platform.failure', domain: 'gatt', operation: 'subscribe', detail: null,
  platformDomain: 'native', platformCode: 'x', platformMessage: null,
  platformMetadata: { same: { kind: 'text', value: 'legacy' } },
  ...(withEntries ? { platformMetadataEntries: entries } : {})
})
const stopFailure = withEntries => ({ kind: 'source', source: source(withEntries) })
const start = (version, withEntries) => ({ kind: 'started', receipt: {
  version, recordingId: 'r', collectionId: '1', state: 'setup-failed', sourceGeneration: null,
  members: [{ memberId: '0', acquisition: 'not-started', delivery: 'unknown', cleanupReleased: false, cleanupError: source(withEntries) }],
  setupFailure: source(withEntries), fenceFailure: source(withEntries), error: stopFailure(withEntries),
  inspectionError: stopFailure(withEntries), driverError: stopFailure(withEntries)
} })
const stop = (version, withEntries) => ({ version, state: 'failed', error: stopFailure(withEntries) })

test('v1 start and stop retain legacy maps without synthesizing entry sequences', () => {
  const receipt = parseOrderedRecordingStartResult(start(1, false), 'r', 1).receipt
  for (const f of [receipt.members[0].cleanupError, receipt.setupFailure, receipt.fenceFailure, receipt.error.source, receipt.inspectionError.source, receipt.driverError.source, parseOrderedRecordingStopReceipt(stop(1, false)).error.source]) {
    expect(f.platformMetadata).toEqual(source(false).platformMetadata)
    expect(f).not.toHaveProperty('platformMetadataEntries')
  }
})

test('v1 rejects v2 entries at every embedded failure position', () => {
  for (const field of ['cleanupError', 'setupFailure', 'fenceFailure', 'error', 'inspectionError', 'driverError']) {
    const value = start(1, false)
    if (field === 'cleanupError') value.receipt.members[0].cleanupError = source(true)
    else value.receipt[field] = ['error', 'inspectionError', 'driverError'].includes(field) ? stopFailure(true) : source(true)
    expect(() => parseOrderedRecordingStartResult(value, 'r', 1)).toThrow('protocol.malformed')
  }
  expect(() => parseOrderedRecordingStopReceipt(stop(1, true))).toThrow('protocol.malformed')
  expect(() => parseOrderedRecordingStopReceipt({ version: 1, state: 'failed', error: { kind: 'storage-cleanup', storage: { kind: 'io', detail: 'x', operation: 'stop', sqliteExtendedCode: null, sqliteCode: null }, source: source(true) } })).toThrow('protocol.malformed')
})

test('v2 requires explicit negotiated version and preserves entries through repeated decoding', () => {
  const initial = start(2, true)
  expect(() => parseOrderedRecordingStartResult(initial, 'r', 1)).toThrow('protocol.malformed')
  const parsed = parseOrderedRecordingStartResult(initial, 'r', 1, 2)
  expect(parsed.receipt.setupFailure.platformMetadataEntries).toEqual(entries)
  expect(parseOrderedRecordingStartResult(parsed, 'r', 1, 2)).toEqual(parsed)
  expect(() => parseOrderedRecordingStopReceipt(stop(2, true))).toThrow('protocol.malformed')
  const stopped = parseOrderedRecordingStopReceipt(stop(2, true), 2)
  expect(stopped.error.source.platformMetadataEntries).toEqual(entries)
  expect(parseOrderedRecordingStopReceipt(stopped, 2)).toEqual(stopped)
  expect(() => parseOrderedRecordingStartResult(start(1, false), 'r', 1, 2)).toThrow('protocol.malformed')
  expect(() => parseOrderedRecordingStopReceipt(stop(1, false), 2)).toThrow('protocol.malformed')
})

test('prepare and owner failures follow the negotiated version, including malformed entries', () => {
  const prepare = (version, withEntries) => ({ kind: 'prepare-failed', receipt: { version, state: 'prepare-failed', recordingId: 'r', sourceStarted: false, journalEffect: 'none', error: stopFailure(withEntries) } })
  const owner = { kind: 'owner-failed', recordingId: 'r', collectionId: '1', error: stopFailure(true) }
  expect(() => parseOrderedRecordingStartResult(prepare(1, true), 'r', 1)).toThrow('protocol.malformed')
  expect(parseOrderedRecordingStartResult(prepare(2, true), 'r', 1, 2).receipt.error.source.platformMetadataEntries).toEqual(entries)
  expect(() => parseOrderedRecordingStartResult(owner, 'r', 1)).toThrow('protocol.malformed')
  expect(parseOrderedRecordingStartResult(owner, 'r', 1, 2).error.source.platformMetadataEntries).toEqual(entries)
  const bad = start(2, true)
  bad.receipt.setupFailure.platformMetadataEntries = [{ key: 'x', value: { kind: 'int', value: '-0' } }]
  expect(() => parseOrderedRecordingStartResult(bad, 'r', 1, 2)).toThrow('protocol.malformed')
  expect(() => parseOrderedRecordingStopReceipt(stop(3, false), 2)).toThrow('protocol.malformed')
})

test('controller rejects an unsupported receipt and never acknowledges or clears it', async () => {
  const acknowledge = jest.fn(), clear = jest.fn()
  const controller = createContinuationRecordingController({ orderedReceiptVersion: async () => 1, stopOrdered: async () => ({ recording: stop(2, true), hostCleanup: { state: 'pending' } }), acknowledge, clear })
  await expect(controller.stopOrdered('r')).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(acknowledge).not.toHaveBeenCalled()
  expect(clear).not.toHaveBeenCalled()
})

test('v2 storage-cleanup source and host cleanup failures survive independent snapshots', () => {
  const storage = { kind: 'io', detail: 'x', operation: 'stop', sqliteExtendedCode: null, sqliteCode: null }
  const input = { version: 2, state: 'failed', error: { kind: 'storage-cleanup', storage, source: source(true) } }
  const first = parseOrderedRecordingStopReceipt(input, 2)
  input.error.source.platformMetadataEntries = []
  expect(first.error.source.platformMetadataEntries).toEqual(entries)
  expect(parseOrderedRecordingStopReceipt(JSON.parse(JSON.stringify(first)), 2)).toEqual(first)

  const { parseOrderedRecordingHostCleanup } = require('../../src/core/continuation-recording')
  const cleanup = { state: 'release-failed', failures: [{ resourceKind: 'lease', error: {
    code: 'platform.failure', domain: 'cleanup', operation: 'release', retryability: 'caller-decides',
    platform: { domain: 'native', code: 'x', safeMessage: 'failed', metadata: {}, metadataEntries: entries }
  } }] }
  const snapshot = parseOrderedRecordingHostCleanup(cleanup)
  expect(parseOrderedRecordingHostCleanup(JSON.parse(JSON.stringify(snapshot)))).toEqual(snapshot)
  expect(snapshot.failures[0].error.platform.metadataEntries).toEqual(entries)
})

test('v2 failed stop retains both the negotiated failure observation and retryable host cleanup', async () => {
  const hostFailure = {
    code: 'platform.failure',
    domain: 'cleanup',
    operation: 'release',
    retryability: 'caller-decides',
    platform: { domain: 'native', code: 'release-failed', safeMessage: 'failed', metadata: {}, metadataEntries: entries }
  }
  const controller = createContinuationRecordingController({
    orderedReceiptVersion: async () => 2,
    stopOrdered: async () => ({
      recording: stop(2, true),
      hostCleanup: { state: 'release-failed', failures: [{ resourceKind: 'ordered-recording', error: hostFailure }] }
    }),
    acknowledge: jest.fn(),
    clear: jest.fn()
  })

  await expect(controller.stopOrdered('r')).resolves.toMatchObject({
    recording: { version: 2, state: 'failed', error: { source: { platformMetadataEntries: entries } } },
    hostCleanup: { state: 'release-failed', failures: [{ error: { platform: { metadataEntries: entries } } }] }
  })
})

test('v2 also accepts map-only failures and rejects unknown or mismatched versions', () => {
  expect(parseOrderedRecordingStartResult(start(2, false), 'r', 1, 2).receipt.setupFailure).not.toHaveProperty('platformMetadataEntries')
  expect(parseOrderedRecordingStopReceipt(stop(2, false), 2).error.source).not.toHaveProperty('platformMetadataEntries')
  for (const version of [0, 3, null]) {
    expect(() => parseOrderedRecordingStartResult(start(version, false), 'r', 1, 2)).toThrow('protocol.malformed')
    expect(() => parseOrderedRecordingStopReceipt(stop(version, false), 2)).toThrow('protocol.malformed')
    expect(() => parseOrderedRecordingStartResult(start(1, false), 'r', 1, version)).toThrow('protocol.malformed')
    expect(() => parseOrderedRecordingStopReceipt(stop(1, false), version)).toThrow('protocol.malformed')
  }
})

test('mobile v1-only controls reject advertised v2 before native stop', async () => {
  const { createSessionOrderedRecordingControls } = require('../../src/backends/reactnative/react-native-ordered-recording-controls')
  const native = {
    orderedRecordingVersion: async () => 2,
    orderedRecordingStart: jest.fn(), orderedRecordingReserve: jest.fn(),
    orderedRecordingStartStatus: jest.fn(), orderedRecordingRetire: jest.fn(),
    orderedRecordingContext: jest.fn(), orderedRecordingStop: jest.fn()
  }
  await expect(createSessionOrderedRecordingControls(native).stop('r')).rejects.toThrow('protocol.incompatible')
  expect(native.orderedRecordingStop).not.toHaveBeenCalled()
})
