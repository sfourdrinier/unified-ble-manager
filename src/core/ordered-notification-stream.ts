// src/core/ordered-notification-stream.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamLimits } from '../backend-contract/streams'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationStream,
  OrderedNotificationTerminal,
  OrderedNotificationValue
} from './ordered-notification-contract'

type DataItem = OrderedNotificationValue | OrderedNotificationLoss

/** Single-consumer FIFO. The producer owns value bytes before calling emit. */
export class CoreOrderedNotificationStream implements OrderedNotificationStream {
  private readonly queue: { readonly item: OrderedNotificationItem; readonly bytes: number }[] = []
  private retainedBytes = 0
  private terminal = false
  private fenced = false
  private fencing = false
  private fenceError: NormalizedBleError | null = null
  private fenceReason: OrderedNotificationTerminal['reason'] = 'closed'
  private claimed = false
  private returned = false
  private pending: ((result: IteratorResult<OrderedNotificationItem, undefined>) => void) | null = null
  private cleanupInFlight: Promise<OrderedNotificationCleanup> | null = null
  private released: OrderedNotificationCleanup | null = null

  constructor(
    readonly limits: StreamLimits,
    private readonly fenceSource: (reason: OrderedNotificationTerminal['reason']) => void,
    private readonly cleanup: () => Promise<OrderedNotificationCleanup>
  ) {
    if (
      !Number.isSafeInteger(limits.itemCapacity) ||
      limits.itemCapacity < 1 ||
      !Number.isSafeInteger(limits.byteCapacity) ||
      !Number.isSafeInteger(limits.reservedControlCapacity) ||
      limits.reservedControlCapacity < 1 ||
      limits.byteCapacity <= limits.reservedControlCapacity
    )
      throw contractError('argument.invalid', 'stream', 'ordered-notification-stream.limits')
  }

  /** False means this item was not admitted; overflow terminates the stream. */
  emit(item: DataItem): boolean {
    if (this.terminal || this.returned || this.fenced || this.fencing) return false
    const bytes = item.kind === 'value' ? item.value.byteLength : 1
    if (
      this.queue.length >= this.limits.itemCapacity ||
      this.retainedBytes + bytes > this.limits.byteCapacity - this.limits.reservedControlCapacity
    ) {
      this.fence('overflow')
      const loss: OrderedNotificationLoss =
        item.kind === 'value'
          ? {
              kind: 'loss',
              scope: { kind: 'member', memberId: item.memberId },
              reason: 'overflow',
              lostValues: 1,
              lostBytes: bytes,
              lostSourceRecords: null
            }
          : item
      if (this.pending !== null && this.queue.length === 0) {
        const resolve = this.pending
        this.pending = null
        resolve({ done: false, value: loss })
      } else {
        this.queue.push({ item: loss, bytes: 0 })
      }
      this.finish({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
      return false
    }
    if (this.pending !== null && this.queue.length === 0) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: false, value: item })
      return true
    }
    this.queue.push({ item, bytes })
    this.retainedBytes += bytes
    return true
  }

  /** Fences source admission before publishing a terminal. */
  finish(terminal: OrderedNotificationTerminal): void {
    if (this.terminal || this.returned || this.fencing) return
    if (this.fenceError === null) this.fence(terminal.reason)
    this.terminal = true
    const result =
      this.fenceError === null ? terminal : { ...terminal, uncertainTail: true, fenceError: this.fenceError }
    if (this.pending !== null && this.queue.length === 0) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: false, value: result })
      return
    }
    this.queue.push({ item: result, bytes: 0 })
  }

  close(): Promise<OrderedNotificationCleanup> {
    if (this.fencing)
      return Promise.reject(
        contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.reentrant-close')
      )
    if (!this.terminal && !this.returned) this.finish({ kind: 'terminal', reason: 'closed', uncertainTail: true })
    else if (!this.fenced) this.fence(this.fenceReason)
    if (this.fenceError !== null) return Promise.reject(new BackendContractError(this.fenceError))
    if (this.released !== null) return Promise.resolve(this.released)
    if (this.cleanupInFlight !== null) return this.cleanupInFlight
    const operation = this.cleanup()
      .then(result => {
        if (result.group.state === 'released' && result.members.every(member => member.cleanup.state === 'released')) {
          this.released = result
        }
        return result
      })
      .finally(() => {
        this.cleanupInFlight = null
      })
    this.cleanupInFlight = operation
    return operation
  }

  [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined> {
    if (this.claimed)
      throw contractError('lifecycle.invariant-violation', 'stream', 'ordered-notification-stream.consumer')
    this.claimed = true
    return {
      next: () => this.next(),
      return: async () => {
        if (this.fencing)
          throw contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.reentrant-return')
        this.fence('closed')
        if (this.fenceError !== null) {
          this.finish({ kind: 'terminal', reason: 'closed', uncertainTail: true })
          throw new BackendContractError(this.fenceError)
        }
        this.returned = true
        this.queue.length = 0
        this.retainedBytes = 0
        if (this.pending !== null) {
          const resolve = this.pending
          this.pending = null
          resolve({ done: true, value: undefined })
        }
        await this.close()
        return { done: true, value: undefined }
      }
    }
  }

  private fence(reason: OrderedNotificationTerminal['reason']): void {
    if (this.fenced || this.fencing) return
    this.fenceReason = reason
    this.fencing = true
    try {
      this.fenceSource(reason)
      this.fenced = true
      this.fenceError = null
    } catch (error) {
      this.fenceError =
        error instanceof BackendContractError
          ? error.normalized
          : contractError('platform.failure', 'stream', 'ordered-notification-stream.fence').normalized
    } finally {
      this.fencing = false
    }
  }

  private next(): Promise<IteratorResult<OrderedNotificationItem, undefined>> {
    if (this.returned) return Promise.resolve({ done: true, value: undefined })
    const entry = this.queue.shift()
    if (entry !== undefined) {
      this.retainedBytes -= entry.bytes
      return Promise.resolve({ done: false, value: entry.item })
    }
    if (this.terminal) return Promise.resolve({ done: true, value: undefined })
    if (this.pending !== null)
      return Promise.reject(
        contractError('lifecycle.invariant-violation', 'stream', 'ordered-notification-stream.concurrent-next')
      )
    return new Promise(resolve => {
      this.pending = resolve
    })
  }
}
