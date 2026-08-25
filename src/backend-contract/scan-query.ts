import type { PeerReference } from './peer-reference'
import { snapshotPeerReference } from './peer-reference'
import { canonicalBleAddress, canonicalUuid } from './primitives'

export interface NormalizedManufacturerDataPattern {
  readonly companyId: number
  readonly dataPrefix: Readonly<Uint8Array> | undefined
  readonly mask: Readonly<Uint8Array> | undefined
}

export interface NormalizedServiceDataPattern {
  readonly service: string
  readonly dataPrefix: Readonly<Uint8Array> | undefined
  readonly mask: Readonly<Uint8Array> | undefined
}

export interface NormalizedScanClause {
  readonly peers: readonly PeerReference[] | null
  /** Canonical radio addresses; matches only peers advertising a public or static random address. */
  readonly addresses: readonly string[] | null
  readonly services: {
    readonly any: readonly string[]
    readonly all: readonly string[]
  } | null
  readonly names: {
    readonly exact: readonly string[]
    readonly prefixes: readonly string[]
  } | null
  readonly manufacturerData: {
    readonly any: readonly NormalizedManufacturerDataPattern[]
    readonly all: readonly NormalizedManufacturerDataPattern[]
  } | null
  readonly serviceData: {
    readonly any: readonly NormalizedServiceDataPattern[]
    readonly all: readonly NormalizedServiceDataPattern[]
  } | null
  readonly rssi: { readonly minimum: number | undefined; readonly maximum: number | undefined } | null
  readonly connectable: boolean | undefined
}

export interface NormalizedScanQuery {
  readonly anyOf: readonly NormalizedScanClause[] | null
  readonly exclude: readonly NormalizedScanClause[] | null
  readonly digest: string
}

export interface NormalizedScanObservation {
  readonly peerReference?: PeerReference
  readonly address?: NormalizedObservationAddress
  readonly localName: string | null
  readonly rssi: number | null
  readonly connectable: boolean | null
  readonly serviceUuids: readonly string[] | null
  readonly manufacturerData: readonly { readonly companyId: number; readonly data: Uint8Array }[] | null
  readonly serviceData: readonly { readonly service: string; readonly data: Uint8Array }[] | null
}

export interface NormalizedObservationAddress {
  /**
   * `opaque` carries a canonical radio address whose native layer reported no
   * address type (Android scan results); it makes no public/static claim, so
   * consumers must not persist it as a stable public identity.
   */
  readonly type: 'public' | 'random' | 'opaque'
  readonly value: string
}

export function snapshotNormalizedScanQuery(query: NormalizedScanQuery): NormalizedScanQuery {
  assertNormalizedQueryShape(query)
  const snapshot = Object.freeze({
    anyOf: snapshotClauseList(query.anyOf),
    exclude: snapshotClauseList(query.exclude),
    digest: query.digest
  })
  if (scanQueryDigest({ anyOf: snapshot.anyOf, exclude: snapshot.exclude }) !== query.digest) {
    throw new Error('normalized scan query digest is invalid')
  }
  return snapshot
}

export function scanQueryDigest(query: Omit<NormalizedScanQuery, 'digest'>): string {
  const encoded = canonicalScanQueryJson(query)
  let hash = 0xcbf29ce484222325n
  for (let index = 0; index < encoded.length; index += 1) {
    hash ^= BigInt(encoded.charCodeAt(index))
    hash = BigInt.asUintN(64, hash * 0x100000001b3n)
  }
  return `scan-query-v1:${hash.toString(16).padStart(16, '0')}`
}

export function canonicalScanQueryJson(value: unknown): string {
  return JSON.stringify(value, (key, entry: unknown) => {
    if ((key === 'peers' || key === 'addresses') && entry === null) return undefined
    return entry instanceof Uint8Array ? bytesToHex(entry) : entry
  })
}

function assertNormalizedQueryShape(query: NormalizedScanQuery): void {
  if (
    typeof query.digest !== 'string' ||
    query.digest.length === 0 ||
    (query.anyOf !== null && !Array.isArray(query.anyOf)) ||
    (query.exclude !== null && !Array.isArray(query.exclude)) ||
    (query.anyOf !== null && query.anyOf.length === 0) ||
    (query.exclude !== null && query.exclude.length === 0)
  ) {
    throw new Error('normalized scan query must be a frozen canonical query')
  }
  assertExactKeys(query, ['anyOf', 'exclude', 'digest'], 'normalized scan query')
  if (query.anyOf !== null) query.anyOf.forEach((clause, index) => assertNormalizedClause(clause, `anyOf[${index}]`))
  if (query.exclude !== null)
    query.exclude.forEach((clause, index) => assertNormalizedClause(clause, `exclude[${index}]`))
}

function assertNormalizedClause(clause: NormalizedScanClause, path: string): void {
  assertObject(clause, `normalized scan query ${path}`)
  assertExactKeys(
    clause,
    ['peers', 'addresses', 'services', 'names', 'manufacturerData', 'serviceData', 'rssi', 'connectable'],
    path
  )
  if (clause.peers !== null && !Array.isArray(clause.peers)) throw invalidNormalized(path)
  assertCanonicalAddressList(clause.addresses, `${path}.addresses`)
  assertUuidField(clause.services, `${path}.services`)
  assertNameField(clause.names, `${path}.names`)
  assertManufacturerField(clause.manufacturerData, `${path}.manufacturerData`)
  assertServiceDataField(clause.serviceData, `${path}.serviceData`)
  if (clause.rssi !== null) {
    assertObject(clause.rssi, `${path}.rssi`)
    assertExactKeys(clause.rssi, ['minimum', 'maximum'], `${path}.rssi`)
    if (
      (clause.rssi.minimum !== undefined && !Number.isFinite(clause.rssi.minimum)) ||
      (clause.rssi.maximum !== undefined && !Number.isFinite(clause.rssi.maximum)) ||
      (clause.rssi.minimum === undefined && clause.rssi.maximum === undefined) ||
      (clause.rssi.minimum !== undefined &&
        clause.rssi.maximum !== undefined &&
        clause.rssi.minimum > clause.rssi.maximum)
    ) {
      throw invalidNormalized(`${path}.rssi`)
    }
  }
  if (clause.connectable !== undefined && typeof clause.connectable !== 'boolean') {
    throw invalidNormalized(`${path}.connectable`)
  }
  if (
    clause.peers === null &&
    clause.addresses === null &&
    clause.services === null &&
    clause.names === null &&
    clause.manufacturerData === null &&
    clause.serviceData === null &&
    clause.rssi === null &&
    clause.connectable === undefined
  ) {
    throw invalidNormalized(`${path}.empty`)
  }
}

function assertUuidField(field: NormalizedScanClause['services'], path: string): void {
  if (field === null) return
  assertObject(field, path)
  assertExactKeys(field, ['any', 'all'], path)
  assertCanonicalUuidList(field.any, `${path}.any`)
  assertCanonicalUuidList(field.all, `${path}.all`)
  if (field.any.length === 0 && field.all.length === 0) throw invalidNormalized(path)
}

function assertNameField(field: NormalizedScanClause['names'], path: string): void {
  if (field === null) return
  assertObject(field, path)
  assertExactKeys(field, ['exact', 'prefixes'], path)
  assertNonEmptyStrings(field.exact, `${path}.exact`)
  assertNonEmptyStrings(field.prefixes, `${path}.prefixes`)
  if (field.exact.length === 0 && field.prefixes.length === 0) throw invalidNormalized(path)
}

function assertManufacturerField(field: NormalizedScanClause['manufacturerData'], path: string): void {
  if (field === null) return
  assertObject(field, path)
  assertExactKeys(field, ['any', 'all'], path)
  assertManufacturerPatterns(field.any, `${path}.any`)
  assertManufacturerPatterns(field.all, `${path}.all`)
  if (field.any.length === 0 && field.all.length === 0) throw invalidNormalized(path)
}

function assertServiceDataField(field: NormalizedScanClause['serviceData'], path: string): void {
  if (field === null) return
  assertObject(field, path)
  assertExactKeys(field, ['any', 'all'], path)
  assertServicePatterns(field.any, `${path}.any`)
  assertServicePatterns(field.all, `${path}.all`)
  if (field.any.length === 0 && field.all.length === 0) throw invalidNormalized(path)
}

function assertManufacturerPatterns(patterns: readonly NormalizedManufacturerDataPattern[], path: string): void {
  if (!Array.isArray(patterns) || patterns.some(pattern => !isObject(pattern))) throw invalidNormalized(path)
  patterns.forEach((pattern, index) => {
    assertExactKeys(pattern, ['companyId', 'dataPrefix', 'mask'], `${path}[${index}]`)
    if (!Number.isSafeInteger(pattern.companyId) || pattern.companyId < 0 || pattern.companyId > 0xffff) {
      throw invalidNormalized(`${path}[${index}].companyId`)
    }
    assertByteFields(pattern.dataPrefix, pattern.mask, `${path}[${index}]`)
  })
}

function assertServicePatterns(patterns: readonly NormalizedServiceDataPattern[], path: string): void {
  if (!Array.isArray(patterns) || patterns.some(pattern => !isObject(pattern))) throw invalidNormalized(path)
  patterns.forEach((pattern, index) => {
    assertExactKeys(pattern, ['service', 'dataPrefix', 'mask'], `${path}[${index}]`)
    assertCanonicalUuid(pattern.service, `${path}[${index}].service`)
    assertByteFields(pattern.dataPrefix, pattern.mask, `${path}[${index}]`)
  })
}

function assertByteFields(
  dataPrefix: Readonly<Uint8Array> | undefined,
  mask: Readonly<Uint8Array> | undefined,
  path: string
): void {
  if (dataPrefix !== undefined && (!(dataPrefix instanceof Uint8Array) || dataPrefix.byteLength === 0)) {
    throw invalidNormalized(`${path}.dataPrefix`)
  }
  if (mask !== undefined && (!(mask instanceof Uint8Array) || mask.byteLength !== dataPrefix?.byteLength)) {
    throw invalidNormalized(`${path}.mask`)
  }
}

function assertCanonicalAddressList(values: readonly string[] | null, path: string): void {
  if (values === null) return
  if (!Array.isArray(values) || values.length === 0 || values.some(value => typeof value !== 'string')) {
    throw invalidNormalized(path)
  }
  values.forEach((value, index) => {
    try {
      if (canonicalBleAddress(value) !== value) throw new Error('not canonical')
    } catch {
      throw invalidNormalized(`${path}[${index}]`)
    }
  })
}

function assertCanonicalUuidList(values: readonly string[], path: string): void {
  if (!Array.isArray(values) || values.some(value => typeof value !== 'string')) throw invalidNormalized(path)
  values.forEach((value, index) => assertCanonicalUuid(value, `${path}[${index}]`))
}

function assertCanonicalUuid(value: string, path: string): void {
  try {
    if (canonicalUuid(value) !== value) throw new Error('not canonical')
  } catch {
    throw invalidNormalized(path)
  }
}

function assertNonEmptyStrings(values: readonly string[], path: string): void {
  if (!Array.isArray(values) || values.some(value => typeof value !== 'string' || value.length === 0)) {
    throw invalidNormalized(path)
  }
}

function assertExactKeys(value: object, keys: readonly string[], path: string): void {
  const actual = Object.keys(value).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw invalidNormalized(path)
  }
}

function assertObject(value: object | null, path: string): asserts value is object {
  if (!isObject(value)) throw invalidNormalized(path)
}

function isObject(value: object | null): value is Record<string, never> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function invalidNormalized(path: string): Error {
  return new Error(`invalid normalized scan query at ${path}`)
}

function snapshotClauseList(clauses: readonly NormalizedScanClause[] | null): readonly NormalizedScanClause[] | null {
  return clauses === null
    ? null
    : Object.freeze(
        clauses
          .map(snapshotClause)
          .sort((left, right) => compareCanonical(canonicalScanQueryJson(left), canonicalScanQueryJson(right)))
      )
}

function snapshotClause(clause: NormalizedScanClause): NormalizedScanClause {
  return Object.freeze({
    peers: clause.peers === null ? null : snapshotPeers(clause.peers),
    addresses: clause.addresses === null ? null : sortedUnique(clause.addresses),
    services:
      clause.services === null
        ? null
        : Object.freeze({ any: sortedUnique(clause.services.any), all: sortedUnique(clause.services.all) }),
    names:
      clause.names === null
        ? null
        : Object.freeze({
            exact: sortedUnique(clause.names.exact),
            prefixes: sortedUnique(clause.names.prefixes)
          }),
    manufacturerData:
      clause.manufacturerData === null
        ? null
        : Object.freeze({
            any: sortedPatterns(clause.manufacturerData.any.map(snapshotManufacturerPattern)),
            all: sortedPatterns(clause.manufacturerData.all.map(snapshotManufacturerPattern))
          }),
    serviceData:
      clause.serviceData === null
        ? null
        : Object.freeze({
            any: sortedPatterns(clause.serviceData.any.map(snapshotServicePattern)),
            all: sortedPatterns(clause.serviceData.all.map(snapshotServicePattern))
          }),
    rssi: clause.rssi === null ? null : Object.freeze({ ...clause.rssi }),
    connectable: clause.connectable
  })
}

function snapshotPeers(peers: readonly PeerReference[]): readonly PeerReference[] {
  const unique = new Map(
    peers.map(peer => [canonicalScanQueryJson(peer), snapshotPeerReference(peer, 'scan.query.peer')])
  )
  return Object.freeze(
    [...unique.entries()].sort(([left], [right]) => compareCanonical(left, right)).map(([, peer]) => peer)
  )
}

function sortedUnique(values: readonly string[]): readonly string[] {
  return Object.freeze([...new Set(values)].sort())
}

function sortedPatterns<Pattern extends NormalizedManufacturerDataPattern | NormalizedServiceDataPattern>(
  patterns: readonly Pattern[]
): readonly Pattern[] {
  return Object.freeze(
    patterns
      .slice()
      .sort((left, right) => compareCanonical(canonicalScanQueryJson(left), canonicalScanQueryJson(right)))
  )
}

function snapshotManufacturerPattern(pattern: NormalizedManufacturerDataPattern): NormalizedManufacturerDataPattern {
  return Object.freeze({
    companyId: pattern.companyId,
    dataPrefix: copyBytes(pattern.dataPrefix),
    mask: copyBytes(pattern.mask)
  })
}

function snapshotServicePattern(pattern: NormalizedServiceDataPattern): NormalizedServiceDataPattern {
  return Object.freeze({
    service: pattern.service,
    dataPrefix: copyBytes(pattern.dataPrefix),
    mask: copyBytes(pattern.mask)
  })
}

function copyBytes(value: Readonly<Uint8Array> | undefined): Readonly<Uint8Array> | undefined {
  return value === undefined ? undefined : new Uint8Array(value)
}

function bytesToHex(value: Readonly<Uint8Array>): string {
  let result = ''
  for (const byte of value) result += byte.toString(16).padStart(2, '0')
  return result
}

function compareCanonical(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}
