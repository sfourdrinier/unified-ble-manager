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
    lostValues: 1,
    lostBytes: 2
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
    lostSourceRecords: 5
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
    lostSourceRecords: 5
  })
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
  expect((await pending).value).toMatchObject({ kind: 'loss', lostValues: 1, lostBytes: 8 })
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
  expect((await consumer.next()).value).toMatchObject({ kind: 'loss', lostValues: 1, lostBytes: 1 })
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
