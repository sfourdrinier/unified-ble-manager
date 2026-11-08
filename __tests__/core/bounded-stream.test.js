// __tests__/core/bounded-stream.test.js

const { capacity, resourceCount } = require('../../src/backend-contract/primitives')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')

function limits(itemCapacity, byteCapacity, reservedControlCapacity) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(byteCapacity),
    reservedControlCapacity: capacity(reservedControlCapacity)
  }
}

describe('CoreBoundedStream', () => {
  test('retains coalesced host-global uncertainty alongside exact overflow through finish', async () => {
    const stream = new CoreBoundedStream(limits(1, 8, 1), 'drop-newest')
    stream.emit('kept', 2)
    stream.emit('lost', 3)
    const notice = {
      kind: 'source-loss',
      scope: 'host-global',
      source: 'notification-ingress',
      lostRecords: resourceCount(2)
    }
    stream.observeSourceLoss(notice)
    stream.observeSourceLoss(notice)
    stream.finishWithReason('closed')
    const iterator = stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toEqual({ ...notice, lostRecords: 4 })
    expect((await iterator.next()).value).toMatchObject({ kind: 'overflow', droppedItems: 1, droppedBytes: 3 })
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', value: 'kept' })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', droppedItems: 1, droppedBytes: 3 })
  })

  test('explicit close after source loss releases queued payload while preserving loss and terminal', async () => {
    const stream = new CoreBoundedStream(limits(2, 10, 1), 'error')
    stream.emit('queued', 4)
    stream.observeSourceLoss({
      kind: 'source-loss',
      scope: 'host-global',
      source: 'notification-ingress',
      lostRecords: resourceCount(1)
    })
    await stream.close()
    expect(stream.retainedPayloadBytes()).toBe(0)
    const iterator = stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'source-loss', lostRecords: 1 })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    expect((await iterator.next()).done).toBe(true)
  })

  test('error policy exposes global uncertainty before its terminal without inventing counters', async () => {
    const stream = new CoreBoundedStream(limits(1, 8, 1), 'error')
    stream.observeSourceLoss({
      kind: 'source-loss',
      scope: 'host-global',
      source: 'advertisement-ingress',
      lostRecords: resourceCount(3)
    })
    stream.closeWithReason('owner-released')
    const iterator = stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'source-loss', lostRecords: 3 })
    expect((await iterator.next()).value).toMatchObject({
      kind: 'terminal',
      reason: 'overflow',
      droppedItems: 0,
      droppedBytes: 0
    })
  })

  test('emits an overflow notice before the retained item after drop-oldest overflow', async () => {
    const stream = new CoreBoundedStream(limits(2, 5, 1), 'drop-oldest')
    stream.emit('first', 2)
    stream.emit('second', 2)
    stream.emit('third', 2)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'overflow' } })
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'second' } })
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'third' } })
    expect(stream.overflowCounters()).toMatchObject({ droppedItems: 1, droppedBytes: 2, replacedItems: 0 })
  })

  test('uses an exact latest key and refuses a non-keyed overflow value', async () => {
    const stream = new CoreBoundedStream(limits(1, 5, 1), 'latest')
    stream.emit('initial', 2, 'peer-a')
    stream.emit('replacement', 2, 'peer-a')
    stream.emit('unkeyed', 2)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'overflow' } })
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: 'replacement' }
    })
    expect(stream.overflowCounters()).toMatchObject({ droppedItems: 1, droppedBytes: 4, replacedItems: 1 })
  })

  test('terminalReason is readable without consuming the iterator and survives delivery', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-oldest')
    expect(stream.terminalReason()).toBeNull()
    stream.finishWithReason('source-failed')
    expect(stream.isTerminal()).toBe(true)
    expect(stream.terminalReason()).toBe('source-failed')
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    expect(stream.terminalReason()).toBe('source-failed')
  })

  test('closes ingress before resolving cleanup and never exposes a later value', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    stream.emit('pending', 2)
    await expect(stream.close()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stream.emit('late', 2)).toMatchObject({ accepted: false, terminated: true })
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'terminal', reason: 'closed' } })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('finishes after retained values and preserves overflow visibility before the terminal', async () => {
    const stream = new CoreBoundedStream(limits(2, 5, 1), 'drop-oldest')
    stream.emit('first', 2)
    stream.emit('second', 2)
    stream.emit('third', 2)
    stream.finishWithReason('connection-lost')
    expect(stream.emit('late', 2)).toMatchObject({ accepted: false, terminated: true })
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'overflow', droppedItems: 1, droppedBytes: 2 }
    })
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'second' } })
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'third' } })
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'connection-lost' }
    })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('preserves a structured source failure on the terminal without changing drop counters', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-oldest')
    const error = {
      code: 'platform.transport',
      domain: 'stream',
      operation: 'tauri.event-send',
      platform: {
        domain: 'btleplug',
        code: 'native-error',
        safeMessage: 'native channel closed',
        metadata: {}
      },
      retryability: 'never'
    }
    stream.emit('peer', 2)
    stream.finishWithReason('source-failed', error)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: 'peer' } })
    await expect(iterator.next()).resolves.toMatchObject({
      value: {
        kind: 'terminal',
        reason: 'source-failed',
        droppedItems: 0,
        droppedBytes: 0,
        error
      }
    })
  })

  test.each(['closed', 'overflow', 'source-failed'])(
    'delivers one %s terminal then settles every concurrent remaining reader',
    async reason => {
      const stream = new CoreBoundedStream(limits(2, 6, 1), 'error')
      const iterator = stream[Symbol.asyncIterator]()
      const first = iterator.next()
      const second = iterator.next()
      const third = iterator.next()

      stream.closeWithReason(reason)

      await expect(first).resolves.toMatchObject({ done: false, value: { kind: 'terminal', reason } })
      await expect(second).resolves.toEqual({ done: true, value: undefined })
      await expect(third).resolves.toEqual({ done: true, value: undefined })
      await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
    }
  )

  test('return removes only its pending reads and preserves delivery for other iterators', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    const cancelled = stream[Symbol.asyncIterator]()
    const active = stream[Symbol.asyncIterator]()
    const firstCancelledNext = cancelled.next()
    const secondCancelledNext = cancelled.next()
    const activeNext = active.next()

    await expect(cancelled.return()).resolves.toEqual({ done: true, value: undefined })
    await expect(firstCancelledNext).resolves.toEqual({ done: true, value: undefined })
    await expect(secondCancelledNext).resolves.toEqual({ done: true, value: undefined })
    expect(stream.consumers).toHaveLength(1)

    stream.emit('active-value', 2)

    await expect(activeNext).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'active-value' } })
    await expect(cancelled.next()).resolves.toEqual({ done: true, value: undefined })
    expect(stream.consumers).toHaveLength(0)
    expect(stream.retainedBytes()).toBe(1)
  })

  test.each(['closed', 'source-failed', 'overflow'])(
    'return cannot consume or revive another iterator after a %s terminal',
    async reason => {
      const stream = new CoreBoundedStream(limits(2, 6, 1), 'error')
      const returned = stream[Symbol.asyncIterator]()
      const active = stream[Symbol.asyncIterator]()
      const returnedNext = returned.next()
      const activeNext = active.next()

      await returned.return()
      stream.closeWithReason(reason)

      await expect(returnedNext).resolves.toEqual({ done: true, value: undefined })
      await expect(activeNext).resolves.toMatchObject({ done: false, value: { kind: 'terminal', reason } })
      await expect(returned.next()).resolves.toEqual({ done: true, value: undefined })
      await expect(active.next()).resolves.toEqual({ done: true, value: undefined })
      expect(stream.consumers).toHaveLength(0)
    }
  )

  test('return is idempotent and cannot steal an already delivered item', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    const iterator = stream[Symbol.asyncIterator]()
    const next = iterator.next()

    stream.emit('delivered-before-return', 2)

    await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
    await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
    await expect(next).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: 'delivered-before-return' }
    })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
    expect(stream.consumers).toHaveLength(0)
    expect(stream.retainedBytes()).toBe(1)
  })

  test('repeated iterator teardown settles every pending read without retaining a consumer queue', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')

    for (let index = 0; index < 8; index += 1) {
      const iterator = stream[Symbol.asyncIterator]()
      const pending = iterator.next()

      await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
      await expect(pending).resolves.toEqual({ done: true, value: undefined })
      expect(stream.consumers).toHaveLength(0)
      expect(stream.retainedBytes()).toBe(1)
    }
  })

  test('coalesces upstream bounded-ingress loss before its next value without discarding the control notice', async () => {
    const stream = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    stream.observeSourceOverflow({
      kind: 'overflow',
      policy: 'drop-oldest',
      droppedItems: resourceCount(3),
      droppedBytes: resourceCount(9),
      replacedItems: resourceCount(0)
    })
    stream.emit('retained', 2)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'overflow', policy: 'drop-oldest', droppedItems: 3, droppedBytes: 9 }
    })
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'retained' } })
    expect(stream.overflowCounters()).toMatchObject({ droppedItems: 3, droppedBytes: 9, replacedItems: 0 })
  })

  test('enforces an aggregate quota before producer retention and exposes one overflow terminal', async () => {
    const quota = new AggregateStreamQuota(7)
    const first = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    const second = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    quota.register(first)
    quota.register(second)

    quota.emit(first, 'first-value', 2)
    quota.emit(second, 'second-value', 2)
    expect(quota.retainedBytes()).toBe(6)
    quota.emit(second, 'overflow', 2)
    const iterator = second[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'overflow' }
    })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })
})
