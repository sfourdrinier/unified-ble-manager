// src/public/host-identity.ts

import { contractError } from '../backend-contract/errors'

function getRandomValues(length: number): Uint8Array {
  const out = new Uint8Array(length)
  if (globalThis.crypto?.getRandomValues) {
    globalThis.crypto.getRandomValues(out)
    return out
  }
  for (let i = 0; i < length; i++) out[i] = Math.floor(Math.random() * 256)
  return out
}

function bytesToHex(bytes: Uint8Array): string {
  let hex = ''
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0')
  return hex
}

function sha256Sync(message: string): string {
  return jsSha256(message)
}

function jsSha256(ascii: string): string {
  // Convert string to UTF-8 bytes
  const bytes: number[] = []
  for (let i = 0; i < ascii.length; i++) {
    const code = ascii.charCodeAt(i)
    if (code < 0x80) bytes.push(code)
    else if (code < 0x800) {
      bytes.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f))
    } else if (code < 0xd800 || code >= 0xe000) {
      bytes.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f))
    } else {
      i++
      const code2 = ascii.charCodeAt(i)
      const cp = 0x10000 + ((code & 0x3ff) << 10) + (code2 & 0x3ff)
      bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f))
    }
  }
  // Padding
  const bitLen = bytes.length * 8
  bytes.push(0x80)
  while (bytes.length % 64 !== 56) bytes.push(0)
  for (let i = 7; i >= 0; i--) bytes.push((bitLen >>> (i * 8)) & 0xff)
  // Initial hash values
  const H = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]
  const K = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2
  ]
  const rightRotate = (v: number, n: number) => (v >>> n) | (v << (32 - n))
  for (let i = 0; i < bytes.length; i += 64) {
    const W = new Array(64)
    for (let t = 0; t < 16; t++)
      W[t] =
        (bytes[i + t * 4]! << 24) | (bytes[i + t * 4 + 1]! << 16) | (bytes[i + t * 4 + 2]! << 8) | bytes[i + t * 4 + 3]!
    for (let t = 16; t < 64; t++) {
      const s0 = rightRotate(W[t - 15]!, 7) ^ rightRotate(W[t - 15]!, 18) ^ (W[t - 15]! >>> 3)
      const s1 = rightRotate(W[t - 2]!, 17) ^ rightRotate(W[t - 2]!, 19) ^ (W[t - 2]! >>> 10)
      W[t] = (W[t - 16]! + s0 + W[t - 7]! + s1) | 0
    }
    let a = H[0]!,
      b = H[1]!,
      c = H[2]!,
      d = H[3]!,
      e = H[4]!,
      f = H[5]!,
      g = H[6]!,
      h = H[7]!
    for (let t = 0; t < 64; t++) {
      const S1 = rightRotate(e, 6) ^ rightRotate(e, 11) ^ rightRotate(e, 25)
      const ch = (e & f) ^ (~e & g)
      const temp1 = (h + S1 + ch + K[t]! + W[t]!) | 0
      const S0 = rightRotate(a, 2) ^ rightRotate(a, 13) ^ rightRotate(a, 22)
      const maj = (a & b) ^ (a & c) ^ (b & c)
      const temp2 = (S0 + maj) | 0
      h = g
      g = f
      f = e
      e = (d + temp1) | 0
      d = c
      c = b
      b = a
      a = (temp1 + temp2) | 0
    }
    H[0] = (H[0]! + a) | 0
    H[1] = (H[1]! + b) | 0
    H[2] = (H[2]! + c) | 0
    H[3] = (H[3]! + d) | 0
    H[4] = (H[4]! + e) | 0
    H[5] = (H[5]! + f) | 0
    H[6] = (H[6]! + g) | 0
    H[7] = (H[7]! + h) | 0
  }
  let hex = ''
  for (let i = 0; i < H.length; i++) {
    const v = H[i]! >>> 0
    hex += ('00000000' + v.toString(16)).slice(-8)
  }
  return hex
}

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
  const digest = sha256Sync(material)
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
  const random = input.randomBytes ?? ((length: number) => getRandomValues(length))
  const bytes = (length: number) => bytesToHex(random(length))
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
