import { trustedWebRecordingError, webRecordingContractError as contractError } from './web-recording-errors'
import { webRecordingReadinessRetryable } from './web-indexeddb-transaction'
import { parseOrderedRecordingHostCleanup, parseOrderedRecordingStopReceipt } from '../core/continuation-recording'
import type { ContinuationRecordingStatus, OrderedRecordingHostStopResult } from '../core/continuation-recording'
import type { WebOrderedRecordingSupervisor } from './web-ordered-recording-supervisor'
import type { WebRecordingRuntimeDiagnostic } from './web-clear-attempt'

/** Identity of one durable producer ownership, scoped to a host's storage partition. */
export interface WebRecordingLiveIdentity {
  readonly recordingId: string
  readonly producerIncarnation: string
  readonly ownerEpoch: string
}

/** Implemented by the GATT host, which retains the supervisor and maps its actual outcomes. */
export interface WebRecordingLiveOwner {
  readonly recordingVersion?: 1 | 2
  /** Retains the supervisor for the entire installed ownership. */
  readonly supervisor: Pick<WebOrderedRecordingSupervisor, 'status' | 'stop' | 'retry'>
  status(): ContinuationRecordingStatus | Promise<ContinuationRecordingStatus>
  runtimeDiagnostic?(): WebRecordingRuntimeDiagnostic
  stopOrdered(): Promise<OrderedRecordingHostStopResult>
  /** Report the result of this retry without waiting for unrelated held leases. */
  retryFailedCleanup?(): Promise<OrderedRecordingHostStopResult>
  /** Current observed stop and cleanup outcome; must not initiate shutdown. */
  stopResult(): OrderedRecordingHostStopResult | Promise<OrderedRecordingHostStopResult>
  /** Proves this exact terminal outcome and identity remain recoverable after removal. */
  terminalRetained?(result: OrderedRecordingHostStopResult): Promise<boolean>
  /** Exact provisional declaration was proven unbound and removed after release. */
  unboundDeclarationRemoved?(): boolean
  /** Retain asynchronous retirement failures for host diagnostics. */
  retirementFailed?(error: unknown): void
  /** Outstanding marker receipts retain their exact recording owner. */
  hasPendingMarkerReceipt?(): boolean
  dispose?(): void
  spaceAvailable(): Promise<void>
}

/** Terminal recording outcomes can release host resources without succeeding. */
export function webRecordingReleased(result: OrderedRecordingHostStopResult, version: 1 | 2 = 1): boolean {
  const recording = parseOrderedRecordingStopReceipt(result.recording, version)
  const cleanup = parseOrderedRecordingHostCleanup(result.hostCleanup)
  return (
    (recording.state === 'finished' || recording.state === 'failed' || recording.state === 'interrupted') &&
    cleanup.state === 'released'
  )
}

interface Entry {
  readonly identity: WebRecordingLiveIdentity
  readonly owner: WebRecordingLiveOwner
  users: number
  stop: Promise<OrderedRecordingHostStopResult> | null
  retryStop: Promise<OrderedRecordingHostStopResult> | null
  retirement: Promise<void> | null
  retiring: boolean
  idle: (() => void)[]
  recoveryTimer: ReturnType<typeof setTimeout> | undefined
  recoveryAttempts: number
}

const unavailable = (): Error => contractError('capability.unavailable', 'restoration', 'web-recording.live-owner')
const same = (a: WebRecordingLiveIdentity, b: WebRecordingLiveIdentity): boolean =>
  a.recordingId === b.recordingId && a.producerIncarnation === b.producerIncarnation && a.ownerEpoch === b.ownerEpoch

/** One registry per host/storage partition; share it with every controller in that host. */
export class WebRecordingLiveOwners {
  private readonly entries = new Map<string, Entry>()

  install(identity: WebRecordingLiveIdentity, owner: WebRecordingLiveOwner): void {
    if (this.entries.has(identity.recordingId)) throw unavailable()
    this.entries.set(identity.recordingId, {
      identity: { ...identity },
      owner,
      users: 0,
      stop: null,
      retryStop: null,
      retirement: null,
      retiring: false,
      idle: [],
      recoveryTimer: undefined,
      recoveryAttempts: 0
    })
  }

  /** Removal cannot discard a live supervisor or race an in-flight controller operation. */
  async remove(identity: WebRecordingLiveIdentity, owner: WebRecordingLiveOwner): Promise<void> {
    const entry = this.entries.get(identity.recordingId)
    if (!entry || !same(entry.identity, identity) || entry.owner !== owner || entry.users !== 0) throw unavailable()
    entry.users++
    try {
      const result = await owner.stopResult()
      if (!webRecordingReleased(result, owner.recordingVersion) || owner.hasPendingMarkerReceipt?.())
        throw unavailable()
      if ((await owner.terminalRetained?.(result)) !== true) throw unavailable()
      if (entry.users !== 1) throw unavailable()
      this.entries.delete(identity.recordingId)
      owner.dispose?.()
    } finally {
      entry.users--
    }
  }

  has(identity: WebRecordingLiveIdentity): boolean {
    const entry = this.entries.get(identity.recordingId)
    return entry !== undefined && same(entry.identity, identity)
  }

  async useByRecordingId<T>(id: string, operation: (owner: WebRecordingLiveOwner) => Promise<T>): Promise<T> {
    const entry = this.entries.get(id)
    if (entry === undefined) throw unavailable()
    return this.use(entry.identity, operation)
  }

  async use<T>(
    identity: WebRecordingLiveIdentity,
    operation: (owner: WebRecordingLiveOwner) => Promise<T>
  ): Promise<T> {
    const entry = this.entries.get(identity.recordingId)
    if (!entry || entry.retiring || !same(entry.identity, identity)) throw unavailable()
    entry.users++
    try {
      return await operation(entry.owner)
    } finally {
      entry.users--
      if (entry.users === 0) for (const done of entry.idle.splice(0)) done()
    }
  }

  async spaceAvailable(identity: WebRecordingLiveIdentity): Promise<void> {
    const entry = this.entries.get(identity.recordingId)
    if (!entry || !same(entry.identity, identity)) throw unavailable()
    await this.use(identity, owner => owner.spaceAvailable())
    await this.retireIfEligible(identity, entry.owner)
  }

  stopOrdered(identity: WebRecordingLiveIdentity, version: 1 | 2): Promise<OrderedRecordingHostStopResult> {
    const entry = this.entries.get(identity.recordingId)
    if (!entry || !same(entry.identity, identity)) return Promise.reject(unavailable())
    if ((entry.owner.recordingVersion ?? 1) !== version)
      return Promise.reject(contractError('protocol.incompatible', 'boundary', 'web-recording.stop.version'))
    if (entry.stop !== null) {
      const stop = entry.stop
      const joinRetry = (retry: Promise<OrderedRecordingHostStopResult>): Promise<OrderedRecordingHostStopResult> =>
        retry.then(result => (result.hostCleanup.state === 'release-failed' ? result : stop))
      if (entry.retryStop !== null) return joinRetry(entry.retryStop)
      if (entry.retiring || !entry.owner.supervisor.status().members?.some(member => member.cleanup === 'failed'))
        return stop
      // Joining the receipt must still give failed member leases a retry; the
      // original stop may be waiting on a different, still-in-flight lease.
      const retry = this.use(identity, async owner => {
        if (owner.retryFailedCleanup !== undefined) return owner.retryFailedCleanup()
        await owner.supervisor.retry(false)
        return owner.stopResult()
      })
      entry.retryStop = retry
      retry.then(
        () => {
          entry.retryStop = null
        },
        () => {
          entry.retryStop = null
        }
      )
      return joinRetry(retry)
    }
    const task = this.use(identity, owner => owner.stopOrdered()).then(async result => {
      await this.retireIfEligible(identity, entry.owner, true)
      return result
    })
    entry.stop = task
    task.then(
      () => {
        entry.stop = null
      },
      () => {
        entry.stop = null
      }
    )
    return task
  }

  /** Recheck after the last receipt is forgotten, including concurrent stop/users. */
  async retireIfEligible(
    identity: WebRecordingLiveIdentity,
    owner: WebRecordingLiveOwner,
    fromStop = false
  ): Promise<void> {
    const entry = this.entries.get(identity.recordingId)
    if (!entry || !same(entry.identity, identity) || entry.owner !== owner) return
    // A pending stop cannot be retired before authority release. Do not hold a
    // capacity acknowledgement behind the physical cleanup it is helping drain.
    if (typeof owner.supervisor.status !== 'function' || !owner.supervisor.status().authorityReleased) return
    if (!fromStop && entry.stop !== null) {
      await entry.stop
      return this.retireIfEligible(identity, owner)
    }
    if (entry.retirement !== null) return entry.retirement
    const task = (async () => {
      entry.retiring = true
      try {
        if (entry.users !== 0) await new Promise<void>(resolve => entry.idle.push(resolve))
        if (owner.unboundDeclarationRemoved?.() === true) {
          if (this.entries.get(identity.recordingId) !== entry) throw unavailable()
          this.entries.delete(identity.recordingId)
          owner.dispose?.()
          return
        }
        const result = await owner.stopResult()
        if (
          !webRecordingReleased(result, owner.recordingVersion) ||
          owner.hasPendingMarkerReceipt?.() === true ||
          owner.terminalRetained === undefined ||
          !owner.supervisor.status().authorityReleased
        )
          return
        if (!(await owner.terminalRetained(result))) return
        if (owner.hasPendingMarkerReceipt?.() === true) return
        if (this.entries.get(identity.recordingId) !== entry) throw unavailable()
        this.entries.delete(identity.recordingId)
        owner.dispose?.()
      } finally {
        entry.retiring = false
      }
    })()
    entry.retirement = task
    try {
      await task
    } catch (error) {
      owner.retirementFailed?.(error)
      if (this.retryableRetirement(error) && entry.recoveryTimer === undefined) {
        const delay = Math.min(5000, 100 * 2 ** Math.min(entry.recoveryAttempts++, 6))
        entry.recoveryTimer = setTimeout(() => {
          entry.recoveryTimer = undefined
          this.retireIfEligible(identity, owner).catch(() => {
            // retireIfEligible retained the failure and schedules transient retries.
          })
        }, delay)
      }
      throw error
    } finally {
      entry.retirement = null
    }
  }

  private retryableRetirement(error: unknown, depth = 0, visited = new Set<unknown>()): boolean {
    if (depth >= 16 || visited.has(error)) return false
    visited.add(error)
    const readiness = webRecordingReadinessRetryable(error)
    if (readiness !== undefined) return readiness
    try {
      if (error instanceof AggregateError)
        return error.errors.slice(0, 64).some(item => this.retryableRetirement(item, depth + 1, visited))
      return error instanceof Error && trustedWebRecordingError(error) === undefined
    } catch {
      return false
    }
  }
}

export function createWebRecordingLiveOwners(): WebRecordingLiveOwners {
  return new WebRecordingLiveOwners()
}
