const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
const { decodeOrderedMarkerWriteStatus } = require('../../src/core/ordered-marker-wire')
const { createContinuationRecordingController } = require('../../src/core/continuation-recording')
const { platformDetailOrThrow } = require('../../src/core/recording-wire')
const { capacity } = require('../../src/backend-contract/primitives')

const failure = () => ({ code: 'platform.failure', domain: 'gatt', operation: 'source', detail: null, platformDomain: 'native', platformCode: '1', platformMessage: null, platformMetadata: {} })
const observation = f => ({ kind: 'failure-observation', failure: f, sourceRejection: null, sourceFailureMember: null, rejectedLoss: null, tailUncertain: false })
const legacyTerminal = f => ({ kind: 'terminal', reason: { kind: 'source-failed', error: f }, uncertainTail: false })
const summaryTerminal = (primary, secondary) => ({ kind: 'terminal', reason: { kind: 'source-failed', error: primary }, uncertainTail: false, failureSummaryVersion: 1, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: secondary, additionalFailureCount: '1', failureCountOverflowed: false, failureObservationsOmitted: false })
function streamFor(record, metadataVersion = 1) {
  const ack = jest.fn(async () => {})
  const stream = new OrderedNativeStream({ itemCapacity: capacity(4), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) }, {
    pollOrderedGroup: async () => ({ deliveryId: '1', record: JSON.stringify(record) }),
    acknowledgeOrderedDelivery: ack,
    status: async () => ({ fenceError: null })
  }, 'p', '1', 'c', 'd', ['m'], metadataVersion)
  return { stream, ack }
}
function recording(record, version = 2) {
  const row = { ordinal: 1, metadata: { kind: 'ordered', version, recordingId: 'r', peerId: 'p', collectionId: '1', sourceGeneration: { kind: 'counter', value: '1' }, deliveryId: '1' }, record }
  const batch = { token: 't', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false }
  const ack = jest.fn()
  const controller = createContinuationRecordingController({ prepare: async () => batch, acknowledge: ack })
  return { controller, ack }
}
const danger = JSON.parse('{"__proto__":{"type":"text","value":"x"},"constructor":{"type":"bool","value":true},"prototype":{"type":"int","value":"1"}}')

test('map-only dangerous keys fail before native delivery acknowledgement', async () => {
  const f = failure(); f.platformMetadata = danger
  const { stream, ack } = streamFor(observation(f))
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow('protocol.malformed')
  expect(ack).not.toHaveBeenCalled()
})

test('map-only dangerous keys fail before a public recording batch can be acknowledged', async () => {
  const f = failure(); f.platformMetadata = JSON.parse('{"__proto__":{"kind":"text","value":"x"},"constructor":{"kind":"bool","value":true},"prototype":{"kind":"int","value":"1"}}')
  const { controller, ack } = recording(observation(f))
  await expect(controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 })).rejects.toThrow('protocol.malformed')
  expect(ack).not.toHaveBeenCalled()
  expect(() => platformDetailOrThrow({ domain: 'n', code: '1', message: null, metadata: JSON.parse('{"__proto__":"x"}') }, 'test')).toThrow('protocol.malformed')
})

test.each(['legacy primary', 'summary primary', 'summary secondary', 'observation'])('native %s validates exact failure fields before acknowledgement', async position => {
  const valid = failure(), invalid = { ...failure(), futureField: true }
  const make = f => position === 'legacy primary' ? legacyTerminal(f) : position === 'summary primary' ? summaryTerminal(f, valid) : position === 'summary secondary' ? summaryTerminal(valid, f) : observation(f)
  const good = streamFor(make(valid))
  expect((await good.stream[Symbol.asyncIterator]().next()).value.kind).toBe(position === 'observation' ? 'failure-observation' : 'terminal')
  expect(good.ack).toHaveBeenCalledTimes(1)
  const bad = streamFor(make(invalid))
  await expect(bad.stream[Symbol.asyncIterator]().next()).rejects.toThrow('protocol.malformed')
  expect(bad.ack).not.toHaveBeenCalled()
})

test('marker errors validate exact failure fields', () => {
  expect(decodeOrderedMarkerWriteStatus({ state: 'rejected', error: failure() }).error.code).toBe('platform.failure')
  expect(() => decodeOrderedMarkerWriteStatus({ state: 'rejected', error: { ...failure(), futureField: true } })).toThrow('protocol.malformed')
})

test('whole failure wrapper size is reported before native acknowledgement', async () => {
  const f = failure()
  f.platformMetadataEntries = Array.from({ length: 16 }, (_, i) => ({ key: String(i), value: { kind: 'text', value: 'x'.repeat(65000) } }))
  f.platformMessage = 'y'.repeat(9000)
  const { stream, ack } = streamFor(observation(f))
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow('bytes.too-large')
  expect(ack).not.toHaveBeenCalled()
})

test('old recording payload remains readable and rejects version-inappropriate or unknown fields', async () => {
  const f = failure()
  const record = { kind: 'terminal', reason: { kind: 'source-failed', failure: f }, tailUncertain: false, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0' }
  const good = recording(record, 1)
  const result = await good.controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 })
  expect(result.records[0].record.reason.failure).not.toHaveProperty('platformMetadataEntries')
  for (const altered of [{ ...f, futureField: true }, { ...f, platformMetadataEntries: [] }]) {
    const bad = recording({ ...record, reason: { kind: 'source-failed', failure: altered } }, 1)
    await expect(bad.controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 })).rejects.toThrow('protocol.malformed')
    expect(bad.ack).not.toHaveBeenCalled()
  }
})

test.each(['primary', 'setup', 'fence', 'cleanup', 'observation', 'legacy secondary'])('recording %s validates versioned failure fields', async position => {
  const f = failure()
  const legacy = position === 'legacy secondary'
  if (!legacy) f.platformMetadataEntries = [{ key: 'k', value: { kind: 'text', value: 'v' } }]
  const recordFor = value => {
    if (position === 'observation') return observation(value)
    if (legacy) return { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: value, additionalFailureCount: '1' }
    const setup = { kind: 'setup-terminal', sourceCreated: false, tailUncertain: false, failure: failure(), setupFailure: null, fenceFailure: null, members: [{ member: '0', acquisition: 'acquired', delivery: 'notification', cleanupReleased: false, cleanupError: null }] }
    if (position === 'primary') setup.failure = value
    if (position === 'setup') setup.setupFailure = value
    if (position === 'fence') setup.fenceFailure = value
    if (position === 'cleanup') setup.members[0].cleanupError = value
    return setup
  }
  // Build the prepared batch with setup-terminal metadata matching the record.
  const prepare = async value => {
    const record = recordFor(value)
    const row = { ordinal: 1, metadata: { kind: 'ordered', version: legacy ? 1 : 2, recordingId: 'r', peerId: 'p', collectionId: '1', sourceGeneration: record.kind === 'setup-terminal' ? null : { kind: 'counter', value: '1' }, deliveryId: record.kind === 'setup-terminal' ? null : '1' }, record }
    const ack = jest.fn()
    const controller = createContinuationRecordingController({ prepare: async () => ({ token: 't', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false }), acknowledge: ack })
    return { result: controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 }), ack }
  }
  const good = await prepare(f)
  await expect(good.result).resolves.toHaveProperty('token', 't')
  const bad = await prepare({ ...f, unknown: true })
  await expect(bad.result).rejects.toThrow('protocol.malformed')
  expect(bad.ack).not.toHaveBeenCalled()
  if (legacy) {
    const incompatible = await prepare({ ...f, platformMetadataEntries: [] })
    await expect(incompatible.result).rejects.toThrow('protocol.malformed')
  }
})

test('oversized mobile platform wrapper is distinguished from malformed input', () => {
  const entries = Array.from({ length: 16 }, (_, i) => ({ key: String(i), value: { kind: 'text', value: 'x'.repeat(65000) } }))
  expect(() => platformDetailOrThrow({ domain: 'n', code: '1', message: 'y'.repeat(9000), metadata: {}, metadataEntries: entries }, 'test')).toThrow('bytes.too-large')
  expect(() => platformDetailOrThrow({ domain: 'n', code: '1', message: null, metadata: {}, future: true }, 'test')).toThrow('protocol.malformed')
})

test('oversized recording failure wrapper is rejected before returning a token', async () => {
  const f = failure()
  f.platformMetadataEntries = Array.from({ length: 16 }, (_, i) => ({ key: String(i), value: { kind: 'text', value: 'x'.repeat(65000) } }))
  f.platformMessage = 'y'.repeat(9000)
  const { controller, ack } = recording(observation(f))
  await expect(controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 })).rejects.toThrow('bytes.too-large')
  expect(ack).not.toHaveBeenCalled()
})

test('shared public platform detail applies the complete wrapper bound', () => {
  const { optionalPlatformErrorDetail } = require('../../src/backend-contract/errors')
  const metadataEntries = Array.from({ length: 16 }, (_, i) => ({ key: String(i), value: { kind: 'text', value: 'x'.repeat(65000) } }))
  expect(() => optionalPlatformErrorDetail({ domain: 'n', code: '1', safeMessage: 'y'.repeat(9000), metadata: {}, metadataEntries }, 'test')).toThrow('bytes.too-large')
})

test.each(['__proto__', 'constructor', 'prototype'])('each map-only dangerous name %s is rejected by live, recording, and mobile paths', async key => {
  const live = failure()
  live.platformMetadata = JSON.parse(`{"${key}":{"type":"text","value":"x"}}`)
  const delivery = streamFor(observation(live))
  await expect(delivery.stream[Symbol.asyncIterator]().next()).rejects.toThrow('protocol.malformed')
  expect(delivery.ack).not.toHaveBeenCalled()
  const durable = failure()
  durable.platformMetadata = JSON.parse(`{"${key}":{"kind":"text","value":"x"}}`)
  const prepared = recording(observation(durable))
  await expect(prepared.controller.prepareOrdered('r', { maxItems: 2, maxBytes: 2000000 })).rejects.toThrow('protocol.malformed')
  expect(prepared.ack).not.toHaveBeenCalled()
  expect(() => platformDetailOrThrow({ domain: 'n', code: '1', message: null, metadata: JSON.parse(`{"${key}":"x"}`) }, 'test')).toThrow('protocol.malformed')
})

test.each(['legacy primary', 'summary primary', 'summary secondary', 'observation'])('native %s preserves a valid extended failure', async position => {
  const extended = { ...failure(), platformMetadataEntries: [{ key: 'k', value: { kind: 'text', value: 'v' } }] }
  const record = position === 'legacy primary' ? legacyTerminal(extended) : position === 'summary primary' ? summaryTerminal(extended, failure()) : position === 'summary secondary' ? summaryTerminal(failure(), extended) : observation(extended)
  const { stream, ack } = streamFor(record, 2)
  const item = (await stream[Symbol.asyncIterator]().next()).value
  const decoded = position === 'summary secondary' ? item.secondaryError : position === 'observation' ? item.failure : item.error
  expect(decoded.platform.metadataEntries).toEqual(extended.platformMetadataEntries)
  expect(ack).toHaveBeenCalledTimes(1)
})
