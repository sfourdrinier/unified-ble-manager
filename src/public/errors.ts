// src/public/errors.ts — public BleError with PR2 recovery catalog

import type { BleErrorCode, BleErrorDomain, PlatformErrorDetail } from '../backend-contract/errors'
import type { Limitation } from '../backend-contract/capabilities'
import { byteLimit, ownBytes } from '../backend-contract/primitives'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'
import { recoveryForCode } from '../backend-contract/recovery'
import type { BleRecovery } from '../backend-contract/recovery'

function freezeSerializableValue(value: SerializableValue): SerializableValue {
  if (value instanceof Uint8Array) return ownBytes(value, byteLimit(value.byteLength))
  if (Array.isArray(value)) return Object.freeze(value.map(entry => freezeSerializableValue(entry)))
  if (isSerializableRecordValue(value)) return freezeSerializableRecord(value)
  return value
}

function isSerializableRecordValue(value: SerializableValue): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function freezeSerializableRecord(record: SerializableRecord): SerializableRecord {
  const copy: Record<string, SerializableValue> = {}
  for (const [key, value] of Object.entries(record)) {
    copy[key] = freezeSerializableValue(value)
  }
  return Object.freeze(copy)
}

function freezePlatformDetail(platform: PlatformErrorDetail | null): PlatformErrorDetail | null {
  if (platform === null) return null
  return Object.freeze({
    domain: platform.domain,
    code: platform.code,
    safeMessage: platform.safeMessage,
    metadata: freezeSerializableRecord(platform.metadata)
  })
}

/**
 * Public application error. All façade and IPC errors rehydrate to BleError.
 * Recovery catalog is deterministic per code; platform detail is preserved separately.
 */
export class BleError extends Error {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PlatformErrorDetail | null
  readonly limitations: readonly Limitation[]
  readonly recovery: BleRecovery

  constructor(
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: string,
    options: {
      readonly platform?: PlatformErrorDetail | null
      readonly limitations?: readonly Limitation[]
    } = {}
  ) {
    const recovery = recoveryForCode(code, operation)
    const platform = freezePlatformDetail(options.platform ?? null)
    super(`${code}: ${operation}`)
    this.name = 'BleError'
    this.code = code
    this.domain = domain
    this.operation = operation
    this.platform = platform
    this.limitations = Object.freeze((options.limitations ?? []).map(limitation => Object.freeze({ ...limitation })))
    this.recovery = Object.freeze({
      disposition: recovery.disposition,
      actions: Object.freeze(recovery.actions.map(action => Object.freeze(action)))
    })
  }
}

export type { BleErrorCode, BleErrorDomain } from '../backend-contract/errors'
export type { BleRecovery, BleRecoveryDisposition, RecoveryAction } from '../backend-contract/recovery'
