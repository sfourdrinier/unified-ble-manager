import { canonicalUuid } from '../backend-contract/primitives'
import { contractError } from '../backend-contract/errors'
import type { AdvertisementObservation } from '../backend-contract/advertisement'
import { assertPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'

export interface ManufacturerDataPattern {
  readonly companyId: number
  readonly dataPrefix?: Readonly<Uint8Array>
  readonly mask?: Readonly<Uint8Array>
}

export interface ServiceDataPattern {
  readonly service: string | number
  readonly dataPrefix?: Readonly<Uint8Array>
  readonly mask?: Readonly<Uint8Array>
}

export interface ScanQuery {
  readonly anyOf?: readonly ScanClause[]
  readonly exclude?: readonly ScanClause[]
}

export interface ScanClause {
  readonly peers?: readonly PeerReference[]
  readonly services?: {
    readonly any?: readonly (string | number)[]
    readonly all?: readonly (string | number)[]
  }
  readonly names?: {
    readonly exact?: readonly string[]
    readonly prefixes?: readonly string[]
  }
  readonly manufacturerData?: {
    readonly any?: readonly ManufacturerDataPattern[]
    readonly all?: readonly ManufacturerDataPattern[]
  }
  readonly serviceData?: {
    readonly any?: readonly ServiceDataPattern[]
    readonly all?: readonly ServiceDataPattern[]
  }
  readonly rssi?: {
    readonly minimum?: number
    readonly maximum?: number
  }
  readonly connectable?: boolean
}

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

interface CompactScanAdvertisement {
  readonly peerId: string
  readonly localName: string | null
  readonly rssi: number | null
  readonly serviceUuids: readonly string[]
  readonly manufacturerData: readonly { readonly companyId: number; readonly data: Uint8Array }[]
  readonly serviceData: readonly { readonly uuid: string; readonly data: Uint8Array }[]
}

export type ScanObservation = AdvertisementObservation<string> | CompactScanAdvertisement | NormalizedScanObservation

export function normalizeScanQuery(query: ScanQuery | undefined = {}): NormalizedScanQuery {
  assertObject(query, 'scan.query')
  assertKeys(query, ['anyOf', 'exclude'], 'scan.query')
  const anyOf = normalizeClauseList(query.anyOf, 'scan.query.anyOf', false)
  const exclude = normalizeClauseList(query.exclude, 'scan.query.exclude', true)
  const base = Object.freeze({
    anyOf,
    exclude
  })
  return Object.freeze({ ...base, digest: digestFor(base) })
}

export function normalizeScanObservation(observation: ScanObservation): NormalizedScanObservation {
  if (isNormalizedObservation(observation)) return cloneNormalizedObservation(observation)
  if (isIpcAdvertisement(observation)) {
    return Object.freeze({
      localName: observation.localName,
      rssi: observation.rssi,
      connectable: null,
      serviceUuids: Object.freeze(observation.serviceUuids.map(uuid => String(canonicalUuid(uuid)))),
      manufacturerData: Object.freeze(
        observation.manufacturerData.map(entry =>
          Object.freeze({ companyId: entry.companyId, data: copyBytes(entry.data) })
        )
      ),
      serviceData: Object.freeze(
        observation.serviceData.map(entry =>
          Object.freeze({ service: String(canonicalUuid(entry.uuid)), data: copyBytes(entry.data) })
        )
      )
    })
  }
  return Object.freeze({
    localName: fieldValue(observation.localName),
    rssi: fieldValue(observation.rssi),
    connectable: fieldValue(observation.connectable),
    serviceUuids: mapField(observation.serviceUuids, values => values.map(uuid => String(canonicalUuid(uuid)))),
    manufacturerData: mapField(observation.manufacturerData, values =>
      values.map(entry => Object.freeze({ companyId: entry.companyIdentifier, data: copyBytes(entry.value) }))
    ),
    serviceData: mapField(observation.serviceData, values =>
      values.map(entry =>
        Object.freeze({ service: String(canonicalUuid(entry.serviceUuid)), data: copyBytes(entry.value) })
      )
    )
  })
}

export function observationMatchesScanQuery(
  query: NormalizedScanQuery,
  observation: NormalizedScanObservation
): boolean {
  const positive = query.anyOf === null || query.anyOf.some(clause => clauseMatches(clause, observation))
  if (!positive) return false
  return query.exclude === null || !query.exclude.some(clause => clauseMatches(clause, observation))
}

function normalizeClauseList(
  clauses: readonly ScanClause[] | undefined,
  operation: string,
  omittedIsNull: boolean
): readonly NormalizedScanClause[] | null {
  if (clauses === undefined) return omittedIsNull ? null : null
  if (!Array.isArray(clauses) || clauses.length === 0) throw invalid(operation)
  return Object.freeze(clauses.map((clause, index) => normalizeClause(clause, `${operation}[${index}]`)))
}

function normalizeClause(clause: ScanClause, operation: string): NormalizedScanClause {
  assertObject(clause, operation)
  assertKeys(
    clause,
    ['peers', 'services', 'names', 'manufacturerData', 'serviceData', 'rssi', 'connectable'],
    operation
  )
  const peers = normalizePeerList(clause.peers, operation)
  const services = normalizeUuidField(clause.services, operation, 'services')
  const names = normalizeNames(clause.names, operation)
  const manufacturerData = normalizeManufacturerField(clause.manufacturerData, operation)
  const serviceData = normalizeServiceDataField(clause.serviceData, operation)
  const rssi = normalizeRssi(clause.rssi, operation)
  if (clause.connectable !== undefined && typeof clause.connectable !== 'boolean')
    throw invalid(`${operation}.connectable`)
  if (
    peers === null &&
    services === null &&
    names === null &&
    manufacturerData === null &&
    serviceData === null &&
    rssi === null &&
    clause.connectable === undefined
  ) {
    throw invalid(`${operation}.empty`)
  }
  return Object.freeze({ peers, services, names, manufacturerData, serviceData, rssi, connectable: clause.connectable })
}

function normalizePeerList(
  values: readonly PeerReference[] | undefined,
  operation: string
): readonly PeerReference[] | null {
  if (values === undefined) return null
  if (!Array.isArray(values) || values.length === 0) throw invalid(`${operation}.peers`)
  const references = values.map((value, index) => {
    try {
      assertPeerReference(value, `${operation}.peers[${index}]`)
      return Object.freeze({ ...value })
    } catch {
      throw invalid(`${operation}.peers[${index}]`)
    }
  })
  const uniqueReferences = new Map(references.map(reference => [peerReferenceKey(reference), reference]))
  return Object.freeze(
    [...uniqueReferences.entries()].sort(([left], [right]) => left.localeCompare(right)).map(([, value]) => value)
  )
}

function peerReferenceKey(reference: PeerReference): string {
  return `${reference.version}|${reference.backendId}|${reference.scope}|${reference.opaqueId}`
}

function normalizeUuidField(
  field: ScanClause['services'],
  operation: string,
  name: string
): NormalizedScanClause['services'] {
  if (field === undefined) return null
  assertObject(field, `${operation}.${name}`)
  assertKeys(field, ['any', 'all'], `${operation}.${name}`)
  const any = normalizeUuidList(field.any, `${operation}.${name}.any`)
  const all = normalizeUuidList(field.all, `${operation}.${name}.all`)
  if (any.length === 0 && all.length === 0) throw invalid(`${operation}.${name}`)
  return Object.freeze({ any, all })
}

function normalizeNames(field: ScanClause['names'], operation: string): NormalizedScanClause['names'] {
  if (field === undefined) return null
  assertObject(field, `${operation}.names`)
  assertKeys(field, ['exact', 'prefixes'], `${operation}.names`)
  const exact = normalizeStringList(field.exact, `${operation}.names.exact`)
  const prefixes = normalizeStringList(field.prefixes, `${operation}.names.prefixes`)
  if (exact.length === 0 && prefixes.length === 0) throw invalid(`${operation}.names`)
  return Object.freeze({ exact, prefixes })
}

function normalizeManufacturerField(
  field: ScanClause['manufacturerData'],
  operation: string
): NormalizedScanClause['manufacturerData'] {
  if (field === undefined) return null
  assertObject(field, `${operation}.manufacturerData`)
  assertKeys(field, ['any', 'all'], `${operation}.manufacturerData`)
  const any = normalizeManufacturerList(field.any, `${operation}.manufacturerData.any`)
  const all = normalizeManufacturerList(field.all, `${operation}.manufacturerData.all`)
  if (any.length === 0 && all.length === 0) throw invalid(`${operation}.manufacturerData`)
  return Object.freeze({ any, all })
}

function normalizeServiceDataField(
  field: ScanClause['serviceData'],
  operation: string
): NormalizedScanClause['serviceData'] {
  if (field === undefined) return null
  assertObject(field, `${operation}.serviceData`)
  assertKeys(field, ['any', 'all'], `${operation}.serviceData`)
  const any = normalizeServiceDataList(field.any, `${operation}.serviceData.any`)
  const all = normalizeServiceDataList(field.all, `${operation}.serviceData.all`)
  if (any.length === 0 && all.length === 0) throw invalid(`${operation}.serviceData`)
  return Object.freeze({ any, all })
}

function normalizeRssi(field: ScanClause['rssi'], operation: string): NormalizedScanClause['rssi'] {
  if (field === undefined) return null
  assertObject(field, `${operation}.rssi`)
  assertKeys(field, ['minimum', 'maximum'], `${operation}.rssi`)
  for (const [name, value] of Object.entries(field)) {
    if (value !== undefined && (typeof value !== 'number' || !Number.isFinite(value)))
      throw invalid(`${operation}.rssi.${name}`)
  }
  if (field.minimum === undefined && field.maximum === undefined) throw invalid(`${operation}.rssi`)
  if (field.minimum !== undefined && field.maximum !== undefined && field.minimum > field.maximum)
    throw invalid(`${operation}.rssi.range`)
  return Object.freeze({ minimum: field.minimum, maximum: field.maximum })
}

function normalizeUuidList(values: readonly (string | number)[] | undefined, operation: string): readonly string[] {
  if (values === undefined) return Object.freeze([])
  if (!Array.isArray(values) || values.length === 0) throw invalid(operation)
  const result: string[] = []
  const seen = new Set<string>()
  for (const value of values) {
    const uuid = String(canonicalUuid(typeof value === 'number' ? value.toString(16) : value))
    if (!seen.has(uuid)) {
      seen.add(uuid)
      result.push(uuid)
    }
  }
  return Object.freeze(result.sort())
}

function normalizeStringList(values: readonly string[] | undefined, operation: string): readonly string[] {
  if (values === undefined) return Object.freeze([])
  if (
    !Array.isArray(values) ||
    values.length === 0 ||
    values.some(value => typeof value !== 'string' || value.length === 0)
  ) {
    throw invalid(operation)
  }
  return Object.freeze(unique(values).sort())
}

function normalizeManufacturerList(
  values: readonly ManufacturerDataPattern[] | undefined,
  operation: string
): readonly NormalizedManufacturerDataPattern[] {
  if (values === undefined) return Object.freeze([])
  if (!Array.isArray(values) || values.length === 0) throw invalid(operation)
  return Object.freeze(
    values
      .map((value, index) => normalizeBytesPattern(value, `${operation}[${index}]`, 'companyId'))
      .sort((left, right) => patternKey(left).localeCompare(patternKey(right)))
  )
}

function normalizeServiceDataList(
  values: readonly ServiceDataPattern[] | undefined,
  operation: string
): readonly NormalizedServiceDataPattern[] {
  if (values === undefined) return Object.freeze([])
  if (!Array.isArray(values) || values.length === 0) throw invalid(operation)
  return Object.freeze(
    values
      .map((value, index) => {
        assertObject(value, `${operation}[${index}]`)
        assertKeys(value, ['service', 'dataPrefix', 'mask'], `${operation}[${index}]`)
        const pattern = normalizeByteFields(value.dataPrefix, value.mask, `${operation}[${index}]`)
        return Object.freeze({
          service: String(
            canonicalUuid(typeof value.service === 'number' ? value.service.toString(16) : value.service)
          ),
          ...pattern
        })
      })
      .sort((left, right) => patternKey(left).localeCompare(patternKey(right)))
  )
}

function patternKey(value: NormalizedManufacturerDataPattern | NormalizedServiceDataPattern): string {
  return JSON.stringify(value, (_key, entry: unknown) => (entry instanceof Uint8Array ? bytesToHex(entry) : entry))
}

function normalizeBytesPattern(
  value: ManufacturerDataPattern,
  operation: string,
  companyKey: 'companyId'
): NormalizedManufacturerDataPattern {
  assertObject(value, operation)
  assertKeys(value, [companyKey, 'dataPrefix', 'mask'], operation)
  if (!Number.isSafeInteger(value.companyId) || value.companyId < 0 || value.companyId > 0xffff)
    throw invalid(`${operation}.companyId`)
  return Object.freeze({ companyId: value.companyId, ...normalizeByteFields(value.dataPrefix, value.mask, operation) })
}

function normalizeByteFields(
  dataPrefix: Readonly<Uint8Array> | undefined,
  mask: Readonly<Uint8Array> | undefined,
  operation: string
): { readonly dataPrefix: Readonly<Uint8Array> | undefined; readonly mask: Readonly<Uint8Array> | undefined } {
  if (mask !== undefined && dataPrefix === undefined) throw invalid(`${operation}.mask-without-prefix`)
  if (dataPrefix !== undefined && (!(dataPrefix instanceof Uint8Array) || dataPrefix.byteLength === 0))
    throw invalid(`${operation}.dataPrefix`)
  if (mask !== undefined && (!(mask instanceof Uint8Array) || mask.byteLength !== dataPrefix?.byteLength))
    throw invalid(`${operation}.mask`)
  return {
    dataPrefix: dataPrefix === undefined ? undefined : copyBytes(dataPrefix),
    mask: mask === undefined ? undefined : copyBytes(mask)
  }
}

function clauseMatches(clause: NormalizedScanClause, observation: NormalizedScanObservation): boolean {
  const peerReference = observation.peerReference
  if (
    clause.peers !== null &&
    (peerReference === undefined || !clause.peers.some(peer => peerReferenceEqual(peer, peerReference)))
  )
    return false
  if (clause.services !== null && !matchesUuidField(clause.services.any, clause.services.all, observation.serviceUuids))
    return false
  if (clause.names !== null) {
    const localName = observation.localName
    if (
      localName === null ||
      (!clause.names.exact.includes(localName) && !clause.names.prefixes.some(prefix => localName.startsWith(prefix)))
    )
      return false
  }
  if (
    clause.manufacturerData !== null &&
    !matchesDataField(
      clause.manufacturerData.any,
      clause.manufacturerData.all,
      observation.manufacturerData,
      (pattern, entry) =>
        pattern.companyId === entry.companyId && matchesBytes(pattern.dataPrefix, pattern.mask, entry.data)
    )
  )
    return false
  if (
    clause.serviceData !== null &&
    !matchesDataField(
      clause.serviceData.any,
      clause.serviceData.all,
      observation.serviceData,
      (pattern, entry) =>
        pattern.service === entry.service && matchesBytes(pattern.dataPrefix, pattern.mask, entry.data)
    )
  )
    return false
  if (
    clause.rssi !== null &&
    (observation.rssi === null ||
      (clause.rssi.minimum !== undefined && observation.rssi < clause.rssi.minimum) ||
      (clause.rssi.maximum !== undefined && observation.rssi > clause.rssi.maximum))
  )
    return false
  return clause.connectable === undefined || observation.connectable === clause.connectable
}

function peerReferenceEqual(left: PeerReference, right: PeerReference): boolean {
  return (
    left.version === right.version &&
    left.backendId === right.backendId &&
    left.scope === right.scope &&
    left.opaqueId === right.opaqueId
  )
}

function matchesUuidField(any: readonly string[], all: readonly string[], observed: readonly string[] | null): boolean {
  if (observed === null) return any.length === 0 && all.length === 0
  return (
    (any.length === 0 || any.some(value => observed.includes(value))) && all.every(value => observed.includes(value))
  )
}

function matchesDataField<Pattern, Entry>(
  any: readonly Pattern[],
  all: readonly Pattern[],
  observed: readonly Entry[] | null,
  matches: (pattern: Pattern, entry: Entry) => boolean
): boolean {
  if (observed === null) return any.length === 0 && all.length === 0
  return (
    (any.length === 0 || any.some(pattern => observed.some(entry => matches(pattern, entry)))) &&
    all.every(pattern => observed.some(entry => matches(pattern, entry)))
  )
}

function matchesBytes(
  prefix: Readonly<Uint8Array> | undefined,
  mask: Readonly<Uint8Array> | undefined,
  observed: Readonly<Uint8Array>
): boolean {
  if (prefix === undefined) return true
  if (prefix.byteLength > observed.byteLength) return false
  for (let index = 0; index < prefix.byteLength; index += 1) {
    const maskByte = mask?.[index] ?? 0xff
    if (((observed[index] ?? 0) & maskByte) !== ((prefix[index] ?? 0) & maskByte)) return false
  }
  return true
}

function digestFor(value: Omit<NormalizedScanQuery, 'digest'>): string {
  const encoded = JSON.stringify(value, (key, entry: unknown) => {
    if (key === 'peers' && entry === null) return undefined
    return entry instanceof Uint8Array ? bytesToHex(entry) : entry
  })
  let hash = 0xcbf29ce484222325n
  for (let index = 0; index < encoded.length; index += 1) {
    hash ^= BigInt(encoded.charCodeAt(index))
    hash = BigInt.asUintN(64, hash * 0x100000001b3n)
  }
  return `scan-query-v1:${hash.toString(16).padStart(16, '0')}`
}

function fieldValue<Value>(
  field: { readonly state: string; readonly value?: Value } | Value | null | undefined
): Value | null {
  if (field === null || field === undefined) return null
  if (isStateField<Value>(field)) {
    return field.state === 'present' ? (field.value ?? null) : null
  }
  return field
}

function isStateField<Value>(value: unknown): value is { readonly state: string; readonly value?: Value } {
  return typeof value === 'object' && value !== null && 'state' in value
}

function mapField<Value, Result>(
  field: { readonly state: string; readonly value?: Value } | null | undefined,
  map: (value: Value) => Result
): Result | null {
  const value = fieldValue(field)
  return value === null ? null : Object.freeze(map(value))
}

function isIpcAdvertisement(value: ScanObservation): value is CompactScanAdvertisement {
  return (
    typeof value === 'object' &&
    value !== null &&
    'peerId' in value &&
    'manufacturerData' in value &&
    !('device' in value)
  )
}

function isNormalizedObservation(value: ScanObservation): value is NormalizedScanObservation {
  if (typeof value !== 'object' || value === null || 'device' in value) return false
  const localName = Reflect.get(value, 'localName')
  const serviceUuids = Reflect.get(value, 'serviceUuids')
  const manufacturerData = Reflect.get(value, 'manufacturerData')
  const serviceData = Reflect.get(value, 'serviceData')
  return (
    (typeof localName === 'string' || localName === null) &&
    (serviceUuids === null || Array.isArray(serviceUuids)) &&
    (manufacturerData === null || Array.isArray(manufacturerData)) &&
    (serviceData === null || Array.isArray(serviceData)) &&
    (manufacturerData === null ||
      manufacturerData.every(
        (entry: unknown) => typeof entry === 'object' && entry !== null && 'companyId' in entry
      )) &&
    (serviceData === null ||
      serviceData.every((entry: unknown) => typeof entry === 'object' && entry !== null && 'service' in entry))
  )
}

function cloneNormalizedObservation(value: NormalizedScanObservation): NormalizedScanObservation {
  return Object.freeze({
    ...value,
    serviceUuids: value.serviceUuids === null ? null : Object.freeze([...value.serviceUuids]),
    manufacturerData:
      value.manufacturerData === null
        ? null
        : Object.freeze(
            value.manufacturerData.map(entry =>
              Object.freeze({ companyId: entry.companyId, data: copyBytes(entry.data) })
            )
          ),
    serviceData:
      value.serviceData === null
        ? null
        : Object.freeze(
            value.serviceData.map(entry => Object.freeze({ service: entry.service, data: copyBytes(entry.data) }))
          )
  })
}

function copyBytes(value: Readonly<Uint8Array>): Uint8Array {
  return new Uint8Array(value)
}

function bytesToHex(value: Uint8Array): string {
  return [...value].map(byte => byte.toString(16).padStart(2, '0')).join('')
}

function unique(values: readonly string[]): string[] {
  return [...new Set(values)]
}

function assertObject(value: unknown, operation: string): void {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw invalid(operation)
}

function assertKeys(value: object, allowed: readonly string[], operation: string): void {
  for (const key of Object.keys(value)) if (!allowed.includes(key)) throw invalid(`${operation}.${key}`)
}

function invalid(operation: string): never {
  throw contractError('scan.filter-invalid', 'scan', operation)
}
