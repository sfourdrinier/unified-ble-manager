// __tests__/public-stream-runtime.test.js

const { capacity } = require('../src/backend-contract/primitives')
const { contractError } = require('../src/backend-contract/errors')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { mapPublicBoundedAsyncStream } = require('../src/public/streams')
const { BleError } = require('../src/public/errors')
const { rehydratePublicError } = require('../src/public/error-bridge')
const { resolveStreamPolicy } = require('../src/public/stream-presets')

function limits(itemCapacity, byteCapacity, reservedControlCapacity) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(byteCapacity),
    reservedControlCapacity: capacity(reservedControlCapacity)
  }
}

describe('public stream runtime projection', () => {
  function sourceWithIterator(iterator, close = async () => ({ state: 'released', failures: [] })) {
    return {
      limits: limits(1, 4, 1),
      overflowPolicy: 'error',
      [Symbol.asyncIterator]: () => iterator,
      close
    }
  }

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

  test('rehydrates source next and return failures without stealing iterator teardown', async () => {
    const nextError = contractError('platform.failure', 'stream', 'public-stream-runtime.next')
    const returnError = contractError('platform.failure', 'stream', 'public-stream-runtime.return')
    const nextSourceIterator = {
      next: async () => {
        throw nextError
      },
      return: jest.fn(async () => ({ done: true, value: undefined })),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const nextPublicStream = mapPublicBoundedAsyncStream(sourceWithIterator(nextSourceIterator), value => value)
    const nextIterator = nextPublicStream[Symbol.asyncIterator]()

    await expect(nextIterator.next()).rejects.toMatchObject({
      code: 'platform.failure',
      operation: 'public-stream-runtime.next'
    })
    expect(nextSourceIterator.return).toHaveBeenCalledTimes(1)

    const returnSourceIterator = {
      next: async () => ({ done: true, value: undefined }),
      return: jest.fn(async () => {
        throw returnError
      }),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const returnPublicStream = mapPublicBoundedAsyncStream(sourceWithIterator(returnSourceIterator), value => value)
    await expect(returnPublicStream[Symbol.asyncIterator]().return()).rejects.toMatchObject({
      code: 'platform.failure',
      operation: 'public-stream-runtime.return'
    })
    expect(returnSourceIterator.return).toHaveBeenCalledTimes(1)
  })

  test('retries a rejected source return, accepts an optional return, and rejects malformed return members', async () => {
    const returnError = contractError('platform.failure', 'stream', 'public-stream-runtime.return-retry')
    let returnAttempts = 0
    const retrySourceIterator = {
      next: async () => ({ done: true, value: undefined }),
      return: jest.fn(async () => {
        returnAttempts += 1
        if (returnAttempts === 1) throw returnError
        return { done: true, value: undefined }
      }),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const retryStream = mapPublicBoundedAsyncStream(sourceWithIterator(retrySourceIterator), value => value)
    const retryIterator = retryStream[Symbol.asyncIterator]()
    await expect(retryIterator.return()).rejects.toMatchObject({
      code: 'platform.failure',
      operation: 'public-stream-runtime.return-retry'
    })
    await expect(retryIterator.return()).resolves.toEqual({ done: true, value: undefined })
    expect(retrySourceIterator.return).toHaveBeenCalledTimes(2)

    const optionalReturnStream = mapPublicBoundedAsyncStream(
      sourceWithIterator({
        next: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      value => value
    )
    await expect(optionalReturnStream[Symbol.asyncIterator]().return()).resolves.toEqual({
      done: true,
      value: undefined
    })

    const malformedReturnStream = mapPublicBoundedAsyncStream(
      sourceWithIterator({
        next: async () => ({ done: true, value: undefined }),
        return: 1,
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      value => value
    )
    await expect(malformedReturnStream[Symbol.asyncIterator]().return()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'stream'
    })
  })

  test('fails closed on malformed stream controls, limits, iterator construction, and metadata', async () => {
    const malformedControls = sourceWithIterator({
      next: async () => ({
        done: false,
        value: { kind: 'overflow', policy: 'invalid', droppedItems: NaN, droppedBytes: 0, replacedItems: 0 }
      }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
    const publicControls = mapPublicBoundedAsyncStream(malformedControls, value => value)
    await expect(publicControls[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'stream'
    })

    const hostileItem = new Proxy(
      { kind: 'overflow' },
      {
        get() {
          throw new Error('stream item getter trap')
        }
      }
    )
    const hostileControls = mapPublicBoundedAsyncStream(
      sourceWithIterator({
        next: async () => ({ done: false, value: hostileItem }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      value => value
    )
    await expect(hostileControls[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'stream'
    })

    const malformedCounter = mapPublicBoundedAsyncStream(
      sourceWithIterator({
        next: async () => ({
          done: false,
          value: { kind: 'overflow', policy: 'error', droppedItems: '1', droppedBytes: 0, replacedItems: 0 }
        }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      value => value
    )
    await expect(malformedCounter[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'stream'
    })

    const malformedLimits = {
      limits: { itemCapacity: NaN, byteCapacity: 1, reservedControlCapacity: 1 },
      overflowPolicy: 'error',
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      close: async () => ({ state: 'released', failures: [] })
    }
    expect(() => mapPublicBoundedAsyncStream(malformedLimits, value => value)).toThrow(BleError)

    expect(() =>
      mapPublicBoundedAsyncStream(
        { ...malformedLimits, limits: { itemCapacity: '2', byteCapacity: 4, reservedControlCapacity: 1 } },
        value => value
      )
    ).toThrow(BleError)

    const constructionFailure = {
      limits: limits(1, 4, 1),
      overflowPolicy: 'error',
      [Symbol.asyncIterator]() {
        throw contractError('protocol.malformed', 'stream', 'public-stream-runtime.iterator-construction')
      },
      close: async () => ({ state: 'released', failures: [] })
    }
    const publicConstructionFailure = mapPublicBoundedAsyncStream(constructionFailure, value => value)
    expect(() => publicConstructionFailure[Symbol.asyncIterator]()).toThrow(BleError)

    const malformedIterator = mapPublicBoundedAsyncStream(
      {
        limits: limits(1, 4, 1),
        overflowPolicy: 'error',
        [Symbol.asyncIterator]() {
          return {
            next: undefined,
            return: async () => ({ done: true, value: undefined }),
            [Symbol.asyncIterator]() {
              return this
            }
          }
        },
        close: async () => ({ state: 'released', failures: [] })
      },
      value => value
    )
    expect(() => malformedIterator[Symbol.asyncIterator]()).toThrow(BleError)

    const forbidden = { constructor: 'forbidden' }
    const cyclic = {}
    cyclic.self = cyclic
    const inherited = Object.create({ inherited: true })
    for (const metadata of [forbidden, cyclic, inherited]) {
      const malformedCleanup = {
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'stream',
            error: {
              code: 'platform.failure',
              domain: 'stream',
              operation: 'public-stream-runtime.metadata',
              platform: { domain: 'test', code: 'E_METADATA', safeMessage: 'bad', metadata },
              retryability: 'never'
            }
          }
        ]
      }
      const publicCleanup = mapPublicBoundedAsyncStream(
        sourceWithIterator(
          {
            next: async () => ({ done: true, value: undefined }),
            return: async () => ({ done: true, value: undefined }),
            [Symbol.asyncIterator]() {
              return this
            }
          },
          async () => malformedCleanup
        ),
        value => value
      )
      await expect(publicCleanup.close()).rejects.toMatchObject({ code: 'protocol.malformed' })
    }
  })

  test('closes the source and aggregates the primary projection error with teardown failure', async () => {
    const primary = contractError('protocol.malformed', 'stream', 'public-stream-runtime.map-value')
    const teardown = contractError('platform.failure', 'cleanup', 'public-stream-runtime.return')
    const sourceIterator = {
      next: async () => ({ done: false, value: { kind: 'value', value: 'value' } }),
      return: jest.fn(async () => {
        throw teardown
      }),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const publicStream = mapPublicBoundedAsyncStream(sourceWithIterator(sourceIterator), () => {
      throw primary
    })
    const iterator = publicStream[Symbol.asyncIterator]()

    let error
    try {
      await iterator.next()
    } catch (caught) {
      error = caught
    }
    expect(error).toBeInstanceOf(AggregateError)
    expect(error.errors).toHaveLength(2)
    expect(error.errors[0]).toMatchObject({ code: 'protocol.malformed' })
    expect(error.errors[1]).toMatchObject({ code: 'platform.failure' })
    expect(sourceIterator.return).toHaveBeenCalledTimes(1)
    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  })

  test('freezes the public stream wrapper and accepts a null-prototype metadata record', async () => {
    const metadata = Object.create(null)
    metadata.bytes = new Uint8Array([4, 5])
    const cleanup = {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'stream',
          error: {
            code: 'platform.failure',
            domain: 'stream',
            operation: 'public-stream-runtime.null-prototype',
            platform: { domain: 'test', code: 'E_OK', safeMessage: 'ok', metadata },
            retryability: 'never'
          }
        }
      ]
    }
    const publicStream = mapPublicBoundedAsyncStream(
      sourceWithIterator(
        {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        },
        async () => cleanup
      ),
      value => value
    )
    expect(Object.isFrozen(publicStream)).toBe(true)
    const projected = await publicStream.close()
    expect(projected.failures[0].error.platform.metadata.bytes).not.toBe(metadata.bytes)
  })

  test('preserves a public mapping error while still closing the source iterator', async () => {
    const projectionError = new BleError('protocol.violation', 'gatt', 'public-stream-runtime.mapping')
    const sourceIterator = {
      next: jest.fn(async () => ({ done: false, value: { kind: 'value', value: 'value' } })),
      return: jest.fn(async () => ({ done: true, value: undefined })),
      [Symbol.asyncIterator]() {
        return this
      }
    }
    const publicStream = mapPublicBoundedAsyncStream(sourceWithIterator(sourceIterator), () => {
      throw projectionError
    })
    const iterator = publicStream[Symbol.asyncIterator]()
    await expect(iterator.next()).rejects.toBe(projectionError)
    expect(sourceIterator.return).toHaveBeenCalledTimes(1)
  })

  test('memoizes a public cleanup snapshot when the source returns one lifecycle record', async () => {
    const sharedCleanup = { state: 'released', failures: [] }
    const source = sourceWithIterator(
      {
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      },
      async () => sharedCleanup
    )
    const publicStream = mapPublicBoundedAsyncStream(source, value => value)
    const first = await publicStream.close()
    const second = await publicStream.close()
    expect(second).toBe(first)
  })

  test('memoizes a released close result across repeated source cleanup records', async () => {
    const close = jest.fn(async () => ({ state: 'released', failures: [] }))
    const publicStream = mapPublicBoundedAsyncStream(
      sourceWithIterator(
        {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        },
        close
      ),
      value => value
    )
    const first = await publicStream.close()
    const second = await publicStream.close()
    expect(second).toBe(first)
    expect(close).toHaveBeenCalledTimes(1)
  })

  test('turns hostile cleanup metadata and malformed cleanup records into public protocol errors', async () => {
    const hostileMetadata = new Proxy(
      {},
      {
        getPrototypeOf() {
          throw new Error('metadata prototype trap')
        }
      }
    )
    const malformedCleanupStream = mapPublicBoundedAsyncStream(
      sourceWithIterator(
        {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        },
        async () => ({ state: 'release-failed', failures: null })
      ),
      value => value
    )
    await expect(malformedCleanupStream.close()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'boundary'
    })

    const inheritedCleanup = Object.assign(Object.create({ inherited: true }), {
      state: 'released',
      failures: []
    })
    const inheritedCleanupStream = mapPublicBoundedAsyncStream(
      sourceWithIterator(
        {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        },
        async () => inheritedCleanup
      ),
      value => value
    )
    await expect(inheritedCleanupStream.close()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'boundary'
    })

    const hostileCleanupStream = mapPublicBoundedAsyncStream(
      sourceWithIterator(
        {
          next: async () => ({ done: true, value: undefined }),
          return: async () => ({ done: true, value: undefined }),
          [Symbol.asyncIterator]() {
            return this
          }
        },
        async () => ({
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'stream',
              error: {
                code: 'platform.failure',
                domain: 'stream',
                operation: 'public-stream-runtime.hostile-metadata',
                platform: { domain: 'test', code: 'E_HOSTILE', safeMessage: 'hostile', metadata: hostileMetadata },
                retryability: 'never'
              }
            }
          ]
        })
      ),
      value => value
    )
    await expect(hostileCleanupStream.close()).rejects.toMatchObject({
      code: 'protocol.malformed',
      domain: 'boundary'
    })

    const rehydrated = rehydratePublicError(
      contractError('platform.failure', 'stream', 'public-stream-runtime.rehydrate-hostile', {
        domain: 'test',
        code: 'E_HOSTILE',
        safeMessage: 'hostile',
        metadata: hostileMetadata
      })
    )
    expect(rehydrated).toMatchObject({ code: 'protocol.malformed', domain: 'boundary' })
    expect(rehydrated).toBeInstanceOf(BleError)
  })

  test('rejects semantically malformed cleanup states and normalized errors', async () => {
    const validFailure = {
      resourceKind: 'stream',
      error: {
        code: 'platform.failure',
        domain: 'stream',
        operation: 'public-stream-runtime.semantic-cleanup',
        platform: null,
        retryability: 'never'
      }
    }
    const malformedCleanups = [
      { state: 'released', failures: [validFailure] },
      { state: 'release-failed', failures: [] },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: { ...validFailure.error, code: 'not-a-code' } }]
      },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: { ...validFailure.error, domain: 'not-a-domain' } }]
      },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: { ...validFailure.error, operation: '' } }]
      },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: { ...validFailure.error, retryability: 'sometimes' } }]
      },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: { ...validFailure.error, platform: undefined } }]
      },
      {
        state: 'release-failed',
        failures: [{ ...validFailure, error: Object.assign(Object.create({ inherited: true }), validFailure.error) }]
      }
    ]
    for (const cleanup of malformedCleanups) {
      const stream = mapPublicBoundedAsyncStream(
        sourceWithIterator(
          {
            next: async () => ({ done: true, value: undefined }),
            return: async () => ({ done: true, value: undefined }),
            [Symbol.asyncIterator]() {
              return this
            }
          },
          async () => cleanup
        ),
        value => value
      )
      await expect(stream.close()).rejects.toMatchObject({
        code: 'protocol.malformed',
        domain: 'boundary'
      })
    }
  })

  test('accepts the normative maximum public stream capacity and rejects one above it', async () => {
    const source = sourceWithIterator({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
    source.limits = { itemCapacity: 65_536, byteCapacity: 65_536, reservedControlCapacity: 1 }
    const publicStream = mapPublicBoundedAsyncStream(source, value => value)
    expect(publicStream.limits.itemCapacity).toBe(65_536)

    const maximumBytes = sourceWithIterator({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
    maximumBytes.limits = { itemCapacity: 1, byteCapacity: 4 * 1024 * 1024, reservedControlCapacity: 1 }
    expect(mapPublicBoundedAsyncStream(maximumBytes, value => value).limits.byteCapacity).toBe(4 * 1024 * 1024)
    expect(
      resolveStreamPolicy({
        preset: 'custom',
        budget: { itemCapacity: 65_536, byteCapacity: 4 * 1024 * 1024, reservedControlCapacity: 1 }
      }).byteCapacity
    ).toBe(4 * 1024 * 1024)

    const aboveMaximum = sourceWithIterator({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
    aboveMaximum.limits = { itemCapacity: 65_537, byteCapacity: 65_536, reservedControlCapacity: 1 }
    expect(() => mapPublicBoundedAsyncStream(aboveMaximum, value => value)).toThrow(BleError)

    const aboveByteMaximum = sourceWithIterator({
      next: async () => ({ done: true, value: undefined }),
      return: async () => ({ done: true, value: undefined }),
      [Symbol.asyncIterator]() {
        return this
      }
    })
    aboveByteMaximum.limits = { itemCapacity: 1, byteCapacity: 4 * 1024 * 1024 + 1, reservedControlCapacity: 1 }
    expect(() => mapPublicBoundedAsyncStream(aboveByteMaximum, value => value)).toThrow(BleError)
    expect(() =>
      resolveStreamPolicy({
        preset: 'custom',
        budget: { itemCapacity: 1, byteCapacity: 4 * 1024 * 1024 + 1, reservedControlCapacity: 1 }
      })
    ).toThrow()
  })
})
