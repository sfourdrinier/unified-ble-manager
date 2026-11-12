// src/backend-contract/errors.ts

import { parseOrderedRecordingFailure } from '../core/ordered-recording'

import type { PortableSerializableRecord, SerializableRecord, SerializableValue } from './primitives'
import { snapshotSerializableRecord, utf8ByteLength, assertSafeSerializablePrototype } from './serializable'

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
/** A lossless ordered platform value. Integer strings use canonical signed i64 decimal. */
export type PlatformMetadataValue =
  | { readonly kind: 'text' | 'int'; readonly value: string }
  | { readonly kind: 'bool'; readonly value: boolean }
export interface PlatformMetadataEntry {
  readonly key: string
  readonly value: PlatformMetadataValue
}

// Bound allocation and encoded size independently; shared with all TypeScript boundaries.
export const MAX_PLATFORM_METADATA_ENTRIES = 1024
export const MAX_PLATFORM_METADATA_TEXT_BYTES = 65536
export const MAX_PLATFORM_METADATA_ENTRIES_BYTES = 1048576

/** Bound the complete encoded failure container, including its surrounding fields. */
export function assertPlatformFailureEnvelopeBound(value: unknown, operation: string): void {
  let encoded: string | undefined
  try {
    encoded = JSON.stringify(value)
  } catch {
    throw contractError('protocol.malformed', 'boundary', operation)
  }
  if (encoded === undefined) throw contractError('protocol.malformed', 'boundary', operation)
  if (
    encoded.length > MAX_PLATFORM_METADATA_ENTRIES_BYTES ||
    utf8ByteLength(encoded) > MAX_PLATFORM_METADATA_ENTRIES_BYTES
  )
    throw contractError('bytes.too-large', 'boundary', operation)
}

/** Validate and own the producer's sequence; never derive it from the compatibility map. */
export function snapshotPlatformMetadataEntries(value: unknown, operation: string): readonly PlatformMetadataEntry[] {
  const bad = (): never => {
    throw contractError('protocol.malformed', 'boundary', operation)
  }
  if (!Array.isArray(value) || value.length > MAX_PLATFORM_METADATA_ENTRIES) return bad()
  let bytes = 2
  const result: PlatformMetadataEntry[] = []
  for (const input of value) {
    if (typeof input !== 'object' || input === null || Array.isArray(input)) return bad()
    assertSafeSerializablePrototype(input, 'boundary', operation)
    if (Object.keys(input).length !== 2 || !Object.hasOwn(input, 'key') || !Object.hasOwn(input, 'value')) return bad()
    const key: unknown = Reflect.get(input, 'key')
    const tagged: unknown = Reflect.get(input, 'value')
    if (
      typeof key !== 'string' ||
      key.length > MAX_PLATFORM_METADATA_TEXT_BYTES ||
      utf8ByteLength(key) > MAX_PLATFORM_METADATA_TEXT_BYTES ||
      typeof tagged !== 'object' ||
      tagged === null ||
      Array.isArray(tagged)
    )
      return bad()
    assertSafeSerializablePrototype(tagged, 'boundary', operation)
    if (Object.keys(tagged).length !== 2 || !Object.hasOwn(tagged, 'kind') || !Object.hasOwn(tagged, 'value'))
      return bad()
    const kind: unknown = Reflect.get(tagged, 'kind')
    const data: unknown = Reflect.get(tagged, 'value')
    let owned: PlatformMetadataValue
    if (kind === 'bool' && typeof data === 'boolean') owned = Object.freeze({ kind, value: data })
    else if (
      kind === 'text' &&
      typeof data === 'string' &&
      data.length <= MAX_PLATFORM_METADATA_TEXT_BYTES &&
      utf8ByteLength(data) <= MAX_PLATFORM_METADATA_TEXT_BYTES
    )
      owned = Object.freeze({ kind, value: data })
    else if (
      kind === 'int' &&
      typeof data === 'string' &&
      data.length <= 20 &&
      /^(0|-?[1-9][0-9]*)$/.test(data) &&
      BigInt(data) >= -9223372036854775808n &&
      BigInt(data) <= 9223372036854775807n
    )
      owned = Object.freeze({ kind, value: data })
    else return bad()
    const entry = Object.freeze({ key, value: owned })
    bytes += utf8ByteLength(JSON.stringify(entry)) + (result.length === 0 ? 0 : 1)
    if (bytes > MAX_PLATFORM_METADATA_ENTRIES_BYTES) return bad()
    result.push(entry)
  }
  return Object.freeze(result)
}

export interface PlatformErrorDetail {
  readonly domain: string
  readonly code: string
  readonly safeMessage: string
  readonly metadata: PortableSerializableRecord
  readonly metadataEntries?: readonly PlatformMetadataEntry[]
}

/** Validated data-only lifecycle detail; omission never invents a native cause. */
export function optionalPlatformErrorDetail(value: unknown, operation: string): PlatformErrorDetail | undefined {
  if (value === undefined || value === null) return undefined
  assertPlatformFailureEnvelopeBound(value, operation)
  if (
    !isPlatformObject(value) ||
    typeof value.domain !== 'string' ||
    value.domain.length === 0 ||
    typeof value.code !== 'string' ||
    value.code.length === 0 ||
    typeof value.safeMessage !== 'string' ||
    !isPlatformRecord(value.metadata)
  ) {
    throw contractError('protocol.malformed', 'boundary', operation)
  }
  return Object.freeze({
    domain: value.domain,
    code: value.code,
    safeMessage: value.safeMessage,
    metadata: snapshotSerializableRecord(value.metadata).value,
    ...(Object.hasOwn(value, 'metadataEntries')
      ? { metadataEntries: snapshotPlatformMetadataEntries(value.metadataEntries, operation) }
      : {})
  })
}

function isPlatformObject(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function isPlatformRecord(value: unknown): value is SerializableRecord {
  return (
    value !== null &&
    typeof value === 'object' &&
    !Array.isArray(value) &&
    !(value instanceof Uint8Array) &&
    serializablePlatformValue(value, new WeakSet())
  )
}

function serializablePlatformValue(value: unknown, active: WeakSet<object>): value is SerializableValue {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return true
  if (typeof value === 'number') return Number.isFinite(value)
  if (typeof value !== 'object') return false
  if (value instanceof Uint8Array) return true
  if (active.has(value)) return false
  active.add(value)
  const valid = Object.values(value).every(entry => serializablePlatformValue(entry, active))
  active.delete(value)
  return valid
}

export function serializePlatformErrorDetail(platform: PlatformErrorDetail): SerializableRecord {
  return snapshotSerializableRecord({
    domain: platform.domain,
    code: platform.code,
    safeMessage: platform.safeMessage,
    metadata: platform.metadata,
    ...(platform.metadataEntries === undefined
      ? {}
      : {
          metadataEntries: snapshotPlatformMetadataEntries(platform.metadataEntries, 'serialize.platform').map(
            entry => ({ key: entry.key, value: { kind: entry.value.kind, value: entry.value.value } })
          )
        })
  }).value
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
  /** Exact native ordered v2 failure, when its metadata revision was negotiated. */
  readonly orderedFailure?: import('../core/ordered-recording').OrderedRecordingFailure
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

/** Validate, own, and preserve the exact negotiated ordered failure evidence. */
export function snapshotOrderedFailure(
  error: Pick<NormalizedBleError, 'code' | 'domain' | 'operation' | 'orderedFailure'>
): import('../core/ordered-recording').OrderedRecordingFailure | undefined {
  if (error.orderedFailure === undefined) return undefined
  const failure = parseOrderedRecordingFailure(error.orderedFailure, 2)
  if (failure.code !== error.code || failure.domain !== error.domain || failure.operation !== error.operation)
    throw contractError('protocol.malformed', 'boundary', 'ordered.failure.identity')
  return failure
}

function orderedFailureWireValue(value: unknown): SerializableValue {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (Array.isArray(value)) return Object.freeze(value.map(orderedFailureWireValue))
  if (typeof value === 'object' && value !== null) {
    const result: Record<string, SerializableValue> = Object.create(null)
    for (const [key, entry] of Object.entries(value)) result[key] = orderedFailureWireValue(entry)
    return Object.freeze(result)
  }
  throw contractError('protocol.malformed', 'boundary', 'ordered.failure.serialize')
}

/** Encode restricted metadata keys as bounded JSON while keeping the exact failure. */
export function orderedFailureWireFields(input: unknown): SerializableRecord {
  const failure = parseOrderedRecordingFailure(input, 2)
  const wire = orderedFailureWireValue(failure)
  function hasRestrictedKey(value: SerializableValue): boolean {
    if (value === null || typeof value !== 'object' || value instanceof Uint8Array) return false
    if (Array.isArray(value)) return value.some(hasRestrictedKey)
    return Object.entries(value).some(
      ([key, entry]) => key === '__proto__' || key === 'constructor' || key === 'prototype' || hasRestrictedKey(entry)
    )
  }
  return hasRestrictedKey(wire) ? { orderedFailureJson: JSON.stringify(wire) } : { orderedFailure: wire }
}

/** Converts a normalized error to the data-only shape used by host boundaries. */
export function serializeNormalizedError(error: NormalizedBleError): SerializableRecord {
  return Object.freeze({
    code: error.code,
    domain: error.domain,
    operation: error.operation,
    retryability: error.retryability,
    ...(error.commit === undefined ? {} : { commit: error.commit }),
    ...(error.orderedFailure === undefined ? {} : orderedFailureWireFields(snapshotOrderedFailure(error))),
    platform: error.platform === null ? null : serializePlatformErrorDetail(error.platform)
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
