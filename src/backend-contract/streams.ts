// src/backend-contract/streams.ts

import type { CleanupRecord, NormalizedBleError } from './errors'
import type { Capacity, ResourceCount } from './primitives'

export type OverflowPolicy = 'latest' | 'drop-oldest' | 'drop-newest' | 'error'
export interface StreamLimits {
  readonly itemCapacity: Capacity
  readonly byteCapacity: Capacity
  readonly reservedControlCapacity: Capacity
}
export interface StreamOverflowNotice {
  readonly kind: 'overflow'
  readonly policy: OverflowPolicy
  readonly droppedItems: ResourceCount
  readonly droppedBytes: ResourceCount
  readonly replacedItems: ResourceCount
}
export interface StreamValue<T> {
  readonly kind: 'value'
  readonly value: T
}
export interface StreamTerminalNotice {
  readonly kind: 'terminal'
  readonly reason:
    | 'closed'
    | 'overflow'
    | 'source-failed'
    | 'owner-released'
    | 'connection-lost'
    | 'service-changed'
    | 'operation-aborted'
    | 'operation-timed-out'
  readonly droppedItems: ResourceCount
  readonly droppedBytes: ResourceCount
  readonly replacedItems: ResourceCount
  /** Structured cause when the source failed while delivering this stream. */
  readonly error?: NormalizedBleError | null
}
export type StreamItem<T> = StreamValue<T> | StreamOverflowNotice | StreamTerminalNotice
export interface BoundedAsyncStreamIterator<T> extends AsyncIterator<StreamItem<T>, undefined, undefined> {
  readonly return: () => Promise<IteratorResult<StreamItem<T>, undefined>>
  [Symbol.asyncIterator](): BoundedAsyncStreamIterator<T>
}
export interface BoundedAsyncStream<T> extends AsyncIterable<StreamItem<T>, undefined, undefined> {
  readonly limits: StreamLimits
  readonly overflowPolicy: OverflowPolicy
  /** Optional synchronous state check for FIFO admission race prevention. */
  readonly isTerminal?: () => boolean
  /** Optional readable terminal cause; must not consume the unicast iterator. */
  readonly terminalReason?: () => StreamTerminalNotice['reason'] | null
  [Symbol.asyncIterator](): BoundedAsyncStreamIterator<T>
  close(): Promise<CleanupRecord>
}
