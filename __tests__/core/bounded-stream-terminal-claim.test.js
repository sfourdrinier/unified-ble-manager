const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { resourceCount } = require('../../src/backend-contract/primitives')

const limits = { itemCapacity: resourceCount(3), byteCapacity: resourceCount(100), reservedControlCapacity: resourceCount(1) }
const failure = { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never', platform: { domain: 'native', code: 'failure', safeMessage: 'detail' } }
const terminalReader = stream => stream.claimConsumer({ retainTerminal: true })

test('source loss retains its exact provenance while the terminal claim preserves the accepted FIFO prefix', async () => {
  const stream = new CoreBoundedStream(limits, 'drop-newest')
  const sourceLosses = []
  stream.onSourceLoss(notice => sourceLosses.push(notice))
  stream.emit('first', 3)
  const sourceLoss = { kind: 'source-loss', scope: 'host-global', source: 'advertisement-ingress', lostRecords: resourceCount(2) }
  stream.observeSourceLoss(sourceLoss)
  stream.finishWithReason('source-failed', failure)
  const owner = terminalReader(stream)
  expect(sourceLosses).toEqual([sourceLoss])
  expect(stream.sourceLossNotices()).toEqual([sourceLoss])
  const value = await owner.next()
  expect(value.claim.item.value).toBe('first')
  value.claim.commit()
  const retained = stream.retainedBytes()
  const result = await owner.next()
  expect(result.kind).toBe('terminal-claim')
  expect(result.claim.item).toMatchObject({ kind: 'terminal', reason: 'source-failed', error: failure })
  expect(stream.claimedTerminalBytes(result.claim)).toBeGreaterThan(0)
  expect(stream.retainedBytes()).toBe(retained)
  result.claim.rollback()
  const terminal = (await stream[Symbol.asyncIterator]().next()).value
  expect(terminal).toBe(result.claim.item)
  expect(stream.retainedBytes()).toBe(0)
  expect(() => result.claim.commit()).toThrow()
})

test('terminal commit excludes competing claim and ordinary readers and settles cleanup once', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  let drained = 0
  stream.onRetainedValuesDrained(() => { drained++ })
  stream.finishWithReason('source-failed', failure)
  const owner = terminalReader(stream)
  const claim = (await owner.next()).claim
  const other = terminalReader(stream)
  const competing = other.next()
  const iterator = stream[Symbol.asyncIterator]()
  const ordinary = iterator.next()
  expect(drained).toBe(0)
  claim.commit()
  expect(await ordinary).toEqual({ done: true, value: undefined })
  expect(await competing).toEqual({ kind: 'done' })
  expect(drained).toBe(1)
  expect(() => claim.rollback()).toThrow()
  expect(stream.retainedBytes()).toBe(0)
})

test('reader cancellation rolls back a terminal claim and rejects stale or foreign identities', async () => {
  const first = new CoreBoundedStream(limits, 'error')
  const second = new CoreBoundedStream(limits, 'error')
  first.finishWithReason('source-failed', failure)
  const owner = terminalReader(first)
  const old = (await owner.next()).claim
  expect(() => second.claimedTerminalBytes(old)).toThrow()
  owner.close()
  expect(() => old.commit()).toThrow()
  const replacement = terminalReader(first)
  const current = (await replacement.next()).claim
  expect(current.item).toBe(old.item)
  expect(() => old.rollback()).toThrow()
  replacement.close()
  expect((await first[Symbol.asyncIterator]().next()).value).toBe(current.item)
})

test('explicit stream close invalidates a held terminal and exposes its replacement', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  stream.finishWithReason('source-failed', failure)
  const claim = (await terminalReader(stream).next()).claim
  await stream.close()
  expect(() => claim.commit()).toThrow()
  expect((await stream[Symbol.asyncIterator]().next()).value.reason).toBe('closed')
})

test('cleanup failure after terminal commit does not resurrect the claim or strand waiters', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  stream.finishWithReason('source-failed', failure)
  stream.onRetainedValuesDrained(() => { throw new Error('cleanup failure') })
  const claim = (await terminalReader(stream).next()).claim
  const pending = stream[Symbol.asyncIterator]().next()
  expect(() => claim.commit()).toThrow('cleanup failure')
  expect(await pending).toEqual({ done: true, value: undefined })
  expect(() => claim.rollback()).toThrow()
  expect(stream.retainedBytes()).toBe(0)
})

test('an already waiting iterator wins over an opted-in reader; cancelling a pending read does not consume a terminal', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const ordinary = stream[Symbol.asyncIterator]().next()
  const owner = terminalReader(stream)
  const pending = owner.next()
  await expect(owner.next()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  stream.finishWithReason('source-failed', failure)
  expect((await ordinary).value.error).toBe(failure)
  expect(await pending).toEqual({ kind: 'done' })

  const another = new CoreBoundedStream(limits, 'error')
  const cancelled = terminalReader(another)
  const wait = cancelled.next()
  cancelled.close()
  expect(await wait).toEqual({ kind: 'done' })
  another.finishWithReason('source-failed', failure)
  expect((await another[Symbol.asyncIterator]().next()).value.error).toBe(failure)
})

test('rollback wakes a waiting iterator with the identical terminal and releases the aggregate charge', async () => {
  const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
  const quota = new AggregateStreamQuota(1000)
  const stream = new CoreBoundedStream(limits, 'error')
  quota.register(stream)
  stream.finishWithReason('source-failed', failure)
  const claim = (await terminalReader(stream).next()).claim
  const held = quota.retainedBytes()
  expect(held).toBeGreaterThan(1)
  const pending = stream[Symbol.asyncIterator]().next()
  expect(quota.retainedBytes()).toBe(held)
  claim.rollback()
  expect((await pending).value).toBe(claim.item)
  expect(quota.retainedBytes()).toBe(0)
})

test('peer loss preserves its exact provenance and later value before the terminal claim', async () => {
  const stream = new CoreBoundedStream(limits, 'drop-newest')
  const peerLoss = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '123', skippedRecords: null }
  const sourceLosses = []
  stream.onSourceLoss(notice => sourceLosses.push(notice))
  stream.emit('before', 3)
  stream.observeSourceLoss(peerLoss)
  expect(stream.emit('after', 3)).toMatchObject({ accepted: true, terminated: false })
  stream.finishWithReason('owner-released')
  const reader = terminalReader(stream)
  const first = await reader.next()
  expect(first.claim.item.value).toBe('before')
  first.claim.commit()
  expect(sourceLosses).toEqual([peerLoss])
  expect(stream.sourceLossNotices()).toEqual([peerLoss])
  expect(await reader.next()).toEqual({ kind: 'control', item: peerLoss })
  const after = await reader.next()
  expect(after.claim.item.value).toBe('after')
  after.claim.commit()
  const terminal = (await reader.next()).claim
  expect(terminal.item.reason).toBe('overflow')
  expect(stream.claimedTerminalBytes(terminal)).toBe(0)
  terminal.commit()
  expect(await reader.next()).toEqual({ kind: 'done' })
})
