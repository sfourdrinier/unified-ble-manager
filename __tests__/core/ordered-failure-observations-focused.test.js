const { decodeOrderedNativeItem } = require('../../src/core/ordered-native-wire')
const { decodeOrderedItem, decodeOrderedCleanup } = require('../../src/ipc/ordered-codec')
const { parseOrderedRecordingBatch } = require('../../src/core/ordered-recording')
const { CoreOrderedNotificationStream, accountOrderedDiscard } = require('../../src/core/ordered-notification-stream')
const { capacity } = require('../../src/backend-contract/primitives')

const context = {
  collectionId: '1',
  connectionGeneration: 'c',
  databaseGeneration: 'd',
  members: ['m'],
  byteCapacity: 8,
  lastOrdinal: null
}
const failure = {
  code: 'platform.failure',
  domain: 'gatt',
  operation: 'source',
  detail: null,
  platformDomain: null,
  platformCode: null,
  platformMessage: null,
  platformMetadata: {}
}
const observation = {
  kind: 'failure-observation',
  failure,
  sourceRejection: null,
  sourceFailureMember: null,
  rejectedLoss: null,
  tailUncertain: false
}
const omitted = { kind: 'failure-observations-omitted', count: '2' }
function batch(records, version = 2) {
  const rows = records.map((record, index) => ({
    ordinal: index + 1,
    metadata: {
      kind: 'ordered',
      version,
      recordingId: 'r',
      peerId: 'p',
      collectionId: '1',
      sourceGeneration: { kind: 'counter', value: '1' },
      deliveryId: String(index + 1)
    },
    record
  }))
  return {
    token: 't',
    records: rows,
    bytes: Buffer.byteLength(rows.map(row => JSON.stringify(row)).join('')),
    more: false
  }
}
function decode(records, version = 2) {
  return parseOrderedRecordingBatch(batch(records, version), 'r', { maxItems: 20, maxBytes: 100000 })
}

test('native and IPC keep disjoint and identical observations as separate controls', () => {
  const second = {
    ...observation,
    failure: null,
    sourceRejection: { kind: 'value', bytes: '7' },
    sourceFailureMember: { member: '0', epoch: '3' },
    tailUncertain: true
  }
  const items = [observation, observation, second, omitted].map(item => decodeOrderedNativeItem(item, context))
  expect(items.map(item => decodeOrderedItem(item))).toEqual(items)
  expect(items[0].failure).toMatchObject({ code: 'platform.failure', operation: 'source' })
  expect(items[2]).toMatchObject({ sourceRejection: { kind: 'value', bytes: '7' }, tailUncertain: true })
  const receipt = accountOrderedDiscard(items)
  expect(receipt).toMatchObject({
    values: '0',
    controls: '4',
    failureObservations: items.slice(0, 3),
    failureObservationsOmitted: [items[3]]
  })
  expect(
    decodeOrderedCleanup({ group: { state: 'released', failures: [] }, members: [], discarded: receipt }).discarded
  ).toEqual(receipt)
})

test('malformed observations and omission counts fail closed on every boundary', () => {
  const empty = { ...observation, failure: null }
  for (const item of [
    empty,
    { ...observation, extra: true },
    { ...observation, tailUncertain: 1 },
    { ...omitted, count: '0' },
    { ...omitted, count: '01' },
    { ...omitted, count: undefined }
  ]) {
    expect(() => decodeOrderedNativeItem(item, context)).toThrow()
    expect(() => decode([item])).toThrow()
  }
  const liveEmpty = { ...empty }
  expect(() => decodeOrderedItem(liveEmpty)).toThrow()
  expect(() => decodeOrderedItem({ ...omitted, count: '0' })).toThrow()
  expect(decodeOrderedNativeItem({ ...omitted, count: null }, context).count).toBeNull()
})

test('v2 recording retains duplicate identities and refuses v1 variants or competing v2 terminal summaries', () => {
  const rows = decode([
    observation,
    observation,
    { ...observation, failure: null, tailUncertain: true },
    omitted
  ]).records
  expect(rows.map(row => row.metadata.deliveryId)).toEqual(['1', '2', '3', '4'])
  expect(rows.map(row => row.record.kind)).toEqual([
    'failure-observation',
    'failure-observation',
    'failure-observation',
    'failure-observations-omitted'
  ])
  expect(() => decode([observation], 1)).toThrow()
  const legacy = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  expect(decode([legacy], 1).records[0].record).toEqual(legacy)
  expect(() => decode([legacy], 2)).toThrow()
  expect(decode([{ kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: true }]).records[0].record).toEqual({
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true
  })
})

test('control capacity rejects an observation without inventing a loss or omission', async () => {
  const stream = new CoreOrderedNotificationStream(
    { itemCapacity: capacity(2), byteCapacity: capacity(8), reservedControlCapacity: capacity(1) },
    () => {},
    async () => ({ group: { state: 'released', failures: [] }, members: [] })
  )
  const first = decodeOrderedNativeItem(observation, context)
  expect(stream.emit(first)).toBe(true)
  expect(stream.emit(first)).toBe(false)
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toEqual(first)
  expect(stream.emit(first)).toBe(true)
  expect((await iterator.next()).value).toEqual(first)
  stream.finish({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
  expect((await iterator.next()).value).toEqual({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
})

test('v2 batches reject duplicate source delivery identities without deduplicating payloads', () => {
  const input = batch([observation, observation])
  input.records[1].metadata.deliveryId = input.records[0].metadata.deliveryId
  input.bytes = Buffer.byteLength(input.records.map(row => JSON.stringify(row)).join(''))
  expect(() => parseOrderedRecordingBatch(input, 'r', { maxItems: 20, maxBytes: 100000 })).toThrow()
})

test('native setup retains new controls before later values and acknowledges distinct delivery ids', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const records = [
    observation,
    observation,
    {
      kind: 'value',
      ordinal: '1',
      memberId: '0',
      bytes: [4],
      sourceMembership: { member: '0', epoch: '1' },
      delivery: 'unknown',
      provenance: 'unknown'
    }
  ]
  const acknowledgements = []
  let next = 0
  const transport = {
    pollOrderedGroup: async () => ({ deliveryId: String(next + 1), record: JSON.stringify(records[next++]) }),
    acknowledgeOrderedDelivery: async ({ deliveryId }) => {
      acknowledgements.push(deliveryId)
    }
  }
  const stream = new OrderedNativeStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) },
    transport,
    'p',
    '1',
    'c',
    'd',
    ['m']
  )
  expect(await stream.setupObservations()).toEqual({ observations: [], error: null })
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('failure-observation')
  expect((await iterator.next()).value.kind).toBe('failure-observation')
  expect((await iterator.next()).value.kind).toBe('value')
  expect(acknowledgements).toEqual(['1', '2', '3'])
})

test('native v2 terminal has no competing historical summary or invented omission', () => {
  const terminal = decodeOrderedNativeItem(
    { kind: 'terminal', reason: { kind: 'closed' }, uncertainTail: true },
    context
  )
  expect(terminal).toEqual({ kind: 'terminal', reason: 'closed', uncertainTail: true })
  expect(() =>
    decodeOrderedNativeItem(
      {
        kind: 'terminal',
        reason: { kind: 'closed' },
        uncertainTail: true,
        sourceRejection: { kind: 'barrier', id: '1' }
      },
      context
    )
  ).toThrow()
})

test('IPC observation rejects undefined or extra error fields and keeps live failure separate from durable failure', () => {
  const live = decodeOrderedNativeItem(observation, context)
  expect(() => decodeOrderedItem({ ...live, failure: undefined, tailUncertain: true })).toThrow()
  expect(() => decodeOrderedItem({ ...live, failure: { ...live.failure, extra: 'unexpected' } })).toThrow()
  expect(() => decode([live])).toThrow()
  expect(() => decodeOrderedItem(observation)).toThrow()
  const durable = { ...observation, failure: { ...failure, platformMetadata: { code: { kind: 'int', value: '42' } } } }
  expect(decode([durable]).records[0].record).toEqual(durable)
  expect(() => decodeOrderedNativeItem(durable, context)).toThrow()
})

test('lost native acknowledgement retries the same observation and id without polling ahead', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const { contractError } = require('../../src/backend-contract/errors')
  const poll = jest.fn(async () => ({ deliveryId: '17', record: JSON.stringify(observation) }))
  const ack = jest.fn().mockRejectedValueOnce(new Error('lost reply')).mockResolvedValueOnce(undefined)
  const transport = {
    pollOrderedGroup: poll,
    acknowledgeOrderedDelivery: ack,
    normalizeError: () => contractError('platform.failure', 'gatt', 'ack')
  }
  const stream = new OrderedNativeStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) },
    transport,
    'p',
    '1',
    'c',
    'd',
    ['m']
  )
  const iterator = stream[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toThrow()
  expect((await iterator.next()).value).toMatchObject({ kind: 'failure-observation' })
  expect(poll).toHaveBeenCalledTimes(1)
  expect(ack.mock.calls.map(([arg]) => arg.deliveryId)).toEqual(['17', '17'])
})

test('a malformed native observation is not acknowledged', async () => {
  const { OrderedNativeStream } = require('../../src/core/ordered-native-stream')
  const ack = jest.fn()
  const transport = {
    pollOrderedGroup: async () => ({ deliveryId: '1', record: JSON.stringify({ ...observation, failure: null }) }),
    acknowledgeOrderedDelivery: ack
  }
  const stream = new OrderedNativeStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) },
    transport,
    'p',
    '1',
    'c',
    'd',
    ['m']
  )
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow()
  expect(ack).not.toHaveBeenCalled()
})
