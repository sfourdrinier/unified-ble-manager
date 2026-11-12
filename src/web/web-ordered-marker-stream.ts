// src/web/web-ordered-marker-stream.ts
import { BackendContractError, contractError } from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import type { CharacteristicPath } from '../backend-contract/gatt'
import type { PublicOperationOptions } from '../backend-contract/operations'
import type { OrderedMarkerWriteHandle, OrderedMarkerWriteStatus } from '../core/ordered-marker-wire'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationStream
} from '../core/ordered-notification-contract'
import type { WebOrderedOwner } from './web-bluetooth-gatt'
import { normalizeWebBluetoothError } from './web-bluetooth-errors'
import type { WebMarkerReceipt } from './web-ordered-marker'

type Path = CharacteristicPath<string, string, string, string, string, 'current'>
type Mode = 'with-response' | 'without-response'
const MAX_ID = 18446744073709551615n

function normalized(error: unknown): NormalizedBleError | null {
  if (error === null || error === undefined) return null
  if (error instanceof BackendContractError) return error.normalized
  if (error instanceof Error)
    return normalizeWebBluetoothError(error, {
      fallbackCode: 'gatt.write-failed',
      domain: 'gatt',
      operation: 'web-gatt.marker-write'
    }).normalized
  return contractError('gatt.write-failed', 'gatt', 'web-gatt.marker-write').normalized
}
function status(receipt: WebMarkerReceipt): OrderedMarkerWriteStatus {
  const marker = receipt.marker
  if (marker.state === 'not-admitted') return { state: 'not-admitted', error: normalized(marker.error) }
  if (marker.state === 'rejected')
    return {
      state: 'rejected',
      error: normalized(marker.error) ?? contractError('stream.overflow', 'stream', 'web-gatt.marker').normalized
    }
  const write = receipt.write
  if (write.state === 'started')
    return { state: 'admitted', ordinal: marker.ordinal, write: { state: 'started', error: null } }
  if (write.state === 'not-started')
    return {
      state: 'admitted',
      ordinal: marker.ordinal,
      write: { state: 'not-started', error: normalized(write.error) }
    }
  return {
    state: 'admitted',
    ordinal: marker.ordinal,
    write: { state: 'settled', error: write.result.ok ? null : normalized(write.result.error) }
  }
}

/** The wrapper owns public marker handles and acknowledges evidence on delivery. */
export class WebOrderedMarkerReservations {
  private nextId = 0n
  private active: { readonly id: string } | null = null
  constructor(
    private readonly owner: {
      markerWrite: (
        ...args: [...Parameters<WebOrderedOwner['markerWrite']>, (number | null)?]
      ) => WebMarkerReceipt | Promise<WebMarkerReceipt>
      hasMarkerReceipt: (id: string) => boolean
      acknowledgeMarker: (id: string) => void | Promise<void>
      forgetMarker: (id: string) => boolean | Promise<boolean>
    },
    private readonly now: () => number,
    private readonly onForgotten?: () => Promise<void>
  ) {}
  reserveMarkerWrite(): OrderedMarkerWriteHandle<Path> {
    let id: string | null = null
    let retired = false
    let cancelledBeforeStart = false
    const cancellation = new AbortController()
    let detachSignal: (() => void) | null = null
    const cancelled = {
      state: 'not-admitted' as const,
      error: contractError('operation.aborted', 'gatt', 'web-gatt.marker-write').normalized
    }
    let request: {
      readonly path: Path
      readonly bytes: Uint8Array
      readonly mode: Mode
      readonly options: PublicOperationOptions
    } | null = null
    const token = { id: '' }
    const read = async () => {
      if (id === null || request === null)
        throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-handle')
      return status(
        await this.owner.markerWrite(
          id,
          request.path,
          request.bytes,
          request.mode,
          cancellation.signal,
          undefined,
          request.options.deadline
        )
      )
    }
    return {
      start: async (path, value, mode, options) => {
        if (retired) throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-retired')
        if (cancelledBeforeStart) return cancelled
        if (id !== null)
          return status(
            await this.owner.markerWrite(id, path, value, mode, cancellation.signal, undefined, options.deadline)
          )
        if (this.active !== null || this.nextId > MAX_ID)
          throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-busy')
        const candidate = this.nextId.toString()
        const bytes = new Uint8Array(value)
        id = candidate
        token.id = candidate
        request = { path, bytes, mode, options }
        if (options.signal !== null) {
          const signal = options.signal
          const abort = () => cancellation.abort()
          if (signal.aborted) abort()
          else {
            signal.addEventListener('abort', abort, { once: true })
            detachSignal = () => signal.removeEventListener('abort', abort)
          }
        }
        this.active = token
        try {
          return status(
            await this.owner.markerWrite(
              candidate,
              path,
              bytes,
              mode,
              cancellation.signal,
              options.deadline !== null && this.now() >= options.deadline,
              options.deadline
            )
          )
        } finally {
          if (this.owner.hasMarkerReceipt(candidate)) {
            this.nextId += 1n
          } else {
            id = null
            request = null
            detachSignal?.()
            detachSignal = null
            if (this.active === token) this.active = null
          }
        }
      },
      status: async () =>
        retired ? { state: 'retired' } : cancelledBeforeStart ? cancelled : id === null ? { state: 'idle' } : read(),
      cancel: async () => {
        if (retired) return { state: 'retired' }
        cancellation.abort()
        if (id !== null) return read()
        cancelledBeforeStart = true
        return cancelled
      },
      forget: async () => {
        if (retired) return this.onForgotten?.()
        if (id !== null && !(await this.owner.forgetMarker(id)))
          throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-forget')
        if (this.active === token) this.active = null
        retired = true
        detachSignal?.()
        detachSignal = null
        await this.onForgotten?.()
      }
    }
  }
  acknowledgeMarker(id: string): void | Promise<void> {
    if (this.active?.id === id) return this.owner.acknowledgeMarker(id)
  }
  hasActiveReceipt(): boolean {
    return this.active !== null
  }
}

export class WebOrderedMarkerStream implements OrderedNotificationStream {
  readonly limits
  private readonly reservations: WebOrderedMarkerReservations
  constructor(
    private readonly inner: OrderedNotificationStream,
    owner: WebOrderedOwner,
    now: () => number
  ) {
    this.limits = inner.limits
    this.reservations = new WebOrderedMarkerReservations(owner, now)
  }
  reserveMarkerWrite(): OrderedMarkerWriteHandle<Path> {
    return this.reservations.reserveMarkerWrite()
  }
  [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined> {
    const iterator = this.inner[Symbol.asyncIterator]()
    return {
      next: async () => {
        const result = await iterator.next()
        if (!result.done) await this.ackItem(result.value)
        return result
      },
      return: async () => {
        await this.discard()
        return { done: true, value: undefined }
      }
    }
  }
  close(): Promise<OrderedNotificationCleanup> {
    return this.inner.close()
  }
  async discard(): Promise<OrderedNotificationCleanup> {
    const cleanup = await this.inner.discard()
    for (const marker of cleanup.discarded?.markers ?? []) await this.ackItem(marker)
    if (cleanup.discarded?.terminal) await this.ackItem(cleanup.discarded.terminal)
    return cleanup
  }
  private async ackItem(item: OrderedNotificationItem): Promise<void> {
    const markerId =
      item.kind === 'marker'
        ? item.markerId
        : item.kind === 'terminal' && item.sourceRejection?.kind === 'marker'
          ? item.sourceRejection.markerId
          : null
    if (markerId !== null) await this.reservations.acknowledgeMarker(markerId)
  }
}
