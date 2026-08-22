import { BackendContractError, contractError } from '../backend-contract/errors'
import type {
  PeerSecurityEvent as InternalPeerSecurityEvent,
  PeerSecurityState as InternalPeerSecurityState,
  SecurityBackend,
  SecurityPairOptions as InternalSecurityPairOptions,
  SecurityPairResult as InternalSecurityPairResult,
  SecurityCancelPairingResult as InternalSecurityCancelPairingResult,
  SecurityUnpairResult as InternalSecurityUnpairResult,
  SecurityPairingAgent as InternalSecurityPairingAgent,
  SecurityPairingChallenge as InternalSecurityPairingChallenge,
  SecurityPairingResponse as InternalSecurityPairingResponse
} from '../backend-contract/security'
import type { FeatureId, Limitation } from '../backend-contract/capabilities'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { rehydratePublicError } from './error-bridge'
import type { OperationOptions } from './operation-options'
import { normalizeOperationOptions } from './operation-options'
import { snapshotBlePeer, type BlePeer } from './ble-manager'
import type { BlePeerDirectory } from './peer-directory'
import { isPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'

export type SecurityBondState = InternalPeerSecurityState['bond']
export type SecurityEncryptionState = InternalPeerSecurityState['encryption']
export type SecurityAuthenticationState = InternalPeerSecurityState['authentication']
export type SecureConnectionsState = InternalPeerSecurityState['secureConnections']

export interface PeerSecurityState {
  readonly bond: SecurityBondState
  readonly encryption: SecurityEncryptionState
  readonly authentication: SecurityAuthenticationState
  readonly secureConnections: SecureConnectionsState
  readonly pairingPossible: boolean | null
  readonly measuredAtMonotonicMs: number
  readonly limitations: readonly Limitation[]
}

export interface PeerSecurityEvent {
  readonly kind: 'state'
  readonly peerId: string
  readonly sequence: number
  readonly state: PeerSecurityState
}

export interface PairOptions extends OperationOptions {
  readonly transport?: 'le' | 'auto'
  readonly protection?: 'system-default' | 'encrypted' | 'authenticated'
  readonly ceremony?: 'system' | PairingAgent
}

export type PairingChallenge =
  | {
      readonly kind: 'confirm'
      readonly peer: BlePeer
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'confirm-passkey'
      readonly peer: BlePeer
      readonly challengeId: string
      readonly passkey: number
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'display-passkey'
      readonly peer: BlePeer
      readonly challengeId: string
      readonly passkey: number
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'provide-pin'
      readonly peer: BlePeer
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'provide-passkey'
      readonly peer: BlePeer
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }

export type PairingResponse =
  | { readonly kind: 'confirm'; readonly confirmed: boolean }
  | { readonly kind: 'confirm-passkey'; readonly confirmed: boolean }
  | { readonly kind: 'display-passkey'; readonly acknowledged: boolean }
  | { readonly kind: 'provide-pin'; readonly pin: string }
  | { readonly kind: 'provide-passkey'; readonly passkey: string }

export interface PairingAgent {
  onChallenge(challenge: PairingChallenge): Promise<PairingResponse>
}

export type PairResult =
  | { readonly outcome: 'paired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'already-paired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'repaired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'rejected'; readonly reason: string | null }
  | { readonly outcome: 'cancelled' }

export type PairCancelResult = InternalSecurityCancelPairingResult
export type UnpairResult = InternalSecurityUnpairResult
export type SecurityPeer = BlePeer | PeerReference

export interface BleSecurity {
  state(peer: SecurityPeer, options?: OperationOptions): Promise<PeerSecurityState>
  watch(peer: SecurityPeer): AsyncIterable<PeerSecurityEvent>
  pair(peer: SecurityPeer, options?: PairOptions): Promise<PairResult>
  cancelPairing(peer: SecurityPeer, options?: OperationOptions): Promise<PairCancelResult>
  unpair(peer: SecurityPeer, options?: OperationOptions): Promise<UnpairResult>
}

export type SecurityRequirement = 'encrypted' | 'authenticated'

export interface RequiredSecurityOptions {
  readonly state?: OperationOptions
  /** Pairing is never implicit; this option is the explicit opt-in. */
  readonly pair?: PairOptions
}

export async function withRequiredSecurity<Value>(
  security: BleSecurity,
  peer: SecurityPeer,
  requirement: SecurityRequirement,
  action: () => Promise<Value>,
  options: RequiredSecurityOptions = {}
): Promise<Value> {
  const initial = await security.state(peer, options.state)
  if (securityRequirementSatisfied(initial, requirement)) return action()
  if (options.pair === undefined) {
    throw rehydratePublicError(contractError('platform.security', 'platform', 'with-required-security'))
  }
  const result = await security.pair(peer, options.pair)
  if (
    (result.outcome !== 'paired' && result.outcome !== 'already-paired' && result.outcome !== 'repaired') ||
    !securityRequirementSatisfied(result.state, requirement)
  ) {
    throw rehydratePublicError(contractError('platform.security', 'platform', 'with-required-security'))
  }
  return action()
}

function securityRequirementSatisfied(state: PeerSecurityState, requirement: SecurityRequirement): boolean {
  if (requirement === 'encrypted') return state.encryption === 'encrypted'
  return state.authentication === 'authenticated'
}

interface SecurityCapabilitySource {
  capability?: (id: FeatureId) => { readonly state: string } | null
  get?: (id: FeatureId) => { readonly state: string } | undefined
}

export function createPublicSecurity(
  backend: SecurityBackend | undefined,
  peers: BlePeerDirectory,
  capabilities: SecurityCapabilitySource,
  now: () => number
): BleSecurity {
  const unsupportedState = (): PeerSecurityState =>
    Object.freeze({
      bond: 'unsupported',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: null,
      measuredAtMonotonicMs: now(),
      limitations: Object.freeze([
        Object.freeze({
          code: 'security-backend-unavailable',
          explanation: 'This instantiated backend does not expose public security state.',
          affectedGuarantee: 'security state measurement'
        })
      ])
    })

  const requireBackend = (capability: FeatureId, operation: string): SecurityBackend => {
    if (backend === undefined || !isCapabilityUsable(readCapability(capabilities, capability))) {
      throw contractError('capability.unsupported', 'capability', operation)
    }
    return backend
  }

  const resolvePeer = async (peer: SecurityPeer, options: OperationOptions): Promise<{ id: string; peer: BlePeer }> => {
    if (isPeerReference(peer)) {
      const resolved = await peers.resolve(peer, options)
      if (resolved === null) throw contractError('peer.not-found', 'connection', 'public-security.resolve-peer')
      return { id: assertPeerId(resolved.id, 'public-security.resolve-peer'), peer: snapshotBlePeer(resolved) }
    }
    if (typeof peer !== 'object' || peer === null || Array.isArray(peer)) {
      throw contractError('argument.invalid', 'connection', 'public-security.peer')
    }
    return { id: assertPeerId(peer.id, 'public-security.peer'), peer: snapshotBlePeer(peer) }
  }

  return {
    state: async (peer, options = {}) => {
      try {
        const normalized = normalizeOperationOptions(options, now)
        if (backend === undefined || !isCapabilityUsable(readCapability(capabilities, 'security:state'))) {
          return unsupportedState()
        }
        const resolved = await resolvePeer(peer, options)
        return snapshotSecurityState(await backend.state(resolved.id, normalized), 'public-security.state')
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    watch: peer => {
      try {
        const security = requireBackend('security:state', 'public-security.watch')
        return mapSecurityEvents(resolvePeer(peer, {}).then(resolved => security.watch(resolved.id)))
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    pair: async (peer, options = {}) => {
      try {
        const security = requireBackend('security:pair', 'public-security.pair')
        const normalized = normalizeOperationOptions(options, now)
        const resolved = await resolvePeer(peer, options)
        if (options.ceremony !== undefined && options.ceremony !== 'system') {
          requireBackend('security:custom-ceremony', 'public-security.pair.custom-ceremony')
        }
        const ceremony = toInternalCeremony(options.ceremony, resolved.peer)
        const pairOptions: InternalSecurityPairOptions = {
          ...normalized,
          transport: options.transport ?? 'auto',
          protection: options.protection ?? 'system-default',
          ceremony
        }
        return snapshotPairResult(await security.pair(resolved.id, pairOptions), 'public-security.pair')
      } catch (error) {
        if (
          error instanceof BackendContractError &&
          (error.normalized.code === 'operation.aborted' || error.normalized.code === 'operation.timed-out')
        ) {
          return Object.freeze({ outcome: 'cancelled' as const })
        }
        throw rehydratePublicError(error)
      }
    },
    cancelPairing: async (peer, options = {}) => {
      try {
        const security = requireBackend('security:cancel-pairing', 'public-security.cancel-pairing')
        const normalized = normalizeOperationOptions(options, now)
        const resolved = await resolvePeer(peer, options)
        return snapshotCancelPairingResult(
          await security.cancelPairing(resolved.id, normalized),
          'public-security.cancel-pairing'
        )
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    unpair: async (peer, options = {}) => {
      try {
        const security = requireBackend('security:unpair', 'public-security.unpair')
        const normalized = normalizeOperationOptions(options, now)
        const resolved = await resolvePeer(peer, options)
        return snapshotUnpairResult(await security.unpair(resolved.id, normalized), 'public-security.unpair')
      } catch (error) {
        throw rehydratePublicError(error)
      }
    }
  }
}

function isCapabilityUsable(capability: { readonly state: string } | null): boolean {
  return capability?.state === 'supported' || capability?.state === 'limited'
}

function toInternalCeremony(ceremony: PairOptions['ceremony'], peer: BlePeer): InternalSecurityPairOptions['ceremony'] {
  if (ceremony === undefined || ceremony === 'system') return 'system'
  if (!isPairingAgent(ceremony)) throw contractError('argument.invalid', 'connection', 'public-security.ceremony')
  const agent = ceremony
  const internalAgent: InternalSecurityPairingAgent = {
    onChallenge: async challenge => {
      const publicChallenge = toPublicPairingChallenge(challenge, peer)
      const response = await agent.onChallenge(publicChallenge)
      return toInternalPairingResponse(response)
    }
  }
  return { kind: 'agent', agent: internalAgent }
}

function isPairingAgent(value: unknown): value is PairingAgent {
  return typeof value === 'object' && value !== null && typeof Reflect.get(value, 'onChallenge') === 'function'
}

function toPublicPairingChallenge(challenge: InternalSecurityPairingChallenge, peer: BlePeer): PairingChallenge {
  if (
    challenge.peerId !== peer.id ||
    typeof challenge.challengeId !== 'string' ||
    challenge.challengeId.length === 0 ||
    !Number.isFinite(challenge.deadlineMonotonicMs)
  ) {
    throw contractError('protocol.violation', 'platform', 'public-security.pairing-challenge')
  }
  if (challenge.kind === 'confirm' || challenge.kind === 'provide-pin' || challenge.kind === 'provide-passkey') {
    return Object.freeze({
      kind: challenge.kind,
      peer,
      challengeId: challenge.challengeId,
      deadlineMonotonicMs: challenge.deadlineMonotonicMs
    })
  }
  if (
    (challenge.kind === 'confirm-passkey' || challenge.kind === 'display-passkey') &&
    Number.isSafeInteger(challenge.passkey) &&
    challenge.passkey >= 0 &&
    challenge.passkey <= 999999
  ) {
    return Object.freeze({
      kind: challenge.kind,
      peer,
      challengeId: challenge.challengeId,
      passkey: challenge.passkey,
      deadlineMonotonicMs: challenge.deadlineMonotonicMs
    })
  }
  throw contractError('protocol.violation', 'platform', 'public-security.pairing-challenge')
}

function toInternalPairingResponse(response: unknown): InternalSecurityPairingResponse {
  if (typeof response !== 'object' || response === null || Array.isArray(response)) {
    throw contractError('argument.invalid', 'connection', 'public-security.pairing-response')
  }
  const kind = Reflect.get(response, 'kind')
  if (kind === 'confirm' || kind === 'confirm-passkey') {
    const confirmed = Reflect.get(response, 'confirmed')
    if (typeof confirmed !== 'boolean') {
      throw contractError('argument.invalid', 'connection', 'public-security.pairing-response.confirmed')
    }
    return Object.freeze({ kind, confirmed })
  }
  if (kind === 'display-passkey') {
    const acknowledged = Reflect.get(response, 'acknowledged')
    if (typeof acknowledged !== 'boolean') {
      throw contractError('argument.invalid', 'connection', 'public-security.pairing-response.acknowledged')
    }
    return Object.freeze({ kind, acknowledged })
  }
  if (kind === 'provide-pin' || kind === 'provide-passkey') {
    const value = Reflect.get(response, kind === 'provide-pin' ? 'pin' : 'passkey')
    if (typeof value !== 'string' || value.length === 0 || value.length > 64) {
      throw contractError('argument.invalid', 'connection', 'public-security.pairing-response.secret')
    }
    return kind === 'provide-pin' ? Object.freeze({ kind, pin: value }) : Object.freeze({ kind, passkey: value })
  }
  throw contractError('argument.invalid', 'connection', 'public-security.pairing-response.kind')
}

function readCapability(source: SecurityCapabilitySource, id: FeatureId): { readonly state: string } | null {
  return source.capability?.(id) ?? source.get?.(id) ?? null
}

function assertPeerId(value: unknown, operation: string): string {
  if (typeof value !== 'string' || value.length === 0) throw contractError('argument.invalid', 'connection', operation)
  return value
}

function snapshotSecurityState(value: InternalPeerSecurityState, operation: string): PeerSecurityState {
  if (
    !isSecurityStateValue(value.bond, ['bonded', 'not-bonded', 'bonding', 'unknown', 'unsupported']) ||
    !isSecurityStateValue(value.encryption, ['encrypted', 'not-encrypted', 'unknown', 'unsupported']) ||
    !isSecurityStateValue(value.authentication, ['authenticated', 'unauthenticated', 'unknown', 'unsupported']) ||
    !isSecurityStateValue(value.secureConnections, ['yes', 'no', 'unknown', 'unsupported']) ||
    (value.pairingPossible !== null && typeof value.pairingPossible !== 'boolean') ||
    !Number.isFinite(value.measuredAtMonotonicMs) ||
    !Array.isArray(value.limitations) ||
    value.limitations.some(limitation => !isLimitation(limitation))
  ) {
    throw contractError('protocol.violation', 'platform', operation)
  }
  return Object.freeze({
    bond: value.bond,
    encryption: value.encryption,
    authentication: value.authentication,
    secureConnections: value.secureConnections,
    pairingPossible: value.pairingPossible,
    measuredAtMonotonicMs: value.measuredAtMonotonicMs,
    limitations: Object.freeze(value.limitations.map(limitation => Object.freeze({ ...limitation })))
  })
}

function snapshotPairResult(value: InternalSecurityPairResult, operation: string): PairResult {
  if (value.outcome === 'paired' || value.outcome === 'already-paired' || value.outcome === 'repaired') {
    return Object.freeze({ outcome: value.outcome, state: snapshotSecurityState(value.state, operation) })
  }
  if (value.outcome === 'rejected') {
    if (value.reason !== null && typeof value.reason !== 'string') {
      throw contractError('protocol.violation', 'platform', operation)
    }
    return Object.freeze({ outcome: 'rejected', reason: value.reason })
  }
  if (value.outcome === 'cancelled') return Object.freeze({ outcome: 'cancelled' })
  throw contractError('protocol.violation', 'platform', operation)
}

function snapshotCancelPairingResult(value: InternalSecurityCancelPairingResult, operation: string): PairCancelResult {
  if (value.outcome !== 'cancelled' && value.outcome !== 'not-pairing') {
    throw contractError('protocol.violation', 'platform', operation)
  }
  return Object.freeze({ outcome: value.outcome })
}

function snapshotUnpairResult(value: InternalSecurityUnpairResult, operation: string): UnpairResult {
  if (value.outcome !== 'unpaired' && value.outcome !== 'already-unpaired' && value.outcome !== 'unsupported') {
    throw contractError('protocol.violation', 'platform', operation)
  }
  return Object.freeze({ outcome: value.outcome })
}

function mapSecurityEvents(
  source: BoundedAsyncStream<InternalPeerSecurityEvent> | Promise<BoundedAsyncStream<InternalPeerSecurityEvent>>
): AsyncIterable<PeerSecurityEvent> {
  return {
    [Symbol.asyncIterator]() {
      const iteratorPromise = Promise.resolve(source).then(value => value[Symbol.asyncIterator]())
      return {
        async next(): Promise<IteratorResult<PeerSecurityEvent, undefined>> {
          const iterator = await iteratorPromise
          const item = await iterator.next()
          if (item.done) return { done: true, value: undefined }
          if (item.value.kind === 'terminal') {
            await iterator.return()
            await (await sourcePromise(source)).close()
            return { done: true, value: undefined }
          }
          if (item.value.kind === 'overflow') {
            await (await sourcePromise(source)).close()
            throw contractError('stream.overflow', 'stream', 'public-security.watch')
          }
          const event = item.value.value
          if (event.kind !== 'state' || typeof event.peerId !== 'string' || !Number.isSafeInteger(event.sequence)) {
            throw contractError('protocol.violation', 'platform', 'public-security.watch-event')
          }
          return {
            done: false,
            value: Object.freeze({
              kind: 'state',
              peerId: event.peerId,
              sequence: event.sequence,
              state: snapshotSecurityState(event.state, 'public-security.watch-event')
            })
          }
        },
        return: async () => {
          const iterator = await iteratorPromise
          await iterator.return()
          await (await sourcePromise(source)).close()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

function sourcePromise(
  source: BoundedAsyncStream<InternalPeerSecurityEvent> | Promise<BoundedAsyncStream<InternalPeerSecurityEvent>>
): Promise<BoundedAsyncStream<InternalPeerSecurityEvent>> {
  return Promise.resolve(source)
}

function isSecurityStateValue(value: unknown, allowed: readonly string[]): value is string {
  return typeof value === 'string' && allowed.includes(value)
}

function isLimitation(value: unknown): value is Limitation {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  return (
    typeof Reflect.get(value, 'code') === 'string' &&
    typeof Reflect.get(value, 'explanation') === 'string' &&
    typeof Reflect.get(value, 'affectedGuarantee') === 'string'
  )
}
