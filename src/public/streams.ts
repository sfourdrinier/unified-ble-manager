// src/public/streams.ts

import type { BoundedAsyncStream, StreamItem } from '../backend-contract/streams'
import { BackendContractError, contractError } from '../backend-contract/errors'
import type { CleanupRecord } from './cleanup'
import { toPublicCleanupRecord } from './cleanup'
import { rehydratePublicError } from './error-bridge'
import { BleError } from './errors'
import type { PortableBoundedAsyncStream, PortableStreamItem } from '../manager/consumer-handles'

export type PublicStreamOverflowPolicy = 'latest' | 'drop-oldest' | 'drop-newest' | 'error'

export type PublicStreamTerminalReason =
  | 'closed'
  | 'overflow'
  | 'source-failed'
  | 'owner-released'
  | 'connection-lost'
  | 'service-changed'
  | 'operation-aborted'
  | 'operation-timed-out'

export interface PublicStreamLimits {
  readonly itemCapacity: number
  readonly byteCapacity: number
  readonly reservedControlCapacity: number
}

export interface PublicStreamOverflowNotice {
  readonly kind: 'overflow'
  readonly policy: PublicStreamOverflowPolicy
  readonly droppedItems: number
  readonly droppedBytes: number
  readonly replacedItems: number
}

export interface PublicStreamTerminalNotice {
  readonly kind: 'terminal'
  readonly reason: PublicStreamTerminalReason
  readonly droppedItems: number
  readonly droppedBytes: number
  readonly replacedItems: number
}

export interface PublicStreamValue<Value> {
  readonly kind: 'value'
  readonly value: Value
}

export type PublicStreamItem<Value> = PublicStreamValue<Value> | PublicStreamOverflowNotice | PublicStreamTerminalNotice

export interface PublicBoundedAsyncStreamIterator<Value>
  extends AsyncIterator<PublicStreamItem<Value>, undefined, undefined> {
  readonly return: () => Promise<IteratorResult<PublicStreamItem<Value>, undefined>>
  [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<Value>
}

export interface PublicBoundedAsyncStream<Value> extends AsyncIterable<PublicStreamItem<Value>, undefined, undefined> {
  readonly limits: PublicStreamLimits
  readonly overflowPolicy: PublicStreamOverflowPolicy
  [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<Value>
  close(): Promise<CleanupRecord>
}

type PublicStreamSource<Value> = BoundedAsyncStream<Value> | PortableBoundedAsyncStream<Value>

type SourceStreamItem<Value> = StreamItem<Value> | PortableStreamItem<Value>

type SourceStreamIterator<Value> = {
  readonly next: () => Promise<IteratorResult<SourceStreamItem<Value>, undefined>>
  readonly return: () => Promise<IteratorResult<SourceStreamItem<Value>, undefined>>
  readonly [Symbol.asyncIterator]: () => SourceStreamIterator<Value>
}

/** Projects backend stream controls and values into the host-neutral public contract. */
export function mapPublicBoundedAsyncStream<InternalValue, PublicValue>(
  source: PublicStreamSource<InternalValue>,
  mapValue: (value: InternalValue) => PublicValue
): PublicBoundedAsyncStream<PublicValue> {
  let limits: PublicStreamLimits
  let overflowPolicy: PublicStreamOverflowPolicy
  try {
    limits = Object.freeze({
      itemCapacity: requireStreamCapacity(source.limits.itemCapacity, 'item-capacity'),
      byteCapacity: requireStreamCapacity(source.limits.byteCapacity, 'byte-capacity'),
      reservedControlCapacity: requireStreamCapacity(source.limits.reservedControlCapacity, 'reserved-control-capacity')
    })
    if (limits.byteCapacity <= limits.reservedControlCapacity) {
      throw contractError('protocol.malformed', 'stream', 'public-stream.limits.byte-capacity')
    }
    overflowPolicy = requireOverflowPolicy(source.overflowPolicy)
  } catch (error) {
    throw rehydratePublicError(
      error instanceof BackendContractError
        ? error
        : contractError('protocol.malformed', 'stream', 'public-stream.source-contract')
    )
  }
  let publicClosePromise: Promise<CleanupRecord> | null = null
  const closePublicStream = (): Promise<CleanupRecord> => {
    if (publicClosePromise !== null) return publicClosePromise
    const operation = Promise.resolve()
      .then(() => source.close())
      .then(toPublicCleanupRecord)
      .catch(error => {
        throw rehydratePublicError(error)
      })
    publicClosePromise = operation.then(
      cleanup => {
        if (cleanup.state === 'release-failed') publicClosePromise = null
        return cleanup
      },
      error => {
        publicClosePromise = null
        throw error
      }
    )
    return publicClosePromise
  }
  const stream: PublicBoundedAsyncStream<PublicValue> = {
    limits,
    overflowPolicy,
    [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<PublicValue> {
      let sourceIterator: SourceStreamIterator<InternalValue>
      try {
        if (typeof source[Symbol.asyncIterator] !== 'function') {
          throw contractError('protocol.malformed', 'stream', 'public-stream.iterator-factory')
        }
        sourceIterator = source[Symbol.asyncIterator]()
        if (
          typeof sourceIterator !== 'object' ||
          sourceIterator === null ||
          typeof sourceIterator.next !== 'function'
        ) {
          throw contractError('protocol.malformed', 'stream', 'public-stream.iterator')
        }
      } catch (error) {
        throw rehydratePublicError(
          error instanceof BackendContractError
            ? error
            : contractError('protocol.malformed', 'stream', 'public-stream.iterator-construction')
        )
      }
      let sourceReturnPromise: Promise<IteratorResult<SourceStreamItem<InternalValue>, undefined>> | null = null
      let sourceClosed = false
      const returnSource = (): Promise<IteratorResult<SourceStreamItem<InternalValue>, undefined>> => {
        if (sourceReturnPromise !== null) return sourceReturnPromise
        sourceClosed = true
        try {
          sourceReturnPromise =
            typeof sourceIterator.return === 'function'
              ? Promise.resolve(sourceIterator.return())
              : Promise.resolve({ done: true, value: undefined })
        } catch (error) {
          sourceReturnPromise = Promise.reject(error)
        }
        return sourceReturnPromise
      }
      const iterator: PublicBoundedAsyncStreamIterator<PublicValue> = {
        async next(): Promise<IteratorResult<PublicStreamItem<PublicValue>, undefined>> {
          if (sourceClosed) return { done: true, value: undefined }
          try {
            const mapped = mapPublicIteratorResult(await sourceIterator.next(), mapValue)
            if (mapped.done) sourceClosed = true
            return mapped
          } catch (error) {
            const primary = rehydratePublicError(error)
            try {
              await returnSource()
            } catch (cleanupError) {
              throw aggregatePublicStreamErrors(primary, cleanupError)
            }
            throw primary
          }
        },
        async return(): Promise<IteratorResult<PublicStreamItem<PublicValue>, undefined>> {
          try {
            return mapPublicIteratorResult(await returnSource(), mapValue)
          } catch (error) {
            throw rehydratePublicError(error)
          }
        },
        [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<PublicValue> {
          return iterator
        }
      }
      return iterator
    },
    close: closePublicStream
  }
  return Object.freeze(stream)
}

function mapPublicIteratorResult<InternalValue, PublicValue>(
  result: IteratorResult<SourceStreamItem<InternalValue>, undefined>,
  mapValue: (value: InternalValue) => PublicValue
): IteratorResult<PublicStreamItem<PublicValue>, undefined> {
  try {
    if (typeof result !== 'object' || result === null || typeof result.done !== 'boolean') {
      throw contractError('protocol.malformed', 'stream', 'public-stream.iterator-result')
    }
    if (result.done) return { done: true, value: undefined }
    if (
      typeof result.value !== 'object' ||
      result.value === null ||
      !Object.prototype.hasOwnProperty.call(result.value, 'kind')
    ) {
      throw contractError('protocol.malformed', 'stream', 'public-stream.item')
    }
    if (result.value.kind === 'value') {
      if (!Object.prototype.hasOwnProperty.call(result.value, 'value')) {
        throw contractError('protocol.malformed', 'stream', 'public-stream.value')
      }
      return {
        done: false,
        value: Object.freeze({ kind: 'value', value: mapValue(result.value.value) })
      }
    }
    if (result.value.kind === 'overflow') {
      const policy = requireOverflowPolicy(result.value.policy)
      return {
        done: false,
        value: Object.freeze({
          kind: 'overflow',
          policy,
          droppedItems: requireStreamCounter(result.value.droppedItems, 'dropped-items'),
          droppedBytes: requireStreamCounter(result.value.droppedBytes, 'dropped-bytes'),
          replacedItems: requireStreamCounter(result.value.replacedItems, 'replaced-items')
        })
      }
    }
    if (result.value.kind !== 'terminal') {
      throw contractError('protocol.malformed', 'stream', 'public-stream.item-kind')
    }
    const reason = requireTerminalReason(result.value.reason)
    return {
      done: false,
      value: Object.freeze({
        kind: 'terminal',
        reason,
        droppedItems: requireStreamCounter(result.value.droppedItems, 'dropped-items'),
        droppedBytes: requireStreamCounter(result.value.droppedBytes, 'dropped-bytes'),
        replacedItems: requireStreamCounter(result.value.replacedItems, 'replaced-items')
      })
    }
  } catch (error) {
    if (error instanceof BackendContractError || error instanceof BleError) throw error
    throw contractError('protocol.malformed', 'stream', 'public-stream.item')
  }
}

function requireStreamCapacity(value: number, label: string): number {
  if (typeof value !== 'number') {
    throw contractError('protocol.malformed', 'stream', `public-stream.limits.${label}`)
  }
  const number = Number(value)
  if (!Number.isSafeInteger(number) || number <= 0) {
    throw contractError('protocol.malformed', 'stream', `public-stream.limits.${label}`)
  }
  return number
}

function requireStreamCounter(value: number, label: string): number {
  if (typeof value !== 'number') {
    throw contractError('protocol.malformed', 'stream', `public-stream.counter.${label}`)
  }
  const number = Number(value)
  if (!Number.isSafeInteger(number) || number < 0) {
    throw contractError('protocol.malformed', 'stream', `public-stream.counter.${label}`)
  }
  return number
}

function requireOverflowPolicy(value: string): PublicStreamOverflowPolicy {
  if (value === 'latest' || value === 'drop-oldest' || value === 'drop-newest' || value === 'error') return value
  throw contractError('protocol.malformed', 'stream', 'public-stream.overflow-policy')
}

function requireTerminalReason(value: string): PublicStreamTerminalReason {
  if (
    value === 'closed' ||
    value === 'overflow' ||
    value === 'source-failed' ||
    value === 'owner-released' ||
    value === 'connection-lost' ||
    value === 'service-changed' ||
    value === 'operation-aborted' ||
    value === 'operation-timed-out'
  ) {
    return value
  }
  throw contractError('protocol.malformed', 'stream', 'public-stream.terminal-reason')
}

function aggregatePublicStreamErrors(primary: unknown, cleanup: unknown): AggregateError {
  return new AggregateError(
    [rehydratePublicError(primary), rehydratePublicError(cleanup)],
    'BLE stream operation and iterator cleanup both failed'
  )
}
