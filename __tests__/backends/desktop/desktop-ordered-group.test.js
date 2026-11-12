// __tests__/backends/desktop/desktop-ordered-group.test.js
const { capacity } = require('../../../src/backend-contract/primitives')
const { DesktopOrderedStream } = require('../../../src/backends/desktop/desktop-ordered-group')

const limits = { itemCapacity: capacity(4), byteCapacity: capacity(100), reservedControlCapacity: capacity(1) }
const delivery = record => ({ deliveryId: record.kind === 'empty' ? null : '1', record: JSON.stringify(record) })
const status = async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: false, cleanupError: null }], sourceGeneration: null, cleanupComplete: false, setupError: null, fenceError: null })
const value = (ordinal, memberId, bytes) => ({ kind: 'value', ordinal, memberId, bytes, sourceMembership: { member: memberId, epoch: '1' }, delivery: 'unknown', provenance: 'unknown' })

test('capacity changes use the versioned route and exact source generation', async () => {
  const increase = jest.fn(async options => ({ previousMaxBytes: 100, maxBytes: options.maxBytes, maximumMaxBytes: 4194304 }))
  const central = { orderedCapacityWireVersion: () => 1, increaseOrderedByteCapacity: increase }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  stream.setSourceGeneration({ kind: 'counter', value: '17' })
  expect(await stream.increaseByteCapacity(200)).toEqual({ previousMaxBytes: 100, maxBytes: 200, maximumMaxBytes: 4194304 })
  expect(increase).toHaveBeenCalledWith({ wireVersion: 1, peerId: 'peer', collectionId: '4', sourceGeneration: { kind: 'counter', value: '17' }, maxBytes: 200 })
  expect(stream.limits.byteCapacity).toBe(200)
})

test('capacity changes fail closed for an incompatible native route', async () => {
  const central = { orderedCapacityWireVersion: () => 2, increaseOrderedByteCapacity: jest.fn() }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  stream.setSourceGeneration({ kind: 'counter', value: '17' })
  await expect(stream.increaseByteCapacity(200)).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(central.increaseOrderedByteCapacity).not.toHaveBeenCalled()
})

test('setup and cleanup status preserve v2 failure entries and nested cleanup', () => {
  const { parseOrderedStatus } = require('../../../src/backends/desktop/desktop-ordered-group')
  const failure = { code: 'platform.failure', domain: 'gatt', operation: 'setup', detail: null,
    platformDomain: 'native', platformCode: 'failed', platformMessage: null, platformMetadata: {},
    platformMetadataEntries: [{ key: 'same', value: { kind: 'int', value: '-9223372036854775808' } }, { key: 'same', value: { kind: 'text', value: 'second' } }],
    platformCleanup: [{ domain: 'cleanup', code: 'refused', message: null, metadata: {} }] }
  const parsed = parseOrderedStatus(JSON.stringify({ members: [{ memberId: '0', acquisition: 'indeterminate', effectiveDelivery: 'unknown', cleanupReleased: false, cleanupError: failure }], sourceGeneration: null, cleanupComplete: false, setupError: failure, fenceError: failure }), ['a'])
  expect(parsed.setupError.orderedFailure.platformMetadataEntries).toEqual(failure.platformMetadataEntries)
  expect(parsed.members[0].cleanupError.orderedFailure.platformCleanup[0].code).toBe('refused')
  expect(parsed.fenceError.orderedFailure).toEqual(parsed.setupError.orderedFailure)
})

test('the native FIFO preserves full-width ordinals, identities and owned bytes', async () => {
  const records = [value('9007199254740993', '0', [1]), value('18446744073709551615', '1', [2])]
  const central = { pollOrderedGroup: async () => delivery(records.shift()), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a', 'b'])
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ ordinal: '9007199254740993', memberId: 'a', value: Uint8Array.from([1]) })
  expect((await iterator.next()).value).toMatchObject({ ordinal: '18446744073709551615', memberId: 'b', value: Uint8Array.from([2]) })
})

test('the native FIFO rejects a repeated ordinal instead of inventing order', async () => {
  const records = [value('5', '0', [1]), value('5', '1', [2])]
  const central = { pollOrderedGroup: async () => delivery(records.shift()), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a', 'b'])[Symbol.asyncIterator]()
  await iterator.next()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('terminal retains a secondary failure and rejected source evidence', async () => {
  const records = [
    { kind: 'terminal', reason: { kind: 'service-changed' }, uncertainTail: true, rejectedValues: null,
      rejectedLoss: null, sourceRejection: { kind: 'barrier', id: '18446744073709551615' }, sourceFailureMember: null,
      secondaryFailure: { code: 'platform.failure', domain: 'platform', operation: 'bridge.control', detail: 'failure', platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} }, additionalFailureCount: '2', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }
  ]
  const central = { pollOrderedGroup: async () => delivery(records.shift()), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'service-changed', sourceRejection: { kind: 'barrier', id: '18446744073709551615' }, secondaryError: { code: 'platform.failure', operation: 'bridge.control' }, additionalFailureCount: '2', failureCountOverflowed: false, failureObservationsOmitted: false })
})

test('failed setup can report initial observations and retain later values until terminal', async () => {
  const records = [value('1', '0', [1])]
  const central = { pollOrderedGroup: async () => delivery(records.shift() ?? { kind: 'empty' }), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  expect(await stream.setupObservations()).toMatchObject({ observations: [{ kind: 'value', ordinal: '1' }], error: null })
  records.push(value('2', '0', [2]), { kind: 'terminal', reason: { kind: 'closed' }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false })
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '2' })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed' })
  expect((await iterator.next()).done).toBe(true)
})

test('member cleanup failure retains its structured cause until a successful retry', async () => {
  let attempts = 0
  const central = {
    closeOrderedGroup: async () => { attempts += 1; return attempts === 1 ? 'pending-source' : 'released' },
    orderedGroupStatus: async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: attempts > 1, cleanupError: attempts > 1 ? null : 'gatt.subscribe-failed|gatt|gatt.ordered.cleanup|never|||physical release failed' }], sourceGeneration: null, cleanupComplete: attempts > 1, setupError: null, fenceError: null }),
    acknowledgeOrderedGroup: async () => {}, forgetOrderedGroup: async () => {}
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  const failed = await stream.close()
  expect(failed.members[0].cleanup).toMatchObject({ state: 'release-failed', failures: [{ error: { code: 'gatt.subscribe-failed', domain: 'gatt' } }] })
  const released = await stream.close()
  expect(released.group.state).toBe('released')
  expect(released.members[0].cleanup).toEqual({ state: 'released', failures: [] })
})

test('discard accounts retained native values and loss before acknowledging cleanup', async () => {
  const records = [value('1', '0', [1, 2]), { kind: 'loss', loss: { scope: { kind: 'unknown' }, unit: 'source-records', count: '9', bytes: null } }, { kind: 'terminal', reason: { kind: 'closed' }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }]
  let attempts = 0
  let acknowledged = false
  const central = {
    pollOrderedGroup: async () => delivery(records.shift() ?? { kind: 'empty' }),
    acknowledgeOrderedDelivery: async () => {},
    closeOrderedGroup: async () => { attempts += 1; return attempts === 1 ? 'pending-drain' : 'released' },
    orderedGroupStatus: async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: true, cleanupError: null }], sourceGeneration: null, cleanupComplete: true, setupError: null, fenceError: null }),
    acknowledgeOrderedGroup: async () => { expect(records).toHaveLength(0); acknowledged = true }, forgetOrderedGroup: async () => {}
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  const result = await stream.discard()
  expect(acknowledged).toBe(true)
  expect(result.discarded).toMatchObject({ values: '1', bytes: '2', controls: '2', uncertainTail: true, terminal: { reason: 'closed' }, losses: [{ lostSourceRecords: '9' }] })
  expect(await stream.close()).toEqual(result)
})

test('a transport error stays structured and the stream can retry', async () => {
  let calls = 0
  const central = { pollOrderedGroup: async () => { calls += 1; if (calls === 1) throw new Error('platform.transport|platform|native.poll|never|||channel unavailable'); return delivery(value('1', '0', [4])) }, acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'platform.transport', domain: 'platform' } })
  expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: Uint8Array.from([4]) })
})

test('host reset retains its structured cause without claiming physical link loss', async () => {
  const frame = { kind: 'terminal', reason: { kind: 'host-reset', error: { code: 'backend.reset', domain: 'core', operation: 'host.reset', detail: 'source overflow', platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} } }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }
  const central = { pollOrderedGroup: async () => delivery(frame), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'host-reset', error: { code: 'backend.reset', domain: 'core' } })
})

test('a lost delivery acknowledgement retries the same owned record', async () => {
  let acknowledgements = 0
  const poll = jest.fn(async () => delivery(value('1', '0', [7])))
  const central = { pollOrderedGroup: poll, acknowledgeOrderedDelivery: async () => { acknowledgements += 1; if (acknowledgements === 1) throw new Error('platform.transport|platform|native.ack|never|||response lost') } }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'platform.transport' } })
  expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: Uint8Array.from([7]) })
  expect(poll).toHaveBeenCalledTimes(1)
  expect(acknowledgements).toBe(2)
})

test('a malformed native record is not acknowledged', async () => {
  const acknowledge = jest.fn(async () => {})
  const central = { pollOrderedGroup: async () => delivery(value('01', '0', [1])), acknowledgeOrderedDelivery: acknowledge }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect(acknowledge).not.toHaveBeenCalled()
})

test('explicit discard settles a malformed native value using retained typed evidence', async () => {
  let settled = false
  let closeCalls = 0
  const deliveryAcks = []
  const central = {
    pollOrderedGroup: async () => settled ? { deliveryId: '2', record: JSON.stringify({ kind: 'terminal', reason: { kind: 'closed' }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }) } : { deliveryId: '1', record: '{bad json' },
    acknowledgeOrderedDelivery: async ({ deliveryId }) => { deliveryAcks.push(deliveryId) },
    closeOrderedGroup: async () => { closeCalls += 1; return closeCalls >= 3 ? 'released' : 'pending-drain' },
    orderedGroupStatus: async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: closeCalls >= 3, cleanupError: null }], sourceGeneration: null, cleanupComplete: closeCalls >= 3, setupError: null, fenceError: null }),
    pollOrderedDiscardReceipt: async () => null,
    discardOrderedDelivery: async ({ deliveryId }) => ({ receiptId: '3', discardedDeliveryId: deliveryId, evidence: JSON.stringify({ kind: 'value', memberId: '0', ordinal: '1', bytes: '4', sourceMembership: { member: '0', epoch: '1' } }) }),
    acknowledgeOrderedDiscardReceipt: async ({ deliveryId }) => { expect(deliveryId).toBe('3'); settled = true },
    acknowledgeOrderedGroup: async () => {}, forgetOrderedGroup: async () => {}
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  const iterator = stream[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  const result = await stream.discard()
  expect(result).toMatchObject({ group: { state: 'released' }, discarded: { values: '1', bytes: '4', controls: '1', terminal: { reason: 'closed' }, trustedValues: [{ memberId: 'a', ordinal: '1', sourceMembership: { member: '0', epoch: '1' }, bytes: '4' }] } })
  expect(deliveryAcks).toEqual(['2'])
})

test('lost discard response replays the retained receipt before its acknowledgement', async () => {
  let receipt = null
  let settled = false
  let terminalAcked = false
  let discardCalls = 0
  const central = {
    pollOrderedGroup: async () => settled ? { deliveryId: '2', record: JSON.stringify({ kind: 'terminal', reason: { kind: 'closed' }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }) } : { deliveryId: '1', record: '{bad json' },
    acknowledgeOrderedDelivery: async () => { terminalAcked = true },
    closeOrderedGroup: async () => terminalAcked ? 'released' : 'pending-drain',
    orderedGroupStatus: async () => JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: terminalAcked, cleanupError: null }], sourceGeneration: null, cleanupComplete: terminalAcked, setupError: null, fenceError: null }),
    pollOrderedDiscardReceipt: async () => receipt,
    discardOrderedDelivery: async () => { discardCalls += 1; receipt = { receiptId: '3', discardedDeliveryId: '1', evidence: JSON.stringify({ kind: 'value', memberId: '0', ordinal: '1', bytes: '4', sourceMembership: { member: '0', epoch: '1' } }) }; throw new Error('platform.transport|platform|discard|never|||response lost') },
    acknowledgeOrderedDiscardReceipt: async () => { receipt = null; settled = true },
    acknowledgeOrderedGroup: async () => {}, forgetOrderedGroup: async () => {}
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  expect((await stream.discard()).group.state).toBe('release-failed')
  const done = await stream.discard()
  expect(done).toMatchObject({ group: { state: 'released' }, discarded: { values: '1', bytes: '4', terminal: { reason: 'closed' } } })
  expect(discardCalls).toBe(1)
})

test('cleanup receipt survives lost acknowledge and forget responses', async () => {
  let ackCalls = 0
  let forgetCalls = 0
  let closeCalls = 0
  let statusCalls = 0
  const central = {
    closeOrderedGroup: async () => { closeCalls += 1; return 'released' },
    orderedGroupStatus: async () => { statusCalls += 1; return JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: true, cleanupError: null }], sourceGeneration: null, cleanupComplete: true, setupError: null, fenceError: null }) },
    acknowledgeOrderedGroup: async () => { ackCalls += 1; if (ackCalls === 1) throw new Error('platform.transport|platform|ack|never|||reply lost') },
    forgetOrderedGroup: async () => { forgetCalls += 1; if (forgetCalls === 1) throw new Error('platform.transport|platform|forget|never|||reply lost') }
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  expect((await stream.close()).group.state).toBe('release-failed')
  expect(forgetCalls).toBe(0)
  expect((await stream.close()).group.state).toBe('release-failed')
  expect((await stream.close()).group.state).toBe('released')
  expect({ ackCalls, forgetCalls, closeCalls, statusCalls }).toEqual({ ackCalls: 2, forgetCalls: 2, closeCalls: 1, statusCalls: 1 })
})

test('marker retains full-width identity and ordinal across delivery replay', async () => {
  let acknowledgements = 0
  const marker = { kind: 'marker', markerId: '18446744073709551615', ordinal: '9007199254740993' }
  const poll = jest.fn(async () => delivery(marker))
  const central = { pollOrderedGroup: poll, acknowledgeOrderedDelivery: async () => { acknowledgements += 1; if (acknowledgements === 1) throw new Error('platform.transport|platform|native.ack|never|||reply lost') } }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'platform.transport' } })
  expect((await iterator.next()).value).toMatchObject({ kind: 'marker', markerId: '18446744073709551615', ordinal: '9007199254740993', sourceMembership: null })
  expect(poll).toHaveBeenCalledTimes(1)
})

test('marker participates in monotonic ordinal validation', async () => {
  const records = [{ kind: 'marker', markerId: '1', ordinal: '5' }, value('5', '0', [1])]
  const central = { pollOrderedGroup: async () => delivery(records.shift()), acknowledgeOrderedDelivery: async () => {} }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('marker')
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('rejected marker evidence retains its exact identity', async () => {
  const record = { kind: 'terminal', reason: { kind: 'ordinal-exhausted' }, uncertainTail: true, rejectedValues: null, rejectedLoss: null, sourceRejection: { kind: 'marker', markerId: '18446744073709551615' }, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0', failureSummaryVersion: 1, failureCountOverflowed: false, failureObservationsOmitted: false }
  const central = { pollOrderedGroup: async () => delivery(record), acknowledgeOrderedDelivery: async () => {}, orderedGroupStatus: status }
  const iterator = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])[Symbol.asyncIterator]()
  expect((await iterator.next()).value.sourceRejection).toEqual({ kind: 'marker', markerId: '18446744073709551615' })
})

test('setup drain returns acknowledged prefix when a later poll fails', async () => {
  let polls = 0
  const central = {
    pollOrderedGroup: async () => {
      polls += 1
      if (polls === 1) return { deliveryId: '1', record: JSON.stringify(value('1', '0', [1])) }
      if (polls === 2) throw new Error('platform.transport|platform|native.poll|never|||channel unavailable')
      return { deliveryId: '2', record: JSON.stringify(value('2', '0', [2])) }
    },
    acknowledgeOrderedDelivery: async () => {}
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  const setup = await stream.setupObservations()
  expect(setup.observations).toMatchObject([{ kind: 'value', ordinal: '1', value: Uint8Array.from([1]) }])
  expect(setup.error).toMatchObject({ code: 'platform.transport', domain: 'platform' })
  expect((await stream[Symbol.asyncIterator]().next()).value).toMatchObject({ ordinal: '2', value: Uint8Array.from([2]) })
})

test('setup drain retains an unresolved ACK without losing or duplicating the acknowledged prefix', async () => {
  let polls = 0
  let secondAcks = 0
  const central = {
    pollOrderedGroup: async () => { polls += 1; return { deliveryId: String(polls), record: JSON.stringify(value(String(polls), '0', [polls])) } },
    acknowledgeOrderedDelivery: async ({ deliveryId }) => { if (deliveryId === '2' && ++secondAcks === 1) throw new Error('platform.transport|platform|native.ack|never|||response lost') }
  }
  const stream = new DesktopOrderedStream(limits, central, 'peer', '4', 'connection', 'database', ['a'])
  const setup = await stream.setupObservations()
  expect(setup.observations).toMatchObject([{ kind: 'value', ordinal: '1' }])
  expect(setup.error).toMatchObject({ code: 'platform.transport' })
  expect((await stream[Symbol.asyncIterator]().next()).value).toMatchObject({ ordinal: '2', value: Uint8Array.from([2]) })
  expect(polls).toBe(2)
  expect(secondAcks).toBe(2)
})
