import { BackendContractError, contractError } from '../../backend-contract/errors'
import type {
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityBackend,
  SecurityPairOptions,
  SecurityPairResult,
  SecurityCancelPairingResult,
  SecurityUnpairResult,
  SecurityPairingAgent,
  SecurityPairingChallenge,
  SecurityPairingResponse
} from '../../backend-contract/security'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import type { DeterministicOperationRuntime } from './deterministic-operation-runtime'
import { capacity } from '../../backend-contract/primitives'
import type { DeterministicVirtualClock } from './virtual-clock'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import type { BoundedAsyncStream } from '../../backend-contract/streams'

interface ActivePairing {
  readonly controller: AbortController
  readonly removeOuterAbort: (() => void) | null
  readonly operation: Promise<SecurityPairResult>
}

const securityStreamLimits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})

const deterministicLimitations = Object.freeze([
  Object.freeze({
    code: 'deterministic-only',
    explanation: 'The security state and pairing ceremony are virtual deterministic test behavior.',
    affectedGuarantee: 'physical-radio interoperability'
  })
])

export interface SecurityStreamOwnershipSnapshot {
  readonly peerCount: number
  readonly streamCount: number
}

const deterministicSecurityOwnershipInspectors = new WeakMap<
  DeterministicSecurityBackend,
  () => SecurityStreamOwnershipSnapshot
>()

export function inspectDeterministicSecurityStreamOwnershipForTests(
  backend: DeterministicSecurityBackend
): SecurityStreamOwnershipSnapshot {
  const inspect = deterministicSecurityOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('deterministic security ownership inspector is missing')
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

export class DeterministicSecurityBackend implements SecurityBackend {
  private readonly states = new Map<string, PeerSecurityState>()
  private readonly streams = new Map<string, Set<OwnedCoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activePairings = new Map<string, ActivePairing>()
  private readonly sequenceByPeer = new Map<string, number>()
  private nextChallenge = 1

  constructor(
    private readonly clock: DeterministicVirtualClock,
    private readonly operations: DeterministicOperationRuntime,
    private readonly assertUsable: (operation: string) => void
  ) {
    deterministicSecurityOwnershipInspectors.set(this, () => securityStreamOwnershipSnapshot(this.streams))
  }

  state(peerId: string, _options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.assertUsable('security.state')
    return Promise.resolve(this.currentState(peerId))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertUsable('security.watch')
    const stream = new OwnedCoreBoundedStream<PeerSecurityEvent>(securityStreamLimits, 'error', () => {
      this.removeStream(peerId, stream)
    })
    const peerStreams = this.streams.get(peerId) ?? new Set<OwnedCoreBoundedStream<PeerSecurityEvent>>()
    peerStreams.add(stream)
    this.streams.set(peerId, peerStreams)
    this.emitTo(stream, this.createEvent(peerId, this.currentState(peerId)))
    return stream
  }

  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    this.assertUsable('security.pair')
    if (options.secureConnections !== undefined && options.secureConnections !== 'prefer') {
      // No backend can select a pairing generation per-pairing, so BlueZ, WinRT
      // and Android all fail closed here. This backend is test infrastructure:
      // if it accepted 'require' or 'disallow' and produced a bond anyway, a
      // consumer's suite would pass against a contract every real radio
      // rejects, which is the one way test infrastructure can actively mislead.
      return Promise.reject(contractError('capability.unsupported', 'capability', 'security.pair.secure-connections'))
    }
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'security.pair.arbitration'))
    }
    const controller = new AbortController()
    const removeOuterAbort = this.bindOuterAbort(options.signal, controller)
    const operation = this.executePair(peerId, options, controller).catch(error => {
      if (
        error instanceof BackendContractError &&
        (error.normalized.code === 'operation.aborted' || error.normalized.code === 'operation.timed-out')
      ) {
        return { outcome: 'cancelled' as const }
      }
      throw error
    })
    this.activePairings.set(peerId, { controller, removeOuterAbort, operation })
    const settleActivePairing = () => {
      const active = this.activePairings.get(peerId)
      if (active?.operation === operation) {
        active.removeOuterAbort?.()
        this.activePairings.delete(peerId)
      }
    }
    operation.then(settleActivePairing, settleActivePairing).catch(() => undefined)
    return operation
  }

  cancelPairing(peerId: string, _options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    this.assertUsable('security.cancel-pairing')
    const active = this.activePairings.get(peerId)
    if (active === undefined) return Promise.resolve({ outcome: 'not-pairing' })
    active.controller.abort()
    return Promise.resolve({ outcome: 'cancelled' })
  }

  unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertUsable('security.unpair')
    const current = this.currentState(peerId)
    if (current.bond !== 'bonded') return Promise.resolve({ outcome: 'already-unpaired' })
    const next = this.createState('not-bonded', 'not-encrypted', 'unauthenticated', 'no')
    this.states.set(peerId, next)
    this.emit(peerId, next)
    return Promise.resolve({ outcome: 'unpaired' })
  }

  reservedBytes(): number {
    let retained = 0
    for (const streams of this.streams.values()) {
      for (const stream of streams) {
        if (stream.isTerminal()) continue
        retained += stream.retainedBytes()
      }
    }
    return retained
  }

  close(): void {
    for (const active of this.activePairings.values()) active.controller.abort()
    this.activePairings.clear()
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) {
        stream.closeWithReason('owner-released')
      }
    }
    this.streams.clear()
  }

  private completePair(peerId: string): SecurityPairResult {
    const current = this.currentState(peerId)
    if (current.bond === 'bonded') return { outcome: 'already-paired', state: current }
    const next = this.createState('bonded', 'encrypted', 'authenticated', 'yes')
    this.states.set(peerId, next)
    this.emit(peerId, next)
    return { outcome: 'paired', state: next }
  }

  private async executePair(
    peerId: string,
    options: SecurityPairOptions,
    controller: AbortController
  ): Promise<SecurityPairResult> {
    if (options.ceremony !== 'system') {
      const challenge = this.createChallenge(peerId)
      const response = await this.awaitChallenge(options.ceremony.agent, challenge, controller, options.deadline)
      if (!this.acceptedResponse(challenge, response)) {
        return { outcome: 'rejected', reason: 'pairing-agent-rejected' }
      }
    }
    const result = await this.operations.run(
      'security-pair',
      { signal: controller.signal, deadline: options.deadline },
      null,
      true,
      () => this.completePair(peerId),
      null,
      null,
      `security:${peerId}`
    )
    return result.value
  }

  private createChallenge(peerId: string): SecurityPairingChallenge {
    const challengeId = `security-challenge-${this.nextChallenge}`
    this.nextChallenge += 1
    const deadlineMonotonicMs = Number(this.clock.now()) + 30_000
    return Object.freeze({
      kind: 'confirm-passkey',
      peerId,
      challengeId,
      passkey: 123456,
      deadlineMonotonicMs
    })
  }

  private awaitChallenge(
    agent: SecurityPairingAgent,
    challenge: SecurityPairingChallenge,
    controller: AbortController,
    operationDeadline: number | null
  ): Promise<SecurityPairingResponse> {
    const challengeDeadline =
      operationDeadline === null
        ? challenge.deadlineMonotonicMs
        : Math.min(Number(operationDeadline), challenge.deadlineMonotonicMs)
    const remaining = challengeDeadline - Number(this.clock.now())
    if (remaining <= 0)
      return Promise.reject(contractError('operation.timed-out', 'core', 'security.pairing-challenge'))
    let deadlineTask: { cancel(): void } | null = this.clock.scheduleAfter(remaining, () => controller.abort())
    let onAbort: (() => void) | null = null
    const aborted = new Promise<SecurityPairingResponse>((_resolve, reject) => {
      onAbort = () => reject(contractError('operation.aborted', 'core', 'security.pairing-challenge'))
      if (controller.signal.aborted) {
        onAbort()
        return
      }
      controller.signal.addEventListener('abort', onAbort, { once: true })
    })
    const response = Promise.race([agent.onChallenge(challenge), aborted])
    const cleanup = () => {
      deadlineTask?.cancel()
      deadlineTask = null
      if (onAbort !== null) controller.signal.removeEventListener('abort', onAbort)
    }
    return response.then(
      value => {
        cleanup()
        return value
      },
      error => {
        cleanup()
        throw error
      }
    )
  }

  private acceptedResponse(challenge: SecurityPairingChallenge, response: SecurityPairingResponse): boolean {
    if (challenge.kind === 'confirm-passkey') {
      return response.kind === 'confirm-passkey' && response.confirmed
    }
    if (challenge.kind === 'confirm') return response.kind === 'confirm' && response.confirmed
    if (challenge.kind === 'display-passkey') return response.kind === 'display-passkey' && response.acknowledged
    if (challenge.kind === 'provide-pin') return response.kind === 'provide-pin' && response.pin.length > 0
    return response.kind === 'provide-passkey' && response.passkey.length > 0
  }

  private currentState(peerId: string): PeerSecurityState {
    const current = this.states.get(peerId)
    if (current !== undefined) return current
    const initial = this.createState('not-bonded', 'not-encrypted', 'unauthenticated', 'no')
    this.states.set(peerId, initial)
    return initial
  }

  private createState(
    bond: PeerSecurityState['bond'],
    encryption: PeerSecurityState['encryption'],
    authentication: PeerSecurityState['authentication'],
    secureConnections: PeerSecurityState['secureConnections']
  ): PeerSecurityState {
    return Object.freeze({
      bond,
      encryption,
      authentication,
      secureConnections,
      pairingPossible: true,
      measuredAtMonotonicMs: Number(this.clock.now()),
      limitations: deterministicLimitations
    })
  }

  private emit(peerId: string, state: PeerSecurityState): void {
    const event = this.createEvent(peerId, state)
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    for (const stream of [...streams]) {
      if (stream.emit(event, 1).terminated) streams.delete(stream)
    }
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private emitTo(stream: OwnedCoreBoundedStream<PeerSecurityEvent>, event: PeerSecurityEvent): void {
    stream.emit(event, 1)
  }

  private removeStream(peerId: string, stream: OwnedCoreBoundedStream<PeerSecurityEvent>): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    streams.delete(stream)
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private createEvent(peerId: string, state: PeerSecurityState): PeerSecurityEvent {
    const sequence = (this.sequenceByPeer.get(peerId) ?? 0) + 1
    this.sequenceByPeer.set(peerId, sequence)
    return Object.freeze({ kind: 'state', peerId, sequence, state })
  }

  private bindOuterAbort(signal: AbortSignal | null, controller: AbortController): (() => void) | null {
    if (signal === null) return null
    const abort = () => controller.abort()
    if (signal.aborted) {
      controller.abort()
      return null
    }
    signal.addEventListener('abort', abort, { once: true })
    return () => signal.removeEventListener('abort', abort)
  }
}
