// contracts/src/streams.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Bounded-stream accounting. Derived from docs/UNIFIED_SEMANTICS.md §11 and
// src/public/{streams,stream-presets,stream-capacity}.ts (read-only
// reference). No queue may grow merely because a consumer stops reading.

import { assertByteCapacity, assertItemCapacity } from './bounds';
import { contractError } from './outcomes';

export type OverflowPolicy = 'latest' | 'drop-oldest' | 'drop-newest' | 'error';

export const OVERFLOW_POLICIES: readonly OverflowPolicy[] = [
  'latest',
  'drop-oldest',
  'drop-newest',
  'error',
] satisfies readonly OverflowPolicy[];

export interface StreamLimits {
  readonly itemCapacity: number;
  readonly byteCapacity: number;
  readonly reservedControlCapacity: number;
}

export function validateStreamLimits(input: {
  readonly itemCapacity: unknown;
  readonly byteCapacity: unknown;
  readonly reservedControlCapacity: unknown;
}): StreamLimits {
  const itemCapacity = assertItemCapacity(input.itemCapacity, 'stream.limits.item-capacity');
  const byteCapacity = assertByteCapacity(input.byteCapacity, 'stream.limits.byte-capacity');
  const reservedControlCapacity = assertItemCapacity(
    input.reservedControlCapacity,
    'stream.limits.reserved-control-capacity',
  );
  if (byteCapacity <= reservedControlCapacity) {
    throw contractError('stream.quota', 'stream', 'stream.limits.control-quota');
  }
  return Object.freeze({ itemCapacity, byteCapacity, reservedControlCapacity });
}

export type StreamName =
  | 'scan-observation'
  | 'notification'
  | 'adapter-state'
  | 'diagnostics'
  | 'restoration-replay';

export interface StreamDefault {
  readonly stream: StreamName;
  readonly itemCapacity: number;
  readonly byteCapacity: number;
  readonly policy: OverflowPolicy;
}

export const RESERVED_CONTROL_CAPACITY: 1 = 1;

export const STREAM_DEFAULTS: readonly StreamDefault[] = [
  { stream: 'scan-observation', itemCapacity: 1, byteCapacity: 524288, policy: 'latest' },
  { stream: 'notification', itemCapacity: 64, byteCapacity: 1048576, policy: 'drop-oldest' },
  { stream: 'adapter-state', itemCapacity: 64, byteCapacity: 65536, policy: 'latest' },
  { stream: 'diagnostics', itemCapacity: 256, byteCapacity: 524288, policy: 'drop-oldest' },
  { stream: 'restoration-replay', itemCapacity: 64, byteCapacity: 262144, policy: 'error' },
] satisfies readonly StreamDefault[];

export interface StreamAccounting {
  readonly admitted: number;
  readonly droppedOldest: number;
  readonly droppedBytes: number;
  readonly replaced: number;
  readonly terminated: boolean;
}

export type AdmissionDecision = 'admit' | 'replace' | 'drop-oldest' | 'drop-newest' | 'terminate';

export interface AdmissionResult {
  readonly decision: AdmissionDecision;
  readonly accounting: StreamAccounting;
}

function requireAccounting(value: StreamAccounting, operation: string): void {
  const counts = [value.admitted, value.droppedOldest, value.droppedBytes, value.replaced];
  for (const count of counts) {
    if (!Number.isSafeInteger(count) || count < 0) {
      throw contractError('argument.invalid', 'stream', operation);
    }
  }
}

// STR-01: drop policies keep the stream active and emit a coalescible
// overflow notice with cumulative counters; error closes ingress with one
// terminal overflow. Counters are monotonic for a stream lifetime.
export function applyStreamAdmission(
  accounting: StreamAccounting,
  input: { readonly policy: OverflowPolicy; readonly atCapacity: boolean; readonly incomingBytes: number },
): AdmissionResult {
  requireAccounting(accounting, 'stream.accounting');
  if (!OVERFLOW_POLICIES.some(policy => policy === input.policy)) {
    throw contractError('argument.invalid', 'stream', 'stream.policy');
  }
  if (
    typeof input.incomingBytes !== 'number' ||
    !Number.isSafeInteger(input.incomingBytes) ||
    input.incomingBytes < 0
  ) {
    throw contractError('bytes.invalid', 'stream', 'stream.incoming-bytes');
  }
  if (!input.atCapacity) {
    return {
      decision: 'admit',
      accounting: Object.freeze({ ...accounting, admitted: accounting.admitted + 1 }),
    };
  }
  switch (input.policy) {
    case 'latest':
      return {
        decision: 'replace',
        accounting: Object.freeze({ ...accounting, replaced: accounting.replaced + 1 }),
      };
    case 'drop-oldest':
      return {
        decision: 'drop-oldest',
        accounting: Object.freeze({
          ...accounting,
          droppedOldest: accounting.droppedOldest + 1,
          droppedBytes: accounting.droppedBytes + input.incomingBytes,
        }),
      };
    case 'drop-newest':
      return {
        decision: 'drop-newest',
        accounting: Object.freeze({
          ...accounting,
          droppedOldest: accounting.droppedOldest + 1,
          droppedBytes: accounting.droppedBytes + input.incomingBytes,
        }),
      };
    case 'error':
      return {
        decision: 'terminate',
        accounting: Object.freeze({ ...accounting, terminated: true }),
      };
  }
}
