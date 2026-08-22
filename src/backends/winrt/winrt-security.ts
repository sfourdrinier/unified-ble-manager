import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream } from '../../core/bounded-stream'
import type {
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityBackend,
  SecurityCancelPairingResult,
  SecurityPairOptions,
  SecurityPairResult,
  SecurityUnpairResult
} from '../../backend-contract/security'
import type {
  WinRtBoundary,
  WinRtPairResult,
  WinRtSecurityState,
  WinRtSecurityStateChangedRecord
} from './winrt-boundary'

export interface WinRtSecurityBoundary extends WinRtBoundary {
  readonly securityState: NonNullable<WinRtBoundary['securityState']>
  readonly pair: NonNullable<WinRtBoundary['pair']>
  readonly cancelPairing: NonNullable<WinRtBoundary['cancelPairing']>
  readonly unpair: NonNullable<WinRtBoundary['unpair']>
  readonly onSecurityState: NonNullable<WinRtBoundary['onSecurityState']>
}

export function isWinRtSecurityBoundary(boundary: WinRtBoundary): boundary is WinRtSecurityBoundary {
  return (
    typeof boundary.securityState === 'function' &&
    typeof boundary.pair === 'function' &&
    typeof boundary.cancelPairing === 'function' &&
    typeof boundary.unpair === 'function' &&
    typeof boundary.onSecurityState === 'function'
  )
}

const securityStreamLimits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})

const securityLimitations = Object.freeze([
  Object.freeze({
    code: 'winrt-protection-level-measurement',
    explanation:
      'WinRT reports pairing state and protection level only when the native boundary supplies it; no weaker state is inferred.',
    affectedGuarantee: 'security measurement completeness'
  })
])

interface ActivePairing {
  readonly operation: ReturnType<NonNullable<WinRtBoundary['pair']>>
  readonly removeOuterAbort: (() => void) | null
}

export class WinRtSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activePairings = new Map<string, ActivePairing>()
  private readonly sequenceByPeer = new Map<string, number>()
  private readonly removeStateListener: () => void

  constructor(
    private readonly boundary: WinRtSecurityBoundary,
    private readonly now: () => number
  ) {
    this.removeStateListener = boundary.onSecurityState(record => this.stateChanged(record))
  }

  async state(peerId: string, _options: PublicOperationOptions): Promise<PeerSecurityState> {
    return this.snapshotState(await this.boundary.securityState(peerId).completion)
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    const stream = new CoreBoundedStream<PeerSecurityEvent>(securityStreamLimits, 'error')
    const peerStreams = this.streams.get(peerId) ?? new Set<CoreBoundedStream<PeerSecurityEvent>>()
    peerStreams.add(stream)
    this.streams.set(peerId, peerStreams)
    this.state(peerId, { signal: null, deadline: null }).then(
      state => this.emit(peerId, state),
      () => stream.closeWithReason('source-failed')
    )
    return stream
  }

  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    if (options.ceremony !== 'system') {
      return Promise.reject(contractError('capability.unsupported', 'capability', 'winrt.security.custom-ceremony'))
    }
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'winrt.security.pair.arbitration'))
    }
    const operation = this.boundary.pair(peerId)
    const removeOuterAbort = bindOuterAbort(options.signal, operation)
    const result = operation.completion
      .then(value => this.snapshotPairResult(value))
      .catch(error => {
        if (error instanceof BackendContractError && error.normalized.code === 'operation.aborted') {
          return { outcome: 'cancelled' as const }
        }
        throw error
      })
    this.activePairings.set(peerId, { operation, removeOuterAbort })
    const settle = () => {
      const active = this.activePairings.get(peerId)
      if (active?.operation === operation) {
        active.removeOuterAbort?.()
        this.activePairings.delete(peerId)
      }
    }
    result.then(settle, settle).catch(() => undefined)
    return result
  }

  async cancelPairing(peerId: string, _options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    const active = this.activePairings.get(peerId)
    if (active === undefined) return { outcome: 'not-pairing' }
    await active.operation.cancel()
    await this.boundary.cancelPairing(peerId).completion
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    return { outcome: await this.boundary.unpair(peerId).completion }
  }

  close(): void {
    this.removeStateListener()
    for (const active of this.activePairings.values()) {
      active.operation.cancel().catch(() => undefined)
      active.removeOuterAbort?.()
    }
    this.activePairings.clear()
    for (const streams of this.streams.values()) {
      for (const stream of streams) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
  }

  private stateChanged(record: WinRtSecurityStateChangedRecord): void {
    this.emit(record.nativePeerId, this.snapshotState(record.state))
  }

  private emit(peerId: string, state: PeerSecurityState): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    const sequence = (this.sequenceByPeer.get(peerId) ?? 0) + 1
    this.sequenceByPeer.set(peerId, sequence)
    const event = Object.freeze({ kind: 'state' as const, peerId, sequence, state })
    for (const stream of [...streams]) {
      if (stream.emit(event, 1).terminated) streams.delete(stream)
    }
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private snapshotState(state: WinRtSecurityState): PeerSecurityState {
    return Object.freeze({
      bond: state.bond,
      encryption: state.encryption,
      authentication: state.authentication,
      secureConnections: state.secureConnections,
      pairingPossible: state.pairingPossible,
      measuredAtMonotonicMs: this.now(),
      limitations: securityLimitations
    })
  }

  private snapshotPairResult(result: WinRtPairResult): SecurityPairResult {
    if (result.outcome === 'cancelled') return { outcome: 'cancelled' }
    if (result.outcome === 'rejected') return { outcome: 'rejected', reason: result.reason }
    if (result.state === null) throw contractError('protocol.violation', 'platform', 'winrt.security.pair-result')
    return { outcome: result.outcome, state: this.snapshotState(result.state) }
  }
}

function bindOuterAbort(
  signal: AbortSignal | null,
  operation: ReturnType<NonNullable<WinRtBoundary['pair']>>
): (() => void) | null {
  if (signal === null) return null
  const abort = () => operation.cancel().catch(() => undefined)
  if (signal.aborted) {
    abort()
    return null
  }
  signal.addEventListener('abort', abort, { once: true })
  return () => signal.removeEventListener('abort', abort)
}
