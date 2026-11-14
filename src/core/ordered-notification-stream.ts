// src/core/ordered-notification-stream.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamLimits } from '../backend-contract/streams'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationMarker,
  OrderedNotificationStream,
  OrderedNotificationTerminal
} from './ordered-notification-contract'

type DataItem = Exclude<OrderedNotificationItem, OrderedNotificationTerminal>

/** Account only records actually retained for this consumer; source loss is a control. */
export function accountOrderedDiscard(
  items: readonly OrderedNotificationItem[]
): NonNullable<OrderedNotificationCleanup['discarded']> {
  let values = 0n
  let bytes = 0n
  let controls = 0n
  let uncertainTail = true
  let terminal: OrderedNotificationTerminal | null = null
  const losses: OrderedNotificationLoss[] = []
  const markers: OrderedNotificationMarker[] = []
  const failureObservations: import('./ordered-notification-contract').OrderedNotificationFailureObservation[] = []
  const failureObservationsOmitted: import('./ordered-notification-contract').OrderedNotificationFailureObservationsOmitted[] =
    []
  for (const item of items) {
    if (item.kind === 'value') {
      values += 1n
      bytes += BigInt(item.value.byteLength)
    } else {
      controls += 1n
      if (item.kind === 'terminal') {
        uncertainTail = item.uncertainTail
        terminal = item
      } else if (item.kind === 'loss') losses.push(item)
      else if (item.kind === 'marker') markers.push(item)
      else if (item.kind === 'failure-observation') failureObservations.push(item)
      else failureObservationsOmitted.push(item)
    }
  }
  return {
    values: values.toString(),
    bytes: bytes.toString(),
    controls: controls.toString(),
    uncertainTail,
    terminal,
    losses,
    markers,
    ...(failureObservations.length === 0 ? {} : { failureObservations }),
    ...(failureObservationsOmitted.length === 0 ? {} : { failureObservationsOmitted })
  }
}

/** Single-consumer FIFO. The producer owns value bytes before calling emit. */
export class CoreOrderedNotificationStream implements OrderedNotificationStream {
  private readonly queue: {
    readonly item: OrderedNotificationItem
    readonly bytes: number
    readonly control: boolean
  }[] = []
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
  private discardRequested = false
  private queuedControls = 0
  private queuedValues = 0
  private markerGroup: string | null
  private reservedMarker: string | null = null
  private markerOutcome: {
    readonly collectionId: string
    readonly markerId: string
    readonly outcome:
      | { readonly state: 'admitted'; readonly ordinal: string }
      | { readonly state: 'rejected' | 'not-admitted' }
  } | null = null
  private lastOrdinal: string | null = null
  private deferredTerminal: OrderedNotificationTerminal | null = null

  constructor(
    readonly limits: StreamLimits,
    private readonly fenceSource: (reason: OrderedNotificationTerminal['reason']) => void,
    private readonly cleanup: () => Promise<OrderedNotificationCleanup>,
    collectionId: string | null = null
  ) {
    this.markerGroup = collectionId
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

  private requireMarkerIdentity(collectionId: string, markerId: string): void {
    if (collectionId.length === 0 || !isCanonicalIngressOrdinal(markerId))
      throw contractError('argument.invalid', 'stream', 'ordered-notification-stream.marker')
    if (this.markerGroup !== null && this.markerGroup !== collectionId)
      throw contractError('ownership.denied', 'stream', 'ordered-notification-stream.marker-group')
  }

  /** Reserve capacity only; the ingress owner assigns the eventual ordinal. */
  reserveMarker(collectionId: string, markerId: string): void {
    this.requireMarkerIdentity(collectionId, markerId)
    if (this.terminal || this.returned || this.fenced || this.fencing)
      throw contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.marker-reserve')
    if (this.reservedMarker === markerId) return
    if (this.reservedMarker !== null || this.queuedControls >= this.limits.reservedControlCapacity)
      throw contractError('stream.quota', 'stream', 'ordered-notification-stream.marker-reserve')
    this.markerGroup = collectionId
    this.reservedMarker = markerId
  }

  /** Only call after the source reports NotAdmitted or cancellation before dispatch. */
  releaseMarkerNotAdmitted(collectionId: string, markerId: string): void {
    this.requireReservedMarker(collectionId, markerId)
    this.reservedMarker = null
    this.markerOutcome = { collectionId, markerId, outcome: { state: 'not-admitted' } }
    this.flushDeferredTerminal()
  }

  /** Consume a matching source rejection, without attributing it to another marker. */
  rejectReservedMarker(collectionId: string, markerId: string): void {
    this.requireReservedMarker(collectionId, markerId)
    this.reservedMarker = null
    this.markerOutcome = { collectionId, markerId, outcome: { state: 'rejected' } }
    if (this.deferredTerminal !== null)
      this.deferredTerminal = {
        ...this.deferredTerminal,
        uncertainTail: true,
        sourceRejection: { kind: 'marker', markerId }
      }
    this.flushDeferredTerminal()
  }

  /** The last exact outcome is retained after a marker leaves the queue. */
  markerAdmissionStatus(
    collectionId: string,
    markerId: string
  ):
    | { readonly state: 'reserved' }
    | { readonly state: 'admitted'; readonly ordinal: string }
    | { readonly state: 'rejected' | 'not-admitted' | 'unknown' } {
    this.requireMarkerIdentity(collectionId, markerId)
    if (this.markerGroup === collectionId && this.reservedMarker === markerId) return { state: 'reserved' }
    if (this.markerOutcome?.collectionId === collectionId && this.markerOutcome.markerId === markerId)
      return this.markerOutcome.outcome
    return { state: 'unknown' }
  }

  private requireReservedMarker(collectionId: string, markerId: string): void {
    this.requireMarkerIdentity(collectionId, markerId)
    if (this.markerGroup !== collectionId || this.reservedMarker !== markerId)
      throw contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.marker-credit')
  }

  private flushDeferredTerminal(): void {
    if (this.reservedMarker !== null || this.deferredTerminal === null) return
    const terminal = this.deferredTerminal
    this.deferredTerminal = null
    this.finish(terminal)
  }

  admitReservedMarker(item: OrderedNotificationMarker): boolean {
    this.requireReservedMarker(item.collectionId, item.markerId)
    if (this.terminal || this.fencing || ((this.fenced || this.returned) && this.deferredTerminal === null))
      throw contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.marker-admit')
    const ordinal = item.ordinal
    if (
      !isCanonicalIngressOrdinal(ordinal) ||
      (this.lastOrdinal !== null &&
        (ordinal.length < this.lastOrdinal.length ||
          (ordinal.length === this.lastOrdinal.length && ordinal <= this.lastOrdinal))) ||
      this.queuedControls >= this.limits.reservedControlCapacity
    ) {
      this.reservedMarker = null
      this.markerOutcome = { collectionId: item.collectionId, markerId: item.markerId, outcome: { state: 'rejected' } }
      const rejection = { kind: 'marker' as const, markerId: item.markerId }
      if (this.deferredTerminal !== null)
        this.deferredTerminal = { ...this.deferredTerminal, uncertainTail: true, sourceRejection: rejection }
      else this.finish({ kind: 'terminal', reason: 'overflow', uncertainTail: true, sourceRejection: rejection })
      this.flushDeferredTerminal()
      return false
    }
    this.reservedMarker = null
    this.markerOutcome = {
      collectionId: item.collectionId,
      markerId: item.markerId,
      outcome: { state: 'admitted', ordinal }
    }
    this.lastOrdinal = ordinal
    if (this.pending !== null && this.queue.length === 0) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: false, value: item })
    } else {
      this.queuedControls += 1
      this.queue.push({ item, bytes: 0, control: true })
    }
    this.flushDeferredTerminal()
    return true
  }

  /** False means this item was not admitted; overflow terminates the stream. */
  emit(item: DataItem): boolean {
    if (item.kind === 'marker') return this.admitReservedMarker(item)
    if (this.terminal || this.returned || this.fenced || this.fencing) return false
    if (
      (item.kind === 'failure-observation' || item.kind === 'failure-observations-omitted') &&
      this.queuedControls + (this.reservedMarker === null ? 0 : 1) >= this.limits.reservedControlCapacity
    )
      return false
    const bytes = item.kind === 'value' ? item.value.byteLength : 0
    const ordinalInvalid =
      item.kind === 'value' &&
      (!isCanonicalIngressOrdinal(item.ordinal) ||
        (this.lastOrdinal !== null &&
          (item.ordinal.length < this.lastOrdinal.length ||
            (item.ordinal.length === this.lastOrdinal.length && item.ordinal <= this.lastOrdinal))))
    if (
      ordinalInvalid ||
      (item.kind === 'value' && this.queuedValues >= this.limits.itemCapacity) ||
      (item.kind === 'loss' &&
        this.queuedControls + (this.reservedMarker === null ? 0 : 1) >= this.limits.reservedControlCapacity) ||
      this.retainedBytes + bytes > this.limits.byteCapacity - this.limits.reservedControlCapacity
    ) {
      this.fence('overflow')
      const loss: OrderedNotificationItem =
        item.kind === 'value'
          ? {
              kind: 'loss',
              scope: { kind: 'member', memberId: item.memberId },
              reason: 'overflow',
              lostValues: '1',
              lostBytes: String(bytes),
              lostSourceRecords: null
            }
          : item
      if (this.pending !== null && this.queue.length === 0) {
        const resolve = this.pending
        this.pending = null
        resolve({ done: false, value: loss })
      } else {
        this.queue.push({ item: loss, bytes: 0, control: false })
      }
      this.finish({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
      return false
    }
    if (item.kind === 'value') this.lastOrdinal = item.ordinal
    if (this.pending !== null && this.queue.length === 0) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: false, value: item })
      return true
    }
    if (item.kind !== 'value') this.queuedControls += 1
    if (item.kind === 'value') this.queuedValues += 1
    this.queue.push({ item, bytes, control: item.kind !== 'value' })
    this.retainedBytes += bytes
    return true
  }

  /** Fences source admission before publishing a terminal. */
  finish(terminal: OrderedNotificationTerminal): void {
    if (this.terminal || (this.returned && !this.discardRequested) || this.fencing) return
    if (terminal.sourceRejection?.kind === 'marker') {
      const markerId = terminal.sourceRejection.markerId
      if (this.reservedMarker === null) {
        if (this.markerOutcome?.markerId !== markerId || this.markerOutcome.outcome.state !== 'rejected')
          throw contractError('protocol.violation', 'stream', 'ordered-notification-stream.marker-rejection')
      } else {
        if (this.reservedMarker !== markerId)
          throw contractError('protocol.violation', 'stream', 'ordered-notification-stream.marker-rejection')
        this.reservedMarker = null
        if (this.markerGroup !== null)
          this.markerOutcome = { collectionId: this.markerGroup, markerId, outcome: { state: 'rejected' } }
      }
      if (this.deferredTerminal !== null) {
        terminal = { ...this.deferredTerminal, uncertainTail: true, sourceRejection: terminal.sourceRejection }
        this.deferredTerminal = null
      }
    }
    if (this.fenceError === null) this.fence(terminal.reason)
    if (this.reservedMarker !== null) {
      this.deferredTerminal ??= terminal
      return
    }
    this.terminal = true
    const result =
      this.fenceError === null ? terminal : { ...terminal, uncertainTail: true, fenceError: this.fenceError }
    if (this.pending !== null && this.queue.length === 0) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: false, value: result })
      return
    }
    this.queue.push({ item: result, bytes: 0, control: false })
  }

  close(): Promise<OrderedNotificationCleanup> {
    if (this.fencing)
      return Promise.reject(
        contractError('lifecycle.invalid-state', 'stream', 'ordered-notification-stream.reentrant-close')
      )
    if (!this.terminal && !this.returned && this.deferredTerminal === null)
      this.finish({ kind: 'terminal', reason: 'closed', uncertainTail: true })
    else if (!this.fenced) this.fence(this.fenceReason)
    if (this.fenceError !== null) return Promise.reject(new BackendContractError(this.fenceError))
    if (this.released !== null) return Promise.resolve(this.released)
    if (this.cleanupInFlight !== null) return this.cleanupInFlight
    const operation = this.cleanup()
      .then(result => {
        if (this.reservedMarker !== null && result.group.state === 'released') {
          result = {
            ...result,
            group: {
              state: 'release-failed',
              failures: [
                {
                  resourceKind: 'ordered-marker-credit',
                  error: contractError(
                    'lifecycle.invalid-state',
                    'cleanup',
                    'ordered-notification-stream.marker-pending'
                  ).normalized
                }
              ]
            }
          }
        }
        if (
          this.discardRequested &&
          result.group.state === 'released' &&
          result.members.every(member => member.cleanup.state === 'released')
        ) {
          const discarded = accountOrderedDiscard(this.queue.map(entry => entry.item))
          this.queue.length = 0
          this.retainedBytes = 0
          this.queuedControls = 0
          this.queuedValues = 0
          result = { ...result, discarded }
        } else if (this.discardRequested)
          result = { ...result, discarded: { values: '0', bytes: '0', controls: '0', uncertainTail: true } }
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

  async discard(): Promise<OrderedNotificationCleanup> {
    this.discardRequested = true
    if (this.pending !== null) {
      const resolve = this.pending
      this.pending = null
      resolve({ done: true, value: undefined })
    }
    const completion = this.close()
    this.returned = true
    if (this.released !== null && this.released.discarded === undefined) {
      const discarded = accountOrderedDiscard(this.queue.map(entry => entry.item))
      this.queue.length = 0
      this.retainedBytes = 0
      this.queuedControls = 0
      this.queuedValues = 0
      this.released = { ...this.released, discarded }
    }
    const completed = await completion
    const result = this.released ?? completed
    return result.discarded === undefined
      ? { ...result, discarded: { values: '0', bytes: '0', controls: '0', uncertainTail: true } }
      : result
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
        await this.discard()
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
      if (entry.control) this.queuedControls -= 1
      if (entry.item.kind === 'value') this.queuedValues -= 1
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
