// contracts/src/identities.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Host/client/adapter/peer/handle identities and generations. Derived from
// docs/UNIFIED_SEMANTICS.md §1/§2/§9 and src/backend-contract/primitives.ts.
// Approved correction AC-02: identities are structural records with validated
// fields, not string-brand intersections, so the frozen wire form carries its
// scope explicitly and needs no type-system assertion to construct.

import { freezeTable } from './freeze';
import { contractError } from './outcomes';

export function isNonEmptyId(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0;
}

function requireId(value: unknown, path: string): string {
  if (!isNonEmptyId(value)) {
    throw contractError('argument.invalid', 'core', path);
  }
  return value;
}

export interface AttachmentTuple {
  readonly attachmentId: string;
  readonly backendInstanceId: string;
  readonly backendGeneration: string;
  readonly adapterId: string;
  readonly adapterGeneration: string;
}

// The unrepeatable tuple that scopes all handles and correlations. A handle
// from backend instance A is stale at instance B even when visible ids repeat.
export function createAttachmentTuple(input: {
  readonly attachmentId: unknown;
  readonly backendInstanceId: unknown;
  readonly backendGeneration: unknown;
  readonly adapterId: unknown;
  readonly adapterGeneration: unknown;
}): AttachmentTuple {
  return Object.freeze({
    attachmentId: requireId(input.attachmentId, 'attachment.attachment-id'),
    backendInstanceId: requireId(input.backendInstanceId, 'attachment.backend-instance-id'),
    backendGeneration: requireId(input.backendGeneration, 'attachment.backend-generation'),
    adapterId: requireId(input.adapterId, 'attachment.adapter-id'),
    adapterGeneration: requireId(input.adapterGeneration, 'attachment.adapter-generation'),
  });
}

export function attachmentTuplesEqual(left: AttachmentTuple, right: AttachmentTuple): boolean {
  return (
    left.attachmentId === right.attachmentId &&
    left.backendInstanceId === right.backendInstanceId &&
    left.backendGeneration === right.backendGeneration &&
    left.adapterId === right.adapterId &&
    left.adapterGeneration === right.adapterGeneration
  );
}

// OWN-02: a foreign or stale handle is rejected before any radio effect.
export function assertSameAttachment(
  handleAttachment: AttachmentTuple,
  currentAttachment: AttachmentTuple,
  operation: string,
): void {
  if (!attachmentTuplesEqual(handleAttachment, currentAttachment)) {
    throw contractError('connection.stale', 'connection', operation);
  }
}

export type PeerIdentityDomain =
  | 'public-address'
  | 'static-random-address'
  | 'resolvable-private-address'
  | 'platform-guid'
  | 'opaque-token';

export const PEER_IDENTITY_DOMAINS: readonly PeerIdentityDomain[] = freezeTable([
  'public-address',
  'static-random-address',
  'resolvable-private-address',
  'platform-guid',
  'opaque-token',
] satisfies readonly PeerIdentityDomain[]);

export interface PeerIdentity {
  readonly attachment: AttachmentTuple;
  readonly domain: PeerIdentityDomain;
  readonly value: string;
}

export function createPeerIdentity(
  attachment: AttachmentTuple,
  domain: unknown,
  value: unknown,
): PeerIdentity {
  if (typeof domain !== 'string' || !PEER_IDENTITY_DOMAINS.some(candidate => candidate === domain)) {
    throw contractError('argument.invalid', 'core', 'peer-identity.domain');
  }
  let resolved: PeerIdentityDomain;
  switch (domain) {
    case 'public-address':
      resolved = 'public-address';
      break;
    case 'static-random-address':
      resolved = 'static-random-address';
      break;
    case 'resolvable-private-address':
      resolved = 'resolvable-private-address';
      break;
    case 'platform-guid':
      resolved = 'platform-guid';
      break;
    case 'opaque-token':
      resolved = 'opaque-token';
      break;
    default:
      throw contractError('argument.invalid', 'core', 'peer-identity.domain');
  }
  return Object.freeze({ attachment, domain: resolved, value: requireId(value, 'peer-identity.value') });
}

// Session-scoped peer key for first/merged/latest delivery. Where privacy
// rotation prevents a stable key, each unlinked identity stays distinct.
export function peerSessionKey(peer: PeerIdentity): string {
  return `${peer.domain}:${peer.value}`;
}

// Address-like values are merely one possible domain and MUST NOT be treated
// as globally stable.
export function isGloballyStableDomain(domain: PeerIdentityDomain): boolean {
  return domain === 'public-address' || domain === 'static-random-address';
}

export interface GattPath {
  readonly attachment: AttachmentTuple;
  readonly peer: PeerIdentity;
  readonly connectionGeneration: string;
  readonly databaseGeneration: string;
  readonly serviceUuid: string;
  readonly serviceOccurrence: number;
  readonly characteristicUuid: string | null;
  readonly characteristicOccurrence: number | null;
  readonly descriptorUuid: string | null;
  readonly descriptorOccurrence: number | null;
  readonly ownerLease: string;
}

function requireOccurrence(value: unknown, path: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw contractError('argument.invalid', 'core', path);
  }
  return value;
}

// Occurrence selects among duplicate UUIDs; UUID-only selection is prohibited.
// A characteristic UUID requires its occurrence and vice versa; the same
// pairing holds for descriptors. A descriptor requires a characteristic, and
// the path attachment must equal the peer attachment scope.
export function createGattPath(input: {
  readonly attachment: AttachmentTuple;
  readonly peer: PeerIdentity;
  readonly connectionGeneration: unknown;
  readonly databaseGeneration: unknown;
  readonly serviceUuid: unknown;
  readonly serviceOccurrence: unknown;
  readonly characteristicUuid?: unknown;
  readonly characteristicOccurrence?: unknown;
  readonly descriptorUuid?: unknown;
  readonly descriptorOccurrence?: unknown;
  readonly ownerLease: unknown;
}): GattPath {
  const characteristicUuid =
    input.characteristicUuid === undefined || input.characteristicUuid === null
      ? null
      : canonicalUuidValue(input.characteristicUuid);
  const descriptorUuid =
    input.descriptorUuid === undefined || input.descriptorUuid === null
      ? null
      : canonicalUuidValue(input.descriptorUuid);
  const characteristicOccurrence =
    input.characteristicOccurrence === undefined || input.characteristicOccurrence === null
      ? null
      : requireOccurrence(input.characteristicOccurrence, 'gatt-path.characteristic-occurrence');
  const descriptorOccurrence =
    input.descriptorOccurrence === undefined || input.descriptorOccurrence === null
      ? null
      : requireOccurrence(input.descriptorOccurrence, 'gatt-path.descriptor-occurrence');
  if ((characteristicUuid === null) !== (characteristicOccurrence === null)) {
    throw contractError('argument.invalid', 'core', 'gatt-path.characteristic-pairing');
  }
  if ((descriptorUuid === null) !== (descriptorOccurrence === null)) {
    throw contractError('argument.invalid', 'core', 'gatt-path.descriptor-pairing');
  }
  if (descriptorUuid !== null && characteristicUuid === null) {
    throw contractError('argument.invalid', 'core', 'gatt-path.descriptor-without-characteristic');
  }
  if (!attachmentTuplesEqual(input.attachment, input.peer.attachment)) {
    throw contractError('peer.scope-mismatch', 'connection', 'gatt-path.peer-scope');
  }
  return Object.freeze({
    attachment: input.attachment,
    peer: input.peer,
    connectionGeneration: requireId(input.connectionGeneration, 'gatt-path.connection-generation'),
    databaseGeneration: requireId(input.databaseGeneration, 'gatt-path.database-generation'),
    serviceUuid: canonicalUuidValue(input.serviceUuid),
    serviceOccurrence: requireOccurrence(input.serviceOccurrence, 'gatt-path.service-occurrence'),
    characteristicUuid,
    characteristicOccurrence,
    descriptorUuid,
    descriptorOccurrence,
    ownerLease: requireId(input.ownerLease, 'gatt-path.owner-lease'),
  });
}

export interface HandleRef {
  readonly attachment: AttachmentTuple;
  readonly peer: PeerIdentity;
  readonly connectionGeneration: string;
  readonly databaseGeneration: string;
  readonly path: GattPath;
  readonly ownerLease: string;
}

export function createHandleRef(path: GattPath, ownerLease: unknown): HandleRef {
  const lease = requireId(ownerLease, 'handle.owner-lease');
  if (lease !== path.ownerLease) {
    throw contractError('ownership.denied', 'core', 'handle.owner-lease');
  }
  return Object.freeze({
    attachment: path.attachment,
    peer: path.peer,
    connectionGeneration: path.connectionGeneration,
    databaseGeneration: path.databaseGeneration,
    path,
    ownerLease: lease,
  });
}

// UUID comparison canonicalizes 16/32/128-bit input to lowercase 128-bit
// Bluetooth-base or vendor form before matching, indexing, or serialization.
export function canonicalUuidValue(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw contractError('argument.invalid', 'core', 'uuid.input');
  }
  const compact = value.replace(/-/g, '').toLowerCase();
  if (!/^[0-9a-f]+$/.test(compact)) {
    throw contractError('argument.invalid', 'core', 'uuid.digits');
  }
  if (compact.length === 4) {
    return `0000${compact}-0000-1000-8000-00805f9b34fb`;
  }
  if (compact.length === 8) {
    return `${compact}-0000-1000-8000-00805f9b34fb`;
  }
  if (compact.length !== 32) {
    throw contractError('argument.invalid', 'core', 'uuid.length');
  }
  const head = compact.slice(0, 8);
  const a = compact.slice(8, 12);
  const b = compact.slice(12, 16);
  const c = compact.slice(16, 20);
  const tail = compact.slice(20);
  return `${head}-${a}-${b}-${c}-${tail}`;
}

export function canonicalBleAddressValue(value: unknown): string {
  if (typeof value !== 'string' || !/^[0-9A-Fa-f]{2}([:-][0-9A-Fa-f]{2}){5}$/.test(value)) {
    throw contractError('argument.invalid', 'core', 'ble-address.input');
  }
  return value.replace(/-/g, ':').toUpperCase();
}

// Generations are opaque and unrepeatable: only the current generation is
// usable, and a stale object is never revived.
export function isGenerationCurrent(used: string, current: string): boolean {
  return used === current;
}

export function assertCurrentGeneration(used: string, current: string, operation: string): void {
  if (used !== current) {
    throw contractError('connection.stale', 'connection', operation);
  }
}
