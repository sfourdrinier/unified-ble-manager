// __tests__/core/ordered-notification-stream.test.js

const { capacity } = require('../../src/backend-contract/primitives')
const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')

const released = { group: { state: 'released', failures: [] }, members: [] }
const limits = { itemCapacity: capacity(2), byteCapacity: capacity(8), reservedControlCapacity: capacity(1) }
function value(ordinal, bytes = [1]) {
  return {
    kind: 'value',
    collectionId: 'c',
    connectionGeneration: 'g',
    databaseGeneration: 'd',
    memberId: 'm',
    ordinal,
    value: Uint8Array.from(bytes),
    delivery: 'unknown',
    provenance: 'unknown'
  }
}

test('wakes pending consumer and drains admitted values before one terminal', async () => {
  const stream = new CoreOrderedNotificationStream(
    limits,
    () => {},
    async () => released
  )
  const iterator = stream[Symbol.asyncIterator]()
  const pending = iterator.next()
  expect(stream.emit(value('1'))).toBe(true)
  expect((await pending).value.ordinal).toBe('1')
  stream.emit(value('2'))
  stream.finish({ kind: 'terminal', reason: 'connection-lost', uncertainTail: true })
  expect((await iterator.next()).value.ordinal).toBe('2')
  expect((await iterator.next()).value.reason).toBe('connection-lost')
  expect((await iterator.next()).done).toBe(true)
  expect(stream.emit(value('3'))).toBe(false)
  expect(() => stream[Symbol.asyncIterator]()).toThrow()
})

test('overflow reserves loss and terminal without displacing earlier records', async () => {
  const stream = new CoreOrderedNotificationStream(
    limits,
    () => {},
    async () => released
  )
  stream.emit(value('1', [1, 2, 3]))
  stream.emit(value('2', [4, 5, 6]))
  expect(stream.emit(value('3', [7, 8]))).toBe(false)
  const consumer = stream[Symbol.asyncIterator]()
  expect((await consumer.next()).value.ordinal).toBe('1')
  expect((await consumer.next()).value.ordinal).toBe('2')
  expect((await consumer.next()).value).toMatchObject({
    kind: 'loss',
    scope: { kind: 'member', memberId: 'm' },
    lostValues: '1',
    lostBytes: '2'
  })
  expect((await consumer.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
  expect((await consumer.next()).done).toBe(true)
})

test('overflow of a loss control preserves source counts and uncertainty', async () => {
  const stream = new CoreOrderedNotificationStream(
    limits,
    () => {},
    async () => released
  )
  stream.emit(value('1'))
  stream.emit(value('2'))
  stream.emit({
    kind: 'loss',
    scope: { kind: 'unknown' },
    reason: 'source-loss',
    lostValues: null,
    lostBytes: null,
    lostSourceRecords: '5'
  })
  stream.emit({
    kind: 'loss',
    scope: { kind: 'unknown' },
    reason: 'source-loss',
    lostValues: null,
    lostBytes: null,
    lostSourceRecords: '6'
  })
  const consumer = stream[Symbol.asyncIterator]()
  await consumer.next()
  await consumer.next()
  expect((await consumer.next()).value).toMatchObject({
    kind: 'loss',
    scope: { kind: 'unknown' },
    reason: 'source-loss',
    lostValues: null,
    lostBytes: null,
    lostSourceRecords: '5'
  })
  expect((await consumer.next()).value).toMatchObject({ kind: 'loss', lostSourceRecords: '6' })
  expect((await consumer.next()).value.reason).toBe('overflow')
})

test('close shares in-flight cleanup and retries failed release', async () => {
  let resolve
  const cleanup = jest
    .fn()
    .mockImplementationOnce(
      () =>
        new Promise(r => {
          resolve = r
        })
    )
    .mockResolvedValueOnce(released)
  const stream = new CoreOrderedNotificationStream(limits, () => {}, cleanup)
  const first = stream.close()
  const shared = stream.close()
  expect(cleanup).toHaveBeenCalledTimes(1)
  resolve({ group: { state: 'release-failed', failures: [] }, members: [] })
  expect((await first).group.state).toBe('release-failed')
  await shared
  expect(await stream.close()).toEqual(released)
  expect(await stream.close()).toEqual(released)
  expect(cleanup).toHaveBeenCalledTimes(2)
})

test('iterator return cancels a pending next and invokes cleanup', async () => {
  const cleanup = jest.fn().mockResolvedValue(released)
  const stream = new CoreOrderedNotificationStream(limits, () => {}, cleanup)
  const consumer = stream[Symbol.asyncIterator]()
  const pending = consumer.next()
  await consumer.return()
  expect(await pending).toEqual({ done: true, value: undefined })
  expect(cleanup).toHaveBeenCalledTimes(1)
})

test('oversized value wakes a waiting consumer with loss instead of admission', async () => {
  const stream = new CoreOrderedNotificationStream(
    limits,
    () => {},
    async () => released
  )
  const consumer = stream[Symbol.asyncIterator]()
  const pending = consumer.next()
  expect(stream.emit(value('1', Array(8).fill(1)))).toBe(false)
  expect((await pending).value).toMatchObject({ kind: 'loss', lostValues: '1', lostBytes: '8' })
  expect((await consumer.next()).value.reason).toBe('overflow')
})

test.each(['close', 'return', 'overflow', 'source-terminal'])('fences before %s and only once', async action => {
  const order = []
  let stream
  const fence = jest.fn(() => {
    order.push('fence')
    expect(stream.emit(value('99'))).toBe(false)
  })
  stream = new CoreOrderedNotificationStream(limits, fence, async () => {
    order.push('cleanup')
    return released
  })
  const consumer = stream[Symbol.asyncIterator]()
  if (action === 'close') {
    await stream.close()
    expect((await consumer.next()).value.reason).toBe('closed')
  } else if (action === 'return') {
    await consumer.return()
  } else if (action === 'overflow') {
    stream.emit(value('1'))
    stream.emit(value('2'))
    expect(stream.emit(value('3'))).toBe(false)
    await consumer.next()
    await consumer.next()
    expect((await consumer.next()).value.kind).toBe('loss')
    expect((await consumer.next()).value.reason).toBe('overflow')
  } else {
    stream.finish({ kind: 'terminal', reason: 'connection-lost', uncertainTail: true })
    expect((await consumer.next()).value.reason).toBe('connection-lost')
  }
  await stream.close()
  expect(fence).toHaveBeenCalledTimes(1)
  expect(order[0]).toBe('fence')
  expect(stream.emit(value('4'))).toBe(false)
})

test('failed overflow fence reports loss and terminal error, then retries before cleanup', async () => {
  const fence = jest
    .fn()
    .mockImplementationOnce(() => {
      throw new Error('failure')
    })
    .mockImplementationOnce(() => {})
  const cleanup = jest.fn().mockResolvedValue(released)
  const stream = new CoreOrderedNotificationStream(limits, fence, cleanup)
  stream.emit(value('1'))
  stream.emit(value('2'))
  expect(stream.emit(value('3'))).toBe(false)
  const consumer = stream[Symbol.asyncIterator]()
  await consumer.next()
  await consumer.next()
  expect((await consumer.next()).value).toMatchObject({ kind: 'loss', lostValues: '1', lostBytes: '1' })
  expect((await consumer.next()).value).toMatchObject({
    kind: 'terminal',
    reason: 'overflow',
    uncertainTail: true,
    fenceError: { code: 'platform.failure' }
  })
  expect(cleanup).not.toHaveBeenCalled()
  await stream.close()
  expect(fence).toHaveBeenCalledTimes(2)
  expect(fence).toHaveBeenCalledWith('overflow')
  expect(cleanup).toHaveBeenCalledTimes(1)
})

test('reentrant close and return cannot start cleanup before fence completes', async () => {
  let stream
  let consumer
  let nestedClose
  let nestedReturn
  const cleanup = jest.fn().mockResolvedValue(released)
  const fence = jest.fn(() => {
    nestedClose = stream.close()
    nestedReturn = consumer.return()
    expect(cleanup).not.toHaveBeenCalled()
  })
  stream = new CoreOrderedNotificationStream(limits, fence, cleanup)
  consumer = stream[Symbol.asyncIterator]()
  await stream.close()
  await expect(nestedClose).rejects.toThrow()
  await expect(nestedReturn).rejects.toThrow()
  expect(cleanup).toHaveBeenCalledTimes(1)
})

test('source and fence errors remain distinct while close retries the fence', async () => {
  const { contractError } = require('../../src/backend-contract/errors')
  const sourceError = contractError('platform.transport', 'stream', 'source').normalized
  const fenceError = contractError('adapter.resetting', 'stream', 'fence')
  const fence = jest
    .fn()
    .mockImplementationOnce(() => {
      throw fenceError
    })
    .mockImplementationOnce(() => {})
  const cleanup = jest.fn().mockResolvedValue(released)
  const stream = new CoreOrderedNotificationStream(limits, fence, cleanup)
  const consumer = stream[Symbol.asyncIterator]()
  stream.finish({ kind: 'terminal', reason: 'source-failed', uncertainTail: false, error: sourceError })
  expect((await consumer.next()).value).toMatchObject({
    reason: 'source-failed',
    uncertainTail: true,
    error: sourceError,
    fenceError: fenceError.normalized
  })
  await stream.close()
  expect(fence).toHaveBeenCalledTimes(2)
  expect(cleanup).toHaveBeenCalledTimes(1)
})

test('discard reports unread values, bytes and control evidence after cleanup', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released)
  stream.emit(value('1', [1, 2]))
  stream.emit({ kind: 'loss', scope: { kind: 'unknown' }, reason: 'source-loss', lostValues: null, lostBytes: null, lostSourceRecords: '7' })
  const result = await stream.discard()
  expect(result.discarded).toMatchObject({ values: '1', bytes: '2', controls: '2', uncertainTail: true, terminal: { reason: 'closed' }, losses: [{ lostSourceRecords: '7' }] })
  expect(await stream.close()).toEqual(result)
})

test('discard retains unread records until a failed cleanup is retried', async () => {
  let attempt = 0
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => {
    attempt += 1
    return attempt === 1 ? { group: { state: 'release-failed', failures: [] }, members: [] } : released
  })
  stream.emit(value('1', [1, 2, 3]))
  const pending = await stream.discard()
  expect(pending.discarded).toMatchObject({ values: '0', bytes: '0', uncertainTail: true })
  const done = await stream.close()
  expect(done.discarded).toMatchObject({ values: '1', bytes: '3', controls: '1', terminal: { reason: 'closed' } })
})

test('discard retains marker evidence as a control', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released)
  stream.reserveMarker('c', '18446744073709551615')
  stream.emit({ kind: 'marker', collectionId: 'c', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '18446744073709551615', ordinal: '2' })
  const result = await stream.discard()
  expect(result.discarded).toMatchObject({ values: '0', bytes: '0', controls: '2', markers: [{ markerId: '18446744073709551615', ordinal: '2' }] })
})

test('invalid marker ordinal retains explicit rejection evidence', async () => {
  const stream = new CoreOrderedNotificationStream({ itemCapacity: capacity(1), byteCapacity: capacity(8), reservedControlCapacity: capacity(1) }, () => {}, async () => released)
  stream.emit(value('1'))
  stream.reserveMarker('c', '4')
  expect(stream.emit({ kind: 'marker', collectionId: 'c', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '4', ordinal: '1' })).toBe(false)
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('value')
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', sourceRejection: { kind: 'marker', markerId: '4' }, uncertainTail: true })
})

test('marker credit reserves a control slot independent of a full value queue', async () => {
  const stream = new CoreOrderedNotificationStream({ itemCapacity: capacity(1), byteCapacity: capacity(8), reservedControlCapacity: capacity(2) }, () => {}, async () => released, 'group')
  stream.emit(value('1'))
  stream.reserveMarker('group', '10')
  expect(stream.markerAdmissionStatus('group', '10')).toEqual({ state: 'reserved' })
  stream.reserveMarker('group', '10')
  expect(stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '10', ordinal: '2' })).toBe(true)
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('value')
  expect((await iterator.next()).value).toMatchObject({ kind: 'marker', markerId: '10', ordinal: '2' })
  expect(stream.markerAdmissionStatus('group', '10')).toEqual({ state: 'admitted', ordinal: '2' })
})

test('marker credit consumes only matching admission, rejection or authoritative non-admission', () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '10')
  expect(() => stream.reserveMarker('other', '10')).toThrow()
  expect(() => stream.reserveMarker('group', '11')).toThrow()
  expect(() => stream.releaseMarkerNotAdmitted('group', '11')).toThrow()
  stream.releaseMarkerNotAdmitted('group', '10')
  expect(stream.markerAdmissionStatus('group', '10')).toEqual({ state: 'not-admitted' })
  stream.reserveMarker('group', '11')
  expect(() => stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '10', ordinal: '1' })).toThrow()
  stream.rejectReservedMarker('group', '11')
  expect(stream.markerAdmissionStatus('group', '11')).toEqual({ state: 'rejected' })
  stream.reserveMarker('group', '12')
  stream.releaseMarkerNotAdmitted('group', '12')
})

test('ordinary controls cannot occupy a reserved marker slot', async () => {
  const stream = new CoreOrderedNotificationStream({ itemCapacity: capacity(8), byteCapacity: capacity(16), reservedControlCapacity: capacity(2) }, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '10')
  const loss = { kind: 'loss', scope: { kind: 'unknown' }, reason: 'source-loss', lostValues: null, lostBytes: null, lostSourceRecords: '1' }
  expect(stream.emit(loss)).toBe(true)
  expect(stream.emit(loss)).toBe(false)
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toEqual(loss)
  expect((await iterator.next()).value).toEqual(loss)
  const terminal = iterator.next()
  expect((await stream.close()).group.state).toBe('release-failed')
  stream.releaseMarkerNotAdmitted('group', '10')
  expect((await terminal).value).toMatchObject({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
  expect((await stream.close()).group.state).toBe('released')
})

test('marker credit does not consume either available value slot', async () => {
  const stream = new CoreOrderedNotificationStream({ itemCapacity: capacity(2), byteCapacity: capacity(8), reservedControlCapacity: capacity(1) }, () => {}, async () => released, 'group')
  expect(stream.emit(value('1'))).toBe(true)
  stream.reserveMarker('group', '5')
  expect(stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '5', ordinal: '2' })).toBe(true)
  expect(stream.emit(value('3'))).toBe(true)
  const iterator = stream[Symbol.asyncIterator]()
  expect([(await iterator.next()).value.kind, (await iterator.next()).value.kind, (await iterator.next()).value.kind]).toEqual(['value', 'marker', 'value'])
})

test('an unrelated terminal waits for admitted marker and preserves FIFO', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '7')
  stream.finish({ kind: 'terminal', reason: 'connection-lost', uncertainTail: true })
  const iterator = stream[Symbol.asyncIterator]()
  const next = iterator.next()
  expect(stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '7', ordinal: '1' })).toBe(true)
  expect((await next).value).toMatchObject({ kind: 'marker', markerId: '7' })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
})

test('matching rejection settles deferred terminal with rejection evidence', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '7')
  stream.finish({ kind: 'terminal', reason: 'service-changed', uncertainTail: true })
  stream.rejectReservedMarker('group', '7')
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'service-changed', sourceRejection: { kind: 'marker', markerId: '7' } })
})

test('marker reservation refuses when all control slots are already occupied', async () => {
  const stream = new CoreOrderedNotificationStream({ itemCapacity: capacity(2), byteCapacity: capacity(8), reservedControlCapacity: capacity(1) }, () => {}, async () => released, 'group')
  expect(stream.emit({ kind: 'loss', scope: { kind: 'unknown' }, reason: 'source-loss', lostValues: null, lostBytes: null, lostSourceRecords: '1' })).toBe(true)
  expect(() => stream.reserveMarker('group', '1')).toThrow()
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('loss')
  expect(() => stream.reserveMarker('group', '1')).not.toThrow()
  stream.releaseMarkerNotAdmitted('group', '1')
})

test('a value cannot reuse an admitted marker ordinal', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '7')
  expect(stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '7', ordinal: '2' })).toBe(true)
  expect(stream.emit(value('2'))).toBe(false)
  const iterator = stream[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('marker')
  expect((await iterator.next()).value).toMatchObject({ kind: 'loss', lostValues: '1' })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
})

test('unmatched marker rejection cannot consume another marker credit', () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '7')
  expect(() => stream.finish({ kind: 'terminal', reason: 'source-failed', uncertainTail: true, sourceRejection: { kind: 'marker', markerId: '8' } })).toThrow()
  expect(stream.markerAdmissionStatus('group', '7')).toEqual({ state: 'reserved' })
  stream.releaseMarkerNotAdmitted('group', '7')
})

test('discard retains a late admitted marker until credit and cleanup settle', async () => {
  const stream = new CoreOrderedNotificationStream(limits, () => {}, async () => released, 'group')
  stream.reserveMarker('group', '9')
  const pending = await stream.discard()
  expect(pending.group.state).toBe('release-failed')
  expect(pending.discarded).toMatchObject({ controls: '0', uncertainTail: true })
  expect(stream.admitReservedMarker({ kind: 'marker', collectionId: 'group', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '9', ordinal: '1' })).toBe(true)
  const final = await stream.close()
  expect(final).toMatchObject({ group: { state: 'released' }, discarded: { controls: '2', markers: [{ markerId: '9', ordinal: '1' }], terminal: { reason: 'closed' } } })
})
