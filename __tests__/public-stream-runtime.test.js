// __tests__/public-stream-runtime.test.js

const { capacity } = require('../src/backend-contract/primitives')
const { contractError } = require('../src/backend-contract/errors')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { mapPublicBoundedAsyncStream } = require('../src/public/streams')
const { BleError } = require('../src/public/errors')

function limits(itemCapacity, byteCapacity, reservedControlCapacity) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(byteCapacity),
    reservedControlCapacity: capacity(reservedControlCapacity)
  }
}

describe('public stream runtime projection', () => {
  test('projects values, overflow counters, terminal counters, and ordering without buffering another queue', async () => {
    const source = new CoreBoundedStream(limits(2, 5, 1), 'drop-oldest')
    source.emit(new Uint8Array([1, 2]), 2)
    source.emit(new Uint8Array([3, 4]), 2)
    source.emit(new Uint8Array([5, 6]), 2)
    source.finishWithReason('connection-lost')

    const publicStream = mapPublicBoundedAsyncStream(source, value => new Uint8Array(value))
    expect(publicStream.limits).toEqual({ itemCapacity: 2, byteCapacity: 5, reservedControlCapacity: 1 })
    expect(typeof publicStream.limits.itemCapacity).toBe('number')

    const iterator = publicStream[Symbol.asyncIterator]()
    const overflow = await iterator.next()
    const retained = await iterator.next()
    const latest = await iterator.next()
    const terminal = await iterator.next()
    const done = await iterator.next()

    expect(overflow).toEqual({
      done: false,
      value: {
        kind: 'overflow',
        policy: 'drop-oldest',
        droppedItems: 1,
        droppedBytes: 2,
        replacedItems: 0
      }
    })
    expect(retained).toEqual({ done: false, value: { kind: 'value', value: new Uint8Array([3, 4]) } })
    expect(latest).toEqual({ done: false, value: { kind: 'value', value: new Uint8Array([5, 6]) } })
    expect(terminal).toEqual({
      done: false,
      value: {
        kind: 'terminal',
        reason: 'connection-lost',
        droppedItems: 1,
        droppedBytes: 2,
        replacedItems: 0
      }
    })
    expect(done).toEqual({ done: true, value: undefined })
    expect(typeof overflow.value.droppedItems).toBe('number')
  })

  test('returns the source iterator and maps close cleanup while preserving idempotent teardown', async () => {
    const source = new CoreBoundedStream(limits(2, 6, 1), 'drop-newest')
    const publicStream = mapPublicBoundedAsyncStream(source, value => value)
    const iterator = publicStream[Symbol.asyncIterator]()
    const pending = iterator.next()

    await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
    await expect(pending).resolves.toEqual({ done: true, value: undefined })
    await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
    await expect(publicStream.close()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('rehydrates source close rejection through the public error bridge', async () => {
    const sourceError = contractError('platform.failure', 'stream', 'public-stream-runtime.close')
    const source = {
      limits: limits(1, 4, 1),
      overflowPolicy: 'error',
      [Symbol.asyncIterator]() {
        return {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        }
      },
      close: async () => {
        throw sourceError
      }
    }
    const publicStream = mapPublicBoundedAsyncStream(source, value => value)

    await expect(publicStream.close()).rejects.toBeInstanceOf(BleError)
    await expect(publicStream.close()).rejects.toMatchObject({
      code: 'platform.failure',
      domain: 'stream',
      operation: 'public-stream-runtime.close'
    })
  })
})
