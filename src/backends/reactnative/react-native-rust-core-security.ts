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
      'The Android public bond API reports bond state; encryption, authentication, and Secure Connections are not inferred.',
    affectedGuarantee: 'encryption, authentication, and Secure Connections measurement'
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
  cancelPairing(args: { peerId: string; operationId: string }): Promise<void>
  /** Tracks `operationId` until the returned remover runs; cancels it when `signal` aborts. */
  watchAbort(signal: AbortSignal | null, operationId: string, operation: string): () => void
}

export class RustCoreSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<OwnedCoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activeResults = new Map<string, Promise<SecurityPairResult>>()
  private readonly sequences = new Map<string, number>()
  private closed = false

  constructor(private readonly host: RustCoreSecurityHost) {}

  async state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> {
    const operation = 'react-native-rust-core.security.state'
    this.assertOpen(operation)
    this.assertAdmission(options, operation)
    const nativePeerId = this.host.nativePeerId(peerId, operation)
    const operationId = this.host.mintOperationId('security-state')
    const removeAbort = this.watchAbort(options, operationId, operation)
    try {
      return this.snapshot(
        await this.host.securityState({ peerId: nativePeerId, operationId, ...this.host.budget(options, operation) })
      )
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
    this.state(peerId, { signal: null, deadline: null }).then(
      state => this.emit(peerId, state),
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
    const result = this.activeResults.get(peerId)
    if (result === undefined) return { outcome: 'not-pairing' }
    this.assertAdmission(options, operation)
    const nativePeerId = this.host.nativePeerId(peerId, operation)
    await this.host.cancelPairing({ peerId: nativePeerId, operationId: this.host.mintOperationId('security-cancel') })
    return cancelOutcomeForPairResult(await result)
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertOpen('react-native-rust-core.security.unpair')
    this.host.nativePeerId(peerId, 'react-native-rust-core.security.unpair')
    return { outcome: 'unsupported' }
  }

  /** A `security` drain record: fan the fact out to the peer's watches. */
  observe(peerId: string, state: WireSecurityState): void {
    if (this.closed) return
    this.emit(peerId, this.snapshot(state))
  }

  close(): void {
    this.closed = true
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
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
