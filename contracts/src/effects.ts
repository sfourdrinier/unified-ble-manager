// contracts/src/effects.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Pure input/effect model, completion signatures, and race arbitration.
// Derived from docs/UNIFIED_SEMANTICS.md §13/§14 (read-only reference).
// A signal requests; the result reports what happened. Native callback
// arrival alone is not success: only a complete validated response contends.

import { contractError } from './outcomes';
import type { HandshakeState } from './version';
import { assertHandshakeComplete } from './version';

export type EffectKind =
  | 'radio.dispatch'
  | 'timer.schedule'
  | 'state.publish'
  | 'cleanup.release'
  | 'observation.deliver';

export interface Effect {
  readonly kind: EffectKind;
  readonly operationId: string;
  readonly detail: string;
}

export function makeEffect(kind: EffectKind, operationId: string, detail: string): Effect {
  if (operationId.length === 0 || detail.length === 0) {
    throw contractError('argument.invalid', 'core', 'effect.input');
  }
  return Object.freeze({ kind, operationId, detail });
}

export type ContenderKind =
  | 'success'
  | 'abort'
  | 'timeout'
  | 'disconnect'
  | 'reset'
  | 'destroy'
  | 'adapter-loss'
  | 'session-stop'
  | 'dispatch-begin';

export interface Contender {
  readonly ingressOrdinal: number;
  readonly kind: ContenderKind;
  readonly valid: boolean;
}

export type CompletionTerminal =
  | 'succeeded'
  | 'aborted'
  | 'timed-out'
  | 'disconnected'
  | 'reset'
  | 'adapter-unavailable'
  | 'destroyed';

export type CommitState = 'committed' | 'not-dispatched' | 'unknown' | 'released';

export interface CompletionRecord {
  readonly operationId: string;
  readonly winner: Contender | null;
  readonly suppressed: readonly Contender[];
  readonly terminal: CompletionTerminal;
  readonly reachedRadio: boolean;
  readonly pathsInvalidBeforeSettlement: boolean;
  readonly commitState: CommitState;
}

function terminalForWinner(kind: ContenderKind): CompletionTerminal {
  switch (kind) {
    case 'success':
      return 'succeeded';
    case 'abort':
      return 'aborted';
    case 'timeout':
      return 'timed-out';
    case 'disconnect':
      return 'disconnected';
    case 'reset':
      return 'reset';
    case 'adapter-loss':
      return 'adapter-unavailable';
    case 'destroy':
      return 'destroyed';
    case 'session-stop':
      return 'aborted';
    case 'dispatch-begin':
      return 'succeeded';
  }
}

// The manager assigns every externally visible contender an ingress ordinal
// under one serialization authority. For a single operation, the first valid
// contender that passes its state guard wins; invalid, duplicate, or stale
// callbacks never contend. OPS-02: a timed-out non-idempotent write ends in
// an explicit unknown commit state with no automatic duplicate.
export function arbitrateContenders(input: {
  readonly operationId: string;
  readonly dispatched: boolean;
  readonly contenders: readonly Contender[];
}): CompletionRecord {
  if (input.operationId.length === 0) {
    throw contractError('argument.invalid', 'core', 'arbitration.operation-id');
  }
  let winner: Contender | null = null;
  const suppressed: Contender[] = [];
  for (const contender of input.contenders) {
    if (
      typeof contender.ingressOrdinal !== 'number' ||
      !Number.isSafeInteger(contender.ingressOrdinal) ||
      contender.ingressOrdinal < 0
    ) {
      throw contractError('argument.invalid', 'core', 'arbitration.ingress-ordinal');
    }
    if (!contender.valid) {
      continue;
    }
    if (winner === null) {
      winner = contender;
    } else if (contender.ingressOrdinal >= winner.ingressOrdinal) {
      suppressed.push(contender);
    } else {
      suppressed.push(winner);
      winner = contender;
    }
  }
  if (winner === null) {
    throw contractError('lifecycle.invariant-violation', 'core', 'arbitration.no-contender');
  }
  const terminal = terminalForWinner(winner.kind);
  const reachedRadio = input.dispatched && winner.kind !== 'abort';
  const pathsInvalidBeforeSettlement =
    winner.kind === 'disconnect' || winner.kind === 'reset' || winner.kind === 'adapter-loss';
  const commitState: CommitState =
    winner.kind === 'success'
      ? 'committed'
      : winner.kind === 'abort' && !input.dispatched
        ? 'not-dispatched'
        : winner.kind === 'timeout' && input.dispatched
          ? 'unknown'
          : winner.kind === 'dispatch-begin'
            ? 'not-dispatched'
            : 'released';
  return Object.freeze({
    operationId: input.operationId,
    winner,
    suppressed: Object.freeze(suppressed),
    terminal,
    reachedRadio,
    pathsInvalidBeforeSettlement,
    commitState,
  });
}

export type HappensBeforePair = readonly [string, string];

export const HAPPENS_BEFORE: readonly HappensBeforePair[] = [
  ['negotiated-version', 'all-work'],
  ['ownership-verification', 'admission'],
  ['generation-invalidation', 'terminal-event'],
  ['stream-ingress-closure', 'stop-resolution'],
  ['stream-ingress-closure', 'remove-resolution'],
  ['stream-ingress-closure', 'destroy-resolution'],
  ['ready', 'first-subscription-value'],
  ['final-overflow-counters', 'stream-terminal'],
  ['cleanup-completion', 'ownership-release'],
  ['backend-generation-publication', 'work-under-generation'],
] satisfies readonly HappensBeforePair[];

// Runs the effect only after a verified handshake. The dispatch callback is
// never invoked on rejection, so nothing reaches the radio (PKG-02, OPS-01).
export function assertHandshakeBeforeEffects(
  state: HandshakeState,
  operation: string,
  dispatch: () => void,
): void {
  assertHandshakeComplete(state, operation);
  dispatch();
}
