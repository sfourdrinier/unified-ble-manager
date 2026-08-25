import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream } from '../../core/bounded-stream'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
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

function isSecurityRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function securityRecord(value: unknown, operation: string): Record<string, unknown> {
  if (!isSecurityRecord(value)) {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  return value
}

function assertSecurityKeys(record: Record<string, unknown>, allowed: readonly string[], operation: string): void {
  try {
    for (const key of Reflect.ownKeys(record)) {
      if (typeof key !== 'string' || !allowed.includes(key)) {
        throw contractError('protocol.malformed', 'platform', operation)
      }
    }
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('protocol.malformed', 'platform', operation)
  }
}

function securityField(record: Record<string, unknown>, key: string, operation: string): unknown {
  try {
    return Reflect.get(record, key)
  } catch {
    throw contractError('protocol.malformed', 'platform', operation)
  }
}

function securityState(value: unknown, operation: string): WinRtSecurityState {
  const record = securityRecord(value, operation)
  assertSecurityKeys(
    record,
    ['bond', 'encryption', 'authentication', 'secureConnections', 'pairingPossible'],
    operation
  )
  const bond = securityField(record, 'bond', operation)
  const encryption = securityField(record, 'encryption', operation)
  const authentication = securityField(record, 'authentication', operation)
  const secureConnections = securityField(record, 'secureConnections', operation)
  const pairingPossible = securityField(record, 'pairingPossible', operation)
  if (
    (bond !== 'bonded' &&
      bond !== 'not-bonded' &&
      bond !== 'bonding' &&
      bond !== 'unknown' &&
      bond !== 'unsupported') ||
    (encryption !== 'encrypted' &&
      encryption !== 'not-encrypted' &&
      encryption !== 'unknown' &&
      encryption !== 'unsupported') ||
    (authentication !== 'authenticated' &&
      authentication !== 'unauthenticated' &&
      authentication !== 'unknown' &&
      authentication !== 'unsupported') ||
    (secureConnections !== 'yes' &&
      secureConnections !== 'no' &&
      secureConnections !== 'unknown' &&
      secureConnections !== 'unsupported') ||
    (pairingPossible !== null && typeof pairingPossible !== 'boolean')
  ) {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  return Object.freeze({ bond, encryption, authentication, secureConnections, pairingPossible })
}

function pairResult(value: unknown, operation: string): WinRtPairResult {
  const record = securityRecord(value, operation)
  assertSecurityKeys(record, ['outcome', 'state', 'reason'], operation)
  const outcome = securityField(record, 'outcome', operation)
  const reason = securityField(record, 'reason', operation)
  const state = securityField(record, 'state', operation)
  if (outcome !== 'paired' && outcome !== 'already-paired' && outcome !== 'rejected' && outcome !== 'cancelled') {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  if (reason !== null && typeof reason !== 'string') {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  if (outcome === 'rejected' || outcome === 'cancelled') {
    if (state !== null) throw contractError('protocol.malformed', 'platform', operation)
    if (outcome === 'cancelled' && reason !== null) {
      throw contractError('protocol.malformed', 'platform', operation)
    }
    return Object.freeze({ outcome, state: null, reason })
  }
  if (reason !== null) throw contractError('protocol.malformed', 'platform', operation)
  const validatedState = securityState(state, operation)
  if (validatedState.bond !== 'bonded') {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  return Object.freeze({ outcome, state: validatedState, reason: null })
}

function unpairOutcome(value: unknown, operation: string): SecurityUnpairResult['outcome'] {
  if (value !== 'unpaired' && value !== 'already-unpaired' && value !== 'unsupported') {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  return value
}

function securityStateChanged(value: unknown, operation: string): WinRtSecurityStateChangedRecord {
  const record = securityRecord(value, operation)
  assertSecurityKeys(record, ['nativePeerId', 'state'], operation)
  const nativePeerId = securityField(record, 'nativePeerId', operation)
  if (typeof nativePeerId !== 'string' || nativePeerId.length === 0) {
    throw contractError('protocol.malformed', 'platform', operation)
  }
  return Object.freeze({ nativePeerId, state: securityState(securityField(record, 'state', operation), operation) })
}

interface ActivePairing {
  readonly operation: WinRtOperationDispatch<WinRtPairResult>
  readonly nativePeerId: string
}

type SecurityLifecycle = 'active' | 'adapter-lost' | 'closed'

export interface SecurityStreamOwnershipSnapshot {
  readonly peerCount: number
  readonly streamCount: number
}

const winRtSecurityOwnershipInspectors = new WeakMap<WinRtSecurityBackend, () => SecurityStreamOwnershipSnapshot>()

export function inspectWinRtSecurityStreamOwnershipForTests(
  backend: WinRtSecurityBackend
): SecurityStreamOwnershipSnapshot {
  const inspect = winRtSecurityOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('winrt security ownership inspector is missing')
  }
  return inspect()
}

function securityStreamOwnershipSnapshot(
  streams: ReadonlyMap<string, ReadonlySet<unknown>>
): SecurityStreamOwnershipSnapshot {
  let streamCount = 0
  for (const peerStreams of streams.values()) {
    streamCount += peerStreams.size
  }
  return { peerCount: streams.size, streamCount }
}

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
    }),
    private readonly nativePeerIdForPeerId: (peerId: string, operation: string) => string = peerId => peerId,
    private readonly peerIdForNativePeerId: (nativePeerId: string) => string | null = nativePeerId => nativePeerId
  ) {
    this.removeStateListener = this.registerStateListener(this.stateListenerGeneration)
    winRtSecurityOwnershipInspectors.set(this, () => securityStreamOwnershipSnapshot(this.streams))
  }

  async state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.assertActive('winrt.security.state')
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'winrt.security.state')
    const operation = this.dispatcher.dispatch(options, 'winrt.security.state', () =>
      this.boundary.securityState(nativePeerId)
    )
    return this.snapshotState(securityState(await operation.completion, 'winrt.security.state'))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertActive('winrt.security.watch')
    const stream = new OwnedCoreBoundedStream<PeerSecurityEvent>(securityStreamLimits, 'error', () => {
      this.removeStream(peerId, stream)
    })
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
    if (options.protection !== 'system-default') {
      return Promise.reject(contractError('capability.unsupported', 'capability', 'winrt.security.pair.protection'))
    }
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'winrt.security.pair.arbitration'))
    }
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'winrt.security.pair')
    const operation = this.dispatcher.dispatch(options, 'winrt.security.pair', () => this.boundary.pair(nativePeerId))
    this.activePairings.set(peerId, { operation, nativePeerId })
    const settle = () => {
      const active = this.activePairings.get(peerId)
      if (active?.operation === operation) {
        this.activePairings.delete(peerId)
      }
    }
    operation.physicalSettlement.then(settle, settle).catch(() => undefined)
    const result = operation.completion
      .then(async value => {
        const snapshot = this.snapshotPairResult(value)
        await operation.physicalSettlement
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
    const acknowledgement = await active.operation.requestCancellation()
    if (acknowledgement.state !== 'cancellation-requested') {
      return { outcome: 'not-pairing' }
    }
    const dispatch = this.dispatcher.dispatch(_options, 'winrt.security.cancel-pairing', () => {
      const nativeCancellation = this.boundary.cancelPairing(active.nativePeerId)
      const completion = Promise.all([active.operation.requestCancellation(), nativeCancellation.completion]).then(
        () => undefined
      )
      return {
        completion,
        cancel: async () => {
          const state = await nativeCancellation.cancel()
          if (state !== 'cancellation-requested' && state !== 'already-terminal' && state !== 'not-cancellable') {
            throw contractError('protocol.malformed', 'boundary', 'winrt.security.cancel-pairing.acknowledgement')
          }
          return 'cancellation-requested' as const
        },
        physicalCompletion: Promise.all([completion, active.operation.physicalSettlement]).then(() => undefined)
      }
    })
    await dispatch.completion
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertActive('winrt.security.unpair')
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'winrt.security.unpair')
    const operation = this.dispatcher.dispatch(_options, 'winrt.security.unpair', () =>
      this.boundary.unpair(nativePeerId)
    )
    return { outcome: unpairOutcome(await operation.completion, 'winrt.security.unpair-result') }
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
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) stream.closeWithReason('connection-lost')
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
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
  }

  private stateChanged(record: WinRtSecurityStateChangedRecord): void {
    const validated = securityStateChanged(record, 'winrt.security.state-change')
    if (this.lifecycle !== 'active') return
    const peerId = this.peerIdForNativePeerId(validated.nativePeerId)
    if (peerId !== null) this.emit(peerId, this.snapshotState(validated.state))
  }

  private registerStateListener(generation: number): () => void {
    return this.boundary.onSecurityState(record => {
      if (generation !== this.stateListenerGeneration || this.lifecycle === 'closed') return
      try {
        this.stateChanged(record)
      } catch {
        for (const streams of [...this.streams.values()]) {
          for (const stream of [...streams]) stream.closeWithReason('source-failed')
        }
        this.streams.clear()
      }
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
    result = pairResult(result, 'winrt.security.pair-result')
    if (result.outcome === 'cancelled') return { outcome: 'cancelled' }
    if (result.outcome === 'rejected') return { outcome: 'rejected', reason: result.reason }
    if (result.state === null) throw contractError('protocol.violation', 'platform', 'winrt.security.pair-result')
    return { outcome: result.outcome, state: this.snapshotState(result.state) }
  }
}
