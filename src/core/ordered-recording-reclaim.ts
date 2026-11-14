// src/core/ordered-recording-reclaim.ts
import { contractError } from '../backend-contract/errors'

/** Opaque process-scoped authority; only the issuing host can authenticate it. */
export interface OrderedRecordingRecoveryHandle {
  readonly kind: 'ordered-recording-recovery'
}

export interface OrderedRecordingReclaimReceipt {
  readonly state:
    | 'reserved'
    | 'pending'
    | 'ready'
    | 'committed'
    | 'releasing'
    | 'release-failed'
    | 'released'
    | 'failed'
    | 'retired'
  readonly error: import('../backend-contract/errors').NormalizedBleError | null
  readonly mappingCommitted: boolean
}

/** The host authenticates the recorder capability and exact session on every call. */
export interface OrderedRecordingReclaimTransport<Status> {
  /** The same caller key is used to recover an ID retained after a lost reply. */
  next(callerKey: string): Promise<string | OrderedRecordingReclaimRefusal>
  /** A refusal proves this caller owns no reservation, including a prior retry. */
  reserve(id: string, callerKey: string): Promise<void | OrderedRecordingReclaimRefusal>
  start(id: string, callerKey: string): Promise<Status | OrderedRecordingReclaimRefusal>
  status(id: string, callerKey: string): Promise<Status | OrderedRecordingReclaimRefusal>
  commit(id: string, callerKey: string): Promise<Status>
  cancel(id: string, callerKey: string): Promise<Status>
  release(id: string, callerKey: string): Promise<Status>
  retire(id: string, callerKey: string): Promise<void>
  acknowledgeRetirement(id: string, callerKey: string): Promise<void>
  forgetRetirement(id: string, callerKey: string): Promise<void>
  /** Confirms receipt of forget; optional for transports without retained host receipts. */
  acknowledgeForget?(id: string, callerKey: string): Promise<void>
}

export interface OrderedRecordingReclaimRefusal {
  readonly state: 'not-reserved'
  readonly error: import('../backend-contract/errors').NormalizedBleError
}

class ReclaimRefused extends Error {
  constructor(readonly receipt: OrderedRecordingReclaimRefusal) {
    super('reclaim not reserved')
  }
}

function isRefusal(value: unknown): value is OrderedRecordingReclaimRefusal {
  if (typeof value !== 'object' || value === null || Reflect.get(value, 'state') !== 'not-reserved') return false
  const error: unknown = Reflect.get(value, 'error')
  return (
    typeof error === 'object' &&
    error !== null &&
    typeof Reflect.get(error, 'code') === 'string' &&
    typeof Reflect.get(error, 'domain') === 'string' &&
    typeof Reflect.get(error, 'operation') === 'string'
  )
}

export type OrderedRecordingReclaimStatus<Status> =
  | Status
  | OrderedRecordingReclaimRefusal
  | { readonly state: 'idle' | 'cancelled' | 'retiring' | 'retired' }

export interface OrderedRecordingReclaimHandle<Status> {
  start(): Promise<OrderedRecordingReclaimStatus<Status>>
  status(): Promise<OrderedRecordingReclaimStatus<Status>>
  commit(): Promise<Status>
  cancel(): Promise<OrderedRecordingReclaimStatus<Status>>
  release(): Promise<Status>
  retire(): Promise<void>
}

/** Retains the exact allocated ID across a lost reserve response. */
export function createOrderedRecordingReclaimReservations<Status>(
  transport: OrderedRecordingReclaimTransport<Status>,
  newCallerKey: () => string
): () => OrderedRecordingReclaimHandle<Status> {
  let active = false
  return () => {
    if (active) throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.reserve')
    const callerKey = newCallerKey()
    if (!callerKey) throw contractError('argument.invalid', 'cleanup', 'ordered-recording.reclaim.reserve')
    active = true
    let id: string | null = null
    let allocationAttempted = false
    let refusal: OrderedRecordingReclaimRefusal | null = null
    let reserved = false
    let cancelled = false
    let retired = false
    let retirementAttempted = false
    let retirementRequested = false
    let retirementAcknowledged = false
    let retirementForgotten = false
    let startsInFlight = 0
    let commitsInFlight = 0
    let queue: Promise<void> = Promise.resolve()
    const serial = <T>(operation: () => Promise<T>): Promise<T> => {
      const result = queue.then(operation)
      queue = result.then(
        () => undefined,
        () => undefined
      )
      return result
    }
    const ensureReserved = async (allowRetiring = false): Promise<string> => {
      if (retired) throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retired')
      if (refusal && (!allowRetiring || id === null))
        throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.refused')
      if (retirementAttempted && !allowRetiring)
        throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retiring')
      if (cancelled) throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.cancelled')
      if (id === null) {
        allocationAttempted = true
        const outcome = await transport.next(callerKey)
        if (typeof outcome !== 'string') {
          allocationAttempted = false
          refusal = outcome
          throw new ReclaimRefused(outcome)
        }
        id = outcome
      }
      if (!reserved) {
        const outcome = await transport.reserve(id, callerKey)
        if (outcome) {
          id = null
          allocationAttempted = false
          refusal = outcome
          throw new ReclaimRefused(outcome)
        }
        reserved = true
      }
      return id
    }
    const performCommit = async (exactId: string): Promise<Status> => {
      try {
        return await transport.commit(exactId, callerKey)
      } finally {
        commitsInFlight -= 1
      }
    }
    return Object.freeze<OrderedRecordingReclaimHandle<Status>>({
      start: async () => {
        try {
          const prepared = await serial(async () => {
            if (retired) throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retired')
            if (cancelled) return { local: { state: 'cancelled' } as const }
            if (refusal) return { local: refusal }
            const exactId = await ensureReserved()
            startsInFlight += 1
            return { exactId }
          })
          if ('local' in prepared && prepared.local !== undefined) return prepared.local
          try {
            const result = await transport.start(prepared.exactId, callerKey)
            if (isRefusal(result) && !retired) refusal = result
            return result
          } finally {
            startsInFlight -= 1
          }
        } catch (error) {
          if (error instanceof ReclaimRefused) return error.receipt
          throw error
        }
      },
      status: async () => {
        if (retired) return { state: 'retired' }
        if (reserved && id !== null) {
          const result = await transport.status(id, callerKey)
          if (retired) return { state: 'retired' }
          if (
            retirementAttempted &&
            typeof result === 'object' &&
            result !== null &&
            Reflect.get(result, 'state') === 'retired'
          )
            return { state: 'retiring' }
          if (isRefusal(result) && !retirementAttempted) refusal = result
          return result
        }
        if (retirementAttempted) return { state: 'retiring' }
        if (cancelled) return { state: 'cancelled' }
        if (refusal) return refusal
        try {
          const prepared = await serial(async () => {
            if (retired) return { local: { state: 'retired' } as const }
            if (retirementAttempted) return { local: { state: 'retiring' } as const }
            if (cancelled) return { local: { state: 'cancelled' } as const }
            if (refusal) return { local: refusal }
            if (id === null && !allocationAttempted) return { local: { state: 'idle' } as const }
            return { exactId: await ensureReserved() }
          })
          if ('local' in prepared && prepared.local !== undefined) return prepared.local
          const result = await transport.status(prepared.exactId, callerKey)
          if (isRefusal(result) && !retired) refusal = result
          return result
        } catch (error) {
          if (error instanceof ReclaimRefused) return error.receipt
          throw error
        }
      },
      // Queue only preparation, never the bridge response. A missing response
      // cannot prevent the exact reservation's status and fence being queried.
      commit: () => {
        if (reserved && id !== null && !retired && !retirementAttempted && !cancelled && !refusal) {
          commitsInFlight += 1
          return performCommit(id)
        }
        return serial(async () => {
          const exactId = await ensureReserved()
          commitsInFlight += 1
          return exactId
        }).then(performCommit)
      },
      cancel: async () => {
        if (retired) return { state: 'retired' }
        if (retirementRequested)
          throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retiring')
        if (refusal) return refusal
        if (reserved && id !== null) return transport.cancel(id, callerKey)
        try {
          const prepared = await serial(async () => {
            if (retired) return { local: { state: 'retired' } as const }
            if (retirementRequested)
              throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retiring')
            if (refusal) return { local: refusal }
            if (id === null && !allocationAttempted) {
              cancelled = true
              return { local: { state: 'cancelled' } as const }
            }
            return { exactId: await ensureReserved(true) }
          })
          if ('local' in prepared && prepared.local !== undefined) return prepared.local
          return await transport.cancel(prepared.exactId, callerKey)
        } catch (error) {
          if (error instanceof ReclaimRefused) return error.receipt
          throw error
        }
      },
      release: async () => {
        if (retirementRequested)
          throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retiring')
        if (reserved && id !== null) return transport.release(id, callerKey)
        const exactId = await serial(async () => {
          if (retirementRequested)
            throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.retiring')
          return ensureReserved(true)
        })
        return transport.release(exactId, callerKey)
      },
      retire: () =>
        serial(async () => {
          if (retired) return
          // Until the native protocol proves a fence also covers a delayed
          // dispatch, do not discard the receipt while a commit may arrive.
          if (startsInFlight > 0 || commitsInFlight > 0)
            throw contractError('lifecycle.invalid-state', 'cleanup', 'ordered-recording.reclaim.request-pending')
          retirementAttempted = true
          if (id !== null || allocationAttempted) {
            try {
              const exactId = await ensureReserved(true)
              if (!retirementRequested) {
                await transport.retire(exactId, callerKey)
                retirementRequested = true
              }
              if (!retirementAcknowledged) {
                await transport.acknowledgeRetirement(exactId, callerKey)
                retirementAcknowledged = true
              }
              if (!retirementForgotten) {
                await transport.forgetRetirement(exactId, callerKey)
                retirementForgotten = true
              }
              await transport.acknowledgeForget?.(exactId, callerKey)
            } catch (error) {
              if (!(error instanceof ReclaimRefused)) throw error
            }
          }
          retired = true
          active = false
        })
    })
  }
}
