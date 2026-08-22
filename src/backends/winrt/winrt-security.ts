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
import { WinRtOperationDispatcher, type WinRtOperationDispatch } from './winrt-operation-dispatcher'

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
  readonly operation: WinRtOperationDispatch<WinRtPairResult>
}

type SecurityLifecycle = 'active' | 'adapter-lost' | 'closed'

export class WinRtSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activePairings = new Map<string, ActivePairing>()
  private readonly sequenceByPeer = new Map<string, number>()
  private removeStateListener: () => void
  private stateListenerGeneration = 0
  private lifecycle: SecurityLifecycle = 'active'

  constructor(
    private readonly boundary: WinRtSecurityBoundary,
    private readonly now: () => number,
    private readonly dispatcher = new WinRtOperationDispatcher({
      now,
      onLateSuccess: () => undefined,
      onLateFailure: () => undefined,
      onCancellationFailure: () => undefined
    })
  ) {
    this.removeStateListener = this.registerStateListener(this.stateListenerGeneration)
  }

  async state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.assertActive('winrt.security.state')
    const operation = this.dispatcher.dispatch(options, 'winrt.security.state', () =>
      this.boundary.securityState(peerId)
    )
    return this.snapshotState(await operation.completion)
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertActive('winrt.security.watch')
    const stream = new CoreBoundedStream<PeerSecurityEvent>(securityStreamLimits, 'error')
    const peerStreams = this.streams.get(peerId) ?? new Set<CoreBoundedStream<PeerSecurityEvent>>()
    peerStreams.add(stream)
    this.streams.set(peerId, peerStreams)
    this.state(peerId, { signal: null, deadline: null }).then(
      state => this.emit(peerId, state),
      () => {
        stream.closeWithReason('source-failed')
        this.removeStream(peerId, stream)
      }
    )
    return stream
  }

  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    this.assertActive('winrt.security.pair')
    if (options.ceremony !== 'system') {
      return Promise.reject(contractError('capability.unsupported', 'capability', 'winrt.security.custom-ceremony'))
    }
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'winrt.security.pair.arbitration'))
    }
    const operation = this.dispatcher.dispatch(options, 'winrt.security.pair', () => this.boundary.pair(peerId))
    this.activePairings.set(peerId, { operation })
    const settle = () => {
      const active = this.activePairings.get(peerId)
      if (active?.operation === operation) {
        this.activePairings.delete(peerId)
      }
    }
    operation.physicalCompletion.then(settle, settle).catch(() => undefined)
    const result = operation.completion
      .then(async value => {
        const snapshot = this.snapshotPairResult(value)
        await operation.physicalCompletion
        return snapshot
      })
      .catch(error => {
        if (error instanceof BackendContractError && error.normalized.code === 'operation.aborted') {
          return { outcome: 'cancelled' as const }
        }
        throw error
      })
    return result
  }

  async cancelPairing(peerId: string, _options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    this.assertActive('winrt.security.cancel-pairing')
    const active = this.activePairings.get(peerId)
    if (active === undefined) return { outcome: 'not-pairing' }
    await active.operation.requestCancellation()
    await this.boundary.cancelPairing(peerId).completion
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertActive('winrt.security.unpair')
    const operation = this.dispatcher.dispatch(_options, 'winrt.security.unpair', () => this.boundary.unpair(peerId))
    return { outcome: await operation.completion }
  }

  resetForAdapterLoss(): void {
    if (this.lifecycle === 'closed') return
    this.lifecycle = 'adapter-lost'
    this.removeStateListener()
    this.stateListenerGeneration += 1
    this.removeStateListener = this.registerStateListener(this.stateListenerGeneration)
    for (const active of this.activePairings.values()) {
      active.operation.requestCancellation().catch(() => undefined)
    }
    for (const streams of this.streams.values()) {
      for (const stream of streams) stream.closeWithReason('connection-lost')
    }
    this.streams.clear()
  }

  adapterRecovered(): void {
    if (this.lifecycle === 'adapter-lost') this.lifecycle = 'active'
  }

  close(): void {
    this.lifecycle = 'closed'
    this.removeStateListener()
    for (const active of this.activePairings.values()) {
      active.operation.requestCancellation().catch(() => undefined)
    }
    for (const streams of this.streams.values()) {
      for (const stream of streams) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
  }

  private stateChanged(record: WinRtSecurityStateChangedRecord): void {
    if (this.lifecycle !== 'active') return
    this.emit(record.nativePeerId, this.snapshotState(record.state))
  }

  private registerStateListener(generation: number): () => void {
    return this.boundary.onSecurityState(record => {
      if (generation !== this.stateListenerGeneration || this.lifecycle === 'closed') return
      this.stateChanged(record)
    })
  }

  private assertActive(operation: string): void {
    if (this.lifecycle === 'closed') throw contractError('lifecycle.destroyed', 'core', operation)
    if (this.lifecycle === 'adapter-lost') throw contractError('lifecycle.invalid-state', 'core', operation)
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

  private removeStream(peerId: string, stream: CoreBoundedStream<PeerSecurityEvent>): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    streams.delete(stream)
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
