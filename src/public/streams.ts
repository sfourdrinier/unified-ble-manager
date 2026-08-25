// src/public/streams.ts

import type { BoundedAsyncStream, StreamItem } from '../backend-contract/streams'
import type { CleanupRecord } from './cleanup'
import { toPublicCleanupRecord } from './cleanup'
import { rehydratePublicError } from './error-bridge'
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
  const limits: PublicStreamLimits = Object.freeze({
    itemCapacity: Number(source.limits.itemCapacity),
    byteCapacity: Number(source.limits.byteCapacity),
    reservedControlCapacity: Number(source.limits.reservedControlCapacity)
  })
  return {
    limits,
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<PublicValue> {
      const sourceIterator: SourceStreamIterator<InternalValue> = source[Symbol.asyncIterator]()
      const iterator: PublicBoundedAsyncStreamIterator<PublicValue> = {
        async next(): Promise<IteratorResult<PublicStreamItem<PublicValue>, undefined>> {
          return mapPublicIteratorResult(await sourceIterator.next(), mapValue)
        },
        async return(): Promise<IteratorResult<PublicStreamItem<PublicValue>, undefined>> {
          return mapPublicIteratorResult(await sourceIterator.return(), mapValue)
        },
        [Symbol.asyncIterator](): PublicBoundedAsyncStreamIterator<PublicValue> {
          return iterator
        }
      }
      return iterator
    },
    close: async (): Promise<CleanupRecord> => {
      try {
        return toPublicCleanupRecord(await source.close())
      } catch (error) {
        throw rehydratePublicError(error)
      }
    }
  }
}

function mapPublicIteratorResult<InternalValue, PublicValue>(
  result: IteratorResult<SourceStreamItem<InternalValue>, undefined>,
  mapValue: (value: InternalValue) => PublicValue
): IteratorResult<PublicStreamItem<PublicValue>, undefined> {
  if (result.done) return { done: true, value: undefined }
  if (result.value.kind === 'value') {
    return {
      done: false,
      value: Object.freeze({ kind: 'value', value: mapValue(result.value.value) })
    }
  }
  if (result.value.kind === 'overflow') {
    return {
      done: false,
      value: Object.freeze({
        kind: 'overflow',
        policy: result.value.policy,
        droppedItems: Number(result.value.droppedItems),
        droppedBytes: Number(result.value.droppedBytes),
        replacedItems: Number(result.value.replacedItems)
      })
    }
  }
  return {
    done: false,
    value: Object.freeze({
      kind: 'terminal',
      reason: result.value.reason,
      droppedItems: Number(result.value.droppedItems),
      droppedBytes: Number(result.value.droppedBytes),
      replacedItems: Number(result.value.replacedItems)
    })
  }
}
