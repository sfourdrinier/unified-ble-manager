const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
const { resourceCount } = require('../../src/backend-contract/primitives')
const { utf8ByteLength } = require('../../src/backend-contract/serializable')

const limits = (items = 2, bytes = 20) => ({
  itemCapacity: resourceCount(items),
  byteCapacity: resourceCount(bytes),
  reservedControlCapacity: resourceCount(1)
})

test('async claim consumer waits without taking ownership and preserves terminal order', async () => {
  const stream = new CoreBoundedStream(limits(), 'error', true)
  const owner = stream.claimConsumer()
  const pending = owner.next()
  stream.emit('first', 4)
  stream.finishWithReason('owner-released')
  const first = await pending
  expect(first.kind).toBe('claim')
  expect(first.claim.item.value).toBe('first')
  expect(stream.retainedBytes()).toBe(5)
  first.claim.commit()
  expect(await owner.next()).toMatchObject({ kind: 'control', item: { kind: 'terminal', reason: 'owner-released' } })
  expect(await owner.next()).toEqual({ kind: 'done' })
})

test('closing an async claim consumer cancels its pending read and rolls back its claim', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const owner = stream.claimConsumer()
  const pending = owner.next()
  owner.close()
  expect(await pending).toEqual({ kind: 'done' })
  stream.emit('still-owned', 3)
  const replacement = stream.claimConsumer()
  const result = await replacement.next()
  expect(result.kind).toBe('claim')
  replacement.close()
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('still-owned')
})

test('async claim reader drains an accepted prefix before its ordered source-loss control', async () => {
  const stream = new CoreBoundedStream(limits(2, 256), 'drop-newest')
  const owner = stream.claimConsumer()
  stream.emit('accepted', 3)
  stream.observeSourceLoss({
    kind: 'source-loss',
    scope: 'host-global',
    source: 'advertisement-ingress',
    lostRecords: resourceCount(2)
  })
  const result = await owner.next()
  expect(result.kind).toBe('claim')
  expect(result.claim.item.value).toBe('accepted')
  result.claim.commit()
  expect(await owner.next()).toMatchObject({ kind: 'control', item: { kind: 'source-loss', lostRecords: 2 } })
  owner.close()
})

test('claim reader retains peer loss between its accepted prefix and later value without terminalizing', async () => {
  const stream = new CoreBoundedStream(limits(4, 256), 'drop-oldest')
  const peerLoss = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '7', skippedRecords: null }
  stream.emit('before', 2)
  stream.observeSourceLoss(peerLoss)
  expect(stream.emit('after', 2)).toMatchObject({ accepted: true, terminated: false })
  const reader = stream.claimConsumer()
  const first = await reader.next()
  expect(first).toMatchObject({ kind: 'claim', claim: { item: { value: 'before' } } })
  first.claim.commit()
  expect(await reader.next()).toEqual({ kind: 'control', item: peerLoss })
  const after = await reader.next()
  expect(after).toMatchObject({ kind: 'claim', claim: { item: { value: 'after' } } })
  after.claim.commit()
  await stream.close()
})

test('source-loss controls use serialized-byte capacity, retain the terminal headroom, and release their aggregate charge', async () => {
  const first = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '1', skippedRecords: null }
  const second = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '2', skippedRecords: null }
  const firstBytes = utf8ByteLength(JSON.stringify(first))
  const stream = new CoreBoundedStream(limits(8, firstBytes + 1), 'drop-oldest')
  const quota = new AggregateStreamQuota(1024)
  quota.register(stream)

  stream.observeSourceLoss(first)
  expect(stream.sourceLossNotices()).toEqual([first])
  expect(quota.retainedBytes()).toBe(firstBytes + 1)
  stream.observeSourceLoss(second)
  expect(stream.sourceLossNotices()).toEqual([first])

  const reader = stream.claimConsumer()
  expect(await reader.next()).toEqual({ kind: 'control', item: first })
  await expect(reader.next()).resolves.toMatchObject({
    kind: 'control',
    item: { kind: 'terminal', reason: 'source-failed', error: { code: 'stream.quota' } }
  })
  expect(quota.retainedBytes()).toBe(0)
})

test('aggregate admission refuses source-loss retention before provenance is published', async () => {
  const loss = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '1', skippedRecords: null }
  const lossBytes = utf8ByteLength(JSON.stringify(loss))
  const stream = new CoreBoundedStream(limits(8, 512), 'drop-oldest')
  const quota = new AggregateStreamQuota(lossBytes)
  quota.register(stream)

  stream.observeSourceLoss(loss)
  expect(stream.sourceLossNotices()).toEqual([])
  const reader = stream.claimConsumer()
  await expect(reader.next()).resolves.toMatchObject({
    kind: 'control',
    item: { kind: 'terminal', reason: 'source-failed', error: { code: 'stream.quota' } }
  })
  expect(quota.retainedBytes()).toBe(0)
})

test('a pending claim read rejects a second read and yields to an already waiting iterator', async () => {
  const stream = new CoreBoundedStream(limits(), 'error')
  const iterator = stream[Symbol.asyncIterator]()
  const ordinary = iterator.next()
  const owner = stream.claimConsumer()
  const pending = owner.next()
  await expect(owner.next()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  stream.emit('ordinary', 3)
  expect((await ordinary).value.value).toBe('ordinary')
  stream.emit('claimed', 3)
  const claimed = await pending
  expect(claimed.kind).toBe('claim')
  expect(claimed.claim.item.value).toBe('claimed')
  claimed.claim.commit()
  owner.close()
})

test('claims retain ownership, exclude competing readers, and rollback restores FIFO', async () => {
  const stream = new CoreBoundedStream(limits(), 'drop-oldest')
  stream.emit('first', 4)
  stream.emit('second', 4)
  const owner = stream.claimConsumer()
  const claim = owner.claim()
  expect(claim.item).toEqual({ kind: 'value', value: 'first' })
  expect(stream.retainedBytes()).toBe(9)
  const next = stream[Symbol.asyncIterator]().next()
  claim.rollback()
  expect((await next).value.value).toBe('first')
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('second')
})

test('quota transfers only admitted claims and reservation stays charged until release', () => {
  const quota = new AggregateStreamQuota(10)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, 'record', 5)
  const owner = stream.claimConsumer()
  const claim = owner.claim()
  expect(() => quota.transfer(stream, claim, 0)).toThrow()
  expect(quota.transfer(stream, claim, 10)).toBeNull()
  expect(quota.retainedBytes()).toBe(6)
  const successor = quota.transfer(stream, claim, 9)
  expect(successor).not.toBeNull()
  expect(quota.retainedBytes()).toBe(10)
  expect(() => claim.commit()).toThrow()
  successor.release()
  successor.release()
  expect(quota.retainedBytes()).toBe(1)
})

test('claim generations and consumer close prevent stale or foreign settlement', () => {
  const quota = new AggregateStreamQuota(20)
  const first = new CoreBoundedStream(limits(), 'error')
  const second = new CoreBoundedStream(limits(), 'error')
  quota.register(first)
  quota.register(second)
  quota.emit(first, 'record', 3)
  const owner = first.claimConsumer()
  const old = owner.claim()
  expect(() => quota.transfer(second, old, 3)).toThrow()
  old.rollback()
  const current = owner.claim()
  expect(() => old.commit()).toThrow()
  owner.close()
  expect(() => current.commit()).toThrow()
  expect(first.retainedBytes()).toBe(4)
})

test('a claimed value survives overflow and precedes a terminal', async () => {
  const stream = new CoreBoundedStream(limits(1, 6), 'drop-oldest')
  stream.emit('accepted', 4)
  const claim = stream.claimConsumer().claim()
  expect(stream.emit('rejected', 4).accepted).toBe(false)
  stream.finishWithReason('source-failed')
  claim.rollback()
  const reader = stream[Symbol.asyncIterator]()
  expect((await reader.next()).value.kind).toBe('overflow')
  expect((await reader.next()).value.value).toBe('accepted')
  expect((await reader.next()).value.reason).toBe('source-failed')
})

test('transfer uses the source claim identity even if a caller replaces its commit method', () => {
  const quota = new AggregateStreamQuota(8)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, 'record', 5)
  const claim = stream.claimConsumer().claim()
  try {
    claim.commit = () => {}
  } catch {
    /* frozen claims are also safe */
  }
  const reservation = quota.transfer(stream, claim, 7)
  expect(quota.retainedBytes()).toBe(8)
  expect(stream.retainedPayloadBytes()).toBe(0)
  reservation.release()
})

test('a transfer exposes its reservation if terminal cleanup throws after commit', async () => {
  const quota = new AggregateStreamQuota(10)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, 'record', 5)
  const claim = stream.claimConsumer().claim()
  stream.finishWithReason('source-failed')
  let remainingCleanupRan = false
  stream.onRetainedValuesDrained(() => {
    throw new Error('cleanup failed')
  })
  stream.onRetainedValuesDrained(() => {
    remainingCleanupRan = true
  })
  const iterator = stream[Symbol.asyncIterator]()
  const terminal = iterator.next()
  const after = iterator.next()
  let failure
  try {
    quota.transfer(stream, claim, 6)
  } catch (error) {
    failure = error
  }
  expect(failure.cause.message).toBe('cleanup failed')
  expect(remainingCleanupRan).toBe(true)
  expect(failure.reservation.byteLength).toBe(6)
  expect((await terminal).value.reason).toBe('source-failed')
  await expect(after).resolves.toEqual({ done: true, value: undefined })
  expect(quota.retainedBytes()).toBe(6)
  failure.reservation.release()
  expect(quota.retainedBytes()).toBe(0)
})

test.each(['latest', 'drop-oldest'])('aggregate projection for %s includes retained views', policy => {
  const quota = new AggregateStreamQuota(10)
  const stream = new CoreBoundedStream(limits(1, 20), policy)
  stream.trackRetainedView({ retainedBytes: () => 4, onRetainedValuesDrained: () => {} })
  quota.register(stream)
  quota.emit(stream, 'old', 2, 'key')
  expect(quota.retainedBytes()).toBe(7)
  expect(quota.emit(stream, 'new', 6, 'key').accepted).toBe(false)
  expect(quota.retainedBytes()).toBeLessThanOrEqual(10)
})

test('error overflow keeps the claimed prefix and invalidates a claim only on explicit close', async () => {
  const stream = new CoreBoundedStream(limits(1, 6), 'error')
  stream.emit('accepted', 4)
  const owner = stream.claimConsumer()
  const claim = owner.claim()
  stream.emit('rejected', 4)
  claim.rollback()
  const reader = stream[Symbol.asyncIterator]()
  expect((await reader.next()).value.value).toBe('accepted')
  expect((await reader.next()).value.reason).toBe('overflow')
  const other = new CoreBoundedStream(limits(), 'error')
  other.emit('discarded', 3)
  const stale = other.claimConsumer().claim()
  await other.close()
  expect(() => stale.commit()).toThrow()
  expect((await other[Symbol.asyncIterator]().next()).value.reason).toBe('closed')
})

test('shared claim admission keeps one occurrence charged until its last reference releases', () => {
  const quota = new AggregateStreamQuota(15)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, { payload: 'accepted' }, 6)
  const claim = stream.claimConsumer().claim()
  expect(quota.shareClaim(stream, claim, 9, 3, 2)).toBeNull()
  expect(quota.retainedBytes()).toBe(7)
  const references = quota.shareClaim(stream, claim, 8, 3, 2)
  expect(references).toHaveLength(2)
  expect(references[0].value).toBe(references[1].value)
  expect(quota.retainedBytes()).toBe(15)
  references[0].release()
  references[0].release()
  expect(quota.retainedBytes()).toBe(12)
  references[1].release()
  expect(quota.retainedBytes()).toBe(1)
  expect(() => references[1].value).toThrow()
})

test('shared claim and cache resize admit together or leave both owners unchanged', () => {
  const quota = new AggregateStreamQuota(16)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  const cache = stream.createRetainedByteAdmission()
  expect(cache(3)).toBe(true)
  quota.emit(stream, 'record', 6)
  const claim = stream.claimConsumer().claim()
  expect(quota.shareClaim(stream, claim, 7, 2, 1, { admission: cache, bytes: 7 })).toBeNull()
  expect(stream.claimedBytes(claim)).toBe(6)
  expect(quota.retainedBytes()).toBe(10)
  const references = quota.shareClaim(stream, claim, 6, 2, 1, { admission: cache, bytes: 7 })
  expect(references).toHaveLength(1)
  expect(quota.retainedBytes()).toBe(16)
  references[0].release()
  expect(quota.retainedBytes()).toBe(8)
  expect(cache(0)).toBe(true)
  expect(quota.retainedBytes()).toBe(1)
})

test('shared claim reports the admitted references when source cleanup fails after commit', async () => {
  const quota = new AggregateStreamQuota(20)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, 'record', 5)
  const claim = stream.claimConsumer().claim()
  stream.finishWithReason('source-failed')
  stream.onRetainedValuesDrained(() => {
    throw new Error('cleanup failed')
  })
  const terminal = stream[Symbol.asyncIterator]().next()
  let failure
  try {
    quota.shareClaim(stream, claim, 6, 2, 2)
  } catch (error) {
    failure = error
  }
  expect(failure.cause.message).toBe('cleanup failed')
  expect(failure.references).toHaveLength(2)
  expect(quota.retainedBytes()).toBe(10)
  expect((await terminal).value.reason).toBe('source-failed')
  failure.references[0].release()
  failure.references[1].release()
  expect(quota.retainedBytes()).toBe(0)
})

test('shared claim rejects invalid reference budgets without consuming the source', async () => {
  const quota = new AggregateStreamQuota(20)
  const stream = new CoreBoundedStream(limits(), 'error')
  quota.register(stream)
  quota.emit(stream, 'original', 5)
  const owner = stream.claimConsumer()
  const claim = owner.claim()
  for (const [ownerBytes, referenceBytes, referenceCount] of [
    [0, 1, 1],
    [1, 0, 1],
    [1, 1, 0],
    [1, 1, 1.5],
    [1, Number.MAX_SAFE_INTEGER, 2]
  ]) {
    expect(() => quota.shareClaim(stream, claim, ownerBytes, referenceBytes, referenceCount)).toThrow()
  }
  expect(quota.retainedBytes()).toBe(6)
  owner.close()
  expect((await stream[Symbol.asyncIterator]().next()).value.value).toBe('original')
})
