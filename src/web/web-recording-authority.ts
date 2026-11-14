import { webRecordingRandomBytes, webRecordingAuthorityCombinedError } from './web-recording-errors'
import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { webRecordingContractError as contractError } from './web-recording-errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import { acquireWebRecordingLock, awaitWebRecordingAbortable } from './web-recording-lock'
import type { WebRecordingLock, WebRecordingLockManager } from './web-recording-lock'
import type { WebOrderedJournalDatabase, WebOrderedRecordingDeclaration } from './web-ordered-recording-journal'
import { probeWebRecordingSharedWorker } from './web-recording-authority-worker-client'
import { createWebAuthorityReservations, createWebAuthorityToken } from './web-authority-reservation'
import type {
  WebAuthorityCoordinator,
  WebAuthorityOwner,
  WebAuthorityRecovery,
  WebAuthorityReconciliation
} from './web-authority-reservation'

declare const crypto: { getRandomValues(bytes: Uint8Array): Uint8Array } | undefined
export interface WebRecordingAuthorityOptions {
  readonly database: WebOrderedJournalDatabase
  readonly locks?: WebRecordingLockManager
  readonly workerUrl?: string
  readonly crypto?: { getRandomValues(bytes: Uint8Array): Uint8Array }
  readonly digest: (id: string) => Promise<string>
  readonly onLoss?: (error: unknown) => void
  readonly signal?: AbortSignal
}
type State = 'prepared' | 'live' | 'sealed' | 'interrupted'
export type WebRecordingDurableStatus =
  | { readonly state: 'missing'; readonly durability: 'strict' | 'default' }
  | {
      readonly state: State
      readonly ownerEpoch: string
      readonly uncertainTail: boolean | null
      readonly durability: 'strict' | 'default'
    }
type Declaration = Omit<WebOrderedRecordingDeclaration, 'ownerEpoch'>
type Lease = Extract<WebRecordingLock, { state: 'acquired' }>
type Transition = {
  readonly id: string
  work: Promise<unknown>
  rejected: boolean
  barrier?: Promise<WebAuthorityReconciliation>
  finishing?: Promise<void>
}
const failure = (detail: string): Error => contractError('protocol.malformed', 'restoration', `web-recording.${detail}`)
const notCurrent = (): Error => contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
const unknown = (): Error =>
  contractError('capability.unavailable', 'restoration', 'web-recording.transaction-outcome-unknown')
const aborted = (): Error => contractError('operation.aborted', 'restoration', 'web-recording.acquire')
const MAX_PENDING = 16
const WAIT_MS = 5000
let pendingReservations = 0
async function waitBounded<T>(work: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => reject(unknown()), WAIT_MS)
  })
  try {
    return await Promise.race([work, timeout])
  } finally {
    clearTimeout(timer)
  }
}
async function committedStatus(database: WebOrderedJournalDatabase, id: string): Promise<WebRecordingDurableStatus> {
  let status:
    | Omit<Exclude<WebRecordingDurableStatus, { state: 'missing' }>, 'durability'>
    | { state: 'missing' }
    | undefined
  let problem: unknown
  const receipt = await runWebIndexedDbTransaction(database, ['recordings'], 'readonly', tx => {
    const request = tx.objectStore('recordings').get(id)
    request.onsuccess = () => {
      try {
        const row = request.result
        if (row === undefined) {
          status = { state: 'missing' }
          return
        }
        if (typeof row !== 'object' || row === null || Array.isArray(row)) throw failure('owner-row')
        const data = Object.fromEntries(Object.entries(row))
        if (
          data.id !== id ||
          data.version !== 1 ||
          data.kind !== 'ordered' ||
          !isCanonicalIngressOrdinal(data.ownerEpoch) ||
          data.ownerEpoch === '0'
        )
          throw failure('owner-row')
        const state = data.ownerState
        if (state !== 'prepared' && state !== 'live' && state !== 'sealed' && state !== 'interrupted')
          throw failure('owner-row')
        status = {
          state,
          ownerEpoch: data.ownerEpoch,
          uncertainTail: state === 'prepared' ? false : state === 'interrupted' ? true : null
        }
      } catch (error) {
        problem = error
        try {
          tx.abort()
        } catch (abortError) {
          problem = webIndexedDbQueueAbortError(error, abortError)
        }
      }
    }
  }).catch(error => {
    throw problem ?? error
  })
  if (problem !== undefined) throw problem
  if (status === undefined) throw failure('incomplete-transaction')
  return { ...status, durability: receipt.durability }
}

/** The caller must supply a cryptographically secure random source. Never log this value. */
export function createWebRecordingIncarnation(source: { getRandomValues(bytes: Uint8Array): Uint8Array }): string {
  const bytes = new Uint8Array(32)
  webRecordingRandomBytes(source, bytes)
  return Array.from(bytes, value => value.toString(16).padStart(2, '0')).join('')
}

/** Use the partition's Web Crypto, without exposing the recording ID in a lock
 * name or a worker message. */
export function webRecordingSha256(source: {
  readonly subtle: { digest(algorithm: 'SHA-256', data: Uint8Array): Promise<ArrayBuffer> }
}): (id: string) => Promise<string> {
  return async id => {
    const hash = await source.subtle.digest('SHA-256', new TextEncoder().encode(id))
    return Array.from(new Uint8Array(hash), byte => byte.toString(16).padStart(2, '0')).join('')
  }
}

/** Set up the coordinator before consulting the common durable authority. A
 * worker HELLO is a compatibility probe; it is never an ownership grant. */
export async function createWebRecordingAuthority(
  options: WebRecordingAuthorityOptions,
  id: string,
  onDisposition?: (state: 'none' | 'held' | 'release-unproven') => void
): Promise<
  | {
      readonly state: 'busy'
      readonly lastCommitted: WebRecordingDurableStatus | { readonly state: 'unavailable'; readonly error: unknown }
    }
  | { readonly state: 'acquired'; readonly authority: WebRecordingAuthority }
> {
  if (!isRecordingId(id)) throw contractError('argument.invalid', 'restoration', 'web-recording.lock-id')
  if (options.signal?.aborted) throw aborted()
  const random = options.crypto ?? (typeof crypto === 'undefined' ? undefined : crypto)
  if (random === undefined) throw contractError('capability.unavailable', 'restoration', 'web-recording.entropy')
  let lost = false
  const fence = { lost: false }
  const onLost = (error: unknown): void => {
    lost = true
    fence.lost = true
    options.onLoss?.(error)
  }
  let lease: Lease
  let coordinator: WebAuthorityCoordinator
  if (options.locks !== undefined) {
    const hash = await awaitWebRecordingAbortable(options.digest(id), options.signal)
    if (!/^[0-9a-f]{64}$/.test(hash)) throw failure('lock-digest')
    const result = await acquireWebRecordingLock(options.locks, async () => hash, id, onLost, options.signal)
    if (result.state === 'busy') {
      try {
        return { state: 'busy', lastCommitted: await committedStatus(options.database, id) }
      } catch (error) {
        return { state: 'busy', lastCommitted: { state: 'unavailable', error } }
      }
    }
    lease = result
    coordinator = { kind: 'locks', name: `ubm-ordered-recording:${hash}` }
  } else if (options.workerUrl !== undefined) {
    const identity = await probeWebRecordingSharedWorker(options.workerUrl, options.signal)
    coordinator = { kind: 'worker', url: identity.url, name: identity.name, type: identity.type }
    let finish: (() => void) | undefined
    const ended = new Promise<{ state: 'released' }>(resolve => {
      finish = () => resolve({ state: 'released' })
    })
    lease = {
      state: 'acquired',
      ended,
      release: async () => {
        finish?.()
      }
    }
  } else throw contractError('capability.unsupported', 'restoration', 'web-recording.no-exclusive-coordinator')
  onDisposition?.('held')
  try {
    const reservation = await awaitWebRecordingAbortable(
      createWebAuthorityReservations(options.database).status(id),
      options.signal
    )
    if (options.signal?.aborted) throw aborted()
    if (lost) throw contractError('capability.unavailable', 'restoration', 'web-recording.authority-lost')
    if (reservation.phase !== 'missing' && reservation.phase !== 'released') {
      let lastCommitted: WebRecordingDurableStatus | { state: 'unavailable'; error: unknown }
      try {
        lastCommitted = await committedStatus(options.database, id)
      } catch (error) {
        lastCommitted = { state: 'unavailable', error }
      }
      await lease.release()
      onDisposition?.('none')
      return { state: 'busy', lastCommitted }
    }
    return {
      state: 'acquired',
      authority: new WebRecordingAuthority(
        options,
        id,
        lease,
        fence,
        coordinator,
        random,
        reservation.phase === 'released' ? reservation.generation : null
      )
    }
  } catch (error) {
    try {
      await lease.release()
      onDisposition?.('none')
    } catch (releaseError) {
      onDisposition?.('release-unproven')
      throw webRecordingAuthorityCombinedError(error, releaseError)
    }
    throw error
  }
}

/** Realm-local coordinator for one exact durable reservation. */
export class WebRecordingAuthority {
  private active = true
  private closing = false
  private owner: WebAuthorityOwner | undefined
  private revokeId: string | undefined
  private revocation: ReturnType<ReturnType<typeof createWebAuthorityReservations>['revokeProvisional']> | undefined
  private revokeBarrier: Promise<WebAuthorityReconciliation> | undefined
  private incarnation: string | undefined
  private pending: Promise<void> | undefined
  private retryReconciliation: (() => Promise<void>) | undefined
  private acquisitionSlot: { held: boolean; workSettled: boolean; reconciling: boolean } | undefined
  private acquiring = false
  private cleanup: Promise<void> | undefined
  private readonly reservations
  private cleanupTransition: Transition | undefined
  private releaseTransition: Transition | undefined
  private armed = false
  private recovery:
    | {
        readonly input: WebAuthorityRecovery
        work: Promise<WebAuthorityOwner>
        barrier?: ReturnType<ReturnType<typeof createWebAuthorityReservations>['reconcileRecovery']>
        slotHeld: boolean
      }
    | undefined
  private recovering: Promise<WebRecordingDurableStatus> | undefined
  constructor(
    private readonly options: WebRecordingAuthorityOptions,
    private readonly id: string,
    private readonly lock: Lease,
    private readonly fence: { lost: boolean },
    private readonly coordinator: WebAuthorityCoordinator,
    private readonly random: { getRandomValues(bytes: Uint8Array): Uint8Array },
    private readonly expectedGeneration: string | null
  ) {
    this.reservations = createWebAuthorityReservations(options.database)
    lock.ended.then(() => {
      this.active = false
    })
  }
  current(): boolean {
    return (
      this.active &&
      !this.fence.lost &&
      !this.closing &&
      this.pending === undefined &&
      !this.acquiring &&
      this.recovery === undefined &&
      this.recovering === undefined
    )
  }
  reservationOwner(): WebAuthorityOwner {
    if (!this.current() || this.owner === undefined) throw notCurrent()
    return { ...this.owner }
  }
  noteOpenCommitted(): void {
    this.armed = true
  }
  async beginCleanup(): Promise<void> {
    const owner = this.owner
    if (!this.current() || owner === undefined) throw notCurrent()
    if (this.cleanupTransition === undefined) {
      const id = createWebAuthorityToken(this.random)
      this.cleanupTransition = this.startTransition(id, () => this.reservations.cleanupPending(owner, id))
    }
    const transition = this.cleanupTransition
    await this.finishTransition(owner, transition, 'cleanup-pending', () =>
      this.reservations.cleanupPending(owner, transition.id)
    )
  }
  private startTransition(id: string, dispatch: () => Promise<unknown>): Transition {
    const transition: Transition = { id, work: Promise.resolve(), rejected: false }
    transition.work = dispatch().catch(error => {
      transition.rejected = true
      throw error
    })
    return transition
  }
  private finishTransition(
    owner: WebAuthorityOwner,
    transition: Transition,
    expected: 'cleanup-pending' | 'released',
    dispatch: () => Promise<unknown>
  ): Promise<void> {
    if (transition.finishing !== undefined) return transition.finishing
    const work = this.finishTransitionWork(owner, transition, expected, dispatch)
    transition.finishing = work
    work.then(
      () => {
        if (transition.finishing === work) transition.finishing = undefined
      },
      () => {
        if (transition.finishing === work) transition.finishing = undefined
      }
    )
    return work
  }
  private async finishTransitionWork(
    owner: WebAuthorityOwner,
    transition: Transition,
    expected: 'cleanup-pending' | 'released',
    dispatch: () => Promise<unknown>
  ): Promise<void> {
    if (transition.rejected) {
      const receipt = await waitBounded(this.transitionBarrier(owner, transition))
      if (receipt.outcome === 'committed' && receipt.result.state === expected) return
      transition.rejected = false
      transition.work = dispatch().catch(error => {
        transition.rejected = true
        throw error
      })
    }
    try {
      await waitBounded(transition.work)
    } catch (error) {
      const receipt = await waitBounded(this.transitionBarrier(owner, transition))
      if (receipt.outcome !== 'committed' || receipt.result.state !== expected) throw error
    }
  }
  private transitionBarrier(owner: WebAuthorityOwner, transition: Transition): Promise<WebAuthorityReconciliation> {
    if (transition.barrier === undefined) {
      const barrier = this.reservations.reconcile(owner, transition.id)
      transition.barrier = barrier
      barrier.then(
        () => {
          if (transition.barrier === barrier) transition.barrier = undefined
        },
        () => {
          if (transition.barrier === barrier) transition.barrier = undefined
        }
      )
    }
    return transition.barrier
  }
  get ended(): Lease['ended'] {
    return this.lock.ended
  }
  createProducerIncarnation(source: { getRandomValues(bytes: Uint8Array): Uint8Array }): string {
    if (!this.current() || this.owner !== undefined) throw notCurrent()
    if (this.incarnation === undefined) this.incarnation = createWebRecordingIncarnation(source)
    return this.incarnation
  }
  async declare(declaration: Declaration): Promise<{
    readonly state: 'prepared'
    readonly ownerEpoch: string
    readonly uncertainTail: false
    /** The reservation core does not return the transaction durability mode. */
    readonly durability: null
  }> {
    if (!this.current() || this.owner !== undefined) throw notCurrent()
    if (declaration.id !== this.id)
      throw contractError('argument.invalid', 'restoration', 'web-recording.declaration-id')
    if (this.incarnation === undefined || declaration.producerIncarnation !== this.incarnation)
      throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.incarnation')
    if (pendingReservations >= MAX_PENDING)
      throw contractError('capability.unavailable', 'restoration', 'web-recording.reservation-limit')
    const ownerToken = createWebAuthorityToken(this.random)
    const operationId = createWebAuthorityToken(this.random)
    const revokeId = createWebAuthorityToken(this.random)
    pendingReservations++
    // One slot covers the original acquisition and any overlapping barrier.
    // A failed barrier cannot free it while the original remains unresolved.
    const slot = { held: true, workSettled: false, reconciling: false }
    this.acquisitionSlot = slot
    const releaseSlot = (): void => {
      if (slot.held) {
        slot.held = false
        pendingReservations--
      }
    }
    this.acquiring = true
    let settle: (() => void) | undefined
    let rejectPending: ((error: unknown) => void) | undefined
    const pending = new Promise<void>((resolve, reject) => {
      settle = resolve
      rejectPending = reject
    })
    this.pending = pending
    pending.catch(() => undefined)
    let abandoned = false
    let interrupted = false
    let work: ReturnType<typeof this.reservations.acquire>
    try {
      work = this.reservations.acquire({
        declaration,
        ownerToken,
        operationId,
        coordinator: this.coordinator,
        expectedGeneration: this.expectedGeneration
      })
    } catch (error) {
      releaseSlot()
      this.acquiring = false
      this.pending = undefined
      settle?.()
      throw error
    }
    work.then(
      () => {
        slot.workSettled = true
        if (abandoned && !slot.reconciling) releaseSlot()
      },
      () => {
        slot.workSettled = true
        if (abandoned && !slot.reconciling) releaseSlot()
      }
    )
    let timer: ReturnType<typeof setTimeout> | undefined
    let abortListener: (() => void) | undefined
    const interruption = new Promise<never>((_resolve, reject) => {
      timer = setTimeout(() => {
        interrupted = true
        reject(unknown())
      }, WAIT_MS)
      abortListener = () => {
        interrupted = true
        reject(aborted())
      }
      this.options.signal?.addEventListener('abort', abortListener, { once: true })
      if (this.options.signal?.aborted) abortListener()
    })
    try {
      const result = await Promise.race([work, interruption])
      if (result.state === 'busy')
        throw contractError('capability.unavailable', 'restoration', 'web-recording.authority-busy')
      this.owner = result.owner
      return { state: 'prepared', ownerEpoch: result.owner.ownerEpoch, uncertainTail: false, durability: null }
    } catch (error) {
      // A completion can be delayed or suppressed even after the transaction
      // committed. The fresh overlapping transaction uses its exact receipt.
      if (interrupted) {
        abandoned = true
        this.fence.lost = true
        this.retryReconciliation = async () => {
          const result = await this.reservations.reconcileAcquire(
            this.id,
            ownerToken,
            operationId,
            this.expectedGeneration
          )
          if (result.outcome === 'absent') return
          if (result.outcome !== 'committed') throw unknown()
          this.owner = result.owner
          this.revokeId = revokeId
          await this.revoke()
        }
        slot.reconciling = true
        const reconcile = this.retryReconciliation()
        reconcile.then(
          () => {
            slot.reconciling = false
            releaseSlot()
            this.acquiring = false
            this.pending = undefined
            this.retryReconciliation = undefined
            settle?.()
          },
          cause => {
            slot.reconciling = false
            if (slot.workSettled) releaseSlot()
            this.acquiring = false
            this.pending = undefined
            rejectPending?.(cause)
            this.options.onLoss?.(cause)
          }
        )
      }
      throw error
    } finally {
      if (timer !== undefined) clearTimeout(timer)
      if (abortListener !== undefined) this.options.signal?.removeEventListener('abort', abortListener)
      if (!abandoned) {
        releaseSlot()
        this.acquiring = false
        this.pending = undefined
        settle?.()
      }
    }
  }
  async recover(): Promise<WebRecordingDurableStatus> {
    if (this.recovering !== undefined) return this.recovering
    if (
      !this.active ||
      this.fence.lost ||
      this.closing ||
      this.pending !== undefined ||
      this.acquiring ||
      this.owner !== undefined
    )
      throw notCurrent()
    if (this.expectedGeneration === null) {
      const status = await committedStatus(this.options.database, this.id)
      if (status.state !== 'missing') throw notCurrent()
      return status
    }
    if (this.recovery === undefined) {
      if (pendingReservations >= MAX_PENDING)
        throw contractError('capability.unavailable', 'restoration', 'web-recording.reservation-limit')
      const input = {
        id: this.id,
        ownerToken: createWebAuthorityToken(this.random),
        operationId: createWebAuthorityToken(this.random),
        expectedGeneration: this.expectedGeneration,
        coordinator: this.coordinator
      }
      pendingReservations++
      const work = this.reservations.recover(input)
      work.catch(() => undefined)
      this.recovery = { input, work, slotHeld: true }
    }
    return this.ensureRecovery()
  }
  private ensureRecovery(): Promise<WebRecordingDurableStatus> {
    if (this.recovering !== undefined) return this.recovering
    const work = this.finishRecovery()
    this.recovering = work
    work.then(
      () => {
        if (this.recovering === work) this.recovering = undefined
      },
      () => {
        if (this.recovering === work) this.recovering = undefined
      }
    )
    return work
  }
  private async finishRecovery(): Promise<WebRecordingDurableStatus> {
    const attempt = this.recovery
    if (attempt === undefined) throw notCurrent()
    let owner: WebAuthorityOwner
    try {
      owner = await waitBounded(attempt.work)
    } catch (cause) {
      // A fresh overlapping transaction checks the exact operation receipt.
      // A status read cannot prove this operation committed.
      if (attempt.barrier === undefined) {
        const barrier = this.reservations.reconcileRecovery(attempt.input)
        attempt.barrier = barrier
        barrier.then(
          () => {
            if (attempt.barrier === barrier) attempt.barrier = undefined
          },
          () => {
            if (attempt.barrier === barrier) attempt.barrier = undefined
          }
        )
      }
      const result = await waitBounded(attempt.barrier)
      if (result.outcome === 'absent') {
        if (attempt.slotHeld) {
          attempt.slotHeld = false
          pendingReservations--
        }
        this.recovery = undefined
        throw cause
      }
      if (result.outcome !== 'committed') throw unknown()
      owner = result.owner
    }
    if (attempt.slotHeld) {
      attempt.slotHeld = false
      pendingReservations--
    }
    this.owner = owner
    this.armed = true
    this.recovery = undefined
    return committedStatus(this.options.database, this.id)
  }
  async revalidate(graphIsCurrent: () => boolean): Promise<void> {
    const owner = this.owner
    if (!this.current() || owner === undefined) throw notCurrent()
    const validation = (async () => {
      try {
        await this.reservations.validateOwner(owner, this.armed ? 'armed' : 'provisional')
        if (!this.active || this.fence.lost || !graphIsCurrent()) throw notCurrent()
      } catch (error) {
        if (!this.fence.lost) {
          this.fence.lost = true
          this.options.onLoss?.(error)
        }
        throw error
      }
    })()
    const pending = validation.then(
      () => undefined,
      () => undefined
    )
    this.pending = pending
    try {
      await validation
    } finally {
      if (this.pending === pending) this.pending = undefined
    }
  }
  private async revoke(): Promise<void> {
    const owner = this.owner
    if (owner === undefined) return
    if (this.revokeId === undefined) this.revokeId = createWebAuthorityToken(this.random)
    const operationId = this.revokeId
    if (this.revocation === undefined) this.revocation = this.reservations.revokeProvisional(owner, operationId)
    const work = this.revocation
    let timer: ReturnType<typeof setTimeout> | undefined
    const timeout = (): Promise<'unknown'> =>
      new Promise(resolve => {
        timer = setTimeout(() => resolve('unknown'), WAIT_MS)
      })
    let result: Awaited<typeof work> | 'unknown'
    try {
      result = await Promise.race([work, timeout()])
    } catch (error) {
      this.revocation = undefined
      throw error
    } finally {
      clearTimeout(timer)
    }
    if (result === 'unknown') {
      if (this.revokeBarrier === undefined) {
        const barrier = this.reservations.reconcile(owner, operationId)
        this.revokeBarrier = barrier
        barrier.then(
          () => {
            if (this.revokeBarrier === barrier) this.revokeBarrier = undefined
          },
          () => {
            if (this.revokeBarrier === barrier) this.revokeBarrier = undefined
          }
        )
      }
      const receipt = await Promise.race([this.revokeBarrier, timeout()]).finally(() => clearTimeout(timer))
      if (receipt === 'unknown' || receipt.outcome !== 'committed' || receipt.result.state !== 'released')
        throw unknown()
    }
    this.owner = undefined
  }
  async close(cleanup: () => Promise<void>): Promise<void> {
    if (this.cleanup !== undefined) return this.cleanup
    this.closing = true
    const run = (async () => {
      if (this.recovering !== undefined || this.recovery !== undefined) await waitBounded(this.ensureRecovery())
      if (this.pending !== undefined) await waitBounded(this.pending)
      if (this.retryReconciliation !== undefined) {
        // Reuse the original slot while its transaction remains outstanding.
        const slot = this.acquisitionSlot
        if (slot === undefined) throw notCurrent()
        if (!slot.held) {
          if (pendingReservations >= MAX_PENDING)
            throw contractError('capability.unavailable', 'restoration', 'web-recording.reservation-limit')
          pendingReservations++
          slot.held = true
        }
        slot.reconciling = true
        const releaseSlot = (): void => {
          if (slot.held) {
            slot.held = false
            pendingReservations--
          }
        }
        const retry = this.retryReconciliation()
        this.pending = retry
        retry.then(
          () => {
            slot.reconciling = false
            releaseSlot()
            this.pending = undefined
            this.retryReconciliation = undefined
          },
          () => {
            slot.reconciling = false
            if (slot.workSettled) releaseSlot()
            this.pending = undefined
          }
        )
        await waitBounded(retry)
      }
      await cleanup()
      if (this.owner !== undefined) {
        if (!this.armed) await this.revoke()
        else {
          if (this.releaseTransition === undefined) {
            const id = createWebAuthorityToken(this.random)
            const owner = this.owner
            this.releaseTransition = this.startTransition(id, () => this.reservations.release(owner, id))
          }
          const owner = this.owner
          const transition = this.releaseTransition
          await this.finishTransition(owner, transition, 'released', () =>
            this.reservations.release(owner, transition.id)
          )
          this.owner = undefined
        }
      }
      await this.lock.release()
      this.active = false
    })()
    this.cleanup = run
    try {
      await run
    } catch (error) {
      this.cleanup = undefined
      this.closing = false
      throw error
    }
  }
  release(): Promise<void> {
    return this.close(() => Promise.resolve())
  }
}
