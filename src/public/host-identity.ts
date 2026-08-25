// src/public/host-identity.ts

import { contractError } from '../backend-contract/errors'
import {
  normalizeRestorationBootstrapRequest,
  RESTORATION_DERIVATION_DOMAIN,
  type NativeRestorationBootstrapIdentity,
  type RestorationBootstrapRequest
} from '../backend-contract/restoration'

function getRandomValues(length: number): Uint8Array {
  const out = new Uint8Array(length)
  if (globalThis.crypto?.getRandomValues) {
    globalThis.crypto.getRandomValues(out)
    return out
  }
  throw contractError(
    'capability.unsupported',
    'core',
    'host-identity.secure-randomness.pass-randomBytes-in-create-options'
  )
}

function bytesToHex(bytes: Uint8Array): string {
  let hex = ''
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0')
  return hex
}

function assertNonEmptyString(value: string, label: string): void {
  if (typeof value !== 'string' || value.length === 0) {
    throw contractError('argument.invalid', 'core', label)
  }
  if (value.length > 256) {
    throw contractError('argument.invalid', 'core', `${label}.too-long`)
  }
}

/** Opaque identity returned by the trusted native host during restoration bootstrap. */
export interface RestorationHostIdentity extends NativeRestorationBootstrapIdentity {
  readonly kind: 'restoration'
}

export interface EphemeralHostIdentity {
  readonly kind: 'ephemeral'
  readonly attachmentNonce: string
  readonly managerNonce: string
  readonly operationNonce: string
}

export interface EphemeralIdentityInput {
  readonly now?: () => number
  readonly randomBytes?: (length: number) => Uint8Array
}

/**
 * The native host is the only restoration derivation authority. This retained
 * advanced symbol is intentionally fail-closed so JavaScript cannot forge a
 * restoration identity from caller-authored application data.
 */
export function deriveRestorationIdentity(_input: {
  readonly applicationId: string
  readonly restorationId: string
  readonly generation?: string
}): never {
  throw contractError('capability.unsupported', 'restoration', 'host-identity.native-authority-required')
}

function requireRandomBytes(random: (length: number) => Uint8Array, length: number): Uint8Array {
  const bytes = random(length)
  if (!(bytes instanceof Uint8Array) || bytes.byteLength !== length) {
    throw contractError('argument.invalid', 'core', 'host-identity.random-bytes')
  }
  return bytes
}

export function createEphemeralHostIdentity(input: EphemeralIdentityInput = {}): EphemeralHostIdentity {
  const random = input.randomBytes ?? ((length: number) => getRandomValues(length))
  const bytes = (length: number) => bytesToHex(requireRandomBytes(random, length))
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
  /**
   * Entropy source for this manager's ephemeral nonces. Required on hosts
   * without WebCrypto unless the host factory supplies a native CSPRNG.
   */
  readonly randomBytes?: (length: number) => Uint8Array
  /** One app-facing restoration token; native derives all internal identity values. */
  readonly restoration?: {
    readonly restorationId: string
    readonly generation?: string
  }
}

export interface DiagnosticsOptions {
  readonly traceMaximumRecords?: number
  readonly traceMaximumBytes?: number
  readonly maximumValueBytes?: number
}

function assertPlainRecord(value: unknown, operation: string): void {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw contractError('argument.invalid', 'core', operation)
  }
}

export function normalizeBleManagerCreateOptions(
  options: BleManagerCreateOptions | undefined
): BleManagerCreateOptions {
  if (options === undefined) {
    return Object.freeze({})
  }
  assertPlainRecord(options, 'options')
  const allowedKeys = new Set(['instanceId', 'adapterId', 'diagnostics', 'restoration', 'randomBytes'])
  if (Object.keys(options).some(key => !allowedKeys.has(key))) {
    throw contractError('argument.invalid', 'core', 'options.unknown-key')
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
  if (options.randomBytes !== undefined && typeof options.randomBytes !== 'function') {
    throw contractError('argument.invalid', 'core', 'options.randomBytes')
  }
  if (options.diagnostics !== undefined) {
    assertPlainRecord(options.diagnostics, 'options.diagnostics')
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
  const restoration: RestorationBootstrapRequest | undefined =
    options.restoration === undefined ? undefined : normalizeRestorationBootstrapRequest(options.restoration)
  return Object.freeze({ ...options, restoration })
}

// Exposed only for contract/vector tests; it contains no derivation implementation.
export const __testing = Object.freeze({
  RESTORATION_DOMAIN_SEPARATOR: RESTORATION_DERIVATION_DOMAIN,
  normalizeRestorationBootstrapRequest
})
