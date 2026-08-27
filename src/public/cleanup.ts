// src/public/cleanup.ts

import type {
  CleanupRecord as BackendCleanupRecord,
  NormalizedBleError as BackendNormalizedBleError,
  PlatformErrorDetail as BackendPlatformErrorDetail,
  BleErrorCode,
  BleErrorDomain
} from '../backend-contract/errors'
import { BackendContractError, BLE_ERROR_CODES, BLE_ERROR_DOMAINS, contractError } from '../backend-contract/errors'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'
import {
  assertAllowedSerializableKey,
  assertSafeSerializablePrototype,
  createOwnedSerializableRecord,
  setOwnedSerializableEntry
} from '../backend-contract/serializable'

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

const cleanupSnapshots = new WeakMap<object, CleanupRecord>()

/** Projects an internal cleanup result without exposing backend brands. */
export function toPublicCleanupRecord(record: CleanupLike): CleanupRecord {
  try {
    if (typeof record !== 'object' || record === null || Array.isArray(record)) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.record')
    }
    assertSafeSerializablePrototype(record, 'boundary', 'public-cleanup.record')
    const existing = cleanupSnapshots.get(record)
    if (existing !== undefined) return existing
    if (record.state !== 'released' && record.state !== 'release-failed') {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.state')
    }
    if (!Array.isArray(record.failures)) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.failures')
    }
    if (record.state === 'released' && record.failures.length !== 0) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.released-failures')
    }
    if (record.state === 'release-failed' && record.failures.length === 0) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.release-failed-failures')
    }
    const snapshot = Object.freeze({
      state: record.state,
      failures: Object.freeze(
        record.failures.map(failure => toPublicCleanupFailure(failure.resourceKind, failure.error))
      )
    })
    cleanupSnapshots.set(record, snapshot)
    return snapshot
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.record')
  }
}

function toPublicCleanupFailure(resourceKind: string, error: NormalizedErrorLike): CleanupFailure {
  if (
    typeof resourceKind !== 'string' ||
    resourceKind.length === 0 ||
    typeof error !== 'object' ||
    error === null ||
    !Object.prototype.hasOwnProperty.call(error, 'code') ||
    !Object.prototype.hasOwnProperty.call(error, 'domain') ||
    !Object.prototype.hasOwnProperty.call(error, 'operation') ||
    !Object.prototype.hasOwnProperty.call(error, 'platform') ||
    !Object.prototype.hasOwnProperty.call(error, 'retryability')
  ) {
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.failure')
  }
  assertSafeSerializablePrototype(error, 'boundary', 'public-cleanup.error')
  return Object.freeze({
    resourceKind,
    error: toPublicNormalizedError(error)
  })
}

export function toPublicNormalizedError(error: NormalizedErrorLike): NormalizedBleError {
  if (
    !BLE_ERROR_CODES.some(candidate => candidate === error.code) ||
    !BLE_ERROR_DOMAINS.some(candidate => candidate === error.domain) ||
    typeof error.operation !== 'string' ||
    error.operation.length === 0 ||
    (error.retryability !== 'never' && error.retryability !== 'caller-decides') ||
    (error.platform !== null &&
      (typeof error.platform !== 'object' || error.platform === null || Array.isArray(error.platform)))
  ) {
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.error')
  }
  if (error.platform !== null) {
    assertSafeSerializablePrototype(error.platform, 'boundary', 'public-cleanup.error.platform')
  }
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
  try {
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
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.record')
    }
    return Object.freeze({
      domain: platform.domain,
      code: platform.code,
      safeMessage: platform.safeMessage,
      metadata: toPublicSerializableRecord(platform.metadata)
    })
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.record')
  }
}

function isSerializableRecordLike(value: SerializableValueLike): value is SerializableRecordLike {
  return (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    !(value instanceof Uint8Array) &&
    isSafeRecordPrototype(value)
  )
}

function isSafeRecordPrototype(value: object): boolean {
  try {
    const prototype = Object.getPrototypeOf(value)
    return prototype === Object.prototype || prototype === null
  } catch {
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.prototype')
  }
}

function toPublicSerializableRecord(record: SerializableRecordLike): PublicSerializableRecord {
  try {
    assertSafeSerializablePrototype(record, 'boundary', 'public-cleanup.metadata.prototype')
    const activeObjects = new WeakSet<object>()
    return snapshotPublicRecord(record, activeObjects)
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.record')
  }
}

function snapshotPublicValue(value: SerializableValueLike, activeObjects: WeakSet<object>): PublicSerializableValue {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return value
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.number')
    }
    return value
  }
  if (value instanceof Uint8Array) return new Uint8Array(value)
  if (Array.isArray(value)) {
    if (activeObjects.has(value)) {
      throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.cycle')
    }
    activeObjects.add(value)
    try {
      return Object.freeze(value.map(entry => snapshotPublicValue(entry, activeObjects)))
    } finally {
      activeObjects.delete(value)
    }
  }
  if (!isSerializableRecordLike(value)) {
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.prototype')
  }
  return snapshotPublicRecord(value, activeObjects)
}

function snapshotPublicRecord(
  record: SerializableRecordLike,
  activeObjects: WeakSet<object>
): PublicSerializableRecord {
  if (activeObjects.has(record)) {
    throw contractError('protocol.malformed', 'boundary', 'public-cleanup.metadata.cycle')
  }
  activeObjects.add(record)
  try {
    const copy = createOwnedSerializableRecord<PublicSerializableValue>()
    for (const [key, value] of Object.entries(record)) {
      assertAllowedSerializableKey(key, 'boundary', 'public-cleanup.metadata.key')
      setOwnedSerializableEntry(
        copy,
        key,
        snapshotPublicValue(value, activeObjects),
        'boundary',
        'public-cleanup.metadata.key'
      )
    }
    return Object.freeze(copy)
  } finally {
    activeObjects.delete(record)
  }
}
