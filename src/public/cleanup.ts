// src/public/cleanup.ts

import type {
  CleanupRecord as BackendCleanupRecord,
  NormalizedBleError as BackendNormalizedBleError,
  PlatformErrorDetail as BackendPlatformErrorDetail,
  BleErrorCode,
  BleErrorDomain
} from '../backend-contract/errors'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'

/** Host-neutral cleanup result used by every application-facing resource. */
export type PublicSerializableValue =
  | boolean
  | number
  | string
  | null
  | Uint8Array
  | readonly PublicSerializableValue[]
  | PublicSerializableRecord

export interface PublicSerializableRecord {
  readonly [key: string]: PublicSerializableValue
}

export interface PublicPlatformErrorDetail {
  readonly domain: string
  readonly code: string
  readonly safeMessage: string
  readonly metadata: PublicSerializableRecord
}

export interface NormalizedBleError {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PublicPlatformErrorDetail | null
  readonly retryability: 'never' | 'caller-decides'
}

export interface CleanupFailure {
  readonly resourceKind: string
  readonly error: NormalizedBleError
}

export interface CleanupRecord {
  readonly state: 'released' | 'release-failed'
  readonly failures: readonly CleanupFailure[]
}

type CleanupLike = BackendCleanupRecord | CleanupRecord
type NormalizedErrorLike = BackendNormalizedBleError | NormalizedBleError
type PlatformErrorLike = BackendPlatformErrorDetail | PublicPlatformErrorDetail
type SerializableRecordLike = SerializableRecord | PublicSerializableRecord
type SerializableValueLike = SerializableValue | PublicSerializableValue

/** Projects an internal cleanup result without exposing backend brands. */
export function toPublicCleanupRecord(record: CleanupLike): CleanupRecord {
  return Object.freeze({
    state: record.state,
    failures: Object.freeze(record.failures.map(failure => toPublicCleanupFailure(failure.resourceKind, failure.error)))
  })
}

function toPublicCleanupFailure(resourceKind: string, error: NormalizedErrorLike): CleanupFailure {
  return Object.freeze({
    resourceKind: String(resourceKind),
    error: toPublicNormalizedError(error)
  })
}

function toPublicNormalizedError(error: NormalizedErrorLike): NormalizedBleError {
  return Object.freeze({
    code: error.code,
    domain: error.domain,
    operation: error.operation,
    platform: toPublicPlatformErrorDetail(error.platform),
    retryability: error.retryability
  })
}

/** Deep-copies platform metadata and removes backend-only byte brands. */
export function toPublicPlatformErrorDetail(
  platform: PlatformErrorLike | null | undefined
): PublicPlatformErrorDetail | null {
  if (platform === null || platform === undefined) return null
  if (
    typeof platform.domain !== 'string' ||
    typeof platform.code !== 'string' ||
    typeof platform.safeMessage !== 'string' ||
    typeof platform.metadata !== 'object' ||
    platform.metadata === null ||
    Array.isArray(platform.metadata) ||
    platform.metadata instanceof Uint8Array
  ) {
    throw new TypeError('platform error detail metadata must be a serializable record')
  }
  return Object.freeze({
    domain: platform.domain,
    code: platform.code,
    safeMessage: platform.safeMessage,
    metadata: toPublicSerializableRecord(platform.metadata)
  })
}

function toPublicSerializableValue(value: SerializableValueLike): PublicSerializableValue {
  if (value === null || typeof value === 'boolean' || typeof value === 'number' || typeof value === 'string') {
    return value
  }
  if (value instanceof Uint8Array) return new Uint8Array(value)
  if (Array.isArray(value)) {
    return Object.freeze(value.map(entry => toPublicSerializableValue(entry)))
  }
  if (!isSerializableRecordLike(value)) {
    throw new TypeError('cleanup metadata contains an unsupported value')
  }
  return toPublicSerializableRecord(value)
}

function isSerializableRecordLike(value: SerializableValueLike): value is SerializableRecordLike {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function toPublicSerializableRecord(record: SerializableRecordLike): PublicSerializableRecord {
  const copy: Record<string, PublicSerializableValue> = {}
  for (const [key, value] of Object.entries(record)) {
    copy[key] = toPublicSerializableValue(value)
  }
  return Object.freeze(copy)
}
