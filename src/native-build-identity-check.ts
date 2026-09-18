// src/native-build-identity-check.ts
//
// Host-neutral runtime half of the native build identity (PR210-18): parse a
// binary's `ubm-native-build-identity/1` record strictly and compare it with
// the identity this package was sealed with
// (`src/generated/native-build-identity.ts`). Every host runs this before its
// first radio call; a mismatch never reaches the radio.

import type { ExpectedNativeBuildIdentity, NativeBuildBinding } from './generated/native-build-identity'

/** The parsed `ubm-native-build-identity/1` record a binary reports. */
export interface NativeBuildIdentityRecord {
  readonly schema: string
  readonly binding: string
  readonly contractRevision: string
  readonly sourceDigest: string
  readonly bindingSchema: string
  readonly target: string
  readonly profile: string
  readonly features: readonly string[]
  readonly rustc: string
}

const STRING_FIELDS = Object.freeze([
  'schema',
  'binding',
  'contractRevision',
  'sourceDigest',
  'bindingSchema',
  'target',
  'profile',
  'rustc'
] as const)

const RECORD_KEYS: readonly string[] = Object.freeze([...STRING_FIELDS, 'features'])

function plainRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype: unknown = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

/**
 * Reads a parsed identity record with exactly the frozen key set. Anything
 * else (a missing key, an extra key, a non-string field) is `null`: an
 * identity that cannot be read is an identity that does not match.
 */
export function readNativeBuildIdentity(value: unknown): NativeBuildIdentityRecord | null {
  if (!plainRecord(value)) return null
  const keys = Object.keys(value)
  if (keys.length !== RECORD_KEYS.length || RECORD_KEYS.some(key => !keys.includes(key))) return null
  const strings = new Map<string, string>()
  for (const field of STRING_FIELDS) {
    const entry = value[field]
    if (typeof entry !== 'string' || entry.length === 0) return null
    strings.set(field, entry)
  }
  const features = value.features
  if (!Array.isArray(features)) return null
  const featureNames: string[] = []
  for (const feature of features) {
    if (typeof feature !== 'string') return null
    featureNames.push(feature)
  }
  const read = (field: (typeof STRING_FIELDS)[number]): string => strings.get(field) ?? ''
  return Object.freeze({
    schema: read('schema'),
    binding: read('binding'),
    contractRevision: read('contractRevision'),
    sourceDigest: read('sourceDigest'),
    bindingSchema: read('bindingSchema'),
    target: read('target'),
    profile: read('profile'),
    features: Object.freeze(featureNames),
    rustc: read('rustc')
  })
}

/** Parses identity JSON text; malformed text is `null`. */
export function parseNativeBuildIdentityText(text: unknown): NativeBuildIdentityRecord | null {
  if (typeof text !== 'string') return null
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch (error) {
    if (error instanceof SyntaxError) return null
    throw error
  }
  return readNativeBuildIdentity(parsed)
}

/**
 * The fields of `actual` that differ from what this package was sealed with
 * for `binding`. `requireRelease` rejects a debug profile (prebuilt
 * artifacts). Empty means the binary is the one the package expects.
 */
export function nativeBuildIdentityMismatches(
  actual: NativeBuildIdentityRecord,
  binding: NativeBuildBinding,
  expected: ExpectedNativeBuildIdentity,
  requireRelease: boolean
): readonly string[] {
  const sealed = expected.bindings[binding]
  const mismatches: string[] = []
  if (actual.schema !== expected.schema) mismatches.push('schema')
  if (actual.binding !== binding) mismatches.push('binding')
  if (actual.contractRevision !== expected.contractRevision) mismatches.push('contractRevision')
  if (actual.sourceDigest !== sealed.sourceDigest) mismatches.push('sourceDigest')
  if (actual.bindingSchema !== sealed.bindingSchema) mismatches.push('bindingSchema')
  if (!sealed.targets.includes(actual.target)) mismatches.push('target')
  if (requireRelease && actual.profile !== 'release') mismatches.push('profile')
  return Object.freeze(mismatches)
}

/** Field-wise equality of two identity records (the same binary answered twice). */
export function nativeBuildIdentitiesEqual(left: NativeBuildIdentityRecord, right: NativeBuildIdentityRecord): boolean {
  return (
    STRING_FIELDS.every(field => left[field] === right[field]) &&
    left.features.length === right.features.length &&
    left.features.every((feature, index) => feature === right.features[index])
  )
}
