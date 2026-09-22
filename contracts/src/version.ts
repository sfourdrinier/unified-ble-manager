// contracts/src/version.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Runtime-vs-contract-vs-build version axes with fail-closed mismatch
// rejection before effects. Derived from docs/UNIFIED_SEMANTICS.md §2 and
// src/backend-contract/primitives.ts (read-only reference).
// Approved correction AC-01: the package/build version is observability only
// and MUST NOT satisfy a runtime handshake.

import { freezeTable } from './freeze';
import { contractError } from './outcomes';

export const CONTRACT_REVISION: 'C-UBM.0.1.2-DRAFT' = 'C-UBM.0.1.2-DRAFT';
export const CONTRACT_STATUS: 'DRAFT' = 'DRAFT';
export const CONTRACT_ACCEPTANCE_GATE: 'U1' = 'U1';
export const BUILD_VERSION_IS_HANDSHAKE_AXIS: false = false;

export type RuntimeAxis =
  | 'backend-contract'
  | 'capability-schema'
  | 'event-schema'
  | 'trace-format'
  | 'native-protocol'
  | 'ipc-protocol';

export const RUNTIME_AXES: readonly RuntimeAxis[] = freezeTable([
  'backend-contract',
  'capability-schema',
  'event-schema',
  'trace-format',
  'native-protocol',
  'ipc-protocol',
] satisfies readonly RuntimeAxis[]);

export function isRuntimeAxis(value: unknown): value is RuntimeAxis {
  return typeof value === 'string' && RUNTIME_AXES.some(axis => axis === value);
}

export interface VersionSpan {
  readonly axis: RuntimeAxis;
  readonly minimum: number;
  readonly maximum: number;
}

export interface NegotiatedAxis {
  readonly axis: RuntimeAxis;
  readonly selected: number;
  readonly localMinimum: number;
  readonly localMaximum: number;
  readonly remoteMinimum: number;
  readonly remoteMaximum: number;
}

export interface CoreHandshakeOffer {
  readonly backendContract: VersionSpan;
  readonly capabilitySchema: VersionSpan;
  readonly eventSchema: VersionSpan;
  readonly traceFormat: VersionSpan;
}

export interface NativeHandshakeOffer extends CoreHandshakeOffer {
  readonly nativeProtocol: VersionSpan;
}

export interface IpcHandshakeOffer extends CoreHandshakeOffer {
  readonly ipcProtocol: VersionSpan;
}

export interface CoreNegotiated {
  readonly backendContract: NegotiatedAxis;
  readonly capabilitySchema: NegotiatedAxis;
  readonly eventSchema: NegotiatedAxis;
  readonly traceFormat: NegotiatedAxis;
}

export interface NativeNegotiated extends CoreNegotiated {
  readonly nativeProtocol: NegotiatedAxis;
}

export interface IpcNegotiated extends CoreNegotiated {
  readonly ipcProtocol: NegotiatedAxis;
}

function assertSpanNumber(value: unknown, path: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw contractError('protocol.malformed', 'core', path);
  }
  return value;
}

export function makeVersionSpan(axis: unknown, minimum: unknown, maximum: unknown): VersionSpan {
  if (!isRuntimeAxis(axis)) {
    throw contractError('protocol.malformed', 'core', 'version-span.axis');
  }
  const low = assertSpanNumber(minimum, `version-span.${axis}.minimum`);
  const high = assertSpanNumber(maximum, `version-span.${axis}.maximum`);
  if (low > high) {
    throw contractError('protocol.malformed', 'core', `version-span.${axis}.range`);
  }
  return Object.freeze({ axis, minimum: low, maximum: high });
}

// Highest common value wins. Disjoint ranges fail closed: there is no
// implicit downgrade, and negotiation completes before either party sends
// mutable work.
export function negotiateVersionSpan(local: VersionSpan, remote: VersionSpan): NegotiatedAxis {
  if (!isRuntimeAxis(local.axis) || !isRuntimeAxis(remote.axis)) {
    throw contractError('protocol.malformed', 'core', 'version-negotiate.axis');
  }
  if (local.axis !== remote.axis) {
    throw contractError('protocol.malformed', 'core', 'version-negotiate.axes');
  }
  assertSpanNumber(local.minimum, `version-negotiate.${local.axis}.local-minimum`);
  assertSpanNumber(local.maximum, `version-negotiate.${local.axis}.local-maximum`);
  assertSpanNumber(remote.minimum, `version-negotiate.${remote.axis}.remote-minimum`);
  assertSpanNumber(remote.maximum, `version-negotiate.${remote.axis}.remote-maximum`);
  if (local.minimum > local.maximum || remote.minimum > remote.maximum) {
    throw contractError('protocol.malformed', 'core', `version-negotiate.${local.axis}.range`);
  }
  const selected = Math.min(local.maximum, remote.maximum);
  if (selected < local.minimum || selected < remote.minimum) {
    throw contractError('protocol.incompatible', 'core', `version-negotiate.${local.axis}`);
  }
  return Object.freeze({
    axis: local.axis,
    selected,
    localMinimum: local.minimum,
    localMaximum: local.maximum,
    remoteMinimum: remote.minimum,
    remoteMaximum: remote.maximum,
  });
}

// A negotiated selection binds only to an offer that contains it. An
// attachment that receives an unoffered version terminates.
export function assertNegotiatedWithinOffer(selected: NegotiatedAxis, offer: VersionSpan): void {
  if (!isRuntimeAxis(selected.axis) || !isRuntimeAxis(offer.axis)) {
    throw contractError('protocol.malformed', 'core', 'version-accepted.axis');
  }
  if (selected.axis !== offer.axis) {
    throw contractError('protocol.malformed', 'core', 'version-accepted.axes');
  }
  if (selected.selected < offer.minimum || selected.selected > offer.maximum) {
    throw contractError('protocol.incompatible', 'core', `version-accepted.${selected.axis}`);
  }
}

export function negotiateCoreOffer(local: CoreHandshakeOffer, remote: CoreHandshakeOffer): CoreNegotiated {
  return {
    backendContract: negotiateVersionSpan(local.backendContract, remote.backendContract),
    capabilitySchema: negotiateVersionSpan(local.capabilitySchema, remote.capabilitySchema),
    eventSchema: negotiateVersionSpan(local.eventSchema, remote.eventSchema),
    traceFormat: negotiateVersionSpan(local.traceFormat, remote.traceFormat),
  };
}

export function negotiateNativeOffer(
  local: NativeHandshakeOffer,
  remote: NativeHandshakeOffer,
): NativeNegotiated {
  return {
    ...negotiateCoreOffer(local, remote),
    nativeProtocol: negotiateVersionSpan(local.nativeProtocol, remote.nativeProtocol),
  };
}

export function negotiateIpcOffer(local: IpcHandshakeOffer, remote: IpcHandshakeOffer): IpcNegotiated {
  return {
    ...negotiateCoreOffer(local, remote),
    ipcProtocol: negotiateVersionSpan(local.ipcProtocol, remote.ipcProtocol),
  };
}

// Contract revisions are never silently equal: a mismatch fails closed even
// when every runtime axis overlaps.
export function assertContractRevisionEqual(local: string, remote: string): void {
  if (local !== CONTRACT_REVISION || remote !== CONTRACT_REVISION || local !== remote) {
    throw contractError('protocol.incompatible', 'core', 'contract-revision.mismatch');
  }
}

export interface HandshakeState {
  readonly complete: boolean;
}

// PKG-02 gate: initialization fails before sensor operation when the binding
// and core identities differ. No effect dispatches on an incomplete handshake.
export function assertHandshakeComplete(state: HandshakeState, operation: string): void {
  if (!state.complete) {
    throw contractError('lifecycle.invalid-state', 'core', operation);
  }
  if (typeof operation !== 'string' || operation.length === 0) {
    throw contractError('argument.invalid', 'core', 'handshake.operation');
  }
}
