// src/public/host-identity.ts

import { createHash, randomBytes } from 'node:crypto'

import { contractError } from '../backend-contract/errors'

/**
 * Host identity factory with two explicit operations:
 * - ephemeral runtime identities (random/monotonic per attachment)
 * - deterministic restoration identity (stable across relaunch)
 */

export interface EphemeralHostIdentity {
  readonly kind: 'ephemeral'
  readonly attachmentNonce: string
  readonly managerNonce: string
  readonly operationNonce: string
}

export interface RestorationHostIdentity {
  readonly kind: 'restoration'
  /** Opaque value returned by the trusted host during bootstrap; JS never forges it. */
  readonly opaqueRestorationId: string
  readonly applicationId: string
  readonly restorationId: string
  readonly generation: string
}

export interface EphemeralIdentityInput {
  readonly now?: () => number
  readonly randomBytes?: (length: number) => Uint8Array
}

const RESTORATION_DOMAIN_SEPARATOR = 'unified-ble-manager:restoration:v1'

function assertNonEmptyString(value: string, label: string): void {
  if (typeof value !== 'string' || value.length === 0) {
    throw contractError('argument.invalid', 'restoration', label)
  }
  if (value.length > 256) {
    throw contractError('argument.invalid', 'restoration', `${label}.too-long`)
  }
}

function normalizeApplicationId(value: string): string {
  assertNonEmptyString(value, 'identity.applicationId')
  // iOS bundle identifier style: lowercase, trim, no surrounding whitespace.
  const normalized = value.trim().toLowerCase()
  assertNonEmptyString(normalized, 'identity.applicationId')
  if (!/^[a-z0-9][a-z0-9._-]*$/.test(normalized)) {
    throw contractError('argument.invalid', 'restoration', 'identity.applicationId.format')
  }
  return normalized
}

function normalizeRestorationId(value: string): string {
  assertNonEmptyString(value, 'identity.restorationId')
  const normalized = value.trim()
  assertNonEmptyString(normalized, 'identity.restorationId')
  if (normalized.includes(':') || normalized.includes('\n')) {
    throw contractError('argument.invalid', 'restoration', 'identity.restorationId.format')
  }
  return normalized
}

function normalizeGeneration(value: string): string {
  if (typeof value !== 'string') {
    throw contractError('argument.invalid', 'restoration', 'identity.generation')
  }
  const normalized = value.trim()
  if (normalized.length === 0) {
    return '0'
  }
  if (normalized.length > 64) {
    throw contractError('argument.invalid', 'restoration', 'identity.generation.too-long')
  }
  return normalized
}

/**
 * Deterministic restoration derivation. Must be domain-separated, stable across
 * relaunch and ordinary package/app updates, and shared with native code.
 * Never uses wall-clock, package version, or per-launch randomness.
 */
export function deriveRestorationIdentity(input: {
  readonly applicationId: string
  readonly restorationId: string
  readonly generation?: string
}): RestorationHostIdentity {
  const applicationId = normalizeApplicationId(input.applicationId)
  const restorationId = normalizeRestorationId(input.restorationId)
  const generation = normalizeGeneration(input.generation ?? '0')

  const material = `${RESTORATION_DOMAIN_SEPARATOR}\n${applicationId}\n${restorationId}\n${generation}`
  const digest = createHash('sha256').update(material, 'utf8').digest('hex')
  // Opaque value: hex digest prefixed with version marker for future rotation.
  const opaqueRestorationId = `ubm-r1:${digest.slice(0, 32)}`

  return Object.freeze({
    kind: 'restoration',
    opaqueRestorationId,
    applicationId,
    restorationId,
    generation
  })
}

export function createEphemeralHostIdentity(input: EphemeralIdentityInput = {}): EphemeralHostIdentity {
  const random = input.randomBytes ?? ((length: number) => randomBytes(length))
  const bytes = (length: number) => Buffer.from(random(length)).toString('hex')
  return Object.freeze({
    kind: 'ephemeral',
    attachmentNonce: bytes(16),
    managerNonce: bytes(16),
    operationNonce: bytes(8)
  })
}

export interface BleManagerCreateOptions {
  /** Optional app-declared stable name for an intentionally distinct manager instance. Does not affect restoration. */
  readonly instanceId?: string
  /** Optional adapter selector (e.g., 'hci0'). Host decides default when omitted. */
  readonly adapterId?: string
  readonly diagnostics?: DiagnosticsOptions
  /** Restoration configuration — when omitted, manager is non-restorable. */
  readonly restoration?: {
    readonly applicationId: string
    readonly restorationId: string
    readonly generation?: string
  }
}

export interface DiagnosticsOptions {
  readonly traceMaximumRecords?: number
  readonly traceMaximumBytes?: number
  readonly maximumValueBytes?: number
}

export function normalizeBleManagerCreateOptions(
  options: BleManagerCreateOptions | undefined
): BleManagerCreateOptions {
  if (options === undefined) {
    return Object.freeze({})
  }
  if (options.instanceId !== undefined) {
    assertNonEmptyString(options.instanceId, 'options.instanceId')
    if (options.instanceId.includes(':')) {
      throw contractError('argument.invalid', 'core', 'options.instanceId.format')
    }
  }
  if (options.adapterId !== undefined) {
    assertNonEmptyString(options.adapterId, 'options.adapterId')
  }
  if (options.diagnostics !== undefined) {
    const d = options.diagnostics
    if (
      d.traceMaximumRecords !== undefined &&
      (!Number.isSafeInteger(d.traceMaximumRecords) || d.traceMaximumRecords <= 0)
    ) {
      throw contractError('argument.invalid', 'core', 'options.diagnostics.traceMaximumRecords')
    }
    if (d.traceMaximumBytes !== undefined && (!Number.isSafeInteger(d.traceMaximumBytes) || d.traceMaximumBytes <= 0)) {
      throw contractError('argument.invalid', 'core', 'options.diagnostics.traceMaximumBytes')
    }
    if (d.maximumValueBytes !== undefined && (!Number.isSafeInteger(d.maximumValueBytes) || d.maximumValueBytes <= 0)) {
      throw contractError('argument.invalid', 'core', 'options.diagnostics.maximumValueBytes')
    }
  }
  if (options.restoration !== undefined) {
    // Validate eagerly; derivation happens in the trusted host.
    normalizeApplicationId(options.restoration.applicationId)
    normalizeRestorationId(options.restoration.restorationId)
    if (options.restoration.generation !== undefined) {
      normalizeGeneration(options.restoration.generation)
    }
  }
  return Object.freeze({ ...options })
}

// Exposed for cross-language fixture generation and tests.
export const __testing = Object.freeze({
  RESTORATION_DOMAIN_SEPARATOR,
  normalizeApplicationId,
  normalizeRestorationId,
  normalizeGeneration
})
