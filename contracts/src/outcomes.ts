// contracts/src/outcomes.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Frozen error identities, terminal outcomes, and recovery dispositions.
// Derived from docs/UNIFIED_SEMANTICS.md §13/§15 and
// src/backend-contract/errors.ts + recovery.ts (read-only reference).
// The code and domain sets below are retained verbatim; recovery keeps the
// 4.x disposition per code with action kinds only (see semantic-map AC-04).

import { freezeTable } from './freeze';

export type BleErrorCode =
  | 'protocol.incompatible'
  | 'protocol.malformed'
  | 'protocol.violation'
  | 'lifecycle.destroyed'
  | 'lifecycle.invalid-state'
  | 'lifecycle.invariant-violation'
  | 'backend.reset'
  | 'adapter.unavailable'
  | 'adapter.powered-off'
  | 'adapter.resetting'
  | 'adapter.selection-required'
  | 'adapter.ambiguous'
  | 'permission.denied'
  | 'permission.restricted'
  | 'permission.not-determined'
  | 'ownership.denied'
  | 'connection.already-owned'
  | 'scan.already-active'
  | 'chooser.busy'
  | 'argument.invalid'
  | 'bytes.invalid'
  | 'bytes.too-large'
  | 'scan.start-failed'
  | 'scan.stop-failed'
  | 'scan.filter-invalid'
  | 'chooser.cancelled'
  | 'chooser.closed'
  | 'chooser.user-activation-required'
  | 'chooser.insecure-context'
  | 'chooser.api-unavailable'
  | 'chooser.optional-service-not-granted'
  | 'chooser.permitted-device-unavailable'
  | 'connection.not-found'
  | 'connection.failed'
  | 'connection.stale'
  | 'connection.lost'
  | 'peer.reference-invalid'
  | 'peer.reference-version-unsupported'
  | 'peer.scope-mismatch'
  | 'peer.not-found'
  | 'operation.aborted'
  | 'operation.timed-out'
  | 'operation.disconnected'
  | 'operation.cancelled-by-destroy'
  | 'operation.reset'
  | 'operation.adapter-unavailable'
  | 'gatt.discovery-required'
  | 'gatt.ambiguous-path'
  | 'gatt.stale-handle'
  | 'gatt.cache-unknown'
  | 'gatt.not-found'
  | 'gatt.property-not-supported'
  | 'gatt.read-failed'
  | 'gatt.write-failed'
  | 'gatt.subscribe-failed'
  | 'gatt.cccd-managed'
  | 'stream.overflow'
  | 'stream.closed'
  | 'stream.quota'
  | 'stream.rate-limited'
  | 'capability.unsupported'
  | 'capability.unavailable'
  | 'capability.limited'
  | 'background.terminated'
  | 'platform.failure'
  | 'platform.security'
  | 'platform.transport';

export const ERROR_CODE_LIST: readonly BleErrorCode[] = freezeTable([
  'protocol.incompatible',
  'protocol.malformed',
  'protocol.violation',
  'lifecycle.destroyed',
  'lifecycle.invalid-state',
  'lifecycle.invariant-violation',
  'backend.reset',
  'adapter.unavailable',
  'adapter.powered-off',
  'adapter.resetting',
  'adapter.selection-required',
  'adapter.ambiguous',
  'permission.denied',
  'permission.restricted',
  'permission.not-determined',
  'ownership.denied',
  'connection.already-owned',
  'scan.already-active',
  'chooser.busy',
  'argument.invalid',
  'bytes.invalid',
  'bytes.too-large',
  'scan.start-failed',
  'scan.stop-failed',
  'scan.filter-invalid',
  'chooser.cancelled',
  'chooser.closed',
  'chooser.user-activation-required',
  'chooser.insecure-context',
  'chooser.api-unavailable',
  'chooser.optional-service-not-granted',
  'chooser.permitted-device-unavailable',
  'connection.not-found',
  'connection.failed',
  'connection.stale',
  'connection.lost',
  'peer.reference-invalid',
  'peer.reference-version-unsupported',
  'peer.scope-mismatch',
  'peer.not-found',
  'operation.aborted',
  'operation.timed-out',
  'operation.disconnected',
  'operation.cancelled-by-destroy',
  'operation.reset',
  'operation.adapter-unavailable',
  'gatt.discovery-required',
  'gatt.ambiguous-path',
  'gatt.stale-handle',
  'gatt.cache-unknown',
  'gatt.not-found',
  'gatt.property-not-supported',
  'gatt.read-failed',
  'gatt.write-failed',
  'gatt.subscribe-failed',
  'gatt.cccd-managed',
  'stream.overflow',
  'stream.closed',
  'stream.quota',
  'stream.rate-limited',
  'capability.unsupported',
  'capability.unavailable',
  'capability.limited',
  'background.terminated',
  'platform.failure',
  'platform.security',
  'platform.transport',
] satisfies readonly BleErrorCode[]);

export function isBleErrorCode(value: unknown): value is BleErrorCode {
  return typeof value === 'string' && ERROR_CODE_LIST.some(code => code === value);
}

// C-UBM 0.1.2 additive amendment (trackourhealth/bun-mono#1188, ledger
// follow-up #9): frozen SIG payload-codec identities. Derived from
// src/profiles/errors.ts `ProfileCodecErrorCode` (read-only reference) with
// byte-identical wire strings. These standards-level payload failures are
// distinct from transport failures: they carry no `BleErrorDomain` and no
// recovery disposition, so they live in their own frozen table. The 67-code
// `BleErrorCode` oracle catalog above is unchanged (wire-compatible,
// additive-only).
export type ProfileCodecErrorCode =
  | 'profile.codec.truncated'
  | 'profile.codec.malformed'
  | 'profile.codec.reserved'
  | 'profile.codec.invalid-value';

export const PROFILE_CODEC_ERROR_CODES: readonly ProfileCodecErrorCode[] = freezeTable([
  'profile.codec.truncated',
  'profile.codec.malformed',
  'profile.codec.reserved',
  'profile.codec.invalid-value',
] satisfies readonly ProfileCodecErrorCode[]);

export function isProfileCodecErrorCode(value: unknown): value is ProfileCodecErrorCode {
  return typeof value === 'string' && PROFILE_CODEC_ERROR_CODES.some(code => code === value);
}

export type BleErrorDomain =
  | 'core'
  | 'adapter'
  | 'scan'
  | 'chooser'
  | 'connection'
  | 'gatt'
  | 'stream'
  | 'capability'
  | 'boundary'
  | 'cleanup'
  | 'restoration'
  | 'ipc'
  | 'platform';

export const ERROR_DOMAIN_LIST: readonly BleErrorDomain[] = freezeTable([
  'core',
  'adapter',
  'scan',
  'chooser',
  'connection',
  'gatt',
  'stream',
  'capability',
  'boundary',
  'cleanup',
  'restoration',
  'ipc',
  'platform',
] satisfies readonly BleErrorDomain[]);

export function isBleErrorDomain(value: unknown): value is BleErrorDomain {
  return typeof value === 'string' && ERROR_DOMAIN_LIST.some(domain => domain === value);
}

export type Recoverability = 'never' | 'caller-decides';

export type RecoveryDisposition =
  | 'none'
  | 'retry-immediately'
  | 'retry-with-backoff'
  | 'after-state-change'
  | 'after-user-action'
  | 'caller-policy';

export type RecoveryActionKind =
  | 'request-permission'
  | 'open-settings'
  | 'wait-for-adapter'
  | 'rescan'
  | 'reselect-peer'
  | 'reconnect'
  | 'rediscover-gatt'
  | 'select-gatt-occurrence'
  | 'pair'
  | 'repair'
  | 'reduce-payload'
  | 'wait-for-write-ready'
  | 'recreate-manager'
  | 'retry';

export interface Recovery {
  readonly disposition: RecoveryDisposition;
  readonly actions: readonly RecoveryActionKind[];
}

export function recoveryFor(code: BleErrorCode): Recovery {
  switch (code) {
    case 'protocol.incompatible':
    case 'protocol.malformed':
    case 'protocol.violation':
    case 'lifecycle.invariant-violation':
    case 'argument.invalid':
    case 'bytes.invalid':
    case 'ownership.denied':
    case 'connection.already-owned':
    case 'scan.already-active':
    case 'chooser.busy':
    case 'peer.reference-invalid':
    case 'peer.reference-version-unsupported':
    case 'peer.scope-mismatch':
    case 'gatt.not-found':
    case 'gatt.property-not-supported':
    case 'gatt.read-failed':
    case 'gatt.write-failed':
    case 'gatt.subscribe-failed':
    case 'capability.unsupported':
    case 'capability.unavailable':
    case 'capability.limited':
      return { disposition: 'none', actions: [] };
    case 'bytes.too-large':
      return { disposition: 'none', actions: ['reduce-payload'] };
    case 'lifecycle.destroyed':
    case 'lifecycle.invalid-state':
    case 'backend.reset':
    case 'operation.cancelled-by-destroy':
      return { disposition: 'none', actions: ['recreate-manager'] };
    case 'adapter.unavailable':
    case 'adapter.resetting':
    case 'adapter.ambiguous':
    case 'operation.adapter-unavailable':
      return { disposition: 'after-state-change', actions: ['wait-for-adapter'] };
    case 'adapter.powered-off':
      return { disposition: 'after-state-change', actions: ['wait-for-adapter'] };
    case 'adapter.selection-required':
      return { disposition: 'after-user-action', actions: ['reselect-peer'] };
    case 'permission.denied':
      return { disposition: 'after-user-action', actions: ['request-permission', 'open-settings'] };
    case 'permission.restricted':
      return { disposition: 'after-user-action', actions: ['open-settings'] };
    case 'permission.not-determined':
      return { disposition: 'after-user-action', actions: ['request-permission'] };
    case 'scan.start-failed':
    case 'scan.stop-failed':
    case 'scan.filter-invalid':
      return { disposition: 'none', actions: ['rescan'] };
    case 'chooser.cancelled':
    case 'chooser.closed':
    case 'chooser.user-activation-required':
    case 'chooser.insecure-context':
    case 'chooser.api-unavailable':
    case 'chooser.optional-service-not-granted':
    case 'chooser.permitted-device-unavailable':
      return { disposition: 'after-user-action', actions: ['reselect-peer'] };
    case 'connection.not-found':
    case 'connection.failed':
    case 'connection.stale':
    case 'connection.lost':
    case 'operation.disconnected':
    case 'operation.reset':
      return { disposition: 'retry-with-backoff', actions: ['reconnect'] };
    case 'peer.not-found':
      return { disposition: 'caller-policy', actions: ['rescan'] };
    case 'gatt.discovery-required':
    case 'gatt.stale-handle':
    case 'gatt.cache-unknown':
      return { disposition: 'retry-immediately', actions: ['rediscover-gatt'] };
    case 'gatt.ambiguous-path':
      return { disposition: 'caller-policy', actions: ['select-gatt-occurrence'] };
    case 'gatt.cccd-managed':
      return { disposition: 'none', actions: ['wait-for-write-ready'] };
    case 'stream.overflow':
    case 'stream.closed':
    case 'stream.quota':
    case 'stream.rate-limited':
      return { disposition: 'retry-with-backoff', actions: ['retry'] };
    case 'background.terminated':
      return { disposition: 'after-state-change', actions: ['reconnect'] };
    case 'platform.failure':
    case 'platform.transport':
      return { disposition: 'caller-policy', actions: [] };
    case 'platform.security':
      return { disposition: 'after-user-action', actions: ['pair', 'repair'] };
    case 'operation.aborted':
    case 'operation.timed-out':
      return { disposition: 'caller-policy', actions: ['retry'] };
  }
}

export interface PlatformDetail {
  readonly domain: string;
  readonly code: string;
  readonly safeMessage: string;
}

// Platform detail carries only safe fields. Raw addresses, peer names,
// advertisement bytes, GATT values, secrets, and platform messages with
// client ownership data MUST NOT be placed in safeMessage.
export function makePlatformDetail(input: {
  readonly domain: unknown;
  readonly code: unknown;
  readonly safeMessage: unknown;
}): PlatformDetail {
  if (typeof input.domain !== 'string' || input.domain.length === 0) {
    throw contractError('argument.invalid', 'core', 'platform-detail.domain');
  }
  if (typeof input.code !== 'string' || input.code.length === 0) {
    throw contractError('argument.invalid', 'core', 'platform-detail.code');
  }
  if (typeof input.safeMessage !== 'string' || input.safeMessage.length === 0) {
    throw contractError('argument.invalid', 'core', 'platform-detail.safe-message');
  }
  return Object.freeze({ domain: input.domain, code: input.code, safeMessage: input.safeMessage });
}

export class ContractError extends Error {
  readonly code: BleErrorCode;
  readonly domain: BleErrorDomain;
  readonly operation: string;
  readonly recoverability: Recoverability;

  constructor(code: BleErrorCode, domain: BleErrorDomain, operation: string) {
    super(`${code} [${domain}] ${operation}`);
    this.name = 'ContractError';
    this.code = code;
    this.domain = domain;
    this.operation = operation;
    this.recoverability = 'caller-decides';
  }
}

export function contractError(
  code: BleErrorCode,
  domain: BleErrorDomain,
  operation: string,
): ContractError {
  if (typeof operation !== 'string' || operation.length === 0) {
    throw new ContractError('argument.invalid', 'core', 'contract-error.operation');
  }
  return new ContractError(code, domain, operation);
}

export function isContractError(value: unknown): value is ContractError {
  return (
    value instanceof ContractError &&
    isBleErrorCode(value.code) &&
    isBleErrorDomain(value.domain)
  );
}

export type OperationTerminalKind =
  | 'succeeded'
  | 'failed'
  | 'aborted'
  | 'timed-out'
  | 'disconnected'
  | 'reset'
  | 'adapter-unavailable'
  | 'destroyed';

export const OPERATION_TERMINAL_KINDS: readonly OperationTerminalKind[] = freezeTable([
  'succeeded',
  'failed',
  'aborted',
  'timed-out',
  'disconnected',
  'reset',
  'adapter-unavailable',
  'destroyed',
] satisfies readonly OperationTerminalKind[]);

export interface TerminalRecord {
  readonly operationId: string;
  readonly kind: OperationTerminalKind;
  readonly cause: BleErrorCode | null;
  readonly ingressOrdinal: number;
  readonly startedAt: number;
  readonly settledAt: number;
}

// Exactly one terminal outcome per operation. A succeeded operation carries
// no cause; every other terminal carries exactly one cause code.
export function makeTerminalRecord(input: {
  readonly operationId: unknown;
  readonly kind: unknown;
  readonly cause: unknown;
  readonly ingressOrdinal: unknown;
  readonly startedAt: unknown;
  readonly settledAt: unknown;
}): TerminalRecord {
  if (typeof input.operationId !== 'string' || input.operationId.length === 0) {
    throw contractError('argument.invalid', 'core', 'terminal-record.operation-id');
  }
  if (
    typeof input.kind !== 'string' ||
    !OPERATION_TERMINAL_KINDS.some(kind => kind === input.kind)
  ) {
    throw contractError('argument.invalid', 'core', 'terminal-record.kind');
  }
  const resolved = asTerminalKind(input.kind);
  if (resolved === 'succeeded' && input.cause !== null) {
    throw contractError('argument.invalid', 'core', 'terminal-record.cause');
  }
  let cause: BleErrorCode | null = null;
  if (resolved !== 'succeeded') {
    if (!isBleErrorCode(input.cause)) {
      throw contractError('argument.invalid', 'core', 'terminal-record.cause');
    }
    cause = input.cause;
  }
  if (
    typeof input.ingressOrdinal !== 'number' ||
    !Number.isSafeInteger(input.ingressOrdinal) ||
    input.ingressOrdinal < 0
  ) {
    throw contractError('argument.invalid', 'core', 'terminal-record.ingress-ordinal');
  }
  if (
    typeof input.startedAt !== 'number' ||
    !Number.isFinite(input.startedAt) ||
    input.startedAt < 0 ||
    typeof input.settledAt !== 'number' ||
    !Number.isFinite(input.settledAt) ||
    input.settledAt < 0 ||
    input.settledAt < input.startedAt
  ) {
    throw contractError('argument.invalid', 'core', 'terminal-record.timing');
  }
  return Object.freeze({
    operationId: input.operationId,
    kind: resolved,
    cause,
    ingressOrdinal: input.ingressOrdinal,
    startedAt: input.startedAt,
    settledAt: input.settledAt,
  });
}

function asTerminalKind(value: unknown): OperationTerminalKind {
  if (typeof value === 'string') {
    for (const kind of OPERATION_TERMINAL_KINDS) {
      if (kind === value) {
        return kind;
      }
    }
  }
  throw contractError('argument.invalid', 'core', 'terminal-record.kind');
}
