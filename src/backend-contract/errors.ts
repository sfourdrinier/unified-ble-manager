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
export interface NormalizedBleError {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PlatformErrorDetail | null
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

/** Converts a normalized error to the data-only shape used by host boundaries. */
export function serializeNormalizedError(error: NormalizedBleError): SerializableRecord {
  return Object.freeze({
    code: error.code,
    domain: error.domain,
    operation: error.operation,
    retryability: error.retryability,
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
    retryability: code === 'operation.aborted' || code === 'operation.timed-out' ? 'caller-decides' : 'never'
  })
}
