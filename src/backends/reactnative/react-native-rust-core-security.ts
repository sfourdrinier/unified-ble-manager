// src/backends/reactnative/react-native-rust-core-security.ts
//
// Android link security on the Rust route: `security.state`,
// `security.pair` and `security.cancel-pairing` on the session, with
// `security` drain records feeding the watches. Semantics follow the legacy
// Android security backend (react-native-android-security.ts): the system
// ceremony only, `auto` pairing of an already-bonded peer runs no ceremony,
// unpair is reported unsupported, and a pair's own answer is what
// `cancelPairing` reports. Deadlines travel to the owner as `budgetMs`; an
// abort cancels exactly the pair operation (`op.cancel`).

import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import {
  cancelOutcomeForPairResult,
  type PeerSecurityEvent,
  type PeerSecurityState,
  type SecurityBackend,
  type SecurityCancelPairingResult,
  type SecurityPairOptions,
  type SecurityPairResult,
  type SecurityUnpairResult
} from '../../backend-contract/security'
import { awaitWithOperationAdmission } from '../../core/unified-ble-core-helpers'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import type { WireSecurityState } from './rust-core-wire'

const streamLimits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})

/** Bytes one retained security event holds (five short enum strings and a clock). */
const SECURITY_EVENT_BYTES = 128

const limitations = Object.freeze([
  Object.freeze({
    code: 'android-link-security-measurement-unavailable',
    explanation:
      'Bond state is observed directly. Link encryption is observed where API 36 events or the SDK 36.1 LE snapshot are available. Peer broadcasts carry no connection generation; event-only state queries and ambiguous null snapshots report unknown. Authentication and Secure Connections are never inferred.',
    affectedGuarantee:
      'authentication and Secure Connections measurement; encryption availability depends on the native runtime'
  })
])

/** What the security backend needs from the backend that owns the session. */
export interface RustCoreSecurityHost {
  readonly now: () => number
  nativePeerId(peerId: string, operation: string): string
  budget(options: PublicOperationOptions, operation: string): { readonly budgetMs?: number }
  mintOperationId(kind: string): string
  securityState(args: { peerId: string; operationId: string; budgetMs?: number }): Promise<WireSecurityState>
  pair(args: {
    peerId: string
    transport: 'auto' | 'le'
    operationId: string
    budgetMs?: number
  }): Promise<{ readonly outcome: 'paired' | 'already-paired' | 'rejected'; readonly state: WireSecurityState }>
  cancelPairing(args: { peerId: string; operationId: string; budgetMs?: number }): Promise<void>
  /** Tracks `operationId` until the returned remover runs; cancels it when `signal` aborts. */
  watchAbort(signal: AbortSignal | null, operationId: string, operation: string): () => void
}

export class RustCoreSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<OwnedCoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activeResults = new Map<string, Promise<SecurityPairResult>>()
  private readonly sequences = new Map<string, number>()
  private readonly sourceFailures = new Map<
    string | null,
    { readonly revision: number; readonly error: BackendContractError }
  >()
  private sourceRevision = 0
  private closed = false
  private readonly shutdown = new AbortController()

  constructor(private readonly host: RustCoreSecurityHost) {}

  async state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> {
    const operation = 'react-native-rust-core.security.state'
    this.assertOpen(operation)
    const peerRevision = this.sourceFailures.get(peerId)?.revision
    const globalRevision = this.sourceFailures.get(null)?.revision
    this.assertAdmission(options, operation)
    const nativePeerId = this.host.nativePeerId(peerId, operation)
    const operationId = this.host.mintOperationId('security-state')
    const removeAbort = this.watchAbort(options, operationId, operation)
    try {
      const state = await this.host.securityState({
        peerId: nativePeerId,
        operationId,
        ...this.host.budget(options, operation)
      })
      const peerFailure = this.sourceFailures.get(peerId)
      const globalFailure = this.sourceFailures.get(null)
      if (peerFailure !== undefined && peerFailure.revision !== peerRevision) throw peerFailure.error
      if (globalFailure !== undefined && globalFailure.revision !== globalRevision) throw globalFailure.error
      if (peerFailure?.revision === peerRevision) this.sourceFailures.delete(peerId)
      if (globalFailure?.revision === globalRevision) this.sourceFailures.delete(null)
      return this.snapshot(state)
    } finally {
      removeAbort()
    }
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertOpen('react-native-rust-core.security.watch')
    const stream = new OwnedCoreBoundedStream<PeerSecurityEvent>(streamLimits, 'error', () => {
      this.removeStream(peerId, stream)
    })
    const streams = this.streams.get(peerId) ?? new Set<OwnedCoreBoundedStream<PeerSecurityEvent>>()
    streams.add(stream)
    this.streams.set(peerId, streams)
    const openingSequence = this.sequences.get(peerId) ?? 0
    this.state(peerId, { signal: null, deadline: null }).then(
      state => {
        if (
          !this.closed &&
          this.streams.get(peerId)?.has(stream) === true &&
          (this.sequences.get(peerId) ?? 0) === openingSequence
        )
          this.emit(peerId, state)
      },
      (error: unknown) => {
        stream.closeWithReason('source-failed', error instanceof BackendContractError ? error.normalized : null)
      }
    )
    return stream
  }

  async pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    const operation = 'react-native-rust-core.security.pair'
    this.assertOpen(operation)
    if (options.ceremony !== 'system') {
      throw contractError('capability.unsupported', 'capability', 'android.security.custom-ceremony')
    }
    if (options.protection !== 'system-default') {
      throw contractError('capability.unsupported', 'capability', 'android.security.pair.protection')
    }
    if (options.secureConnections !== undefined && options.secureConnections !== 'prefer') {
      throw contractError('capability.unsupported', 'capability', 'android.security.pair.secure-connections')
    }
    this.assertAdmission(options, operation)
    if (this.activeResults.has(peerId)) {
      throw contractError('ownership.denied', 'platform', 'android.security.pair.arbitration')
    }
    const nativePeerId = this.host.nativePeerId(peerId, operation)
    const operationId = this.host.mintOperationId('security-pair')
    const budget = this.host.budget(options, operation)
    const removeAbort = this.watchAbort(options, operationId, operation)
    const result = (async (): Promise<SecurityPairResult> => {
      try {
        const answer = await this.host.pair({
          peerId: nativePeerId,
          transport: options.transport,
          operationId,
          ...budget
        })
        if (answer.outcome === 'rejected') return { outcome: 'rejected', reason: null }
        return { outcome: answer.outcome, state: this.snapshot(answer.state) }
      } catch (error) {
        // The owner's answer to an aborted pair is the pair's own outcome:
        // cancelled. A deadline stays `operation.timed-out` (the owner's
        // report), never re-labelled.
        if (error instanceof BackendContractError && error.normalized.code === 'operation.aborted') {
          return { outcome: 'cancelled' }
        }
        throw error
      } finally {
        removeAbort()
        this.activeResults.delete(peerId)
      }
    })()
    this.activeResults.set(peerId, result)
    return result
  }

  async cancelPairing(peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    const operation = 'react-native-rust-core.security.cancel-pairing'
    this.assertOpen(operation)
    this.assertAdmission(options, operation)
    if (options.deadline !== null && options.deadline <= this.host.now()) {
      throw contractError('operation.timed-out', 'core', operation)
    }
    this.host.budget(options, operation)
    const result = this.activeResults.get(peerId)
    if (result === undefined) return { outcome: 'not-pairing' }
    const nativePeerId = this.host.nativePeerId(peerId, operation)
    const operationId = this.host.mintOperationId('security-cancel')
    const waiting = new AbortController()
    const abort = (): void => waiting.abort()
    options.signal?.addEventListener('abort', abort, { once: true })
    this.shutdown.signal.addEventListener('abort', abort, { once: true })
    const removeAbort = this.host.watchAbort(waiting.signal, operationId, operation)
    const admission = { ...options, signal: waiting.signal }
    try {
      const budget = this.host.budget(options, operation)
      this.assertOpen(operation)
      this.assertAdmission(admission, operation)
      if (options.deadline !== null && options.deadline <= this.host.now()) {
        throw contractError('operation.timed-out', 'core', operation)
      }
      // Observe the native receipt before a caller deadline/abort can reject
      // its wait. The native acknowledgment still owns its late settlement.
      const acknowledgment = this.host.cancelPairing({ peerId: nativePeerId, operationId, ...budget }).then(
        () => null,
        (error: unknown) => ({ error })
      )
      const answer = await awaitWithOperationAdmission(acknowledgment, admission, this.host.now, operation)
      if (answer !== null) throw answer.error
      // Stopping this caller's wait never drops the pairing's own result or
      // cancels its operation identity. A later caller can observe that fact.
      return cancelOutcomeForPairResult(await awaitWithOperationAdmission(result, admission, this.host.now, operation))
    } catch (error) {
      if (this.closed && error instanceof BackendContractError && error.normalized.code === 'operation.aborted') {
        throw contractError('lifecycle.destroyed', 'core', operation)
      }
      throw error
    } finally {
      removeAbort()
      options.signal?.removeEventListener('abort', abort)
      this.shutdown.signal.removeEventListener('abort', abort)
    }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertOpen('react-native-rust-core.security.unpair')
    this.host.nativePeerId(peerId, 'react-native-rust-core.security.unpair')
    return { outcome: 'unsupported' }
  }

  /** A `security` drain record: fan the fact out to the peer's watches. */
  observe(peerId: string, state: WireSecurityState): void {
    if (this.closed) return
    this.sourceFailures.delete(peerId)
    this.sourceFailures.delete(null)
    this.emit(peerId, this.snapshot(state))
  }

  sourceFailed(peerId: string | null, error: BackendContractError): void {
    if (this.closed) return
    this.sourceFailures.set(peerId, { revision: ++this.sourceRevision, error })
    const streams =
      peerId === null ? [...this.streams.values()].flatMap(group => [...group]) : [...(this.streams.get(peerId) ?? [])]
    for (const stream of streams) stream.closeWithReason('source-failed', error.normalized)
  }

  close(): void {
    this.closed = true
    this.shutdown.abort()
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
    this.sourceFailures.clear()
  }

  /** Live watch count (resource accounting and leak tests). */
  watchCount(): number {
    let count = 0
    for (const streams of this.streams.values()) count += streams.size
    return count
  }

  private snapshot(state: WireSecurityState): PeerSecurityState {
    return Object.freeze({ ...state, measuredAtMonotonicMs: this.host.now(), limitations })
  }

  private assertOpen(operation: string): void {
    if (this.closed) throw contractError('lifecycle.destroyed', 'core', operation)
  }

  private assertAdmission(options: PublicOperationOptions, operation: string): void {
    if (options.signal?.aborted === true) throw contractError('operation.aborted', 'core', operation)
  }

  private watchAbort(options: PublicOperationOptions, operationId: string, operation: string): () => void {
    return this.host.watchAbort(options.signal ?? null, operationId, operation)
  }

  private emit(peerId: string, state: PeerSecurityState): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    const sequence = (this.sequences.get(peerId) ?? 0) + 1
    this.sequences.set(peerId, sequence)
    const event = Object.freeze({ kind: 'state' as const, peerId, sequence, state })
    for (const stream of [...streams]) stream.emit(event, SECURITY_EVENT_BYTES)
  }

  private removeStream(peerId: string, stream: OwnedCoreBoundedStream<PeerSecurityEvent>): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    streams.delete(stream)
    if (streams.size === 0) {
      this.streams.delete(peerId)
      this.sequences.delete(peerId)
    }
  }
}
