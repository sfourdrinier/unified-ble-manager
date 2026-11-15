const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
const { resourceCount } = require('../../src/backend-contract/primitives')

const limits = (items = 2, bytes = 20) => ({
  itemCapacity: resourceCount(items),
  byteCapacity: resourceCount(bytes),
  reservedControlCapacity: resourceCount(1)
})

test('staged shared references remain hidden and release their exact quota on delivery', async () => {
  const quota = new AggregateStreamQuota(30)
  const source = new CoreBoundedStream(limits(), 'error')
  const left = new CoreBoundedStream(limits(), 'error')
  const right = new CoreBoundedStream(limits(), 'error')
  for (const stream of [source, left, right]) quota.register(stream)
  quota.emit(source, { bytes: new Uint8Array([1, 2]) }, 6)
  const refs = quota.shareClaim(source, source.claimConsumer().claim(), 8, 2, 2)
  const pendingLeft = left[Symbol.asyncIterator]().next()
  const pendingRight = right[Symbol.asyncIterator]().next()
  const a = left.stageOwned(refs[0], 6, value => ({ bytes: value.bytes.slice() }))
  const b = right.stageOwned(refs[1], 6, value => ({ bytes: value.bytes.slice() }))
  let delivered = false
  pendingLeft.then(() => {
    delivered = true
  })
  await Promise.resolve()
  expect(delivered).toBe(false)
  expect(quota.retainedBytes()).toBe(15)
  a.publish()
  expect((await pendingLeft).value.value.bytes).toEqual(new Uint8Array([1, 2]))
  expect(quota.retainedBytes()).toBe(13)
  b.publish()
  expect((await pendingRight).value.value.bytes).toEqual(new Uint8Array([1, 2]))
  expect(quota.retainedBytes()).toBe(3)
})

const identity = value => value
const owned = (value, release = jest.fn()) => ({ value, release })

test('transaction publishes every queue before any delivery callback can reenter', async () => {
  const left = new CoreBoundedStream(limits(), 'error')
  const right = new CoreBoundedStream(limits(), 'error')
  const rightReader = right.claimConsumer()
  let reentrant
  const a = left.stageOwned(owned('left', () => { reentrant = rightReader.claim() }), 3, identity)
  const b = right.stageOwned(owned('right'), 3, identity)
  const pending = left[Symbol.asyncIterator]().next()
  CoreBoundedStream.publishOwned([a, b])
  expect((await pending).value.value).toBe('left')
  expect(reentrant.item.value).toBe('right')
  reentrant.commit()
  rightReader.close()
})

test('duplicate transaction stages are rejected without publishing or losing ownership', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const ref = owned('value')
  const stage = stream.stageOwned(ref, 3, identity)
  expect(() => CoreBoundedStream.publishOwned([stage, stage])).toThrow()
  expect(ref.release).not.toHaveBeenCalled()
  stage.publish()
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('value')
})

test('publication flushes other queues even when one delivery cleanup fails', async () => {
  const left = new CoreBoundedStream(limits(), 'error')
  const right = new CoreBoundedStream(limits(), 'error')
  const a = left.stageOwned(owned('left', () => { throw new Error('cleanup') }), 3, identity)
  const b = right.stageOwned(owned('right'), 3, identity)
  const first = left[Symbol.asyncIterator]().next()
  const second = right[Symbol.asyncIterator]().next()
  expect(() => CoreBoundedStream.publishOwned([a, b])).toThrow('cleanup')
  expect((await first).value.value).toBe('left')
  expect((await second).value.value).toBe('right')
})

test('drop-oldest preserves an evictable prefix if a staged entry still prevents admission', async () => {
  const stream = new CoreBoundedStream(limits(2, 9), 'drop-oldest')
  const first = owned('kept')
  stream.stageOwned(first, 2, identity).publish()
  const second = stream.stageOwned(owned('staged'), 6, identity)
  expect(stream.emit('cannot fit', 3).accepted).toBe(false)
  expect(first.release).not.toHaveBeenCalled()
  second.publish()
  const reader = stream[Symbol.asyncIterator]()
  expect((await reader.next()).value.kind).toBe('overflow')
  expect((await reader.next()).value.value).toBe('kept')
  expect((await reader.next()).value.value).toBe('staged')
})

test('refusal keeps ownership with caller and cancellation frees only its own staged record', async () => {
  const stream = new CoreBoundedStream(limits(1, 8), 'drop-newest')
  const first = owned('first')
  const second = owned('second')
  const stage = stream.stageOwned(first, 4, identity)
  expect(stream.stageOwned(second, 4, identity)).toBeNull()
  expect(second.release).not.toHaveBeenCalled()
  stage.cancel()
  stage.cancel()
  expect(first.release).toHaveBeenCalledTimes(1)
  expect(stream.retainedPayloadBytes()).toBe(0)
  const next = stream.stageOwned(second, 4, identity)
  next.publish()
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('second')
  expect(second.release).toHaveBeenCalledTimes(1)
})

test.each(['drop-oldest', 'latest'])(
  '%s releases the evicted reference only after replacement succeeds',
  async policy => {
    const stream = new CoreBoundedStream(limits(1, 8), policy)
    const first = owned('first')
    stream.stageOwned(first, 4, identity, 'key').publish()
    expect(stream.emit('second', 4, 'key').accepted).toBe(true)
    expect(first.release).toHaveBeenCalledTimes(1)
    const reader = stream[Symbol.asyncIterator]()
    expect((await reader.next()).value.kind).toBe('overflow')
    expect((await reader.next()).value.value).toBe('second')
  }
)

test('close releases staged, published, and claimed references once and invalidates old handles', async () => {
  const stream = new CoreBoundedStream(limits(3), 'error')
  const first = owned('first')
  const second = owned('second')
  const third = owned('third')
  stream.stageOwned(first, 3, identity).publish()
  const claim = stream.claimConsumer().claim()
  stream.stageOwned(second, 3, identity).publish()
  const stage = stream.stageOwned(third, 3, identity)
  await stream.close()
  expect([first, second, third].map(ref => ref.release.mock.calls.length)).toEqual([1, 1, 1])
  expect(() => claim.rollback()).toThrow()
  expect(() => stage.publish()).toThrow()
  stage.cancel()
  expect(third.release).toHaveBeenCalledTimes(1)
})

test('a claimed reference survives rollback, competing consumers and overflow until delivered', async () => {
  const stream = new CoreBoundedStream(limits(1, 8), 'drop-oldest')
  const first = owned('first')
  stream.stageOwned(first, 4, identity).publish()
  const claim = stream.claimConsumer().claim()
  const reader = stream[Symbol.asyncIterator]()
  const pending = reader.next()
  expect(stream.emit('overflow', 4).accepted).toBe(false)
  expect(first.release).not.toHaveBeenCalled()
  claim.rollback()
  expect((await pending).value.kind).toBe('overflow')
  expect((await reader.next()).value.value).toBe('first')
  expect(first.release).toHaveBeenCalledTimes(1)
})

test('close continues releasing other references and settles readers when cleanup throws and reenters', async () => {
  const stream = new CoreBoundedStream(limits(3), 'error')
  const first = owned(
    'first',
    jest.fn(() => {
      stream.closeWithReason('closed')
      throw new Error('release failed')
    })
  )
  const second = owned('second')
  stream.stageOwned(first, 3, identity).publish()
  stream.stageOwned(second, 3, identity).publish()
  const claim = stream.claimConsumer().claim()
  const reader = stream[Symbol.asyncIterator]()
  const terminal = reader.next()
  expect(() => stream.closeWithReason('closed')).toThrow('release failed')
  expect(first.release).toHaveBeenCalledTimes(1)
  expect(second.release).toHaveBeenCalledTimes(1)
  expect((await terminal).value.reason).toBe('closed')
  expect(() => claim.rollback()).toThrow()
})

test('shared mutable values are copied only when each consumer receives them', async () => {
  const original = { bytes: new Uint8Array([1, 2]) }
  const first = owned(original)
  const second = owned(original)
  const left = new CoreBoundedStream(limits(), 'error')
  const right = new CoreBoundedStream(limits(), 'error')
  const copy = jest.fn(value => ({ bytes: value.bytes.slice() }))
  left.stageOwned(first, 3, copy).publish()
  right.stageOwned(second, 3, copy).publish()
  expect(copy).not.toHaveBeenCalled()
  const firstValue = (await left[Symbol.asyncIterator]().next()).value.value
  firstValue.bytes[0] = 9
  const secondValue = (await right[Symbol.asyncIterator]().next()).value.value
  expect(secondValue.bytes[0]).toBe(1)
  expect(copy).toHaveBeenCalledTimes(2)
})

test('cancel after an iterator has returned preserves the staged value for another reader', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const item = owned('kept')
  const iterator = stream[Symbol.asyncIterator]()
  const pending = iterator.next()
  const stage = stream.stageOwned(item, 3, identity)
  await iterator.return()
  expect((await pending).done).toBe(true)
  stage.publish()
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('kept')
  expect(item.release).toHaveBeenCalledTimes(1)
})

test('staged entries block eviction and visibility of later entries', async () => {
  const stream = new CoreBoundedStream(limits(2, 9), 'drop-oldest')
  const first = owned('first')
  const stage = stream.stageOwned(first, 4, identity)
  stream.emit('later', 3)
  expect(stream.emit('refused', 3).accepted).toBe(false)
  expect(first.release).not.toHaveBeenCalled()
  const reader = stream[Symbol.asyncIterator]()
  expect((await reader.next()).value.kind).toBe('overflow')
  const pending = reader.next()
  stage.publish()
  expect((await pending).value.value).toBe('first')
  expect((await reader.next()).value.value).toBe('later')
})

test('pending readers settle even if delivery release throws and reenters close', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const first = owned(
    'first',
    jest.fn(() => {
      stream.closeWithReason('closed')
      throw new Error('release failed')
    })
  )
  const reader = stream[Symbol.asyncIterator]()
  const value = reader.next()
  const terminal = reader.next()
  const stage = stream.stageOwned(first, 3, identity)
  expect(() => stage.publish()).toThrow('release failed')
  expect((await value).value.value).toBe('first')
  expect((await terminal).value.reason).toBe('closed')
  expect(first.release).toHaveBeenCalledTimes(1)
})

test('queue capacity can be reserved before source transfer, then attached without early delivery', async () => {
  const quota = new AggregateStreamQuota(24)
  const source = new CoreBoundedStream(limits(), 'error')
  const target = new CoreBoundedStream(limits(1), 'error')
  quota.register(source)
  quota.register(target)
  quota.emit(source, { data: [1] }, 5)
  const claim = source.claimConsumer().claim()
  const staged = target.reserveOwned(5)
  expect(target.reserveOwned(5)).toBeNull()
  expect(quota.retainedBytes()).toBe(7)
  const pending = target[Symbol.asyncIterator]().next()
  const [reference] = quota.shareClaim(source, claim, 6, 2, 1)
  staged.attach(reference, value => ({ data: value.data.slice() }))
  expect(quota.retainedBytes()).toBe(10)
  staged.publish()
  expect((await pending).value.value).toEqual({ data: [1] })
  expect(quota.retainedBytes()).toBe(2)
})

test('a cancelled or closed reservation refuses attachment without taking its reference', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const cancelled = stream.reserveOwned(3)
  cancelled.cancel()
  const closing = stream.reserveOwned(3)
  await stream.close()
  const ref = owned('caller')
  expect(() => cancelled.attach(ref, identity)).toThrow()
  expect(() => closing.attach(ref, identity)).toThrow()
  expect(ref.release).not.toHaveBeenCalled()
})

test('failed delivery copy keeps the reference and queued reader for retry', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const ref = owned('value')
  let fail = true
  const stage = stream.stageOwned(ref, 3, value => {
    if (fail) throw new Error('copy failed')
    return value
  })
  const pending = stream[Symbol.asyncIterator]().next()
  expect(() => stage.publish()).toThrow('copy failed')
  expect(ref.release).not.toHaveBeenCalled()
  fail = false
  stream.emit('wake', 1)
  expect((await pending).value.value).toBe('value')
  expect(ref.release).toHaveBeenCalledTimes(1)
})

test('delivery cleanup errors reject an immediate read instead of throwing synchronously', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const ref = owned(
    'value',
    jest.fn(() => {
      throw new Error('release failed')
    })
  )
  stream.stageOwned(ref, 3, identity).publish()
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow('release failed')
  expect(ref.release).toHaveBeenCalledTimes(1)
})

test('a copy that reenters close cannot remove another queued value twice', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const first = owned('first')
  const second = owned('second')
  stream
    .stageOwned(first, 3, value => {
      stream.closeWithReason('closed')
      return value
    })
    .publish()
  stream.stageOwned(second, 3, identity).publish()
  await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow()
  expect(first.release).toHaveBeenCalledTimes(1)
  expect(second.release).toHaveBeenCalledTimes(1)
  expect(stream.retainedPayloadBytes()).toBe(0)
})

test('a delivery cleanup failure preserves the receiver-owned item in the rejection', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const ref = owned(
    'payload',
    jest.fn(() => {
      throw new Error('release failed')
    })
  )
  stream.stageOwned(ref, 3, identity).publish()
  let failure
  try {
    await stream[Symbol.asyncIterator]().next()
  } catch (error) {
    failure = error
  }
  expect(failure.item).toEqual({ kind: 'value', value: 'payload' })
  expect(failure.cause.message).toBe('release failed')
})

test('committing an owned claim releases its original reference and retains the successor quota', () => {
  const quota = new AggregateStreamQuota(30)
  const source = new CoreBoundedStream(limits(), 'error')
  const target = new CoreBoundedStream(limits(), 'error')
  quota.register(source)
  quota.register(target)
  quota.emit(source, 'payload', 5)
  const [ref] = quota.shareClaim(source, source.claimConsumer().claim(), 6, 2, 1)
  target.stageOwned(ref, 5, identity).publish()
  const claim = target.claimConsumer().claim()
  const reservation = quota.transfer(target, claim, 8)
  expect(quota.retainedBytes()).toBe(10)
  expect(() => ref.value).toThrow()
  reservation.release()
  expect(quota.retainedBytes()).toBe(2)
})

test('failed latest replacement keeps the previous owned value and handle', async () => {
  const stream = new CoreBoundedStream(limits(1, 6), 'latest')
  const ref = owned('kept')
  stream.stageOwned(ref, 3, identity, 'key').publish()
  expect(stream.emit('too large', 6, 'key').accepted).toBe(false)
  expect(ref.release).not.toHaveBeenCalled()
  const reader = stream[Symbol.asyncIterator]()
  expect((await reader.next()).value.kind).toBe('overflow')
  expect((await reader.next()).value.value).toBe('kept')
  expect(ref.release).toHaveBeenCalledTimes(1)
})

test('refused shared transfer rolls back queue reservations without consuming the source', async () => {
  const quota = new AggregateStreamQuota(12)
  const source = new CoreBoundedStream(limits(), 'error')
  const target = new CoreBoundedStream(limits(1), 'error')
  quota.register(source)
  quota.register(target)
  quota.emit(source, 'source', 6)
  const owner = source.claimConsumer()
  const claim = owner.claim()
  const stage = target.reserveOwned(6)
  expect(quota.shareClaim(source, claim, 8, 3, 1)).toBeNull()
  stage.cancel()
  claim.rollback()
  expect((await source[Symbol.asyncIterator]().next()).value.value).toBe('source')
  expect(target.retainedPayloadBytes()).toBe(0)
  expect(quota.retainedBytes()).toBe(2)
})

test('stale source rollback after close releases its owned claim and leaves a reserved destination empty', async () => {
  const source = new CoreBoundedStream(limits(), 'error')
  const target = new CoreBoundedStream(limits(), 'error')
  const ref = owned('source')
  source.stageOwned(ref, 4, identity).publish()
  const claim = source.claimConsumer().claim()
  const stage = target.reserveOwned(4)
  await source.close()
  expect(() => claim.rollback()).toThrow()
  stage.cancel()
  expect(ref.release).toHaveBeenCalledTimes(1)
  expect(target.retainedPayloadBytes()).toBe(0)
})

test('cancel reports a release exception but frees capacity and permits other staged cleanup', () => {
  const stream = new CoreBoundedStream(limits(2), 'error')
  const first = owned(
    'first',
    jest.fn(() => {
      throw new Error('cancel failed')
    })
  )
  const second = owned('second')
  const a = stream.stageOwned(first, 3, identity)
  const b = stream.stageOwned(second, 3, identity)
  expect(() => a.cancel()).toThrow('cancel failed')
  a.cancel()
  b.cancel()
  expect(first.release).toHaveBeenCalledTimes(1)
  expect(second.release).toHaveBeenCalledTimes(1)
  expect(stream.retainedPayloadBytes()).toBe(0)
  expect(stream.reserveOwned(3)).not.toBeNull()
})

test('a prior cleanup failure does not interrupt a later reentrant release', () => {
  const stream = new CoreBoundedStream(limits(3, 12), 'drop-oldest')
  let reentrantCompleted = false
  const first = owned(
    'first',
    jest.fn(() => {
      throw new Error('first failed')
    })
  )
  const second = owned(
    'second',
    jest.fn(() => {
      stream.emit('reentrant', 1)
      reentrantCompleted = true
    })
  )
  stream.stageOwned(first, 4, identity).publish()
  stream.stageOwned(second, 4, identity).publish()
  expect(() => stream.emit('replacement', 8)).toThrow('first failed')
  expect(reentrantCompleted).toBe(true)
  expect(second.release).toHaveBeenCalledTimes(1)
})
