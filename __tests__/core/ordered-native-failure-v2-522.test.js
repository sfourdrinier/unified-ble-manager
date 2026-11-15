const { decodeOrderedNativeItem } = require('../../src/core/ordered-native-wire')
const { decodeOrderedMarkerWriteStatus } = require('../../src/core/ordered-marker-wire')
const { parseOrderedRecordingBatch, parseOrderedRecordingFailure } = require('../../src/core/ordered-recording')

const entries = [
  { key: '__proto__', value: { kind: 'int', value: '-9223372036854775808' } },
  { key: '__proto__', value: { kind: 'int', value: '9223372036854775807' } }
]
const base = () => ({ code: 'platform.failure', domain: 'gatt', operation: 'source', detail: null,
  platformDomain: 'D', platformCode: 'C', platformMessage: null, platformMetadata: {}, platformMetadataEntries: entries })
const extended = () => ({ ...base(), platformCleanup: [{ domain: 'cleanup', code: 'c', message: null,
  metadata: JSON.parse('{"constructor":{"kind":"bool","value":true}}'), metadataEntries: entries,
  cleanup: [{ domain: '', code: '', message: '', metadata: {} }], cleanupFailures: [base()] }],
  platformCleanupFailures: [{ ...base(), platformCleanupFailures: [base()] }] })
const native = f => ({ ...f, platformMetadata: Object.fromEntries(Object.entries(f.platformMetadata).map(([k, v]) => [k, { type: v.kind, value: v.value }])) })
const ctx = { collectionId: '1', connectionGeneration: 'c', databaseGeneration: 'd', members: ['m'], byteCapacity: 8, lastOrdinal: null, metadataVersion: 2 }
const obs = failure => ({ kind: 'failure-observation', failure, sourceRejection: { kind: 'fence', membership: { member: '7', epoch: '9' } }, sourceFailureMember: { member: '3', epoch: '4' }, rejectedLoss: null, tailUncertain: false })
const terminal = failure => ({ kind: 'terminal', reason: { kind: 'source-failed', error: native(failure) }, uncertainTail: false, rejectedValues: null, rejectedLoss: null, sourceRejection: { kind: 'fence', membership: { member: '7', epoch: '9' } }, sourceFailureMember: { member: '3', epoch: '4' }, secondaryFailure: native(failure), additionalFailureCount: '1', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false })
const batch = failure => { const row = { ordinal: 1, metadata: { kind: 'ordered', version: 2, recordingId: 'r', peerId: 'p', collectionId: '1', sourceGeneration: { kind: 'counter', value: '8' }, deliveryId: '10' }, record: obs(failure) }; return { token: 't', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false } }

test('v2 live primary, secondary and observation retain independent nested failures and fence identities', () => {
  const source = extended()
  const t = decodeOrderedNativeItem(terminal(source), ctx)
  expect(t.error.orderedFailure).toEqual(source)
  expect(t.secondaryError.orderedFailure).toEqual(source)
  expect(t.sourceRejection.membership).toEqual({ member: '7', epoch: '9' })
  expect(t.sourceFailureMember).toEqual({ member: '3', epoch: '4' })
  const o = decodeOrderedNativeItem(obs(native(source)), ctx)
  expect(o.failure.orderedFailure).toEqual(source)
  source.platformCleanupFailures[0].code = 'changed'
  expect(o.failure.orderedFailure.platformCleanupFailures[0].code).toBe('platform.failure')
})

test('v2 marker and recording replay preserve the same recursive payload', () => {
  const source = extended()
  const marker = decodeOrderedMarkerWriteStatus({ state: 'admitted', ordinal: '42', write: { state: 'settled', error: native(source) } }, 2)
  expect(marker.write.error.orderedFailure).toEqual(source)
  const result = parseOrderedRecordingBatch(batch(source), 'r', { maxItems: 2, maxBytes: 1048576 })
  expect(result.records[0].record.failure).toEqual(source)
  expect(result.records[0].record.sourceRejection.membership).toEqual({ member: '7', epoch: '9' })
  expect(parseOrderedRecordingBatch(batch(source), 'r', { maxItems: 2, maxBytes: 1048576 })).toEqual(result)
})

test('v1 and unnegotiated readers refuse every extended failure position', () => {
  const f = native(extended())
  expect(() => decodeOrderedNativeItem(obs(f), { ...ctx, metadataVersion: 1 })).toThrow()
  expect(() => decodeOrderedNativeItem(obs(f), { ...ctx, metadataVersion: undefined })).toThrow()
  expect(() => decodeOrderedNativeItem(terminal(extended()), { ...ctx, metadataVersion: 1 })).toThrow()
  expect(() => decodeOrderedMarkerWriteStatus({ state: 'rejected', error: f })).toThrow()
  expect(() => parseOrderedRecordingFailure(extended(), 1)).toThrow()
})

test('rejects malformed, invalid signed integers, excess depth and oversized failures', () => {
  for (const failure of [
    { ...extended(), extra: 1 },
    { ...extended(), platformCleanup: [{ ...extended().platformCleanup[0], unexpected: true }] },
    { ...extended(), platformCleanupFailures: [{ ...base(), platformMetadataEntries: [{ key: 'x', value: { kind: 'int', value: '9223372036854775808' } }] }] },
    { ...extended(), platformCleanupFailures: [{ ...base(), platformMetadataEntries: [{ key: 'x', value: { kind: 'int', value: '-9223372036854775809' } }] }] },
    { ...extended(), platformCleanup: 'bad' },
    { ...extended(), detail: 'x'.repeat(1048577) }
  ]) {
    expect(() => parseOrderedRecordingFailure(failure, 2)).toThrow()
    expect(() => decodeOrderedNativeItem(obs(native(failure)), ctx)).toThrow()
  }
  let deep = base()
  for (let i = 0; i < 33; i++) deep = { ...base(), platformCleanupFailures: [deep] }
  expect(() => parseOrderedRecordingFailure(deep, 2)).toThrow()
})

test('unknown terminal and membership fields are rejected before identities can be discarded', () => {
  const frame = terminal(extended())
  expect(() => decodeOrderedNativeItem({ ...frame, extra: true }, ctx)).toThrow()
  expect(() => decodeOrderedNativeItem({ ...frame, reason: { ...frame.reason, extra: true } }, ctx)).toThrow()
  expect(() => decodeOrderedNativeItem({ ...frame, sourceFailureMember: { member: '3', epoch: '4', extra: true } }, ctx)).toThrow()
  expect(() => decodeOrderedNativeItem({ ...frame, sourceRejection: { kind: 'fence', membership: { member: '7', epoch: '9', extra: true } } }, ctx)).toThrow()
})

test('unnegotiated v2 delivery is retried without a native acknowledgement', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const pollOrderedGroup = jest.fn(async () => ({ deliveryId: '7', record: JSON.stringify(obs(native(extended()))) }))
  const acknowledgeOrderedDelivery = jest.fn(async () => {})
  const stream = new OrderedNativeStream({ itemCapacity: 4, byteCapacity: 4096, reservedControlCapacity: 1 }, { pollOrderedGroup, acknowledgeOrderedDelivery }, 'p', '1', 'c', 'd', ['m'])
  const iterator = stream[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toThrow()
  await expect(iterator.next()).rejects.toThrow()
  expect(pollOrderedGroup).toHaveBeenCalledTimes(2)
  expect(acknowledgeOrderedDelivery).not.toHaveBeenCalled()
})

test('standalone recording failures require an explicit version for extended metadata', () => {
  expect(() => parseOrderedRecordingFailure(extended())).toThrow()
  expect(parseOrderedRecordingFailure(extended(), 2)).toEqual(extended())
})

test('v1 payload retains its legacy normalized shape', () => {
  const old = native(base())
  delete old.platformMetadataEntries
  const value = decodeOrderedNativeItem(obs(old), { ...ctx, metadataVersion: 1 })
  expect(value.failure).not.toHaveProperty('orderedFailure')
  expect(value.failure.platform).toEqual({ domain: 'D', code: 'C', safeMessage: '', metadata: {} })
})

test('setup, fence and member cleanup failures remain independent during v2 replay', () => {
  const input = batch(base())
  input.records[0].metadata.sourceGeneration = null
  input.records[0].metadata.deliveryId = null
  input.records[0].record = { kind: 'setup-terminal', sourceCreated: false, tailUncertain: false,
    failure: extended(), setupFailure: extended(), fenceFailure: extended(),
    members: [{ member: '3', acquisition: 'acquired', delivery: 'unknown', cleanupReleased: false, cleanupError: extended() }] }
  input.bytes = Buffer.byteLength(JSON.stringify(input.records[0]))
  const record = parseOrderedRecordingBatch(input, 'r', { maxItems: 2, maxBytes: 1048576 }).records[0].record
  for (const cause of [record.failure, record.setupFailure, record.fenceFailure, record.members[0].cleanupError]) expect(cause).toEqual(extended())
  expect(record.members[0].member).toBe('3')
})

test('the v2 recursion limit admits depth 32 and refuses depth 33', () => {
  let failure = base()
  for (let i = 0; i < 32; i++) failure = { ...base(), platformCleanupFailures: [failure] }
  expect(() => parseOrderedRecordingFailure(failure, 2)).not.toThrow()
  expect(() => parseOrderedRecordingFailure({ ...base(), platformCleanupFailures: [failure] }, 2)).toThrow()
})

test('entry-only extensions require the negotiated metadata revision', () => {
  const entryOnly = native(base())
  expect(() => decodeOrderedNativeItem(obs(entryOnly), { ...ctx, metadataVersion: 1 })).toThrow()
  expect(() => decodeOrderedMarkerWriteStatus({ state: 'rejected', error: entryOnly })).toThrow()
  expect(decodeOrderedNativeItem(obs(entryOnly), ctx).failure.orderedFailure.platformMetadataEntries).toEqual(entries)
})

test('oversized legacy-shaped failures are rejected before acknowledgement', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const failure = native(base())
  delete failure.platformMetadataEntries
  failure.detail = 'x'.repeat(1048577)
  const pollOrderedGroup = jest.fn(async () => ({ deliveryId: '7', record: JSON.stringify(obs(failure)) }))
  const acknowledgeOrderedDelivery = jest.fn(async () => {})
  const stream = new OrderedNativeStream({ itemCapacity: 4, byteCapacity: 4096, reservedControlCapacity: 1 }, { pollOrderedGroup, acknowledgeOrderedDelivery }, 'p', '1', 'c', 'd', ['m'])
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow()
  expect(acknowledgeOrderedDelivery).not.toHaveBeenCalled()
})


test('v2 native stream delivers the exact nested observation once and acknowledges after validation', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const { capacity } = require('../../src/backend-contract/primitives')
  const original = obs(native(extended()))
  const acknowledgeOrderedDelivery = jest.fn(async () => {})
  const stream = new OrderedNativeStream({ itemCapacity: capacity(4), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) }, {
    pollOrderedGroup: async () => ({ deliveryId: '7', record: JSON.stringify(original) }),
    acknowledgeOrderedDelivery
  }, 'p', '1', 'c', 'd', ['m'], 2)
  const result = (await stream[Symbol.asyncIterator]().next()).value
  expect(result.failure.orderedFailure).toEqual(extended())
  expect(result.sourceRejection).toEqual(original.sourceRejection)
  expect(result.sourceFailureMember).toEqual(original.sourceFailureMember)
  expect(acknowledgeOrderedDelivery).toHaveBeenCalledTimes(1)
})

test('IPC observation preserves nested ordered cleanup and source evidence', () => {
  const { decodeOrderedItem } = require('../../src/ipc/ordered-codec')
  const decoded = decodeOrderedNativeItem(obs(native(extended())), ctx)
  expect(decodeOrderedItem(decoded)).toEqual(decoded)
})
