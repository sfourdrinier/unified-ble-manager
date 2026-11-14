// src/core/ordered-marker-handle.ts
import { contractError } from '../backend-contract/errors'
import type { PublicOperationOptions } from '../backend-contract/operations'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import type { OrderedMarkerWriteHandle, OrderedMarkerWriteStatus } from './ordered-marker-wire'

export interface OrderedMarkerTransport<Selector> {
  reserve(key: string): Promise<string>
  start(id: string, selector: Selector, value: Uint8Array, withResponse: boolean): Promise<OrderedMarkerWriteStatus>
  status(id: string): Promise<OrderedMarkerWriteStatus>
  cancel(id: string): Promise<OrderedMarkerWriteStatus>
  forget(id: string): Promise<void>
}
export function createOrderedMarkerReservations<Target, Selector>(
  transport: OrderedMarkerTransport<Selector>,
  resolve: (target: Target) => Selector,
  now: () => number,
  newKey: () => string
) {
  let active: object | null = null
  return (): OrderedMarkerWriteHandle<Target> => {
    const identity = {}
    const reservationKey = newKey()
    let retired = false
    let reservation: Promise<string> | null = null
    let reservationAttempted = false
    let request: {
      readonly target: Target
      readonly value: Uint8Array
      readonly mode: 'with-response' | 'without-response'
      readonly selector: Selector
    } | null = null
    let cancelled: OrderedMarkerWriteStatus | null = null
    const malformed = () => contractError('protocol.malformed', 'boundary', 'ordered.marker')
    const reserve = () => {
      if (retired) throw contractError('lifecycle.invalid-state', 'gatt', 'ordered.marker.retired')
      if (active !== null && active !== identity)
        throw contractError('lifecycle.invalid-state', 'gatt', 'ordered.marker.busy')
      active = identity
      if (reservation === null) {
        reservationAttempted = true
        const pending = transport.reserve(reservationKey).then(id => {
          if (!isCanonicalIngressOrdinal(id)) throw malformed()
          return id
        })
        reservation = pending.catch(error => {
          reservation = null
          throw error
        })
      }
      return reservation
    }
    let cancelTask: Promise<{ readonly status: OrderedMarkerWriteStatus } | { readonly error: unknown }> | null = null
    let watchedSignal: AbortSignal | null = null
    let abortListener: (() => void) | null = null
    const cancelNative = async (): Promise<OrderedMarkerWriteStatus> => {
      if (cancelled !== null) return cancelled
      if (!reservationAttempted) {
        cancelled = {
          state: 'not-admitted',
          error: contractError('operation.aborted', 'gatt', 'ordered.marker').normalized
        }
        return cancelled
      }
      return transport.cancel(await reserve())
    }
    const requestCancel = () => {
      if (cancelTask === null)
        cancelTask = cancelNative().then(
          status => ({ status }),
          error => ({ error })
        )
      return cancelTask
    }
    const cancelResult = async () => {
      const pending = await requestCancel()
      cancelTask = null
      if ('error' in pending) throw pending.error
      return pending.status
    }
    const clearSignal = () => {
      if (watchedSignal && abortListener) watchedSignal.removeEventListener('abort', abortListener)
      watchedSignal = null
      abortListener = null
    }
    return {
      start: async (target, value, mode, controls: PublicOperationOptions) => {
        if (retired) throw contractError('lifecycle.invalid-state', 'gatt', 'ordered.marker.retired')
        if (cancelled !== null) return cancelled
        if (request !== null) {
          if (
            request.target !== target ||
            request.mode !== mode ||
            request.value.length !== value.length ||
            request.value.some((byte, index) => byte !== value[index])
          )
            throw contractError('argument.invalid', 'gatt', 'ordered.marker.retry')
        } else {
          if (controls.signal?.aborted || (controls.deadline !== null && controls.deadline <= now())) {
            cancelled = {
              state: 'not-admitted',
              error: contractError(
                controls.signal?.aborted ? 'operation.aborted' : 'operation.timed-out',
                'gatt',
                'ordered.marker'
              ).normalized
            }
            return cancelled
          }
          request = { target, value: new Uint8Array(value), mode, selector: resolve(target) }
          if (controls.signal) {
            watchedSignal = controls.signal
            abortListener = () => {
              requestCancel()
            }
            watchedSignal.addEventListener('abort', abortListener, { once: true })
          }
        }
        const id = await reserve()
        if (watchedSignal?.aborted || (controls.deadline !== null && controls.deadline <= now())) return cancelResult()
        const result = await transport.start(id, request.selector, request.value, request.mode === 'with-response')
        if (watchedSignal?.aborted) return cancelResult()
        return result
      },
      status: async () => {
        if (retired) return { state: 'retired' }
        if (cancelled !== null) return cancelled
        if (cancelTask !== null) await cancelResult()
        if (!reservationAttempted) return { state: 'idle' }
        return transport.status(await reserve())
      },
      cancel: async () => {
        if (retired) return { state: 'retired' }
        return cancelResult()
      },
      forget: async () => {
        if (retired) return
        if (reservationAttempted) {
          await transport.forget(await reserve())
        }
        retired = true
        clearSignal()
        if (active === identity) active = null
      }
    }
  }
}
