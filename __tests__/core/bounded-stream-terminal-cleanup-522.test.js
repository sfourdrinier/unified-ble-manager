const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
const { resourceCount } = require('../../src/backend-contract/primitives')

const limits = {
  itemCapacity: resourceCount(3),
  byteCapacity: resourceCount(512),
  reservedControlCapacity: resourceCount(1)
}
const failure = {
  code: 'platform.failure',
  domain: 'scan',
  operation: 'scan',
  retryability: 'never',
  platform: { domain: 'native', code: 'failure', safeMessage: 'detail' }
}

test('close retains aggregate charge through terminal claim, rollback, and final delivery', async () => {
  const quota = new AggregateStreamQuota(1000)
  const stream = new CoreBoundedStream(limits, 'error')
  quota.register(stream)
  const unregister = jest.fn(() => quota.unregister(stream))
  stream.onRetainedValuesDrained(unregister)
  stream.closeWithReason('source-failed', failure)
  const held = stream.retainedBytes()
  expect(held).toBeGreaterThan(1)
  expect(quota.retainedBytes()).toBe(held)
  const reader = stream.claimConsumer({ retainTerminal: true })
  const claim = (await reader.next()).claim
  claim.rollback()
  expect(quota.retainedBytes()).toBe(held)
  expect((await stream[Symbol.asyncIterator]().next()).value).toBe(claim.item)
  expect(unregister).toHaveBeenCalledTimes(1)
  expect(quota.retainedBytes()).toBe(0)
})

test('closing after peer control loss retains its control reservation through terminal commit', async () => {
  const quota = new AggregateStreamQuota(1000)
  const stream = new CoreBoundedStream(limits, 'drop-newest')
  quota.register(stream)
  stream.onRetainedValuesDrained(() => quota.unregister(stream))
  const loss = { kind: 'source-loss', scope: 'peer', peerId: 'peer', generation: '1', skippedRecords: null }
  stream.observeSourceLoss(loss)
  stream.observeSourceLoss(loss)
  await stream.close()
  const held = stream.retainedBytes()
  expect(held).toBeGreaterThan(1)
  expect(quota.retainedBytes()).toBe(held)
  const reader = stream.claimConsumer({ retainTerminal: true })
  expect((await reader.next()).item).toEqual(loss)
  expect((await reader.next()).item).toEqual(loss)
  const claim = (await reader.next()).claim
  expect(claim.item.error).toBeUndefined()
  expect(stream.claimedTerminalBytes(claim)).toBe(0)
  claim.commit()
  expect(quota.retainedBytes()).toBe(0)
})

test('source-loss cleanup failure reports the exact terminal evidence once without deadlocking the reader', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const loss = {
    kind: 'source-loss',
    scope: 'host-global',
    source: 'notification-ingress',
    lostRecords: resourceCount(1)
  }
  const evidence = {
    ref: 'source-loss-terminal',
    byteLength: 1,
    digest: 'sha256:source-loss-terminal',
    formatVersion: 1,
    durability: 'committed',
    scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' }
  }
  const cleanup = new Error('source-loss cleanup failed')
  stream.observeSourceLoss(loss)
  stream.onRetainedValuesDrained(() => {
    throw cleanup
  })
  stream.finishWithReason('source-failed', failure, evidence)

  const iterator = stream[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({
    item: { kind: 'terminal', reason: 'source-failed', error: failure, evidence },
    cause: cleanup
  })
  await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
})

test('pending hydration reports source-loss cleanup failure with its exact terminal once', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const loss = {
    kind: 'source-loss',
    scope: 'host-global',
    source: 'notification-ingress',
    lostRecords: resourceCount(1)
  }
  const evidence = {
    ref: 'hydrated-source-loss-terminal',
    byteLength: 1,
    digest: 'sha256:hydrated-source-loss-terminal',
    formatVersion: 1,
    durability: 'committed',
    scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' }
  }
  const cleanup = new Error('hydrated source-loss cleanup failed')
  stream.setTerminalEvidenceHydrator(async terminal => terminal)
  stream.observeSourceLoss(loss)
  stream.onRetainedValuesDrained(() => {
    throw cleanup
  })
  const iterator = stream[Symbol.asyncIterator]()
  const pending = iterator.next()
  stream.finishWithReason('source-failed', failure, evidence)

  await expect(pending).rejects.toMatchObject({
    item: { kind: 'terminal', reason: 'source-failed', error: failure, evidence },
    cause: cleanup
  })
  await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
})

test('immediate hydration reports source-loss cleanup failure with its exact terminal once', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const loss = {
    kind: 'source-loss',
    scope: 'host-global',
    source: 'notification-ingress',
    lostRecords: resourceCount(1)
  }
  const evidence = {
    ref: 'immediate-hydrated-source-loss-terminal',
    byteLength: 1,
    digest: 'sha256:immediate',
    formatVersion: 1,
    durability: 'committed',
    scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' }
  }
  const cleanup = new Error('immediate hydrated source-loss cleanup failed')
  stream.setTerminalEvidenceHydrator(async terminal => terminal)
  stream.observeSourceLoss(loss)
  stream.onRetainedValuesDrained(() => {
    throw cleanup
  })
  stream.finishWithReason('source-failed', failure, evidence)
  const iterator = stream[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ item: { kind: 'terminal', evidence }, cause: cleanup })
  await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
})

test('a callback registered after close waits for terminal and retained view release', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  let releaseView
  stream.trackRetainedView({
    retainedBytes: () => 4,
    onRetainedValuesDrained: callback => {
      releaseView = callback
    }
  })
  stream.closeWithReason('source-failed', failure)
  const drained = jest.fn()
  stream.onRetainedValuesDrained(drained)
  releaseView()
  expect(drained).not.toHaveBeenCalled()
  const claim = (await stream.claimConsumer({ retainTerminal: true }).next()).claim
  claim.commit()
  expect(drained).toHaveBeenCalledTimes(1)
})

test('terminal delivery waits for a retained view before releasing aggregate charge', async () => {
  const quota = new AggregateStreamQuota(1000)
  const stream = new CoreBoundedStream(limits, 'error')
  let releaseView
  stream.trackRetainedView({
    retainedBytes: () => 4,
    onRetainedValuesDrained: callback => {
      releaseView = callback
    }
  })
  quota.register(stream)
  const unregister = jest.fn(() => quota.unregister(stream))
  stream.onRetainedValuesDrained(unregister)
  stream.closeWithReason('source-failed', failure)
  const claim = (await stream.claimConsumer({ retainTerminal: true }).next()).claim
  claim.commit()
  expect(quota.retainedBytes()).toBe(4)
  expect(unregister).not.toHaveBeenCalled()
  releaseView()
  expect(unregister).toHaveBeenCalledTimes(1)
  expect(quota.retainedBytes()).toBe(0)
})

test.each(['ordinary', 'legacy claim'])(
  '%s reader reports cleanup failure with the delivered terminal exactly once',
  async kind => {
    const stream = new CoreBoundedStream(limits, 'error')
    stream.finishWithReason('source-failed', failure)
    const cleanupError = new Error('cleanup failed')
    const cleanup = jest.fn(() => {
      throw cleanupError
    })
    const otherCleanup = jest.fn()
    stream.onRetainedValuesDrained(cleanup)
    stream.onRetainedValuesDrained(otherCleanup)
    const reader = kind === 'ordinary' ? stream[Symbol.asyncIterator]() : stream.claimConsumer()
    let pending
    expect(() => {
      pending = reader.next()
    }).not.toThrow()
    let rejection
    try {
      await pending
    } catch (error) {
      rejection = error
    }
    expect(rejection).toMatchObject({
      item: { kind: 'terminal', reason: 'source-failed', error: failure },
      cause: cleanupError
    })
    expect(cleanup).toHaveBeenCalledTimes(1)
    expect(otherCleanup).toHaveBeenCalledTimes(1)
    expect(await reader.next()).toEqual(kind === 'ordinary' ? { done: true, value: undefined } : { kind: 'done' })
    expect(cleanup).toHaveBeenCalledTimes(1)
  }
)

test('a waiting iterator receives its terminal while the producer reports cleanup failure', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const reader = stream[Symbol.asyncIterator]()
  const pending = reader.next()
  const after = reader.next()
  const cleanupError = new Error('cleanup failed')
  stream.onRetainedValuesDrained(() => {
    throw cleanupError
  })
  expect(() => stream.finishWithReason('source-failed', failure)).toThrow(cleanupError)
  expect((await pending).value).toMatchObject({ kind: 'terminal', error: failure })
  expect(await after).toEqual({ done: true, value: undefined })
})

test('pending delivery reports both value release and terminal cleanup failures', async () => {
  const stream = new CoreBoundedStream(limits, 'error')
  const releaseError = new Error('value release failed')
  const cleanupError = new Error('terminal cleanup failed')
  const stage = stream.stageOwned(
    {
      value: 'value',
      release: () => {
        throw releaseError
      }
    },
    2,
    value => value
  )
  const reader = stream[Symbol.asyncIterator]()
  const value = reader.next()
  const terminal = reader.next()
  stream.onRetainedValuesDrained(() => {
    throw cleanupError
  })
  stream.finishWithReason('source-failed', failure)
  let rejection
  try {
    stage.publish()
  } catch (error) {
    rejection = error
  }
  expect(rejection).toBeInstanceOf(AggregateError)
  expect(rejection.errors).toEqual(expect.arrayContaining([releaseError, cleanupError]))
  expect((await value).value.value).toBe('value')
  expect((await terminal).value.error).toBe(failure)
})

test('pending delivery reports release and copy failures and preserves the queued retry', async () => {
  const releaseError = new Error('value release failed')
  const copyError = new Error('value copy failed')
  let failCopy = true
  const another = new CoreBoundedStream(limits, 'error')
  const held = another.stageOwned(
    {
      value: 'first',
      release: () => {
        throw releaseError
      }
    },
    2,
    value => value
  )
  const blocked = another.stageOwned({ value: 'second', release: () => {} }, 2, value => {
    if (failCopy) throw copyError
    return value
  })
  held.publish()
  const pendingReader = another[Symbol.asyncIterator]()
  // A held claim prevents the first next from delivering synchronously.
  const claim = another.claimConsumer().claim()
  const pendingValue = pendingReader.next()
  const retry = pendingReader.next()
  blocked.publish()
  let rejection
  try {
    claim.rollback()
  } catch (error) {
    rejection = error
  }
  expect(rejection).toBeInstanceOf(AggregateError)
  expect(rejection.errors).toEqual(expect.arrayContaining([releaseError, copyError]))
  expect((await pendingValue).value.value).toBe('first')
  failCopy = false
  another.finishWithReason('closed')
  expect((await retry).value.value).toBe('second')
})
