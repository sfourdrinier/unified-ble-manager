const { capacity } = require('../../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')

function limits() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(64),
    reservedControlCapacity: capacity(8)
  }
}

describe('CoreBoundedStream close after finish', () => {
  test('finish then close discards queued values and yields the close reason', async () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    stream.finishWithReason('closed')
    stream.closeWithReason('owner-released')
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('finish without close still drains queued values before terminal', async () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    stream.finishWithReason('closed')
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { kind: 'value', value: 'queued' } })
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'closed' }
    })
  })

  test('second close after owner close is a no-op', async () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    stream.closeWithReason('owner-released')
    stream.closeWithReason('closed')
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
  })

  test('close after finish zeros retained value and payload bytes', () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    expect(stream.retainedPayloadBytes()).toBe(8)
    stream.finishWithReason('closed')
    stream.closeWithReason('owner-released')
    expect(stream.retainedPayloadBytes()).toBe(0)
    expect(stream.retainedBytes()).toBeGreaterThanOrEqual(0)
  })

  test('close after finish clears pending overflow accounting exactly once', async () => {
    const stream = new CoreBoundedStream(
      {
        itemCapacity: capacity(1),
        byteCapacity: capacity(16),
        reservedControlCapacity: capacity(8)
      },
      'drop-oldest'
    )
    stream.emit('first', 6)
    stream.emit('second', 6)
    stream.finishWithReason('closed')
    stream.closeWithReason('owner-released')
    const iterator = stream[Symbol.asyncIterator]()
    const first = await iterator.next()
    expect(first.value.kind === 'overflow' || first.value.kind === 'terminal').toBe(true)
    if (first.value.kind === 'overflow') {
      await expect(iterator.next()).resolves.toMatchObject({
        done: false,
        value: { kind: 'terminal', reason: 'owner-released' }
      })
    }
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('pending reader and concurrent close observe one terminal and no value', async () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    stream.finishWithReason('closed')
    const iterator = stream[Symbol.asyncIterator]()
    stream.closeWithReason('owner-released')
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('finish after owner close cannot resurrect draining state', async () => {
    const stream = new CoreBoundedStream(limits(), 'drop-oldest')
    stream.emit('queued', 8)
    stream.closeWithReason('owner-released')
    stream.finishWithReason('closed')
    expect(stream.emit('late', 8)).toMatchObject({ accepted: false, terminated: true })
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
  })
})
