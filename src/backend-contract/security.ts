import { contractError } from './errors'
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

/**
 * What a cancellation actually achieved, not what it requested.
 *
 * A cancellation can lose the race, and it can also arrive at something that
 * was never going to bond. Each of those is a different fact and gets its own
 * word, mirroring `SecurityPairResult` so that a word means the same thing in
 * both types:
 *
 * - `'cancelled'`   your cancellation stopped it
 * - `'not-pairing'` there was nothing to stop
 * - `'paired'`      the bond completed before your cancellation arrived
 * - `'rejected'`    the peer refused it; nobody cancelled anything
 *
 * `'paired'` is deliberately not `'already-paired'`, which in `SecurityPairResult`
 * means the peer was bonded BEFORE the call. A pairing that FAILS is not a
 * fourth outcome: `cancelPairing()` rejects with the same error the pairing
 * rejected with, because inventing a resolved outcome for a failure would be
 * the same substitution this type exists to prevent.
 */
export type SecurityCancelPairingResult =
  | { readonly outcome: 'cancelled' }
  | { readonly outcome: 'not-pairing' }
  | { readonly outcome: 'paired' }
  | { readonly outcome: 'rejected'; readonly reason: string | null }
export type SecurityUnpairResult = { readonly outcome: 'unpaired' | 'already-unpaired' | 'unsupported' }

/**
 * The cancellation outcome implied by the pairing's OWN result.
 *
 * Every backend answers `cancelPairing()` by asking the in-flight pairing what
 * happened to it, rather than forming a second, independent opinion: two
 * observations of one fact can disagree, and that disagreement is the defect -
 * `pair()` reporting `paired` while `cancelPairing()` reported `cancelled` for
 * the same operation. Reading the pairing's own answer makes them incapable of
 * disagreeing, and keeps the four backends saying one thing.
 */
export function cancelOutcomeForPairResult(result: SecurityPairResult): SecurityCancelPairingResult {
  switch (result.outcome) {
    // One fact - a bond exists because of this operation - so one word. The
    // re-pairing distinction stays on `pair()` for callers who need it.
    case 'paired':
    case 'already-paired':
    case 'repaired':
      return { outcome: 'paired' }
    // The peer refused. Nobody cancelled anything, and saying otherwise would
    // claim credit for stopping something that stopped itself.
    case 'rejected':
      return { outcome: 'rejected', reason: result.reason }
    case 'cancelled':
      return { outcome: 'cancelled' }
  }
}

/**
 * The cancellation's outcome, bounded by the CANCELLING caller's own options.
 *
 * `cancelPairing()` reads the pairing's result rather than forming a second
 * opinion, which is what stops the two calls contradicting each other. The cost
 * is that it now waits for the pairing to settle - and until this existed, that
 * wait was bounded only by the options the *pairing's* caller passed. A caller
 * that supplied a deadline to `cancelPairing()` had it validated at admission
 * and then ignored, which is the shape this package keeps removing: accepting an
 * option and discarding it.
 *
 * On expiry this REJECTS with `operation.timed-out` or `operation.aborted`. It
 * never substitutes `'cancelled'`, because a caller who stopped waiting has
 * learned nothing about the bond - and answering `'cancelled'` would reintroduce
 * the exact lie the result vocabulary was widened to remove, this time under a
 * deadline. `state()` and `watch()` remain authoritative for the bond.
 */
export async function boundedCancelOutcome(
  pairing: Promise<SecurityPairResult>,
  options: PublicOperationOptions,
  now: () => number,
  operation: string
): Promise<SecurityCancelPairingResult> {
  if (options.signal?.aborted === true) {
    throw contractError('operation.aborted', 'core', operation)
  }
  if (options.deadline !== null && options.deadline <= now()) {
    throw contractError('operation.timed-out', 'core', operation)
  }
  if (options.signal === null && options.deadline === null) {
    return cancelOutcomeForPairResult(await pairing)
  }

  let onAbort: (() => void) | null = null
  let timer: ReturnType<typeof setTimeout> | null = null
  const stopWaiting = new Promise<never>((_resolve, reject) => {
    if (options.signal !== null) {
      onAbort = () => reject(contractError('operation.aborted', 'core', operation))
      options.signal.addEventListener('abort', onAbort, { once: true })
    }
    if (options.deadline !== null) {
      // Scheduled only when the caller asked to be bounded, and cleared on
      // every exit, so a prompt cancellation never waits on a timer.
      timer = setTimeout(
        () => reject(contractError('operation.timed-out', 'core', operation)),
        Math.max(0, Number(options.deadline) - now())
      )
    }
  })
  try {
    return cancelOutcomeForPairResult(await Promise.race([pairing, stopWaiting]))
  } finally {
    if (timer !== null) clearTimeout(timer)
    if (onAbort !== null && options.signal !== null) options.signal.removeEventListener('abort', onAbort)
  }
}

export interface SecurityBackend {
  state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState>
  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent>
  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult>
  cancelPairing(peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult>
  unpair(peerId: string, options: PublicOperationOptions): Promise<SecurityUnpairResult>
  close?(): void
}
