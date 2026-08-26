import type { Limitation } from './capabilities'
import type { PublicOperationOptions } from './operations'
import type { BoundedAsyncStream } from './streams'

export type SecurityBondState = 'bonded' | 'not-bonded' | 'bonding' | 'unknown' | 'unsupported'
export type SecurityEncryptionState = 'encrypted' | 'not-encrypted' | 'unknown' | 'unsupported'
export type SecurityAuthenticationState = 'authenticated' | 'unauthenticated' | 'unknown' | 'unsupported'
export type SecureConnectionsState = 'yes' | 'no' | 'unknown' | 'unsupported'

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

export type SecurityPairingChallenge =
  | {
      readonly kind: 'confirm'
      readonly peerId: string
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'confirm-passkey'
      readonly peerId: string
      readonly challengeId: string
      readonly passkey: number
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'display-passkey'
      readonly peerId: string
      readonly challengeId: string
      readonly passkey: number
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'provide-pin'
      readonly peerId: string
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }
  | {
      readonly kind: 'provide-passkey'
      readonly peerId: string
      readonly challengeId: string
      readonly deadlineMonotonicMs: number
    }

export type SecurityPairingResponse =
  | { readonly kind: 'confirm'; readonly confirmed: boolean }
  | { readonly kind: 'confirm-passkey'; readonly confirmed: boolean }
  | { readonly kind: 'display-passkey'; readonly acknowledged: boolean }
  | { readonly kind: 'provide-pin'; readonly pin: string }
  | { readonly kind: 'provide-passkey'; readonly passkey: string }

export interface SecurityPairingAgent {
  onChallenge(challenge: SecurityPairingChallenge): Promise<SecurityPairingResponse>
}

export type SecurityPairingCeremony = 'system' | { readonly kind: 'agent'; readonly agent: SecurityPairingAgent }

export interface SecurityPairOptions extends PublicOperationOptions {
  readonly transport: 'le' | 'auto'
  readonly protection: 'system-default' | 'encrypted' | 'authenticated'
  readonly ceremony: SecurityPairingCeremony
  /**
   * Preferred LE pairing generation. 'prefer' (default) leaves the choice to
   * the platform; 'require' insists on Secure Connections; 'disallow' requests
   * LE Legacy pairing for peers that reject Secure Connections. Backends that
   * cannot honour the request return capability.unsupported.
   */
  readonly secureConnections?: 'require' | 'prefer' | 'disallow'
}

export type SecurityPairResult =
  | { readonly outcome: 'paired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'already-paired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'repaired'; readonly state: PeerSecurityState }
  | { readonly outcome: 'rejected'; readonly reason: string | null }
  | { readonly outcome: 'cancelled' }

export type SecurityCancelPairingResult = { readonly outcome: 'cancelled' | 'not-pairing' }
export type SecurityUnpairResult = { readonly outcome: 'unpaired' | 'already-unpaired' | 'unsupported' }

export interface SecurityBackend {
  state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState>
  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent>
  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult>
  cancelPairing(peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult>
  unpair(peerId: string, options: PublicOperationOptions): Promise<SecurityUnpairResult>
  close?(): void
}
