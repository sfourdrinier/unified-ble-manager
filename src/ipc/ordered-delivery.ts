// src/ipc/ordered-delivery.ts

import { contractError } from '../backend-contract/errors'

/** One retained item makes a lost IPC response retryable without reordering. */
export type OrderedDeliveryPoll<T> =
  | { readonly state: 'empty' }
  | { readonly state: 'closed' }
  | { readonly state: 'item'; readonly deliveryId: string; readonly item: T }

export class OrderedDelivery<T> {
  private nextId = 1n
  private lastAcknowledged: string | null = null
  private retained: { readonly deliveryId: string; readonly item: T } | null = null
  private readonly replacements: T[] = []
  private forcedEnd = false
  private pending = false
  private pendingCompletion: Promise<void> | null = null
  private closed = false
  private failure: { readonly error: unknown } | null = null

  constructor(
    private readonly iterator: AsyncIterator<T, undefined, undefined>,
    private readonly onFailure?: (error: unknown) => T
  ) {}

  /** Preserve explicit loss and a terminal when an IPC record cannot fit. */
  replaceRetainedWithLoss(loss: T, controls: readonly T[]): void {
    if (this.retained === null || this.pending || this.forcedEnd)
      throw contractError('lifecycle.invariant-violation', 'ipc', 'ordered-delivery.replace')
    this.retained = { deliveryId: this.retained.deliveryId, item: loss }
    this.replacements.push(...controls)
    this.forcedEnd = true
  }

  async snapshotRetained(): Promise<readonly T[]> {
    if (this.pendingCompletion !== null) await this.pendingCompletion
    const items: T[] = []
    if (this.retained !== null) items.push(this.retained.item)
    items.push(...this.replacements)
    return items
  }

  commitAbandonment(): void {
    this.retained = null
    this.replacements.length = 0
    this.forcedEnd = true
    this.closed = true
  }

  get current(): { readonly deliveryId: string; readonly item: T } | null {
    return this.retained
  }

  get drained(): boolean {
    return this.closed && this.retained === null && !this.pending
  }

  poll(acknowledgement?: string): OrderedDeliveryPoll<T> {
    if (acknowledgement !== undefined) {
      if (this.retained?.deliveryId === acknowledgement) {
        this.lastAcknowledged = acknowledgement
        this.retained = null
      } else if (this.lastAcknowledged !== acknowledgement) {
        throw contractError('protocol.violation', 'ipc', 'ordered-delivery.acknowledgement')
      }
    }
    if (this.failure !== null) throw this.failure.error
    if (this.retained !== null) return { state: 'item', ...this.retained }
    if (this.forcedEnd) {
      const item = this.replacements.shift()
      if (item !== undefined) {
        this.retained = { deliveryId: String(this.nextId), item }
        this.nextId += 1n
        return { state: 'item', ...this.retained }
      }
      this.closed = true
    }
    if (this.closed) return { state: 'closed' }
    if (!this.pending) {
      this.pending = true
      this.pendingCompletion = this.iterator.next().then(
        result => {
          this.pending = false
          if (result.done) this.closed = true
          else {
            this.retained = { deliveryId: String(this.nextId), item: result.value }
            this.nextId += 1n
          }
        },
        error => {
          this.pending = false
          if (this.onFailure === undefined) this.failure = { error }
          else {
            this.retained = { deliveryId: String(this.nextId), item: this.onFailure(error) }
            this.nextId += 1n
            this.forcedEnd = true
          }
        }
      )
    }
    return { state: 'empty' }
  }
}
