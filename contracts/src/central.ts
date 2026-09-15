// contracts/src/central.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Central API surface: scan/connect/discover/GATT/subscribe validation,
// write-mode discipline, long-write planning, and the connection-controls
// façade. Derived from docs/UNIFIED_SEMANTICS.md §6/§8/§9/§10/§17 and
// src/public/ble-manager.ts + src/backend-contract/connection-controls.ts
// (read-only reference).

import { assertTimeoutMs } from './bounds';
import { canonicalUuidValue } from './identities';
import type { GattPath } from './identities';
import { contractError } from './outcomes';

export type ScanDuplicatePolicy = 'all' | 'first' | 'merged';
export type ScanMergePolicy = 'none' | 'latest-by-timestamp';

export interface ScanRequest {
  readonly serviceUuids: readonly string[];
  readonly duplicatePolicy: ScanDuplicatePolicy;
  readonly mergePolicy: ScanMergePolicy;
  readonly timeoutMs: number;
  readonly hasAbortSignal: boolean;
}

// Empty filters mean the platform's broad scan. Unsupported filter fields
// fail instead of silently broadening or narrowing the scan.
export function validateScanRequest(input: {
  readonly serviceUuids: readonly unknown[];
  readonly duplicatePolicy: unknown;
  readonly mergePolicy: unknown;
  readonly timeoutMs: unknown;
  readonly hasAbortSignal: unknown;
  readonly unsupportedFilterFields?: readonly unknown[];
}): ScanRequest {
  const unsupported = input.unsupportedFilterFields ?? [];
  for (const field of unsupported) {
    if (typeof field === 'string' && field.length > 0) {
      throw contractError('capability.unsupported', 'scan', 'scan.filter');
    }
  }
  const serviceUuids: string[] = [];
  for (const uuid of input.serviceUuids) {
    serviceUuids.push(canonicalUuidValue(uuid));
  }
  if (input.duplicatePolicy !== 'all' && input.duplicatePolicy !== 'first' && input.duplicatePolicy !== 'merged') {
    throw contractError('argument.invalid', 'scan', 'scan.duplicate-policy');
  }
  if (input.mergePolicy !== 'none' && input.mergePolicy !== 'latest-by-timestamp') {
    throw contractError('argument.invalid', 'scan', 'scan.merge-policy');
  }
  return Object.freeze({
    serviceUuids: Object.freeze(serviceUuids),
    duplicatePolicy: input.duplicatePolicy,
    mergePolicy: input.mergePolicy,
    timeoutMs: assertTimeoutMs(input.timeoutMs, 'scan.timeout'),
    hasAbortSignal: input.hasAbortSignal === true,
  });
}

export type WriteMode = 'with-response' | 'without-response' | 'long-write';

export interface WriteRequest {
  readonly path: GattPath;
  readonly mode: WriteMode;
  readonly valueByteLength: number;
}

// Write mode is mandatory. A requested mode that is unavailable is rejected
// rather than replaced with a different mode.
export function validateWriteRequest(input: {
  readonly path: GattPath;
  readonly currentConnectionGeneration: string;
  readonly currentDatabaseGeneration: string;
  readonly mode: unknown;
  readonly valueByteLength: unknown;
  readonly effectiveMaximum: number | null;
  readonly modeSupported: boolean;
}): WriteRequest {
  if (input.mode !== 'with-response' && input.mode !== 'without-response' && input.mode !== 'long-write') {
    throw contractError('argument.invalid', 'gatt', 'write.mode');
  }
  if (!input.modeSupported) {
    throw contractError('capability.unsupported', 'capability', 'write.mode');
  }
  if (input.path.connectionGeneration !== input.currentConnectionGeneration) {
    throw contractError('gatt.stale-handle', 'gatt', 'write.connection-generation');
  }
  if (input.path.databaseGeneration !== input.currentDatabaseGeneration) {
    throw contractError('gatt.stale-handle', 'gatt', 'write.database-generation');
  }
  if (
    typeof input.valueByteLength !== 'number' ||
    !Number.isSafeInteger(input.valueByteLength) ||
    input.valueByteLength < 0
  ) {
    throw contractError('bytes.invalid', 'gatt', 'write.length');
  }
  if (input.effectiveMaximum === null) {
    throw contractError('capability.unavailable', 'capability', 'write.maximum');
  }
  if (input.valueByteLength > input.effectiveMaximum) {
    throw contractError('bytes.too-large', 'gatt', 'write.length');
  }
  return Object.freeze({ path: input.path, mode: input.mode, valueByteLength: input.valueByteLength });
}

export interface LongWritePlan {
  readonly segmentMaximum: number;
  readonly segments: number;
  readonly atomic: false;
}

// The segment maximum is the minimum of the effective operation payload
// limit, negotiated directional limit, and declared backend limit. A
// sequential emulation MUST NOT claim a native atomic transaction.
export function planLongWrite(input: {
  readonly valueByteLength: unknown;
  readonly operationPayloadLimit: number | null;
  readonly negotiatedDirectionalLimit: number | null;
  readonly backendLimit: number | null;
}): LongWritePlan {
  if (
    typeof input.valueByteLength !== 'number' ||
    !Number.isSafeInteger(input.valueByteLength) ||
    input.valueByteLength < 0
  ) {
    throw contractError('bytes.invalid', 'gatt', 'long-write.length');
  }
  const limits = [input.operationPayloadLimit, input.negotiatedDirectionalLimit, input.backendLimit];
  let segmentMaximum = -1;
  for (const limit of limits) {
    if (limit === null) {
      throw contractError('capability.unavailable', 'capability', 'long-write.maximum');
    }
    if (!Number.isSafeInteger(limit) || limit <= 0) {
      throw contractError('argument.invalid', 'gatt', 'long-write.maximum');
    }
    if (segmentMaximum < 0 || limit < segmentMaximum) {
      segmentMaximum = limit;
    }
  }
  if (segmentMaximum <= 0) {
    throw contractError('argument.invalid', 'gatt', 'long-write.maximum');
  }
  const segments = Math.floor((input.valueByteLength + segmentMaximum - 1) / segmentMaximum);
  return Object.freeze({ segmentMaximum, segments, atomic: false });
}

export interface CentralControl {
  readonly method: string;
  readonly capabilityId: string;
  readonly acceptanceIsProof: boolean;
}

// A request and an observation are different facts. Accepted means the
// backend accepted the request for dispatch; it MUST NOT be presented as
// proof that the controller or peer selected the requested parameters.
export const CENTRAL_CONTROLS: readonly CentralControl[] = [
  { method: 'readRssi', capabilityId: 'connection:rssi', acceptanceIsProof: false },
  { method: 'effectiveMtu', capabilityId: 'connection:effective-mtu', acceptanceIsProof: false },
  { method: 'requestMtu', capabilityId: 'connection:request-mtu', acceptanceIsProof: false },
  { method: 'requestPriority', capabilityId: 'connection:priority', acceptanceIsProof: false },
  { method: 'parameters', capabilityId: 'connection:parameters', acceptanceIsProof: false },
  { method: 'readPhy', capabilityId: 'connection:phy', acceptanceIsProof: false },
  { method: 'requestPhy', capabilityId: 'connection:phy', acceptanceIsProof: false },
  { method: 'requestSubrate', capabilityId: 'connection:subrate', acceptanceIsProof: false },
  { method: 'maximumWriteLength', capabilityId: 'gatt:maximum-write-length', acceptanceIsProof: false },
  {
    method: 'writeReadiness',
    capabilityId: 'gatt:write-without-response-readiness',
    acceptanceIsProof: false,
  },
] satisfies readonly CentralControl[];
