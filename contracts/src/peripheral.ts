// contracts/src/peripheral.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Generic peripheral extension primitives. 4.x is central-only; this module
// freezes the role boundary (one host authority, distinct role resources,
// truthful role concurrency, shared lifecycle primitives) WITHOUT embedding
// commercial or physiological behavior. See semantic-map AC-05.

import { canonicalUuidValue } from './identities';
import { contractError } from './outcomes';

export type RoleKind = 'central' | 'peripheral';

export interface RoleConcurrency {
  readonly central: boolean;
  readonly peripheral: boolean;
  readonly simultaneous: boolean;
}

export function validateRoleConcurrency(input: {
  readonly central: unknown;
  readonly peripheral: unknown;
  readonly simultaneous: unknown;
}): RoleConcurrency {
  if (typeof input.central !== 'boolean' || typeof input.peripheral !== 'boolean' || typeof input.simultaneous !== 'boolean') {
    throw contractError('argument.invalid', 'core', 'roles.concurrency');
  }
  if (input.simultaneous && !(input.central && input.peripheral)) {
    throw contractError('argument.invalid', 'core', 'roles.simultaneous');
  }
  return Object.freeze({
    central: input.central,
    peripheral: input.peripheral,
    simultaneous: input.simultaneous,
  });
}

export type GattProperty =
  | 'broadcast'
  | 'read'
  | 'write-without-response'
  | 'write'
  | 'notify'
  | 'indicate'
  | 'signed-write'
  | 'extended-properties';

export interface DescriptorDecl {
  readonly uuid: string;
}

export interface CharacteristicDecl {
  readonly uuid: string;
  readonly properties: readonly GattProperty[];
  readonly descriptors: readonly DescriptorDecl[];
}

export interface ServiceDecl {
  readonly uuid: string;
  readonly primary: boolean;
  readonly characteristics: readonly CharacteristicDecl[];
}

function asGattProperty(value: unknown): GattProperty {
  switch (value) {
    case 'broadcast':
      return 'broadcast';
    case 'read':
      return 'read';
    case 'write-without-response':
      return 'write-without-response';
    case 'write':
      return 'write';
    case 'notify':
      return 'notify';
    case 'indicate':
      return 'indicate';
    case 'signed-write':
      return 'signed-write';
    case 'extended-properties':
      return 'extended-properties';
    default:
      throw contractError('argument.invalid', 'core', 'peripheral.property');
  }
}

export function validateServiceDecl(input: {
  readonly uuid: unknown;
  readonly primary: unknown;
  readonly characteristics: readonly {
    readonly uuid: unknown;
    readonly properties: readonly unknown[];
    readonly descriptors: readonly { readonly uuid: unknown }[];
  }[];
}): ServiceDecl {
  if (typeof input.primary !== 'boolean') {
    throw contractError('argument.invalid', 'core', 'peripheral.primary');
  }
  const characteristics: CharacteristicDecl[] = [];
  for (const characteristic of input.characteristics) {
    const properties: GattProperty[] = [];
    for (const property of characteristic.properties) {
      properties.push(asGattProperty(property));
    }
    const descriptors: DescriptorDecl[] = [];
    for (const descriptor of characteristic.descriptors) {
      descriptors.push({ uuid: canonicalUuidValue(descriptor.uuid) });
    }
    characteristics.push({
      uuid: canonicalUuidValue(characteristic.uuid),
      properties: Object.freeze(properties),
      descriptors: Object.freeze(descriptors),
    });
  }
  return Object.freeze({
    uuid: canonicalUuidValue(input.uuid),
    primary: input.primary,
    characteristics: Object.freeze(characteristics),
  });
}

// SRV-07: advertising payloads beyond host limits are explicitly rejected
// (or follow an approved reduced plan); fields are never silently dropped.
export function assertAdvertisementWithinLimits(input: {
  readonly payloadBytes: unknown;
  readonly hostMaximum: number | null;
  readonly fields: readonly string[];
}): void {
  if (
    typeof input.payloadBytes !== 'number' ||
    !Number.isSafeInteger(input.payloadBytes) ||
    input.payloadBytes < 0
  ) {
    throw contractError('bytes.invalid', 'core', 'peripheral.advertisement.length');
  }
  if (input.hostMaximum === null) {
    throw contractError('capability.unavailable', 'capability', 'peripheral.advertisement.maximum');
  }
  if (input.payloadBytes > input.hostMaximum) {
    throw contractError('bytes.too-large', 'core', 'peripheral.advertisement.length');
  }
}

export interface CccdBinding {
  readonly peerLease: string;
  readonly characteristic: string;
  readonly enabled: boolean;
  readonly kind: 'notification' | 'indication';
}

export interface CccdReconciliation {
  readonly bindings: readonly CccdBinding[];
  readonly physicalEnabled: boolean;
}

// SRV-04 / GATT-03: one peer's CCCD change leaves other peers correct.
// Physical enablement is reference counted; the last consumer out disables it.
export function reconcileCccdOnUnsubscribe(input: {
  readonly bindings: readonly CccdBinding[];
  readonly leavingPeerLease: string;
  readonly characteristic: string;
}): CccdReconciliation {
  const bindings: CccdBinding[] = [];
  for (const binding of input.bindings) {
    if (binding.peerLease === input.leavingPeerLease && binding.characteristic === input.characteristic) {
      continue;
    }
    bindings.push(binding);
  }
  let physicalEnabled = false;
  for (const binding of bindings) {
    if (binding.characteristic === input.characteristic && binding.enabled) {
      physicalEnabled = true;
    }
  }
  return Object.freeze({ bindings: Object.freeze(bindings), physicalEnabled });
}

// SRV-03: a cancelled or partially validated prepared transaction never
// reports atomic success.
export function assertAtomicCommit(input: {
  readonly preparedSegments: unknown;
  readonly validatedSegments: unknown;
  readonly cancelled: unknown;
  readonly failedValidation: unknown;
}): void {
  if (
    typeof input.preparedSegments !== 'number' ||
    !Number.isSafeInteger(input.preparedSegments) ||
    input.preparedSegments <= 0 ||
    typeof input.validatedSegments !== 'number' ||
    !Number.isSafeInteger(input.validatedSegments) ||
    input.validatedSegments < 0
  ) {
    throw contractError('argument.invalid', 'core', 'peripheral.prepared.segments');
  }
  if (input.cancelled === true) {
    throw contractError('operation.aborted', 'core', 'peripheral.prepared.cancelled');
  }
  if (input.failedValidation === true || input.validatedSegments !== input.preparedSegments) {
    throw contractError('protocol.violation', 'core', 'peripheral.prepared.atomic');
  }
}

export type ServerResponseVerdict = 'respond' | 'stale-ignored' | 'deadline-expired';

// SRV-06: at most one valid response per request; stale completions cannot
// affect a new request.
export function arbitrateServerResponse(input: {
  readonly requestId: string;
  readonly responded: boolean;
  readonly nowMs: number;
  readonly deadlineMs: number;
}): ServerResponseVerdict {
  if (input.requestId.length === 0) {
    throw contractError('argument.invalid', 'core', 'peripheral.response.request');
  }
  if (input.responded) {
    return 'stale-ignored';
  }
  if (!(input.nowMs < input.deadlineMs)) {
    return 'deadline-expired';
  }
  return 'respond';
}

export interface TargetedNotification {
  readonly targetPeerLease: string;
  readonly characteristic: string;
  readonly valueByteLength: number;
}

// SRV-05: only the intended peer receives private data.
export function validateTargetedNotification(input: {
  readonly targetPeerLease: unknown;
  readonly characteristic: unknown;
  readonly valueByteLength: unknown;
}): TargetedNotification {
  if (typeof input.targetPeerLease !== 'string' || input.targetPeerLease.length === 0) {
    throw contractError('ownership.denied', 'core', 'peripheral.notify.target');
  }
  if (typeof input.characteristic !== 'string' || input.characteristic.length === 0) {
    throw contractError('argument.invalid', 'core', 'peripheral.notify.characteristic');
  }
  if (
    typeof input.valueByteLength !== 'number' ||
    !Number.isSafeInteger(input.valueByteLength) ||
    input.valueByteLength < 0
  ) {
    throw contractError('bytes.invalid', 'core', 'peripheral.notify.length');
  }
  return Object.freeze({
    targetPeerLease: input.targetPeerLease,
    characteristic: input.characteristic,
    valueByteLength: input.valueByteLength,
  });
}

// Generic UBM carries no product policy: declarations with commercial or
// physiological keys are rejected at the boundary.
export const PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS: readonly string[] = [
  'ecg',
  'heart',
  'physio',
  'rr-interval',
  'contact',
  'payment',
  'sku',
  'subscription',
] satisfies readonly string[];

export function assertGenericPeripheralDecl(declaration: { readonly [key: string]: unknown }): void {
  for (const key of Object.keys(declaration)) {
    const lowered = key.toLowerCase();
    for (const forbidden of PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS) {
      if (lowered.includes(forbidden)) {
        throw contractError('argument.invalid', 'core', 'peripheral.generic-decl');
      }
    }
  }
}
