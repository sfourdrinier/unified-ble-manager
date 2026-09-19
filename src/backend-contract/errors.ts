// src/backend-contract/errors.ts

import type { SerializableRecord } from './primitives'

export const BLE_ERROR_CODES = Object.freeze([
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
  'platform.transport'
] as const)
export type BleErrorCode = (typeof BLE_ERROR_CODES)[number]
export const BLE_ERROR_DOMAINS = Object.freeze([
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
  'platform'
] as const)
export type BleErrorDomain = (typeof BLE_ERROR_DOMAINS)[number]
export interface PlatformErrorDetail {
  readonly domain: string
  readonly code: string
  readonly safeMessage: string
  readonly metadata: SerializableRecord
}
/**
 * Whether the operation that failed may be repeated. `never` includes an
 * operation that was dispatched and may already have committed at the
 * peripheral; `caller-decides` means nothing was committed, so repeating it is
 * the caller's policy.
 */
export type BleRetryability = 'never' | 'caller-decides'

export const BLE_RETRYABILITIES: readonly BleRetryability[] = Object.freeze(['never', 'caller-decides'])

/**
 * What the operation's owner knows about whether a failed operation took
 * effect: `not-dispatched` means nothing reached the radio; `uncertain` means
 * it was dispatched and may already have committed at the peripheral, so the
 * caller should read the peer's state before deciding anything. The same words
 * cross the mobile wire.
 */
export type BleCommitUncertainty = 'not-dispatched' | 'uncertain'

export const BLE_COMMIT_UNCERTAINTIES: readonly BleCommitUncertainty[] = Object.freeze(['not-dispatched', 'uncertain'])

export interface NormalizedBleError {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PlatformErrorDetail | null
  readonly retryability: BleRetryability
  /**
   * The commit state the operation's owner reported, when it reported one;
   * `null` when it said it does not know. Absent from errors whose owner
   * does not state it.
   */
  readonly commit?: BleCommitUncertainty | null
}
export interface CleanupFailure {
  readonly resourceKind: string
  readonly error: NormalizedBleError
}
export interface CleanupRecord {
  readonly state: 'released' | 'release-failed'
  readonly failures: readonly CleanupFailure[]
}

/** Converts a normalized error to the data-only shape used by host boundaries. */
export function serializeNormalizedError(error: NormalizedBleError): SerializableRecord {
  return Object.freeze({
    code: error.code,
    domain: error.domain,
    operation: error.operation,
    retryability: error.retryability,
    ...(error.commit === undefined ? {} : { commit: error.commit }),
    platform:
      error.platform === null
        ? null
        : Object.freeze({
            domain: error.platform.domain,
            code: error.platform.code,
            safeMessage: error.platform.safeMessage,
            metadata: error.platform.metadata
          })
  })
}

export class BackendContractError extends Error {
  readonly normalized: NormalizedBleError
  constructor(normalized: NormalizedBleError) {
    super(`${normalized.code}: ${normalized.operation}`)
    this.name = 'BackendContractError'
    this.normalized = normalized
  }
}
/**
 * Builds a normalized error whose retryability is derived from its code:
 * `operation.aborted`, `operation.timed-out` and `stream.overflow` are
 * `caller-decides`, every other code is `never`. That derivation is only true
 * when the operation had no effect — it was never dispatched, or it commits
 * nothing (a read, or an observation stream that only drops what it saw). An
 * operation that was dispatched and may commit at the peripheral (a write)
 * must be reported with {@link commitUncertainError} instead.
 */
export function contractError(
  code: BleErrorCode,
  domain: BleErrorDomain,
  operation: string,
  platform: PlatformErrorDetail | null = null
): BackendContractError {
  if (operation.length === 0) {
    throw new Error('operation must be non-empty')
  }
  return new BackendContractError({
    code,
    domain,
    operation,
    platform,
    retryability: retryabilityForCode(code)
  })
}

/**
 * The retryability an error has when the operation that produced it reported
 * none of its own: `caller-decides` for `operation.aborted`,
 * `operation.timed-out` and `stream.overflow`, `never` for every other code.
 * An overflow drops observations without committing anything at the
 * peripheral, so repeating the scan or subscription is the caller's policy —
 * matching the recovery catalog, which already advises retry with backoff.
 * It is the default for an operation that had no effect, never a replacement
 * for the operation's answer.
 */
export function retryabilityForCode(code: BleErrorCode): BleRetryability {
  return code === 'operation.aborted' || code === 'operation.timed-out' || code === 'stream.overflow'
    ? 'caller-decides'
    : 'never'
}

/**
 * Builds the error for an operation that was dispatched and may already have
 * committed at the peripheral, such as a write that was aborted or timed out
 * after it reached the radio. The commit is uncertain, so the error is never
 * retryable: repeating the operation could apply its effect twice.
 */
export function commitUncertainError(
  code: BleErrorCode,
  domain: BleErrorDomain,
  operation: string,
  platform: PlatformErrorDetail | null = null
): BackendContractError {
  return new BackendContractError({
    ...contractError(code, domain, operation, platform).normalized,
    retryability: 'never',
    commit: 'uncertain'
  })
}
