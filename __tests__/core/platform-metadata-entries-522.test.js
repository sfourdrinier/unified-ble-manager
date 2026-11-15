const {
  optionalPlatformErrorDetail,
  serializeNormalizedError,
  contractError
} = require('../../src/backend-contract/errors')
const { toPublicPlatformErrorDetail, toPublicCleanupRecord } = require('../../src/public/cleanup')
const { rehydratePublicError } = require('../../src/public/error-bridge')
const { decodeOrderedNativeItem } = require('../../src/core/ordered-native-wire')
const { decodeOrderedItem } = require('../../src/ipc/ordered-codec')
const { parseOrderedRecordingBatch } = require('../../src/core/ordered-recording')
const { platformDetailOrThrow, remotePlatformDetail } = require('../../src/core/recording-wire')
const {
  parseDesktopRustCoreLifecyclePlatform,
  desktopRustCoreError
} = require('../../src/backends/desktop/desktop-rust-core-binding')

const entries = [
  { key: 'originalMetadataStart', value: { kind: 'int', value: '9223372036854775807' } },
  { key: 'nsErrorDomain', value: { kind: 'text', value: 'é' } },
  { key: 'nsErrorDomain', value: { kind: 'text', value: 'é' } },
  { key: '__proto__', value: { kind: 'bool', value: true } },
  { key: 'originalMetadataStart', value: { kind: 'int', value: '-9223372036854775808' } }
]
const platform = () => ({
  domain: 'native',
  code: '1',
  safeMessage: 'failure',
  metadata: { originalMetadataStart: '-9223372036854775808' },
  metadataEntries: JSON.parse(JSON.stringify(entries))
})
const rawFailure = () => ({
  code: 'platform.failure',
  domain: 'gatt',
  operation: 'source',
  detail: null,
  platformDomain: 'native',
  platformCode: '1',
  platformMessage: 'failure',
  platformMetadata: {},
  platformMetadataEntries: JSON.parse(JSON.stringify(entries))
})
const observation = failure => ({
  kind: 'failure-observation',
  failure,
  sourceRejection: null,
  sourceFailureMember: null,
  rejectedLoss: null,
  tailUncertain: false
})
const context = {
  collectionId: '1',
  connectionGeneration: 'c',
  databaseGeneration: 'd',
  members: ['m'],
  byteCapacity: 8,
  lastOrdinal: null,
  metadataVersion: 2
}
function batch(failure, version) {
  const row = {
    ordinal: 1,
    metadata: {
      kind: 'ordered',
      version,
      recordingId: 'r',
      peerId: 'p',
      collectionId: '1',
      sourceGeneration: { kind: 'counter', value: '1' },
      deliveryId: '1'
    },
    record: observation(failure)
  }
  return { token: 't', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false }
}

test('public errors and cleanup snapshot ordered duplicate entries independently of the source', () => {
  const source = platform()
  const validated = optionalPlatformErrorDetail(source, 'test')
  const err = contractError('platform.failure', 'gatt', 'source', validated)
  const serialized = serializeNormalizedError(err.normalized)
  const publicError = rehydratePublicError(err)
  const cleanup = toPublicCleanupRecord({
    state: 'release-failed',
    failures: [{ resourceKind: 'source', error: err.normalized }]
  })
  source.metadataEntries[0].value.value = '0'
  for (const detail of [validated, serialized.platform, publicError.platform, cleanup.failures[0].error.platform]) {
    expect(detail.metadataEntries).toEqual(entries)
    expect(Object.isFrozen(detail.metadataEntries)).toBe(true)
    expect(Object.isFrozen(detail.metadataEntries[0].value)).toBe(true)
  }
  expect(toPublicPlatformErrorDetail({ ...platform(), metadataEntries: [] }).metadataEntries).toEqual([])
  expect(
    optionalPlatformErrorDetail({ domain: 'x', code: 'y', safeMessage: '', metadata: {} }, 'test')
  ).not.toHaveProperty('metadataEntries')
})

test('rejects malformed and oversized entries without coercion', () => {
  for (const bad of [
    [{ key: 'a', value: { kind: 'int', value: '-0' } }],
    [{ key: 'a', value: { kind: 'int', value: '9223372036854775808' } }],
    [{ key: 'a', value: { kind: 'int', value: 1 } }],
    [{ key: 'a', value: { kind: 'bool', value: true, extra: 1 } }],
    [{ key: 'a', value: { kind: 'unknown', value: 'x' } }],
    Array.from({ length: 1025 }, () => ({ key: '', value: { kind: 'text', value: '' } })),
    [{ key: 'é'.repeat(32769), value: { kind: 'text', value: '' } }]
  ])
    expect(() => optionalPlatformErrorDetail({ ...platform(), metadataEntries: bad }, 'test')).toThrow()
})

test('native ordered observations survive IPC projection without merging occurrences', () => {
  const item = decodeOrderedNativeItem(observation(rawFailure()), context)
  expect(item.failure.platform.metadataEntries).toEqual(entries)
  expect(decodeOrderedItem(item).failure.platform.metadataEntries).toEqual(entries)
  expect(
    decodeOrderedNativeItem(observation({ ...rawFailure(), platformMetadataEntries: [] }), context).failure.platform
      .metadataEntries
  ).toEqual([])
})

test('v2 recording retains entries while legacy data stays absent and v1 cannot claim entries', () => {
  const v2 = parseOrderedRecordingBatch(batch(rawFailure(), 2), 'r', { maxItems: 2, maxBytes: 1048576 })
  expect(v2.records[0].record.failure.platformMetadataEntries).toEqual(entries)
  const legacy = rawFailure()
  delete legacy.platformMetadataEntries
  expect(
    parseOrderedRecordingBatch(batch(legacy, 2), 'r', { maxItems: 2, maxBytes: 1048576 }).records[0].record.failure
  ).not.toHaveProperty('platformMetadataEntries')
  const row = batch(rawFailure(), 1)
  row.records[0].record = {
    kind: 'terminal',
    reason: { kind: 'source-failed', failure: rawFailure() },
    tailUncertain: false,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  row.bytes = Buffer.byteLength(JSON.stringify(row.records[0]))
  expect(() => parseOrderedRecordingBatch(row, 'r', { maxItems: 2, maxBytes: 1048576 })).toThrow()
})

test('desktop and mobile platform detail decoders retain supplied entries', () => {
  const native = { domain: 'native', code: '1', message: null, metadata: {}, metadataEntries: entries }
  expect(parseDesktopRustCoreLifecyclePlatform(JSON.stringify(native)).metadataEntries).toEqual(entries)
  const wire = `platform.failure|gatt|source|never||${JSON.stringify(native)}|detail`
  expect(desktopRustCoreError(new Error(wire), 'source').normalized.platform.metadataEntries).toEqual(entries)
  const mobile = platformDetailOrThrow(native, 'test')
  expect(
    remotePlatformDetail({ code: 'platform.failure', operation: 'source', detail: null, platform: mobile })
      .metadataEntries
  ).toEqual(entries)
})

test('compatibility maps omit dangerous properties while ordered entries retain them', () => {
  const failure = rawFailure()
  failure.platformMetadata = JSON.parse(
    '{"__proto__":{"type":"text","value":"native"},"constructor":{"type":"bool","value":true},"ok":{"type":"text","value":"kept"}}'
  )
  const decoded = decodeOrderedNativeItem(observation(failure), context).failure.platform
  expect(Object.keys(decoded.metadata)).toEqual(['ok'])
  expect(decoded.metadataEntries).toEqual(entries)
  const invalid = rawFailure()
  invalid.platformMetadata = JSON.parse('{"__proto__":{"type":"unknown","value":"x"}}')
  expect(() => decodeOrderedNativeItem(observation(invalid), context)).toThrow()
  const native = JSON.parse(
    '{"domain":"native","code":"1","message":null,"metadata":{"__proto__":"native","ok":"kept"},"metadataEntries":[]}'
  )
  expect(Object.keys(parseDesktopRustCoreLifecyclePlatform(JSON.stringify(native)).metadata)).toEqual(['ok'])
  expect(Object.keys(platformDetailOrThrow(native, 'test').metadata)).toEqual(['ok'])
})

test('entry budgets include encoded contents and do not accept extra entry fields', () => {
  const tooLarge = Array.from({ length: 20 }, (_, i) => ({
    key: String(i),
    value: { kind: 'text', value: 'x'.repeat(65536) }
  }))
  expect(() => optionalPlatformErrorDetail({ ...platform(), metadataEntries: tooLarge }, 'test')).toThrow()
  expect(() =>
    optionalPlatformErrorDetail(
      { ...platform(), metadataEntries: [{ key: 'a', value: { kind: 'text', value: '' }, extra: 1 }] },
      'test'
    )
  ).toThrow()
})

test('legacy v1 terminal stays readable and v2 setup failures remain distinct', () => {
  const legacy = rawFailure()
  delete legacy.platformMetadataEntries
  const old = batch(legacy, 1)
  old.records[0].record = {
    kind: 'terminal',
    reason: { kind: 'source-failed', failure: legacy },
    tailUncertain: false,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  old.bytes = Buffer.byteLength(JSON.stringify(old.records[0]))
  const oldFailure = parseOrderedRecordingBatch(old, 'r', { maxItems: 2, maxBytes: 1048576 }).records[0].record.reason
    .failure
  expect(oldFailure).not.toHaveProperty('platformMetadataEntries')
  const setup = batch(rawFailure(), 2)
  setup.records[0].metadata.sourceGeneration = null
  setup.records[0].metadata.deliveryId = null
  setup.records[0].record = {
    kind: 'setup-terminal',
    sourceCreated: false,
    tailUncertain: false,
    failure: rawFailure(),
    setupFailure: rawFailure(),
    fenceFailure: null,
    members: [
      {
        member: '0',
        acquisition: 'acquired',
        delivery: 'notification',
        cleanupReleased: false,
        cleanupError: rawFailure()
      }
    ]
  }
  setup.bytes = Buffer.byteLength(JSON.stringify(setup.records[0]))
  const record = parseOrderedRecordingBatch(setup, 'r', { maxItems: 2, maxBytes: 1048576 }).records[0].record
  expect(record.failure.platformMetadataEntries).toEqual(entries)
  expect(record.setupFailure.platformMetadataEntries).toEqual(entries)
  expect(record.members[0].cleanupError.platformMetadataEntries).toEqual(entries)
})
