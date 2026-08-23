import type { PeerReference } from './peer-reference'
import { snapshotPeerReference } from './peer-reference'

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
  readonly localName: string | null
  readonly rssi: number | null
  readonly connectable: boolean | null
  readonly serviceUuids: readonly string[] | null
  readonly manufacturerData: readonly { readonly companyId: number; readonly data: Uint8Array }[] | null
  readonly serviceData: readonly { readonly service: string; readonly data: Uint8Array }[] | null
}

export function snapshotNormalizedScanQuery<NormalizedQuery extends NormalizedScanQuery>(
  query: NormalizedQuery
): NormalizedQuery {
  assertNormalizedQueryShape(query)
  const snapshot = Object.freeze({
    ...query,
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
    if (key === 'peers' && entry === null) return undefined
    return entry instanceof Uint8Array ? bytesToHex(entry) : entry
  })
}

function assertNormalizedQueryShape(query: NormalizedScanQuery): void {
  if (
    typeof query.digest !== 'string' ||
    query.digest.length === 0 ||
    (query.anyOf !== null && !Array.isArray(query.anyOf)) ||
    (query.exclude !== null && !Array.isArray(query.exclude))
  ) {
    throw new Error('normalized scan query must be a frozen canonical query')
  }
}

function snapshotClauseList(clauses: readonly NormalizedScanClause[] | null): readonly NormalizedScanClause[] | null {
  return clauses === null ? null : Object.freeze(clauses.map(snapshotClause))
}

function snapshotClause(clause: NormalizedScanClause): NormalizedScanClause {
  return Object.freeze({
    peers:
      clause.peers === null
        ? null
        : Object.freeze(clause.peers.map(peer => snapshotPeerReference(peer, 'scan.query.peer'))),
    services:
      clause.services === null
        ? null
        : Object.freeze({ any: Object.freeze([...clause.services.any]), all: Object.freeze([...clause.services.all]) }),
    names:
      clause.names === null
        ? null
        : Object.freeze({
            exact: Object.freeze([...clause.names.exact]),
            prefixes: Object.freeze([...clause.names.prefixes])
          }),
    manufacturerData:
      clause.manufacturerData === null
        ? null
        : Object.freeze({
            any: Object.freeze(clause.manufacturerData.any.map(snapshotManufacturerPattern)),
            all: Object.freeze(clause.manufacturerData.all.map(snapshotManufacturerPattern))
          }),
    serviceData:
      clause.serviceData === null
        ? null
        : Object.freeze({
            any: Object.freeze(clause.serviceData.any.map(snapshotServicePattern)),
            all: Object.freeze(clause.serviceData.all.map(snapshotServicePattern))
          }),
    rssi: clause.rssi === null ? null : Object.freeze({ ...clause.rssi }),
    connectable: clause.connectable
  })
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
