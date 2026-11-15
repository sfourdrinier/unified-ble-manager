const { desktopRustCoreError } = require('../../../src/backends/desktop/desktop-rust-core-binding')
const { toPublicNormalizedError, toPublicPlatformErrorDetail } = require('../../../src/public/cleanup')
const { DesktopOrderedStream } = require('../../../src/backends/desktop/desktop-ordered-group')
const { capacity } = require('../../../src/backend-contract/primitives')
const { createContinuationRecordingController } = require('../../../src/core/continuation-recording')

// This fixture represents the exact ordered sequence supplied by a native producer.
// In particular the final originalMetadataStart is native data, not the wrapper marker.
const entries = () => [
  { key: 'originalMetadataStart', value: { kind: 'text', value: '3' } },
  { key: 'originalMetadataCount', value: { kind: 'text', value: '8' } },
  { key: 'mechanism', value: { kind: 'text', value: 'peer-source' } },
  { key: 'nsErrorDomain', value: { kind: 'text', value: 'first' } },
  { key: 'nsErrorDomain', value: { kind: 'text', value: 'second' } },
  { key: 'nsErrorDomain', value: { kind: 'text', value: 'second' } },
  { key: 'originalMetadataStart', value: { kind: 'text', value: 'native-collision' } },
  { key: '__proto__', value: { kind: 'text', value: 'prototype data' } },
  { key: 'constructor', value: { kind: 'bool', value: true } },
  { key: 'prototype', value: { kind: 'int', value: '-9223372036854775808' } },
  { key: 'nativeText', value: { kind: 'text', value: 'é "雪"' } }
]
const metadata = { originalMetadataStart: 'native-collision', originalMetadataCount: '8', mechanism: 'peer-source', nsErrorDomain: 'second', nativeText: 'é "雪"' }
const failure = () => ({ code: 'platform.failure', domain: 'platform', operation: 'peer.source', detail: 'source failed', platformDomain: 'corebluetooth', platformCode: 'overflow', platformMessage: 'source failed', platformMetadata: Object.fromEntries(Object.entries(metadata).map(([key, value]) => [key, { type: 'text', value }])), platformMetadataEntries: entries() })
const recordFailure = () => ({ ...failure(), platformMetadata: Object.fromEntries(Object.entries(metadata).map(([key, value]) => [key, { kind: 'text', value }])) })
const status = async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: false, cleanupError: null }], sourceGeneration: null, cleanupComplete: false, setupError: null, fenceError: null })
const terminal = () => ({ kind: 'terminal', reason: { kind: 'source-failed', error: failure() }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: { member: '0', epoch: '9' }, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false })

test('ordinary desktop rejection retains duplicate ordered native metadata through public error projection', () => {
  const platform = { domain: 'corebluetooth', code: 'overflow', message: 'source failed', metadata, metadataEntries: entries() }
  const wire = `platform.failure|platform|peer.source|never||${JSON.stringify(platform)}|source failed`
  const error = toPublicNormalizedError(desktopRustCoreError(new Error(wire), 'gatt.subscribe').normalized)
  expect(error).toMatchObject({ code: 'platform.failure', domain: 'platform', operation: 'gatt.subscribe', platform: { domain: 'corebluetooth', code: 'overflow', metadata, metadataEntries: entries() } })
  expect(Object.hasOwn(error.platform.metadata, '__proto__')).toBe(false)
})

test('public platform detail copies and freezes ordered entries while retaining dangerous names as values', () => {
  const input = { domain: 'corebluetooth', code: 'overflow', safeMessage: 'source failed', metadata: { ...metadata }, metadataEntries: entries() }
  const output = toPublicPlatformErrorDetail(input)
  expect(output.metadataEntries).toEqual(entries())
  expect(output.metadataEntries).not.toBe(input.metadataEntries)
  expect(Object.isFrozen(output.metadataEntries)).toBe(true)
  expect(Object.isFrozen(output.metadataEntries[0])).toBe(true)
  expect(Object.isFrozen(output.metadataEntries[0].value)).toBe(true)
  input.metadataEntries[0].value.value = 'changed'
  expect(output.metadataEntries[0].value.value).toBe('3')
  expect(Object.getPrototypeOf(output.metadata)).toBe(null)
})

test('old platform data does not invent ordered entries', () => {
  const old = toPublicPlatformErrorDetail({ domain: 'native', code: 'old', safeMessage: '', metadata: { x: 'last' } })
  expect(Object.hasOwn(old, 'metadataEntries')).toBe(false)
})

test('explicitly empty metadata entries remain distinct from an absent sequence', () => {
  const empty = toPublicPlatformErrorDetail({ domain: 'native', code: 'empty', safeMessage: '', metadata: {}, metadataEntries: [] })
  expect(empty.metadataEntries).toEqual([])
})

test('desktop ordered terminal keeps source membership and entries across a lost delivery ACK', async () => {
  const poll = jest.fn(async () => ({ deliveryId: '7', record: JSON.stringify(terminal()) }))
  let attempts = 0
  const acknowledge = jest.fn(async () => { if (++attempts === 1) throw new Error('platform.transport|platform|native.ack|never|||response lost') })
  const limits = { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) }
  const iterator = new DesktopOrderedStream(limits, { pollOrderedGroup: poll, acknowledgeOrderedDelivery: acknowledge, orderedGroupStatus: status }, 'peer', '4', 'connection', 'database', ['heart'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'platform.transport' } })
  const item = (await iterator.next()).value
  expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed', sourceFailureMember: { member: '0', epoch: '9' }, error: { code: 'platform.failure', platform: { metadata, metadataEntries: entries() } } })
  expect(poll).toHaveBeenCalledTimes(1)
  expect(acknowledge).toHaveBeenCalledTimes(2)
})

function page(version, withEntries) {
  const cause = recordFailure()
  if (!withEntries) delete cause.platformMetadataEntries
  const row = { ordinal: 1, metadata: { kind: 'ordered', version, recordingId: 'sample', peerId: 'peer', collectionId: '4', sourceGeneration: { kind: 'counter', value: '1' }, deliveryId: '7' }, record: { kind: 'terminal', reason: { kind: 'source-failed', failure: cause }, tailUncertain: true } }
  return { token: 'token-1', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false }
}
function recordingAccess(batch) {
  let acked = false
  return { prepare: jest.fn(async () => acked ? { token: null, records: [], bytes: 0, more: false } : JSON.parse(JSON.stringify(batch))), acknowledge: jest.fn(async (id, token) => { acked = true; return { token, acknowledged: true, records: 1 } }) }
}

test('ordered recording replay retains duplicate entries and identity until explicit ACK', async () => {
  // Version 2 is the unreleased format shared with failure observations.
  const access = recordingAccess(page(2, true))
  const controller = createContinuationRecordingController(access)
  const first = await controller.prepareOrdered('sample', { maxItems: 4, maxBytes: 16384 })
  const replay = await controller.prepareOrdered('sample', { maxItems: 4, maxBytes: 16384 })
  expect(replay).toEqual(first)
  expect(first.records[0]).toMatchObject({ ordinal: 1, metadata: { deliveryId: '7' }, record: { reason: { kind: 'source-failed', failure: { platformMetadataEntries: entries() } } } })
  expect(access.acknowledge).not.toHaveBeenCalled()
  await controller.acknowledge('sample', first.token)
  expect((await controller.prepareOrdered('sample', { maxItems: 4, maxBytes: 16384 })).records).toEqual([])
})

test('old ordered recording replay never synthesizes a sequence from its compatibility map', async () => {
  const controller = createContinuationRecordingController(recordingAccess(page(2, false)))
  const result = await controller.prepareOrdered('sample', { maxItems: 4, maxBytes: 16384 })
  const cause = result.records[0].record.reason.failure
  expect(cause.platformMetadata.nsErrorDomain).toEqual({ kind: 'text', value: 'second' })
  expect(Object.hasOwn(cause, 'platformMetadataEntries')).toBe(false)
})


test.each([
  [{ key: 'n', value: { kind: 'int', value: '9223372036854775808' } }],
  [{ key: 'n', value: { kind: 'int', value: '-0' } }],
  [{ key: 'n', value: { kind: 'text', value: 1 } }],
  [{ key: 'n', value: { kind: 'unknown', value: '1' } }],
  [{ key: 'n', value: { kind: 'text', value: 'a', extra: true } }]
])('public projection rejects malformed ordered metadata entries %#', invalid => {
  expect(() => toPublicPlatformErrorDetail({ domain: 'native', code: 'error', safeMessage: '', metadata: {}, metadataEntries: invalid })).toThrow()
})

test('malformed ordered metadata is not acknowledged after a valid extended frame is accepted', async () => {
  const limits = { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) }
  const makeIterator = (frame, acknowledge) => new DesktopOrderedStream(limits, {
    pollOrderedGroup: async () => ({ deliveryId: '7', record: JSON.stringify(frame) }),
    acknowledgeOrderedDelivery: acknowledge, orderedGroupStatus: status
  }, 'peer', '4', 'connection', 'database', ['heart'])[Symbol.asyncIterator]()
  const validAck = jest.fn(async () => {})
  expect((await makeIterator(terminal(), validAck).next()).value.error.platform.metadataEntries).toEqual(entries())
  expect(validAck).toHaveBeenCalledTimes(1)
  const frame = terminal()
  frame.reason.error.platformMetadataEntries[0].value = { kind: 'int', value: '01' }
  const invalidAck = jest.fn(async () => {})
  await expect(makeIterator(frame, invalidAck).next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(invalidAck).not.toHaveBeenCalled()
})


test('legacy ordered terminal fixture preserves membership and retries a lost ACK', async () => {
  const frame = terminal()
  delete frame.reason.error.platformMetadataEntries
  const poll = jest.fn(async () => ({ deliveryId: '7', record: JSON.stringify(frame) }))
  let attempts = 0
  const central = { pollOrderedGroup: poll, orderedGroupStatus: status, acknowledgeOrderedDelivery: async () => { if (++attempts === 1) throw new Error('platform.transport|platform|native.ack|never|||response lost') } }
  const limits = { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['heart'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'platform.transport' } })
  const item = (await iterator.next()).value
  expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed', sourceFailureMember: { member: '0', epoch: '9' }, error: { platform: { metadata } } })
  expect(Object.hasOwn(item.error.platform, 'metadataEntries')).toBe(false)
  expect(poll).toHaveBeenCalledTimes(1)
  expect(attempts).toBe(2)
})

test('version 1 recording replay also leaves ordered metadata absent', async () => {
  const batch = page(1, false)
  Object.assign(batch.records[0].record, { rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0' })
  batch.bytes = Buffer.byteLength(JSON.stringify(batch.records[0]))
  const controller = createContinuationRecordingController(recordingAccess(batch))
  const result = await controller.prepareOrdered('sample', { maxItems: 4, maxBytes: 16384 })
  expect(Object.hasOwn(result.records[0].record.reason.failure, 'platformMetadataEntries')).toBe(false)
})
