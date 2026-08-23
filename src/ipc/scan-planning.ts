import { contractError } from '../backend-contract/errors'
import type { PeerReference } from '../backend-contract/peer-reference'
import { byteLimit, ownBytes } from '../backend-contract/primitives'
import type { SerializableRecord } from '../backend-contract/primitives'
import type {
  NormalizedManufacturerDataPattern,
  NormalizedScanClause,
  NormalizedScanQuery,
  NormalizedServiceDataPattern
} from '../backend-contract/scan-query'
import { snapshotScanPlan } from '../backend-contract/scan-planning'
import type { ScanPlan } from '../backend-contract/scan-planning'
import {
  normalizeScanQuery,
  type ManufacturerDataPattern,
  type ScanClause,
  type ServiceDataPattern
} from '../public/scan-query'

/**
 * The normalized query contains optional fields whose `undefined` values are
 * intentionally omitted by JSON. IPC snapshots do not carry `undefined`, so
 * this wire projection uses `null` for those optional values and reconstructs
 * the canonical normalized query in main.
 */
export function encodeIpcScanQuery(query: NormalizedScanQuery): SerializableRecord {
  return Object.freeze({
    anyOf: encodeClauseList(query.anyOf),
    exclude: encodeClauseList(query.exclude),
    digest: query.digest
  })
}

export function encodeIpcScanPlan(plan: ScanPlan): SerializableRecord {
  const snapshot = snapshotScanPlan(plan)
  return Object.freeze({
    sourceQuery: encodeIpcScanQuery(snapshot.sourceQuery),
    queryDigest: snapshot.queryDigest,
    residualQueryDigest: snapshot.residualQueryDigest,
    nativeGuarantee: snapshot.nativeGuarantee,
    native: encodeProjection(snapshot.native),
    residual: Object.freeze({
      predicates: encodePredicates(snapshot.residual.predicates),
      complete: snapshot.residual.complete,
      query: encodeIpcScanQuery(snapshot.residual.query)
    }),
    unavailable: encodePredicates(snapshot.unavailable),
    limitations: encodeLimitations(snapshot.limitations),
    estimatedCost: snapshot.estimatedCost
  })
}

/** Decodes and re-normalizes the main-owned query before planning or scanning. */
export function decodeIpcScanQuery(value: unknown, operation = 'ipc.scan.query'): NormalizedScanQuery {
  try {
    const record = requiredRecord(value, operation)
    assertExactKeys(record, ['anyOf', 'exclude', 'digest'], operation)
    const digest = record.digest
    if (typeof digest !== 'string' || digest.length === 0) throw malformed(operation)
    const anyOf = decodeClauseList(record.anyOf, operation)
    const exclude = decodeClauseList(record.exclude, operation)
    const query = normalizeScanQuery({
      ...(anyOf === null ? {} : { anyOf }),
      ...(exclude === null ? {} : { exclude })
    })
    if (query.digest !== digest) throw malformed(operation)
    return query
  } catch {
    throw malformed(operation)
  }
}

function encodeClauseList(clauses: readonly NormalizedScanClause[] | null): readonly SerializableRecord[] | null {
  return clauses === null ? null : Object.freeze(clauses.map(encodeClause))
}

function encodeClause(clause: NormalizedScanClause): SerializableRecord {
  return Object.freeze({
    peers: clause.peers === null ? null : Object.freeze(clause.peers.map(encodePeerReference)),
    services:
      clause.services === null ? null : Object.freeze({ any: [...clause.services.any], all: [...clause.services.all] }),
    names:
      clause.names === null
        ? null
        : Object.freeze({ exact: [...clause.names.exact], prefixes: [...clause.names.prefixes] }),
    manufacturerData:
      clause.manufacturerData === null
        ? null
        : Object.freeze({
            any: clause.manufacturerData.any.map(encodeManufacturerPattern),
            all: clause.manufacturerData.all.map(encodeManufacturerPattern)
          }),
    serviceData:
      clause.serviceData === null
        ? null
        : Object.freeze({
            any: clause.serviceData.any.map(encodeServicePattern),
            all: clause.serviceData.all.map(encodeServicePattern)
          }),
    rssi:
      clause.rssi === null
        ? null
        : Object.freeze({
            minimum: clause.rssi.minimum === undefined ? null : clause.rssi.minimum,
            maximum: clause.rssi.maximum === undefined ? null : clause.rssi.maximum
          }),
    connectable: clause.connectable === undefined ? null : clause.connectable
  })
}

function encodePeerReference(reference: PeerReference): SerializableRecord {
  return Object.freeze({
    version: reference.version,
    backendId: reference.backendId,
    scope: reference.scope,
    opaqueId: reference.opaqueId
  })
}

function encodeProjection(projection: ScanPlan['native']): SerializableRecord {
  return Object.freeze({ predicates: encodePredicates(projection.predicates), complete: projection.complete })
}

function encodePredicates(predicates: ScanPlan['native']['predicates']): readonly SerializableRecord[] {
  return Object.freeze(
    predicates.map(predicate =>
      Object.freeze({
        clauseSet: predicate.clauseSet,
        clauseIndex: predicate.clauseIndex,
        field: predicate.field,
        operator: predicate.operator
      })
    )
  )
}

function encodeLimitations(limitations: ScanPlan['limitations']): readonly SerializableRecord[] {
  return Object.freeze(
    limitations.map(limitation =>
      Object.freeze({
        code: limitation.code,
        predicate: Object.freeze({
          clauseSet: limitation.predicate.clauseSet,
          clauseIndex: limitation.predicate.clauseIndex,
          field: limitation.predicate.field,
          operator: limitation.predicate.operator
        }),
        explanation: limitation.explanation,
        effect: limitation.effect
      })
    )
  )
}

function encodeManufacturerPattern(pattern: NormalizedManufacturerDataPattern): SerializableRecord {
  return Object.freeze({
    companyId: pattern.companyId,
    dataPrefix:
      pattern.dataPrefix === undefined ? null : ownBytes(pattern.dataPrefix, byteLimit(pattern.dataPrefix.byteLength)),
    mask: pattern.mask === undefined ? null : ownBytes(pattern.mask, byteLimit(pattern.mask.byteLength))
  })
}

function encodeServicePattern(pattern: NormalizedServiceDataPattern): SerializableRecord {
  return Object.freeze({
    service: pattern.service,
    dataPrefix:
      pattern.dataPrefix === undefined ? null : ownBytes(pattern.dataPrefix, byteLimit(pattern.dataPrefix.byteLength)),
    mask: pattern.mask === undefined ? null : ownBytes(pattern.mask, byteLimit(pattern.mask.byteLength))
  })
}

function decodeClauseList(value: unknown, operation: string): readonly ScanClause[] | null {
  if (value === null) return null
  if (!Array.isArray(value)) throw malformed(operation)
  return Object.freeze(value.map(item => decodeClause(item, operation)))
}

function decodeClause(value: unknown, operation: string): ScanClause {
  const record = requiredRecord(value, operation)
  assertExactKeys(
    record,
    ['peers', 'services', 'names', 'manufacturerData', 'serviceData', 'rssi', 'connectable'],
    operation
  )
  const peers = decodePeers(record.peers, operation)
  const services = decodeUuidField(record.services, operation)
  const names = decodeNames(record.names, operation)
  const manufacturerData = decodeManufacturerField(record.manufacturerData, operation)
  const serviceData = decodeServiceDataField(record.serviceData, operation)
  const rssi = decodeRssi(record.rssi, operation)
  const connectable = decodeConnectable(record.connectable, operation)
  return {
    ...(peers === null ? {} : { peers }),
    ...(services === null ? {} : { services }),
    ...(names === null ? {} : { names }),
    ...(manufacturerData === null ? {} : { manufacturerData }),
    ...(serviceData === null ? {} : { serviceData }),
    ...(rssi === null ? {} : { rssi }),
    ...(connectable === null ? {} : { connectable })
  }
}

function decodePeers(value: unknown, operation: string): readonly PeerReference[] | undefined {
  if (value === null) return undefined
  if (!Array.isArray(value)) throw malformed(operation)
  return Object.freeze(value.map(item => decodePeerReference(item, operation)))
}

function decodePeerReference(value: unknown, operation: string): PeerReference {
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['version', 'backendId', 'scope', 'opaqueId'], operation)
  if (record.version !== 1 || typeof record.backendId !== 'string' || typeof record.opaqueId !== 'string') {
    throw malformed(operation)
  }
  const scope = decodePeerScope(record.scope, operation)
  return Object.freeze({
    version: 1,
    backendId: record.backendId,
    scope,
    opaqueId: record.opaqueId
  })
}

function decodePeerScope(value: unknown, operation: string): PeerReference['scope'] {
  if (value === 'application' || value === 'origin' || value === 'system') return value
  throw malformed(operation)
}

function decodeUuidField(value: unknown, operation: string): ScanClause['services'] | undefined {
  if (value === null) return undefined
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['any', 'all'], operation)
  const any = requiredStrings(record.any, operation)
  const all = requiredStrings(record.all, operation)
  return { ...(any.length === 0 ? {} : { any }), ...(all.length === 0 ? {} : { all }) }
}

function decodeNames(value: unknown, operation: string): ScanClause['names'] | undefined {
  if (value === null) return undefined
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['exact', 'prefixes'], operation)
  const exact = requiredStrings(record.exact, operation)
  const prefixes = requiredStrings(record.prefixes, operation)
  return { ...(exact.length === 0 ? {} : { exact }), ...(prefixes.length === 0 ? {} : { prefixes }) }
}

function decodeManufacturerField(value: unknown, operation: string): ScanClause['manufacturerData'] | undefined {
  if (value === null) return undefined
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['any', 'all'], operation)
  const any = decodeManufacturerPatterns(record.any, operation)
  const all = decodeManufacturerPatterns(record.all, operation)
  return {
    ...(any.length === 0 ? {} : { any }),
    ...(all.length === 0 ? {} : { all })
  }
}

function decodeServiceDataField(value: unknown, operation: string): ScanClause['serviceData'] | undefined {
  if (value === null) return undefined
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['any', 'all'], operation)
  const any = decodeServicePatterns(record.any, operation)
  const all = decodeServicePatterns(record.all, operation)
  return {
    ...(any.length === 0 ? {} : { any }),
    ...(all.length === 0 ? {} : { all })
  }
}

function decodeManufacturerPatterns(value: unknown, operation: string): readonly ManufacturerDataPattern[] {
  if (!Array.isArray(value)) throw malformed(operation)
  return Object.freeze(value.map(item => decodeManufacturerPattern(item, operation)))
}

function decodeServicePatterns(value: unknown, operation: string): readonly ServiceDataPattern[] {
  if (!Array.isArray(value)) throw malformed(operation)
  return Object.freeze(value.map(item => decodeServicePattern(item, operation)))
}

function decodeManufacturerPattern(value: unknown, operation: string): ManufacturerDataPattern {
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['companyId', 'dataPrefix', 'mask'], operation)
  const companyId = record.companyId
  if (typeof companyId !== 'number' || !Number.isSafeInteger(companyId) || companyId < 0 || companyId > 0xffff) {
    throw malformed(operation)
  }
  return {
    companyId,
    ...decodeBytePair(record, operation)
  }
}

function decodeServicePattern(value: unknown, operation: string): ServiceDataPattern {
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['service', 'dataPrefix', 'mask'], operation)
  const service = record.service
  if (typeof service !== 'string' && typeof service !== 'number') throw malformed(operation)
  return {
    service,
    ...decodeBytePair(record, operation)
  }
}

function decodeBytePair(
  record: SerializableRecord,
  operation: string
): {
  readonly dataPrefix?: Readonly<Uint8Array>
  readonly mask?: Readonly<Uint8Array>
} {
  const dataPrefix = decodeOptionalBytes(record.dataPrefix, operation)
  const mask = decodeOptionalBytes(record.mask, operation)
  if (mask !== undefined && (dataPrefix === undefined || mask.byteLength !== dataPrefix.byteLength)) {
    throw malformed(operation)
  }
  return {
    ...(dataPrefix === undefined ? {} : { dataPrefix }),
    ...(mask === undefined ? {} : { mask })
  }
}

function decodeOptionalBytes(value: unknown, operation: string): Readonly<Uint8Array> | undefined {
  if (value === null) return undefined
  if (!(value instanceof Uint8Array) || value.byteLength === 0) throw malformed(operation)
  return new Uint8Array(value)
}

function decodeRssi(value: unknown, operation: string): ScanClause['rssi'] | undefined {
  if (value === null) return undefined
  const record = requiredRecord(value, operation)
  assertExactKeys(record, ['minimum', 'maximum'], operation)
  const minimum = optionalFiniteNumber(record.minimum, operation)
  const maximum = optionalFiniteNumber(record.maximum, operation)
  return { ...(minimum === undefined ? {} : { minimum }), ...(maximum === undefined ? {} : { maximum }) }
}

function decodeConnectable(value: unknown, operation: string): boolean | undefined {
  if (value === null) return undefined
  if (typeof value !== 'boolean') throw malformed(operation)
  return value
}

function optionalFiniteNumber(value: unknown, operation: string): number | undefined {
  if (value === null) return undefined
  if (typeof value !== 'number' || !Number.isFinite(value)) throw malformed(operation)
  return value
}

function requiredStrings(value: unknown, operation: string): readonly string[] {
  if (!Array.isArray(value) || value.some(item => typeof item !== 'string')) throw malformed(operation)
  return Object.freeze(value)
}

function requiredRecord(value: unknown, operation: string): SerializableRecord {
  if (!isSerializableRecord(value)) {
    throw malformed(operation)
  }
  return value
}

function isSerializableRecord(value: unknown): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function assertExactKeys(record: SerializableRecord, keys: readonly string[], operation: string): void {
  const actual = Object.keys(record).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw malformed(operation)
  }
}

function malformed(operation: string): Error {
  return contractError('protocol.malformed', 'ipc', operation)
}
