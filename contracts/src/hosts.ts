// contracts/src/hosts.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Host/lease/resource types and multi-client arbitration. Derived from
// docs/UNIFIED_SEMANTICS.md §1/§3/§5/§20 and
// src/backend-contract/{identity,backend,ipc}.ts (read-only reference).

import { contractError } from './outcomes';

export type HostKind =
  | 'browser'
  | 'native-mobile'
  | 'node'
  | 'desktop-native'
  | 'desktop-webview'
  | 'test';

export type ManagerMode = 'owning' | 'borrowing';

export type AdapterAvailability = 'available' | 'unavailable' | 'unsupported' | 'unknown';

export type AdapterAuthorization =
  | 'granted'
  | 'denied'
  | 'restricted'
  | 'not-determined'
  | 'unavailable'
  | 'unknown';

export type AdapterPower = 'on' | 'off' | 'resetting' | 'unsupported' | 'unknown';

export interface AdapterSnapshot {
  readonly availability: AdapterAvailability;
  readonly authorization: AdapterAuthorization;
  readonly power: AdapterPower;
  readonly backendGeneration: string;
  readonly updatedAt: number;
  readonly safeReason: string | null;
}

// The one readiness predicate for authorization, shared so semantics cannot
// drift: only an explicit refusal blocks. Unknown (nothing measured) and
// not-determined (the user has not been asked yet) must not block, because
// the platform prompt is raised by using the radio, not by reading state.
export function isAuthorizationBlocking(authorization: AdapterAuthorization): boolean {
  return (
    authorization === 'denied' || authorization === 'restricted' || authorization === 'unavailable'
  );
}

export type OwnershipDecision =
  | { readonly kind: 'grant-physical' }
  | { readonly kind: 'grant-lease'; readonly lease: string }
  | { readonly kind: 'reject'; readonly code: 'scan.already-active' | 'connection.already-owned' | 'chooser.busy' | 'ownership.denied' };

// One physical scan controller. A second non-shared request fails without
// changing the first; an explicitly shared request with an identical share
// token receives an independently bounded stream.
export function arbitrateScanRequest(input: {
  readonly physicalActive: boolean;
  readonly shareToken?: string;
}): OwnershipDecision {
  if (!input.physicalActive) {
    return { kind: 'grant-physical' };
  }
  if (typeof input.shareToken === 'string' && input.shareToken.length > 0) {
    return { kind: 'grant-lease', lease: `scan-share:${input.shareToken}` };
  }
  return { kind: 'reject', code: 'scan.already-active' };
}

// OWN-01: two clients request the same peer. Multiple leases share one
// physical link only when the backend reports sharing support; the final
// release or explicit owner disconnect ends the link.
export function arbitrateConnectionRequest(input: {
  readonly sharingSupported: boolean;
  readonly existingLeases: number;
}): OwnershipDecision {
  if (
    typeof input.existingLeases !== 'number' ||
    !Number.isSafeInteger(input.existingLeases) ||
    input.existingLeases < 0
  ) {
    throw contractError('argument.invalid', 'core', 'connection-arbitration.leases');
  }
  if (input.existingLeases === 0) {
    return { kind: 'grant-physical' };
  }
  if (input.sharingSupported) {
    return { kind: 'grant-lease', lease: `connection-lease:${input.existingLeases + 1}` };
  }
  return { kind: 'reject', code: 'connection.already-owned' };
}

export interface OwnershipTransfer {
  readonly resourceKind: string;
  readonly sourceClient: string;
  readonly destinationClient: string;
  readonly generation: string;
  readonly transferEpoch: number;
}

// Ownership transfer requires an authenticated acceptance record. No resource
// is shared merely because two clients use equal filters or peer identifiers.
export function validateOwnershipTransfer(input: {
  readonly resourceKind: unknown;
  readonly sourceClient: unknown;
  readonly destinationClient: unknown;
  readonly generation: unknown;
  readonly transferEpoch: unknown;
}): OwnershipTransfer {
  if (typeof input.resourceKind !== 'string' || input.resourceKind.length === 0) {
    throw contractError('ownership.denied', 'core', 'ownership-transfer.resource');
  }
  if (typeof input.sourceClient !== 'string' || input.sourceClient.length === 0) {
    throw contractError('ownership.denied', 'core', 'ownership-transfer.source');
  }
  if (typeof input.destinationClient !== 'string' || input.destinationClient.length === 0) {
    throw contractError('ownership.denied', 'core', 'ownership-transfer.destination');
  }
  if (typeof input.generation !== 'string' || input.generation.length === 0) {
    throw contractError('ownership.denied', 'core', 'ownership-transfer.generation');
  }
  if (
    typeof input.transferEpoch !== 'number' ||
    !Number.isSafeInteger(input.transferEpoch) ||
    input.transferEpoch < 0
  ) {
    throw contractError('ownership.denied', 'core', 'ownership-transfer.epoch');
  }
  return Object.freeze({
    resourceKind: input.resourceKind,
    sourceClient: input.sourceClient,
    destinationClient: input.destinationClient,
    generation: input.generation,
    transferEpoch: input.transferEpoch,
  });
}
