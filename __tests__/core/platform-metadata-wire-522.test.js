const { contractError } = require('../../src/backend-contract/errors')
const { BleError } = require('../../src/public/errors')
const { decodeOrderedMarkerWriteStatus } = require('../../src/core/ordered-marker-wire')
const {
  encodeNativeContinuationFailure,
  decodeNativeContinuationEnvelope
} = require('../../src/core/native-continuation-envelope')
const { createContinuationRecordingController } = require('../../src/core/continuation-recording')

const entries = [
  { key: 'same', value: { kind: 'int', value: '-9223372036854775808' } },
  { key: '__proto__', value: { kind: 'text', value: 'é' } },
  { key: 'same', value: { kind: 'bool', value: true } },
  { key: 'constructor', value: { kind: 'int', value: '9223372036854775807' } },
  { key: 'prototype', value: { kind: 'text', value: '' } }
]
const nativeFailure = () => ({
  code: 'platform.failure',
  domain: 'gatt',
  operation: 'write',
  detail: null,
  platformDomain: 'native',
  platformCode: 'error',
  platformMessage: 'failed',
  platformMetadata: { same: { type: 'text', value: 'map-only' } },
  platformMetadataEntries: JSON.parse(JSON.stringify(entries))
})
const platform = () => ({
  domain: 'native',
  code: 'error',
  safeMessage: 'failed',
  metadata: { same: 'map-only' },
  metadataEntries: JSON.parse(JSON.stringify(entries))
})
const markerStates = failure => [
  { state: 'rejected', error: failure },
  { state: 'not-admitted', error: failure },
  ...['not-started', 'started', 'settled'].map(state => ({
    state: 'admitted',
    ordinal: '1',
    write: { state, error: failure }
  }))
]
const markerError = status => (status.state === 'admitted' ? status.write.error : status.error)

test('marker errors preserve the owned typed ordered sequence at every failure location', () => {
  for (const template of markerStates(nativeFailure())) {
    const status = JSON.parse(JSON.stringify(template))
    const decoded = markerError(decodeOrderedMarkerWriteStatus(status, 2))
    expect(decoded.platform.metadataEntries).toEqual(entries)
    expect(Object.isFrozen(decoded.platform.metadataEntries)).toBe(true)
    status.state === 'admitted'
      ? status.write.error.platformMetadataEntries.pop()
      : status.error.platformMetadataEntries.pop()
    expect(decoded.platform.metadataEntries).toEqual(entries)
  }
})

test('marker accepts legacy map only without inventing entries and rejects unknown fields', () => {
  const legacy = nativeFailure()
  delete legacy.platformMetadataEntries
  for (const status of markerStates(legacy)) {
    expect(markerError(decodeOrderedMarkerWriteStatus(status)).platform).not.toHaveProperty('metadataEntries')
    expect(() => decodeOrderedMarkerWriteStatus({ ...status, extra: true })).toThrow('protocol.malformed')
  }
  expect(() => decodeOrderedMarkerWriteStatus({ state: 'rejected', error: { ...legacy, extra: true } })).toThrow(
    'protocol.malformed'
  )
})

const invalidEntries = [
  null,
  [{ key: 'x', value: { kind: 'int', value: '-0' } }],
  [{ key: 'x', value: { kind: 'int', value: '9223372036854775808' } }],
  [{ key: 'x', value: { kind: 'bool', value: 1 } }],
  [{ key: 'x', value: { kind: 'text', value: '', extra: true } }],
  Array.from({ length: 1025 }, () => ({ key: '', value: { kind: 'text', value: '' } })),
  [{ key: 'é'.repeat(32769), value: { kind: 'text', value: '' } }],
  Array.from({ length: 17 }, () => ({ key: 'x', value: { kind: 'text', value: 'x'.repeat(65536) } }))
]
test.each(invalidEntries)('marker rejects malformed or over-budget entries %#', bad => {
  expect(() =>
    decodeOrderedMarkerWriteStatus({ state: 'rejected', error: { ...nativeFailure(), platformMetadataEntries: bad } })
  ).toThrow('protocol.malformed')
})

test.each(['backend', 'public'])(
  'continuation %s failure round trips exact sequence without synthesizing legacy entries',
  kind => {
    const original =
      kind === 'backend'
        ? contractError('platform.failure', 'gatt', 'write', platform())
        : new BleError('platform.failure', 'gatt', 'write', { platform: platform() })
    const text = encodeNativeContinuationFailure(original)
    expect(JSON.parse(text).error.platform.metadataEntries).toEqual(entries)
    try {
      decodeNativeContinuationEnvelope(text, 'host')
      throw new Error('expected failure')
    } catch (error) {
      expect(error.normalized.platform.metadataEntries).toEqual(entries)
    }
    const old = platform()
    delete old.metadataEntries
    const encoded = encodeNativeContinuationFailure(contractError('platform.failure', 'gatt', 'write', old))
    expect(JSON.parse(encoded).error.platform).not.toHaveProperty('metadataEntries')
    try {
      decodeNativeContinuationEnvelope(encoded, 'host')
      throw new Error('expected failure')
    } catch (error) {
      expect(error.normalized.platform).not.toHaveProperty('metadataEntries')
    }
  }
)

test.each(invalidEntries)('continuation decoder rejects malformed or over-budget entries %#', bad => {
  const wire = {
    ok: false,
    error: {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'write',
      detail: null,
      platform: { domain: 'native', code: 'error', message: null, metadata: {}, metadataEntries: bad }
    },
    commit: null,
    retryability: 'caller-decides'
  }
  expect(() => decodeNativeContinuationEnvelope(JSON.stringify(wire), 'host')).toThrow(
    /protocol\.malformed|bytes\.too-large/
  )
})

function batch(version, failure) {
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
    record: { kind: 'terminal', reason: { kind: 'source-failed', failure }, tailUncertain: false }
  }
  if (version === 1)
    Object.assign(row.record, {
      rejectedValues: null,
      rejectedLoss: null,
      sourceRejection: null,
      sourceFailureMember: null,
      secondaryFailure: null,
      additionalFailureCount: '0'
    })
  return { token: 'token', records: [row], bytes: Buffer.byteLength(JSON.stringify(row)), more: false }
}
test('prepared v2 preserves entries; v1 and unknown versions reject before any acknowledgement', async () => {
  const failure = nativeFailure()
  failure.platformMetadata = { same: { kind: 'text', value: 'map-only' } }
  let prepared = batch(2, failure)
  const acknowledge = jest.fn()
  const controller = createContinuationRecordingController({ prepare: async () => prepared, acknowledge })
  const result = await controller.prepareOrdered('r', { maxItems: 2, maxBytes: 1048576 })
  expect(result.records[0].record.reason.failure.platformMetadataEntries).toEqual(entries)
  for (const version of [1, 3]) {
    prepared = batch(version, failure)
    await expect(controller.prepareOrdered('r', { maxItems: 2, maxBytes: 1048576 })).rejects.toMatchObject({
      code: 'protocol.malformed'
    })
    expect(acknowledge).not.toHaveBeenCalled()
  }
  prepared = batch(2, { ...failure, platformMetadataEntries: [{ key: 'bad', value: { kind: 'int', value: '-0' } }] })
  await expect(controller.prepareOrdered('r', { maxItems: 2, maxBytes: 1048576 })).rejects.toMatchObject({
    code: 'protocol.malformed'
  })
  expect(acknowledge).not.toHaveBeenCalled()
  const old = { ...failure }
  delete old.platformMetadataEntries
  prepared = batch(1, old)
  expect(
    (await controller.prepareOrdered('r', { maxItems: 2, maxBytes: 1048576 })).records[0].record.reason.failure
  ).not.toHaveProperty('platformMetadataEntries')
  expect(acknowledge).not.toHaveBeenCalled()
})

test('explicitly empty sequences remain present on both boundaries', () => {
  const marker = nativeFailure()
  marker.platformMetadataEntries = []
  expect(decodeOrderedMarkerWriteStatus({ state: 'rejected', error: marker }, 2).error.platform.metadataEntries).toEqual(
    []
  )
  const detail = platform()
  detail.metadataEntries = []
  const encoded = encodeNativeContinuationFailure(contractError('platform.failure', 'gatt', 'write', detail))
  expect(JSON.parse(encoded).error.platform.metadataEntries).toEqual([])
  expect(() => decodeNativeContinuationEnvelope(encoded, 'host')).toThrow()
  try {
    decodeNativeContinuationEnvelope(encoded, 'host')
  } catch (error) {
    expect(error.normalized.platform.metadataEntries).toEqual([])
  }
})

test('continuation encoding rejects an over-budget envelope even when its entries fit their own budget', () => {
  const detail = platform()
  detail.metadata = { padding: 'x'.repeat(2000) }
  detail.metadataEntries = Array.from({ length: 16 }, () => ({
    key: 'x',
    value: { kind: 'text', value: 'x'.repeat(65450) }
  }))
  expect(() => encodeNativeContinuationFailure(contractError('platform.failure', 'gatt', 'write', detail))).toThrow(
    'bytes.too-large'
  )
})
