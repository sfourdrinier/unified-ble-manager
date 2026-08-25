// src/public/cleanup.ts

import type {
  CleanupRecord as BackendCleanupRecord,
  NormalizedBleError as BackendNormalizedBleError,
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
  const platform: PublicPlatformErrorDetail | null =
    error.platform === null || error.platform === undefined
      ? null
      : Object.freeze({
          domain: error.platform.domain,
          code: error.platform.code,
          safeMessage: error.platform.safeMessage,
          metadata: toPublicSerializableRecord(error.platform.metadata)
        })
  return Object.freeze({
    code: error.code,
    domain: error.domain,
    operation: error.operation,
    platform,
    retryability: error.retryability
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
