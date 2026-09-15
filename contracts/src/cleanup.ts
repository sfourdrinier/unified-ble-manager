// contracts/src/cleanup.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Cleanup receipts, resource counters, and early-exit governance. Derived
// from docs/UNIFIED_SEMANTICS.md §4/§22 and src/public/cleanup.ts
// (read-only reference). Cleanup never swallows an error or treats a failed
// release as success.

import { freezeTable } from './freeze';
import { contractError } from './outcomes';
import { ContractError } from './outcomes';

export type CleanupState = 'released' | 'release-failed';

export interface CleanupFailure {
  readonly resourceKind: string;
  readonly error: ContractError;
}

export interface CleanupRecord {
  readonly state: CleanupState;
  readonly failures: readonly CleanupFailure[];
}

function requireFailure(input: unknown, operation: string): CleanupFailure {
  if (typeof input !== 'object' || input === null || Array.isArray(input)) {
    throw contractError('protocol.malformed', 'boundary', operation);
  }
  const candidate: { resourceKind?: unknown; error?: unknown } = input;
  if (typeof candidate.resourceKind !== 'string' || candidate.resourceKind.length === 0) {
    throw contractError('protocol.malformed', 'boundary', operation);
  }
  if (!(candidate.error instanceof ContractError)) {
    throw contractError('protocol.malformed', 'boundary', operation);
  }
  return Object.freeze({ resourceKind: candidate.resourceKind, error: candidate.error });
}

// A single-resource record is either successful (zero failures) or failed
// (one or more failures). Batch cleanup continues after an individual
// failure and returns one composite record.
export function makeCleanupRecord(input: {
  readonly state: unknown;
  readonly failures: readonly unknown[];
}): CleanupRecord {
  if (input.state !== 'released' && input.state !== 'release-failed') {
    throw contractError('protocol.malformed', 'boundary', 'cleanup.state');
  }
  const failures: CleanupFailure[] = [];
  for (const entry of input.failures) {
    failures.push(requireFailure(entry, 'cleanup.failure'));
  }
  if (input.state === 'released' && failures.length !== 0) {
    throw contractError('protocol.malformed', 'boundary', 'cleanup.released-failures');
  }
  if (input.state === 'release-failed' && failures.length === 0) {
    throw contractError('protocol.malformed', 'boundary', 'cleanup.release-failed-failures');
  }
  return Object.freeze({ state: input.state, failures: Object.freeze(failures) });
}

export function combineCleanupRecords(records: readonly CleanupRecord[]): CleanupRecord {
  const failures: CleanupFailure[] = [];
  for (const record of records) {
    for (const failure of record.failures) {
      failures.push(failure);
    }
  }
  if (failures.length === 0) {
    return makeCleanupRecord({ state: 'released', failures: [] });
  }
  return makeCleanupRecord({ state: 'release-failed', failures });
}

export type ResourceCounterKind =
  | 'scan-controllers'
  | 'scan-consumers'
  | 'chooser-sessions'
  | 'connection-leases'
  | 'physical-links'
  | 'database-snapshots'
  | 'cccd-enablements'
  | 'subscription-consumers'
  | 'queued-operations'
  | 'dispatched-operations'
  | 'retained-buffers'
  | 'restoration-records'
  | 'orphan-owners';

export interface CounterLedger {
  count(kind: ResourceCounterKind): number;
  increment(kind: ResourceCounterKind): CounterLedger;
  decrement(kind: ResourceCounterKind): CounterLedger;
}

// Counter increment happens before its resource becomes observable;
// decrement happens before ownership release. Underflow is a protocol
// failure that forces the affected backend to reset.
export function createCounterLedger(counts?: {
  readonly [key: string]: number;
}): CounterLedger {
  const snapshot: { [key: string]: number } = {};
  if (counts !== undefined) {
    for (const key of Object.keys(counts)) {
      const value = counts[key];
      if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
        throw contractError('argument.invalid', 'core', 'counters.snapshot');
      }
      snapshot[key] = value;
    }
  }
  const count = (kind: ResourceCounterKind): number => {
    const value = snapshot[kind];
    return typeof value === 'number' ? value : 0;
  };
  const increment = (kind: ResourceCounterKind): CounterLedger => {
    const next: { [key: string]: number } = { ...snapshot };
    next[kind] = count(kind) + 1;
    return createCounterLedger(next);
  };
  const decrement = (kind: ResourceCounterKind): CounterLedger => {
    if (count(kind) <= 0) {
      throw contractError('lifecycle.invariant-violation', 'core', 'counters.underflow');
    }
    const next: { [key: string]: number } = { ...snapshot };
    next[kind] = count(kind) - 1;
    return createCounterLedger(next);
  };
  return { count, increment, decrement };
}

export interface EarlyExitRow {
  readonly path: string;
  readonly resourceAction: string;
}

export const EARLY_EXIT_CLEANUP: readonly EarlyExitRow[] = freezeTable([
  { path: 'validation-failure', resourceAction: 'allocate-nothing' },
  { path: 'pre-abort', resourceAction: 'allocate-nothing' },
  { path: 'queue-abort', resourceAction: 'remove-queue-node-release-input' },
  { path: 'queue-deadline', resourceAction: 'remove-queue-node-release-input' },
  { path: 'dispatch-failure', resourceAction: 'release-token-invalidate-correlation' },
  { path: 'connect-failure', resourceAction: 'release-transport-remove-lease' },
  { path: 'discovery-failure', resourceAction: 'discard-partial-snapshot' },
  { path: 'subscribe-failure', resourceAction: 'unregister-consumer-decrement-enablement' },
  { path: 'scan-stop', resourceAction: 'close-ingress-cancel-platform-release-owner' },
  { path: 'disconnect', resourceAction: 'close-children-settle-invalidate-paths' },
  { path: 'peer-loss', resourceAction: 'close-children-settle-invalidate-paths' },
  { path: 'reset', resourceAction: 'close-admission-drain-retain-bounded-cleanup' },
  { path: 'destroy', resourceAction: 'close-admission-drain-retain-bounded-cleanup' },
  { path: 'reload', resourceAction: 'close-admission-release-old-client' },
  { path: 'late-callback', resourceAction: 'suppress-release-temporary-change-nothing' },
] satisfies readonly EarlyExitRow[]);

