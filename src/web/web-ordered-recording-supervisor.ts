import {
  WebRetainedRecordingPump,
  type WebRetainedAcknowledgedDelivery,
  type WebRetainedRecordingJournal
} from '../core/web-retained-recording-pump'
import type { WebIngressTerminalReason, WebRetainedIngressGroup } from './web-notification-ingress'
import type { WebRecordingTerminalObservation } from './web-ordered-recording-journal'
import { webRecordingBoundedPending, webRecordingReadinessRetryable } from './web-indexeddb-transaction'
import { webRecordingOriginalFailure, webRecordingReconciledAbsentDelivery } from './web-recording-journal-bridge'

function retryableReadiness(error: unknown, depth = 0, visited = new Set<unknown>()): boolean {
  if (depth >= 16 || visited.has(error)) return false
  visited.add(error)
  const original = webRecordingOriginalFailure(error)
  if (original !== error) return retryableReadiness(original, depth + 1, visited)
  const readiness = webRecordingReadinessRetryable(error)
  if (readiness !== undefined) return readiness
  try {
    if (error instanceof AggregateError)
      return error.errors.slice(0, 64).some(child => retryableReadiness(child, depth + 1, visited))
    return false
  } catch {
    return false
  }
}
function outcomeUnknown(error: unknown): boolean {
  try {
    return error instanceof WebRecordingOutcomeUnknown
  } catch {
    return false
  }
}

type TerminalObservation = Omit<WebRecordingTerminalObservation, 'deliveryId' | 'operationId'>

/** The source owner creates these before enable and retains late enable effects. */
export interface WebRecordingMemberLease {
  readonly id: string
  enable(): Promise<void>
  /** Must release only this lease and be retryable after failure. */
  release(): Promise<void>
}

/** Installed synchronously before any enable. A failed install retains any
 * unresolved rollback for release to reconcile. The owner
 * wakes when work may be available, including controls and terminal. Wake is a hint. */
export interface WebRecordingSourceOwner {
  /** Allocate immutable epochs without installing routes or invoking leases. */
  reserve(): { memberships(): readonly { readonly member: string; readonly epoch: string }[] }
  install(wake: () => void): {
    readonly source: WebRetainedIngressGroup
    readonly leases: readonly WebRecordingMemberLease[]
  }
  /** Releases the source reservation, after its FIFO and member leases. */
  release(): Promise<void>
  /** Detach a fenced source without disposing its retained FIFO. */
  releaseRetainingDeliveries?(): Promise<void>
  /** Synchronous authority, connection and graph revalidation on restoration. */
  isCurrent(): boolean
  acknowledged?(delivery: WebRetainedAcknowledgedDelivery): void | Promise<void>
  hasMarkerDebt?(): boolean
}

/** Every mutating operation must check the current durable owner epoch. prepare
 * acquires exclusive authority; open declares the reserved membership and
 * commits the exact source-open intent. Commit resolves only on transaction completion (or
 * exact-identity reconciliation). No implementation here supplies storage. */
export interface WebRecordingSupervisorJournal extends WebRetainedRecordingJournal {
  prepare(signal?: AbortSignal): Promise<void>
  open(
    source: { memberships(): readonly { readonly member: string; readonly epoch: string }[] | null },
    mayOpen?: () => boolean
  ): Promise<void>
  /** An overlapping barrier and exact operation receipt, never a status guess. */
  reconcileOpen?(): Promise<'committed' | 'aborted' | 'unknown'>
  finalizeUnstarted?(reason: WebIngressTerminalReason, onCommitted: () => void): Promise<void>
  onAuthorityLoss?(handler: () => void): void
  beginCleanup?(): Promise<void>
  /** A repeated seal must serialize behind and verify the exact prior receipt. */
  canRetrySeal?(): boolean
  seal(): Promise<void>
  observeTerminal?(observation: TerminalObservation): Promise<void>
  /** Ordered exact-identity read behind a dispatched observation. */
  reconcileObservation?(): Promise<'committed' | 'absent' | 'unknown'>
  releaseAuthority(): Promise<void>
  classifyFailure(error: unknown): 'capacity' | 'failure'
  /** True only with proof that the transaction did not commit. */
  isDefinitiveFailure?(error: unknown): boolean
  /** Once the rejected operation has settled, retry is safe only if the port
   * reconciles ambiguous effects before issuing another mutation. */
  canRetryUnknown?(): boolean
}

export type WebRecordingSupervisorState =
  | 'preparing'
  | 'active'
  | 'waiting-source'
  | 'waiting-drain'
  | 'waiting-capacity'
  | 'failed'
  | 'finished'

export interface WebRecordingSupervisorStatus {
  readonly state: WebRecordingSupervisorState
  readonly sourceStarted: boolean | 'unknown'
  readonly terminalCommitted: boolean
  readonly terminalReason: WebIngressTerminalReason | null
  readonly sealed: boolean
  readonly sourceReleased: boolean
  readonly authorityReleased: boolean
  /** The worker, rather than the source, caused the terminal fence. */
  readonly workerStoppedSource: boolean
  readonly failure: unknown | null
  readonly storageOutcomeUnknown: boolean
  /** A terminal-observation write failed and remains to be retried. */
  readonly observationFailure: boolean
  readonly storageUncertaintyEvidence: readonly unknown[]
  readonly setupFailure: unknown | null
  readonly setupFailurePhase: 'setup' | 'other'
  /** This port has no operation that durably commits failure evidence. */
  readonly runtimeFailure: { readonly error: unknown; readonly persisted: false } | null
  readonly runtimeFailurePhase: 'setup' | 'cleanup' | 'other'
  readonly cleanupFailure: unknown | null
  readonly members: readonly {
    readonly id: string
    readonly acquisition: 'pending' | 'enabling' | 'acquired' | 'failed'
    readonly acquisitionFailure: unknown | null
    readonly cleanup: 'not-needed' | 'pending' | 'released' | 'failed'
    readonly cleanupFailure: unknown | null
  }[]
}

type MemberState = {
  lease: WebRecordingMemberLease
  acquisition: 'pending' | 'enabling' | 'acquired' | 'failed'
  acquisitionFailure: unknown | null
  cleanup: 'not-needed' | 'pending' | 'released' | 'failed'
  cleanupFailure: unknown | null
  releasing: Promise<void> | null
}

const STORAGE_OUTCOME_TIMEOUT_MS = 30_000

class WebRecordingOutcomeUnknown extends Error {
  constructor() {
    super('Web recording storage outcome is unknown')
    this.name = 'WebRecordingOutcomeUnknown'
  }
}

/** Realm-local owner. Public receipts, locks, graph validation and persistent
 * journal implementations are supplied by the integrating host. If open
 * commits after cancellation or installation fails, there is no source FIFO
 * from which to prove a terminal: authority remains held for recovery. Public
 * integration must reconcile that case and retain the owner through cleanup. */
export class WebOrderedRecordingSupervisor {
  private source: WebRetainedIngressGroup | null = null
  private pump: WebRetainedRecordingPump | null = null
  private members: MemberState[] = []
  private startResult: Promise<WebRecordingSupervisorStatus> | null = null
  private work: Promise<void> | null = null
  private unknownOperation: Promise<unknown> | null = null
  private unknownOperationKind: 'seal' | 'observation' | 'other' | null = null
  private sealReconciliation: Promise<void> | null = null
  private openReconciliation: Promise<'committed' | 'aborted' | 'unknown'> | null = null
  private unknownRejection = false
  private recoveryMadeProgress = false
  private opened = false
  private setupDone = false
  private stopped = false
  private stopReason: WebIngressTerminalReason = 'source-failed'
  private authorityLost = false
  private openState: 'not-opened' | 'opening' | 'committed' | 'aborted' | 'unknown' = 'not-opened'
  private abortedTerminalDiscarded = false
  private sourceReleased = false
  private ownerReserved = false
  private authorityPreparationStarted = false
  private authorityReleased = false
  private workerStoppedSource = false
  private sealed = false
  private terminalObservation: TerminalObservation | null = null
  private observationRetained = false
  private observationFailure = false
  private observationOutcomeUnknown = false
  private readonly observationReconciliationFailures: unknown[] = []
  private readonly observedReconciliationAttempts = new WeakSet<Promise<unknown>>()
  private cleanupPending = false
  private sourceSealed = false
  private terminalCommitted = false
  private unstartedFinalized = false
  private failure: unknown | null = null
  private lastRuntimeFailure: unknown | null = null
  private lastRuntimeFailurePhase: 'setup' | 'cleanup' | 'other' = 'other'
  // Recovery follows the failure that blocked work, not later observations of
  // independently dispatched cleanup failures.
  private recoveryFailure: unknown | null = null
  private setupFailure: unknown | null = null
  private setupFailurePhase: 'setup' | 'other' = 'setup'
  private cleanupFailure: unknown | null = null
  private capacity = false
  private blocked = false
  private wakePending = false
  private startWaiters = 0
  private startState: WebRecordingSupervisorState = 'preparing'
  private readonly preparationCancellation = new AbortController()
  private preparingAuthority = false
  private retirementNotified = false
  private recoveryTimer: ReturnType<typeof setTimeout> | undefined
  private recoveryAttempts = 0

  constructor(
    private readonly owner: WebRecordingSourceOwner,
    private readonly journal: WebRecordingSupervisorJournal,
    private readonly observeFinal?: () => Promise<TerminalObservation>,
    private readonly onAuthorityReleased?: () => Promise<void>
  ) {
    journal.onAuthorityLoss?.(() => this.loseAuthority())
  }

  /** Abort cancels only this waiter; pre-aborted calls do not start ownership. */
  start(signal?: AbortSignal): Promise<WebRecordingSupervisorStatus> {
    if (signal?.aborted) return Promise.reject(aborted())
    if (this.startResult === null) this.startResult = Promise.resolve().then(() => this.startOwner())
    this.startWaiters += 1
    let waiting = true
    const releaseWaiter = (): void => {
      if (waiting) {
        waiting = false
        this.startWaiters -= 1
      }
    }
    const result = waitFor(this.startResult, signal, () => {
      releaseWaiter()
      if (this.startWaiters === 0 && this.source === null && !this.setupDone) {
        if (this.preparingAuthority) this.preparationCancellation.abort()
        this.stop('operation-aborted')
      }
    })
    return result.then(
      value => {
        releaseWaiter()
        return value
      },
      error => {
        releaseWaiter()
        throw error
      }
    )
  }

  private async startOwner(): Promise<WebRecordingSupervisorStatus> {
    let phase: 'setup' | 'other' = 'setup'
    try {
      if (this.stopped) return this.status()
      const reservation = this.owner.reserve()
      this.ownerReserved = true
      this.authorityPreparationStarted = true
      this.preparingAuthority = true
      phase = 'other'
      try {
        await this.journal.prepare(this.preparationCancellation.signal)
      } finally {
        this.preparingAuthority = false
      }
      if (!this.stopped) {
        this.openState = 'opening'
        try {
          await this.waitForStorage(
            this.journal.open(reservation, () => !this.stopped && !this.authorityLost),
            () => {
              this.opened = true
              this.openState = 'committed'
            }
          )
          this.opened = true
          this.openState = 'committed'
        } catch (error) {
          this.openState =
            !outcomeUnknown(error) && this.journal.isDefinitiveFailure?.(error) === true ? 'aborted' : 'unknown'
          throw error
        }
        if (this.journal.canAcknowledge?.() === false) this.loseAuthority()
        if (this.stopped) return this.status()
        phase = 'setup'
        const installed = this.owner.install(() => this.wake())
        this.source = installed.source
        this.members = installed.leases.map(lease => ({
          lease,
          acquisition: 'pending',
          acquisitionFailure: null,
          cleanup: 'not-needed',
          cleanupFailure: null,
          releasing: null
        }))
        this.pump = new WebRetainedRecordingPump(
          {
            ...installed.source,
            peek: () => {
              const head = installed.source.peek()
              return head?.record.kind === 'terminal' && !this.canPublishTerminal() ? null : head
            }
          },
          {
            canAcknowledge: () => !this.authorityLost && this.journal.canAcknowledge?.() !== false,
            commit: async delivery => {
              if (this.authorityLost) throw new Error('Web recording authority lost')
              const ordinal = await this.journal.commit(delivery)
              if (delivery.record.kind === 'terminal') this.terminalCommitted = true
              if (this.authorityLost) throw new Error('Web recording authority lost')
              return ordinal
            },
            acknowledged: delivery => this.owner.acknowledged?.(delivery),
            retire: id => {
              if (this.authorityLost) throw new Error('Web recording authority lost')
              return this.journal.retire(id)
            }
          }
        )
        installed.source.onTerminal(() => {
          if (!this.stopped) this.stopReason = installed.source.terminalReason ?? 'source-failed'
          this.stopped = true
          this.wake()
        })
        if (
          this.members.length === 0 ||
          new Set(this.members.map(member => member.lease.id)).size !== this.members.length ||
          this.members.some(member => member.lease.id.length === 0)
        ) {
          this.setupFailure = new Error('Invalid Web recording member identities')
          this.recordRuntimeFailure(this.setupFailure, 'setup')
          this.fence('source-failed')
        }
        if (installed.source.terminal) this.stopped = true
        this.wake()
        for (const member of this.members) {
          if (this.journal.canAcknowledge?.() === false) this.loseAuthority()
          if (this.stopped || this.authorityLost) break
          member.acquisition = 'enabling'
          member.cleanup = 'pending'
          try {
            await member.lease.enable()
            member.acquisition = 'acquired'
          } catch (error) {
            member.acquisition = 'failed'
            member.acquisitionFailure = error
            throw error
          }
        }
        if (!this.stopped) this.startState = 'active'
      }
    } catch (error) {
      this.setupFailure = error
      this.setupFailurePhase = phase
      this.recordRuntimeFailure(error, phase)
      if (this.openState === 'unknown') {
        this.failure = error
        this.recoveryFailure = error
        this.capacity = false
        this.blocked = true
      }
      this.fence('source-failed')
    } finally {
      this.setupDone = true
      this.wake()
    }
    return this.status()
  }

  /** Fences synchronously; dispatched enables and cleanup remain owned. */
  stop(reason: WebIngressTerminalReason = 'closed'): WebRecordingSupervisorStatus {
    this.fence(reason)
    this.wake()
    return this.status()
  }

  /** Resume a marker ACK that previously failed after its evidence advances. */
  markerEvidenceProgress(): void {
    ;(async () => {
      if (this.work !== null) await this.work
      if (this.blocked) await this.retry(false)
      else this.wake()
    })().catch(error => {
      this.failure = error
      this.recordRuntimeFailure(error)
      this.recoveryFailure = error
      this.blocked = true
      this.fence('source-failed')
    })
  }

  /** Call only when pagehide actually invalidates the underlying connection. */
  pagehide(connectionInvalidated: boolean): void {
    if (connectionInvalidated) this.stop('connection-lost')
  }

  /** A surviving realm must validate its authority and exact graph synchronously. */
  restore(): boolean {
    let current = false
    try {
      current = !this.authorityLost && this.owner.isCurrent()
    } catch (error) {
      this.setupFailure ??= error
      this.recordRuntimeFailure(error, 'setup')
    }
    if (!current) {
      this.stop('connection-lost')
      return false
    }
    return !this.stopped
  }

  /** The journal's epoch check fences any already-dispatched transaction. */
  loseAuthority(): void {
    clearTimeout(this.recoveryTimer)
    this.recoveryTimer = undefined
    this.authorityLost = true
    const authorityFailure = new Error('Web recording authority lost')
    authorityFailure.name = 'WebRecordingAuthorityLost'
    const storageUnresolved =
      this.unknownOperation !== null ||
      this.unknownRejection ||
      this.observationOutcomeUnknown ||
      outcomeUnknown(this.failure)
    this.failure =
      this.failure !== null
        ? new AggregateError(
            [this.failure, authorityFailure],
            storageUnresolved
              ? 'Web recording authority lost with unresolved storage'
              : 'Web recording authority lost after a failure'
          )
        : authorityFailure
    this.recordRuntimeFailure(this.failure)
    this.recoveryFailure = this.failure
    this.fence('source-failed')
    this.wake()
  }

  /** Retain retirement failures without changing the already-released result. */
  noteRetirementFailure(error: unknown): void {
    this.recordRuntimeFailure(error, 'cleanup')
  }

  private recordRuntimeFailure(error: unknown, phase: 'setup' | 'cleanup' | 'other' = 'other'): void {
    this.lastRuntimeFailure = error
    this.lastRuntimeFailurePhase = phase
  }

  status(): WebRecordingSupervisorStatus {
    let state: WebRecordingSupervisorState = this.startState
    if (this.authorityReleased) state = 'finished'
    else if (this.capacity) state = 'waiting-capacity'
    else if (this.cleanupFailure !== null || this.failure !== null || this.setupFailure !== null) state = 'failed'
    else if (this.stopped) state = !this.setupDone ? 'waiting-source' : 'waiting-drain'
    return {
      state,
      sourceStarted: this.members.some(member => member.acquisition === 'acquired')
        ? true
        : this.members.some(member => member.acquisition === 'enabling' || member.acquisition === 'failed')
          ? 'unknown'
          : false,
      terminalCommitted: this.terminalCommitted,
      terminalReason: this.stopped ? this.stopReason : null,
      sealed: this.sealed,
      sourceReleased: this.sourceReleased,
      authorityReleased: this.authorityReleased,
      workerStoppedSource: this.workerStoppedSource,
      failure: this.failure ?? this.setupFailure,
      storageOutcomeUnknown:
        this.unknownOperation !== null ||
        this.unknownRejection ||
        this.observationOutcomeUnknown ||
        outcomeUnknown(this.failure),
      observationFailure: this.observationFailure,
      storageUncertaintyEvidence: [
        this.failure,
        this.unknownOperation === null && !this.observationOutcomeUnknown ? null : this.recoveryFailure,
        ...this.observationReconciliationFailures
      ].filter(error => error !== null),
      setupFailure: this.setupFailure,
      setupFailurePhase: this.setupFailurePhase,
      runtimeFailure: this.lastRuntimeFailure === null ? null : { error: this.lastRuntimeFailure, persisted: false },
      runtimeFailurePhase: this.lastRuntimeFailurePhase,
      cleanupFailure: this.cleanupFailure,
      members: this.members.map(({ lease, acquisition, acquisitionFailure, cleanup, cleanupFailure }) => ({
        id: lease.id,
        acquisition,
        acquisitionFailure,
        cleanup,
        cleanupFailure
      }))
    }
  }

  /** Retry exact retained work after capacity, storage or cleanup failure. */
  async retry(waitForCleanup = true): Promise<WebRecordingSupervisorStatus> {
    // Only failures already observed by this request are retries. A release
    // that fails while this request is waiting remains visible to its caller.
    const failedMembers = new Set(this.members.filter(member => member.cleanup === 'failed'))
    const retryMembers = (): void => {
      if (failedMembers.size === 0) return
      this.cleanupFailure =
        this.members.find(member => member.cleanup === 'failed' && !failedMembers.has(member))?.cleanupFailure ?? null
      for (const member of this.members) {
        if (member.cleanup === 'failed' && failedMembers.has(member)) member.cleanup = 'pending'
      }
    }
    if (this.work !== null) {
      const retryBlockedWork = this.blocked
      await this.work
      if (!retryBlockedWork) {
        retryMembers()
        if (failedMembers.size > 0) await this.schedule()
        if (waitForCleanup) await this.waitForMemberCleanup()
        return this.status()
      }
    }
    if (this.openState === 'unknown' && this.journal.reconcileOpen !== undefined) {
      try {
        const outcome = await this.reconcileOpen()
        if (this.openState === 'unknown' && outcome !== 'unknown') {
          this.openState = outcome
          this.opened = outcome === 'committed'
          this.unknownOperation = null
          this.unknownRejection = false
          if (!this.authorityLost) {
            this.blocked = false
            this.failure = null
          }
        }
      } catch (error) {
        this.recordRuntimeFailure(error)
        this.recoveryFailure = error
      }
    }
    if (
      this.unknownOperation !== null &&
      this.unknownOperationKind === 'observation' &&
      !this.authorityLost &&
      this.journal.reconcileObservation !== undefined
    ) {
      const original = this.unknownOperation
      try {
        const outcome = await this.journal.reconcileObservation()
        if ((outcome === 'committed' || outcome === 'absent') && this.unknownOperation === original) {
          this.unknownOperation = null
          this.unknownOperationKind = null
          this.observationOutcomeUnknown = false
          this.observationFailure = outcome === 'absent'
          this.unknownRejection = false
          if (!this.authorityLost && this.journal.canAcknowledge?.() !== false) {
            this.observationRetained = outcome === 'committed'
            this.cleanupFailure = null
            this.blocked = false
            this.failure = null
          }
        }
      } catch (error) {
        const pending = webRecordingBoundedPending(error)
        if (pending === undefined) this.retainObservationReconciliationFailure(error)
        else if (!this.observedReconciliationAttempts.has(pending)) {
          this.retainObservationReconciliationFailure(error)
          this.observedReconciliationAttempts.add(pending)
          pending.catch(lateError => this.retainObservationReconciliationFailure(lateError))
        }
        this.recordRuntimeFailure(error)
      }
    }
    if (this.unknownOperation !== null && this.unknownOperationKind === 'seal' && this.journal.canRetrySeal?.()) {
      const reconciliation = this.sealReconciliation ?? this.journal.seal()
      this.sealReconciliation = reconciliation
      try {
        await this.waitForStorage(
          reconciliation,
          () => {
            this.sealed = true
          },
          'seal'
        )
        this.sealed = true
        this.sealReconciliation = null
        this.unknownOperation = null
        this.unknownOperationKind = null
        this.unknownRejection = false
        this.blocked = false
        this.failure = null
      } catch (error) {
        if (!outcomeUnknown(error)) this.sealReconciliation = null
        this.recordStorageFailure(error)
      }
    }
    retryMembers()
    if (
      this.unknownOperation !== null ||
      this.openState === 'opening' ||
      this.openState === 'unknown' ||
      (this.unknownRejection && !this.journal.canRetryUnknown?.())
    ) {
      // The original operation still owns its outcome. Retrying it here could
      // issue a second write before the first transaction has settled.
      await this.schedule()
      return this.status()
    }
    if (!this.observationFailure) this.unknownRejection = false
    this.capacity = false
    this.blocked = false
    if (failedMembers.size === 0 && !this.members.some(member => member.cleanup === 'failed'))
      this.cleanupFailure = null
    // The failed operation is retried, but the failure remains visible until it succeeds.
    await this.schedule()
    if (waitForCleanup) await this.waitForMemberCleanup()
    return this.status()
  }

  /** Wait only for the failed leases retried by this call, not unrelated releases. */
  async retryFailedCleanup(): Promise<WebRecordingSupervisorStatus> {
    const retried = this.members.filter(member => member.cleanup === 'failed')
    await this.retry(false)
    await Promise.all(retried.flatMap(member => (member.releasing === null ? [] : [member.releasing])))
    return this.status()
  }

  private async waitForMemberCleanup(): Promise<void> {
    if (this.blocked) return
    const releases = this.members.flatMap(member => (member.releasing === null ? [] : [member.releasing]))
    if (releases.length === 0) return
    await Promise.all(releases)
    await this.schedule()
  }

  private async reconcileOpen(): Promise<'committed' | 'aborted' | 'unknown'> {
    const reconcile = this.journal.reconcileOpen
    if (reconcile === undefined) return 'unknown'
    const run = this.openReconciliation ?? Promise.resolve().then(() => reconcile.call(this.journal))
    this.openReconciliation = run
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      const result = await Promise.race([
        run.then(outcome => ({ pending: false as const, outcome })),
        new Promise<{ pending: true }>(resolve => {
          timer = setTimeout(() => resolve({ pending: true }), STORAGE_OUTCOME_TIMEOUT_MS)
        })
      ])
      if (result.pending) return 'unknown'
      this.openReconciliation = null
      if (result.outcome !== 'committed' && result.outcome !== 'aborted' && result.outcome !== 'unknown')
        throw new Error('Invalid Web recording open reconciliation')
      return result.outcome
    } catch (error) {
      this.openReconciliation = null
      throw error
    } finally {
      if (timer !== undefined) clearTimeout(timer)
    }
  }

  private fence(reason: WebIngressTerminalReason): void {
    if (!this.stopped) this.stopReason = reason
    this.stopped = true
    this.source?.retire(reason)
  }

  private canPublishTerminal(): boolean {
    return (
      this.sourceSealed &&
      this.owner.hasMarkerDebt?.() !== true &&
      this.setupDone &&
      this.members.every(m => m.cleanup === 'not-needed' || m.cleanup === 'released')
    )
  }

  private wake(): void {
    this.wakePending = true
    this.schedule().catch(error => {
      this.failure = error
      this.recordRuntimeFailure(error)
      this.recoveryFailure = error
      this.blocked = true
      this.fence('source-failed')
    })
  }

  private schedule(): Promise<void> {
    if (this.work !== null) return this.work
    const task = Promise.resolve().then(async () => {
      do {
        this.wakePending = false
        await this.advance()
      } while (this.wakePending && !this.blocked)
    })
    this.work = task
    task.then(
      () => {
        this.work = null
        if (this.authorityReleased && !this.retirementNotified && this.onAuthorityReleased !== undefined) {
          this.retirementNotified = true
          this.onAuthorityReleased().catch(error => {
            this.retirementNotified = false
            this.cleanupFailure = error
            this.recordRuntimeFailure(error, 'cleanup')
            this.recoveryFailure = error
            this.blocked = true
            this.scheduleRecovery()
          })
        }
        // A wake may arrive after the loop's last check but before this
        // settlement reaction. It still owns a pass, even if work was set.
        if (this.wakePending && !this.blocked) this.wake()
        this.scheduleRecovery()
      },
      () => {
        this.work = null
      }
    )
    return task
  }

  private scheduleRecovery(): void {
    if (
      this.recoveryTimer !== undefined ||
      this.authorityLost ||
      !this.blocked ||
      this.capacity ||
      (this.unknownOperation !== null &&
        !(this.unknownOperationKind === 'observation' && this.journal.reconcileObservation !== undefined)) ||
      (this.unknownRejection && !this.journal.canRetryUnknown?.()) ||
      (!(this.unknownOperationKind === 'observation' && this.journal.reconcileObservation !== undefined) &&
        !retryableReadiness(this.recoveryFailure))
    )
      return
    // An uncertain delivery becoming definitively absent is progress, not a
    // fresh storage outage. Retry its retained head without another backoff.
    const delay = this.recoveryMadeProgress ? 0 : Math.min(5000, 100 * 2 ** Math.min(this.recoveryAttempts++, 6))
    this.recoveryMadeProgress = false
    this.recoveryTimer = setTimeout(() => {
      this.recoveryTimer = undefined
      if (this.authorityLost || !this.blocked) return
      this.retry().catch(error => {
        this.recordRuntimeFailure(error)
        this.recoveryFailure = error
        this.failure = error
        this.blocked = true
        this.scheduleRecovery()
      })
    }, delay)
  }

  private async waitForStorage<T>(
    run: Promise<T>,
    onLateSuccess?: () => void,
    kind: 'seal' | 'observation' | 'other' = 'other'
  ): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined
    const deadline = new Promise<{ readonly kind: 'unknown' }>(resolve => {
      timer = setTimeout(() => resolve({ kind: 'unknown' }), STORAGE_OUTCOME_TIMEOUT_MS)
    })
    try {
      const completed = run.then(value => ({ kind: 'completed' as const, value }))
      let pending: Promise<unknown> = run
      let result: { readonly kind: 'completed'; readonly value: T } | { readonly kind: 'unknown' }
      try {
        result = await Promise.race([completed, deadline])
      } catch (error) {
        const attempt = kind === 'observation' ? webRecordingBoundedPending(error) : undefined
        if (attempt === undefined) throw error
        pending = attempt
        result = { kind: 'unknown' }
      }
      if (result.kind === 'completed') return result.value
      this.unknownOperation = pending
      this.unknownOperationKind = kind
      // A late successful receipt is authoritative. A late rejection is not
      // assumed to prove abort; retain the uncertainty for reconciliation.
      pending.then(
        () => {
          const resume = (): void => {
            if (this.unknownOperation !== pending) return
            this.unknownOperation = null
            this.unknownOperationKind = null
            onLateSuccess?.()
            if (!this.authorityLost && outcomeUnknown(this.recoveryFailure)) {
              this.blocked = false
              this.failure = null
              this.capacity = false
              this.wake()
            }
          }
          if (this.work === null) resume()
          else this.work.then(resume, resume)
        },
        error => {
          const settle = (): void => {
            if (this.unknownOperation !== pending) {
              if (kind === 'observation') {
                const evidence = [this.failure, this.lastRuntimeFailure, this.cleanupFailure, error].filter(
                  (value, index, all) => value !== null && all.indexOf(value) === index
                )
                const reported = new AggregateError(evidence, 'Web recording observation settled after reconciliation')
                if (this.authorityLost) this.failure = reported
                this.recordRuntimeFailure(reported)
              }
              return
            }
            this.unknownOperation = null
            this.unknownOperationKind = null
            if (kind === 'observation' && this.journal.isDefinitiveFailure?.(error) === true)
              this.observationOutcomeUnknown = false
            const earlier = [this.failure, this.recoveryFailure, this.cleanupFailure].filter(
              (previous, index, all) => previous !== null && all.indexOf(previous) === index
            )
            this.recordStorageFailure(error, undefined, earlier)
            if (!this.authorityLost) this.scheduleRecovery()
          }
          if (this.work === null) settle()
          else this.work.then(settle, settle)
        }
      )
      throw new WebRecordingOutcomeUnknown()
    } finally {
      if (timer !== undefined) clearTimeout(timer)
    }
  }

  private retainObservationReconciliationFailure(error: unknown): void {
    if (this.observationReconciliationFailures.includes(error)) return
    // Bound retained host objects while preserving the first failures and the
    // most recent one. The public projection has its own bounded redaction.
    if (this.observationReconciliationFailures.length === 8) this.observationReconciliationFailures.pop()
    this.observationReconciliationFailures.push(error)
  }

  private recordStorageFailure(error: unknown, deliveryId?: string, earlier: readonly unknown[] = []): void {
    if (outcomeUnknown(error)) {
      this.failure =
        earlier.length === 0
          ? error
          : new AggregateError([...earlier, error], 'Web recording storage outcome remains unknown')
      if (earlier.length > 0) this.unknownRejection = true
      this.capacity = false
    } else if (this.journal.isDefinitiveFailure?.(error) === true) {
      this.failure =
        earlier.length === 0 ? error : new AggregateError([...earlier, error], 'Web recording storage failed')
      this.capacity = this.journal.classifyFailure(error) === 'capacity'
      this.recoveryMadeProgress =
        deliveryId !== undefined &&
        webRecordingReconciledAbsentDelivery(error) === deliveryId &&
        !this.capacity &&
        retryableReadiness(error)
    } else {
      this.unknownRejection = true
      this.failure = new AggregateError(
        [...earlier, new WebRecordingOutcomeUnknown(), error],
        'Web recording storage outcome remains unknown'
      )
      this.capacity = false
    }
    this.recordRuntimeFailure(this.failure)
    this.recoveryFailure = this.failure
    this.blocked = true
  }

  private async recordCleanupPending(): Promise<void> {
    if (
      this.stopped &&
      this.opened &&
      !this.authorityLost &&
      !this.cleanupPending &&
      this.unknownOperation === null &&
      this.journal.beginCleanup !== undefined
    ) {
      try {
        await this.waitForStorage(this.journal.beginCleanup(), () => {
          this.cleanupPending = true
        })
        this.cleanupPending = true
      } catch (error) {
        this.recordStorageFailure(error)
      }
    }
  }

  private cleanupMembers(): void {
    for (const member of this.members) {
      if (member.cleanup !== 'pending' || member.releasing !== null || member.acquisition === 'enabling') continue
      // Each lease owns one exact release; another lease cannot hold up its dispatch.
      const release = Promise.resolve()
        .then(() => member.lease.release())
        .then(
          () => {
            member.cleanup = 'released'
            member.cleanupFailure = null
          },
          error => {
            member.cleanup = 'failed'
            member.cleanupFailure = error
            this.cleanupFailure = error
            this.recordRuntimeFailure(error, 'cleanup')
          }
        )
        .then(() => {
          member.releasing = null
          this.wake()
        })
      member.releasing = release
    }
  }

  private sealSourceAfterCleanup(): void {
    if (
      !this.sourceSealed &&
      this.setupDone &&
      this.members.every(member => member.cleanup === 'not-needed' || member.cleanup === 'released')
    ) {
      this.source?.seal?.()
      this.sourceSealed = true
    }
  }

  private async advance(): Promise<void> {
    await this.recordCleanupPending()
    if (
      this.opened &&
      !this.authorityReleased &&
      !this.authorityLost &&
      !this.blocked &&
      this.pump !== null &&
      this.source !== null
    ) {
      let deliveryId: string | undefined
      try {
        for (;;) {
          if (this.authorityLost) break
          if (this.stopped) {
            await this.recordCleanupPending()
            // A live listener can keep replenishing the retained FIFO. Start
            // its exact cleanup now while this worker continues to drain it.
            this.cleanupMembers()
            this.sealSourceAfterCleanup()
            if (this.blocked) break
          }
          // A terminal is only publishable after every dispatched member has
          // completed cleanup. The source retains the exact head meanwhile.
          if (
            this.source.peek()?.record.kind === 'terminal' &&
            !this.canPublishTerminal() &&
            !this.pump.hasPendingAcknowledgement()
          ) {
            break
          }
          deliveryId = this.pump.hasPendingAcknowledgement() ? undefined : this.source.peek()?.id
          const outcome = await this.waitForStorage(this.pump.runOnce())
          // A callback or stop can enqueue after the pump observed empty but
          // before its promise settled. Never seal across that wakeup.
          if (outcome === 'empty') {
            const head = this.source.peek()
            if (head === null || (head.record.kind === 'terminal' && !this.canPublishTerminal())) {
              // Stop may have arrived while the empty read was in flight.
              if (this.stopped) this.cleanupMembers()
              break
            }
          }
        }
        if (!this.authorityLost && this.failure !== null && this.stopped) this.failure = null
      } catch (error) {
        if (!this.authorityLost) {
          this.recordStorageFailure(error, deliveryId)
        }
        this.blocked = true
        if (!this.stopped && !this.authorityLost) this.workerStoppedSource = true
        this.fence('source-failed')
      }
    }
    if (!this.stopped) return
    await this.recordCleanupPending()
    if (
      this.setupDone &&
      this.opened &&
      this.source === null &&
      !this.authorityLost &&
      !this.blocked &&
      !this.unstartedFinalized &&
      this.journal.finalizeUnstarted !== undefined
    ) {
      if (this.ownerReserved && !this.sourceReleased) {
        try {
          await this.owner.release()
          this.sourceReleased = true
        } catch (error) {
          this.cleanupFailure = error
          this.recordRuntimeFailure(error, 'cleanup')
          this.blocked = true
          return
        }
      }
      try {
        await this.waitForStorage(
          this.journal.finalizeUnstarted(this.stopReason, () => {
            this.terminalCommitted = true
          }),
          () => {
            this.unstartedFinalized = true
          }
        )
        this.unstartedFinalized = true
        if (!this.authorityLost) this.failure = null
      } catch (error) {
        this.recordStorageFailure(error)
      }
    }
    // Storage failures still require physical cleanup, but reservation and lock
    // remain held until the retained FIFO and terminal are durably resolved.
    this.cleanupMembers()
    if (this.cleanupFailure !== null || this.members.some(m => m.cleanup !== 'not-needed' && m.cleanup !== 'released'))
      return
    try {
      this.sealSourceAfterCleanup()
    } catch (error) {
      this.cleanupFailure = error
      this.recordRuntimeFailure(error, 'cleanup')
      this.blocked = true
      return
    }
    if (
      this.setupDone &&
      !this.authorityLost &&
      !this.blocked &&
      this.source?.peek() !== null &&
      this.source !== null
    ) {
      // Marker settlement or acknowledgement wakes the supervisor when debt clears.
      if (this.source.peek()?.record.kind === 'terminal' && !this.canPublishTerminal()) return
      // Cleanup can settle after the pump stopped at the terminal head.
      this.wakePending = true
      return
    }
    if (this.setupDone && this.opened && !this.authorityLost && !this.blocked && !this.terminalCommitted) {
      this.failure = new Error('Web recording source ended without a committed terminal')
      this.recordRuntimeFailure(this.failure)
      this.recoveryFailure = this.failure
      this.blocked = true
    }
    if (
      this.setupDone &&
      !this.authorityLost &&
      this.failure === null &&
      !this.blocked &&
      this.opened &&
      !this.sealed
    ) {
      try {
        await this.waitForStorage(
          this.journal.seal(),
          () => {
            this.sealed = true
          },
          'seal'
        )
        this.sealed = true
      } catch (error) {
        this.recordStorageFailure(error)
      }
    }
    // A reservation has no source or deliveries and may be discarded even if
    // the durable open result is unresolved. The authority stays fenced below.
    if (this.ownerReserved && this.source === null && !this.sourceReleased) {
      try {
        await this.owner.release()
        this.sourceReleased = true
      } catch (error) {
        this.cleanupFailure = error
        this.recordRuntimeFailure(error, 'cleanup')
        this.blocked = true
        return
      }
    }
    if (this.openState === 'aborted' && this.source !== null) {
      // Only discard the synthetic terminal when no other observation is
      // ahead of it and retain that fact across a failed release attempt.
      const head = this.source.peek()
      if (head?.record.kind === 'terminal' && this.source.ackExact(head.id) && this.source.peek() === null)
        this.abortedTerminalDiscarded = true
      if (this.abortedTerminalDiscarded && !this.sourceReleased) {
        try {
          await this.owner.release()
          this.sourceReleased = true
        } catch (error) {
          this.cleanupFailure = error
          this.recordRuntimeFailure(error, 'cleanup')
          this.blocked = true
          return
        }
      }
    }
    if (
      this.source !== null &&
      !this.sourceReleased &&
      (this.authorityLost ||
        this.blocked ||
        this.openState === 'opening' ||
        this.openState === 'unknown' ||
        this.openState === 'aborted') &&
      this.owner.releaseRetainingDeliveries !== undefined
    ) {
      try {
        await this.owner.releaseRetainingDeliveries()
        this.sourceReleased = true
      } catch (error) {
        this.cleanupFailure = error
        this.recordRuntimeFailure(error, 'cleanup')
        this.blocked = true
        return
      }
    }
    if (
      this.setupDone &&
      this.authorityLost &&
      !this.opened &&
      (this.openState === 'aborted' || this.openState === 'not-opened') &&
      this.unknownOperation === null &&
      this.source === null &&
      (!this.ownerReserved || this.sourceReleased) &&
      !this.authorityReleased &&
      this.authorityPreparationStarted
    ) {
      // No source was installed and open is proven absent. The authority port
      // still owns any exact provisional acquisition reconciliation.
      try {
        await this.journal.releaseAuthority()
        this.authorityReleased = true
      } catch (error) {
        this.cleanupFailure = error
        this.recordRuntimeFailure(error, 'cleanup')
        this.blocked = true
      }
      return
    }
    if (
      !this.setupDone ||
      this.authorityLost ||
      (this.failure !== null && !this.observationFailure) ||
      this.blocked ||
      this.openState === 'opening' ||
      this.openState === 'unknown' ||
      (this.openState === 'aborted' && this.source !== null && !this.sourceReleased) ||
      (this.openState === 'aborted' && this.source !== null && !this.abortedTerminalDiscarded) ||
      (this.opened && !this.sealed)
    )
      return
    let finalPhase: 'cleanup' | 'other' = 'cleanup'
    let observingStorage = false
    try {
      if (this.ownerReserved && !this.sourceReleased) {
        await this.owner.release()
        this.sourceReleased = true
      }
      if (this.opened && this.observeFinal !== undefined && !this.observationRetained) {
        finalPhase = 'other'
        if (this.journal.observeTerminal === undefined)
          throw new Error('Web recording terminal observation unavailable')
        this.terminalObservation ??= await this.observeFinal()
        observingStorage = true
        await this.waitForStorage(
          this.journal.observeTerminal(this.terminalObservation),
          () => {
            // The receipt resolves storage even if authority was lost meanwhile;
            // only a current owner may acknowledge it or release authority.
            this.observationOutcomeUnknown = false
            this.observationFailure = false
            this.unknownRejection = false
            if (!this.authorityLost && this.journal.canAcknowledge?.() !== false) {
              this.observationRetained = true
              if (this.cleanupFailure === this.recoveryFailure && outcomeUnknown(this.cleanupFailure))
                this.cleanupFailure = null
            }
          },
          'observation'
        )
        observingStorage = false
        this.observationFailure = false
        this.observationOutcomeUnknown = false
        this.unknownRejection = false
        if (this.authorityLost || this.journal.canAcknowledge?.() === false)
          throw new Error('Web recording authority lost')
        this.observationRetained = true
        this.failure = null
      }
      if (!this.authorityReleased && this.authorityPreparationStarted) {
        finalPhase = 'cleanup'
        // prepare may have failed ambiguously; the port owns reconciliation.
        await this.journal.releaseAuthority()
        this.authorityReleased = true
      }
    } catch (error) {
      this.cleanupFailure = error
      if (observingStorage) {
        this.observationFailure = true
        this.observationOutcomeUnknown = outcomeUnknown(error) || this.journal.isDefinitiveFailure?.(error) !== true
        this.recordStorageFailure(error, undefined, this.authorityLost && this.failure !== null ? [this.failure] : [])
      } else {
        this.recordRuntimeFailure(error, finalPhase)
        this.recoveryFailure = error
        this.blocked = true
      }
    }
  }
}

function aborted(): Error {
  const error = new Error('Operation aborted')
  error.name = 'AbortError'
  return error
}

function waitFor<T>(promise: Promise<T>, signal: AbortSignal | undefined, onCancelled: () => void): Promise<T> {
  if (signal === undefined) return promise
  return new Promise((resolve, reject) => {
    const onAbort = (): void => {
      signal.removeEventListener('abort', onAbort)
      onCancelled()
      reject(aborted())
    }
    signal.addEventListener('abort', onAbort, { once: true })
    // Preparation can synchronously trigger abort before the listener exists.
    // Recheck after registration so the final pre-admission waiter still fences.
    if (signal.aborted) onAbort()
    promise.then(
      value => {
        signal.removeEventListener('abort', onAbort)
        resolve(value)
      },
      error => {
        signal.removeEventListener('abort', onAbort)
        reject(error)
      }
    )
  })
}
