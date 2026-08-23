import { canonicalUuid } from '../backend-contract/primitives'
import { contractError } from '../backend-contract/errors'
import type { AdvertisementObservation } from '../backend-contract/advertisement'
import { canonicalScanQueryJson, scanQueryDigest } from '../backend-contract/scan-query'
import { assertPeerReference, encodePeerReference, isPeerReference, snapshotPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'
import type {
  NormalizedManufacturerDataPattern,
  NormalizedScanClause,
  NormalizedScanObservation,
  NormalizedScanQuery,
  NormalizedServiceDataPattern
} from '../backend-contract/scan-query'
export type {
  NormalizedManufacturerDataPattern,
  NormalizedScanClause,
  NormalizedScanObservation,
  NormalizedScanQuery,
  NormalizedServiceDataPattern
} from '../backend-contract/scan-query'

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

interface CompactScanAdvertisement {
  readonly peerId: string
  readonly peerReference?: PeerReference
  readonly localName: string | null
  readonly rssi: number | null
  readonly txPowerLevel: number | null
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
  return Object.freeze({ ...base, digest: scanQueryDigest(base) })
}

export function normalizeScanObservation(observation: ScanObservation): NormalizedScanObservation {
  if (isNormalizedObservation(observation)) return cloneNormalizedObservation(observation)
  if (isIpcAdvertisement(observation)) {
    const peerReference =
      observation.peerReference === undefined
        ? undefined
        : snapshotPeerReference(observation.peerReference, 'scan.observation.peer-reference')
    return Object.freeze({
      ...(peerReference === undefined ? {} : { peerReference }),
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
  if (isUnscopedObservation(observation) || !isNativeObservationShape(observation)) throw invalid('scan.observation')
  const peerReference =
    observation.peerReference === undefined
      ? undefined
      : snapshotPeerReference(observation.peerReference, 'scan.observation.peer-reference')
  return Object.freeze({
    ...(peerReference === undefined ? {} : { peerReference }),
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
  const normalized = clauses.map((clause, index) => normalizeClause(clause, `${operation}[${index}]`))
  return Object.freeze(
    normalized.sort((left, right) => compareCanonical(canonicalScanQueryJson(left), canonicalScanQueryJson(right)))
  )
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
    assertPeerReference(value, `${operation}.peers[${index}]`)
    return Object.freeze({
      version: 1,
      backendId: value.backendId,
      scope: value.scope,
      opaqueId: value.opaqueId
    })
  })
  const uniqueReferences = new Map(references.map(reference => [peerReferenceKey(reference), reference]))
  return Object.freeze(
    [...uniqueReferences.entries()].sort(([left], [right]) => compareCanonical(left, right)).map(([, value]) => value)
  )
}

function peerReferenceKey(reference: PeerReference): string {
  return encodePeerReference(reference)
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
      .sort((left, right) => compareCanonical(patternKey(left), patternKey(right)))
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
      .sort((left, right) => compareCanonical(patternKey(left), patternKey(right)))
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

function compareCanonical(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
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
  if (typeof value !== 'object' || value === null || !('state' in value)) return false
  const state = Reflect.get(value, 'state')
  if (state === 'present') {
    if (!('value' in value) || !('provenance' in value)) throw invalid('scan.observation.field-present')
    return true
  }
  if (state === 'absent' || state === 'unavailable') {
    if (!('reason' in value) || !('provenance' in value)) throw invalid('scan.observation.field-absent')
    return true
  }
  throw invalid('scan.observation.field-state')
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
    !('device' in value) &&
    hasExactObservationKeys(
      value,
      ['peerId', 'localName', 'rssi', 'serviceUuids', 'manufacturerData', 'serviceData'],
      ['peerReference', 'txPowerLevel']
    ) &&
    isIpcAdvertisementValues(value)
  )
}

function isIpcAdvertisementValues(value: ScanObservation): boolean {
  if (typeof value !== 'object' || value === null || !('peerId' in value)) return false
  const candidate = value
  return (
    typeof candidate.peerId === 'string' &&
    candidate.peerId.length > 0 &&
    (candidate.peerReference === undefined || isPeerReference(candidate.peerReference)) &&
    (candidate.localName === null || typeof candidate.localName === 'string') &&
    (candidate.rssi === null || (typeof candidate.rssi === 'number' && Number.isFinite(candidate.rssi))) &&
    (candidate.txPowerLevel === undefined ||
      candidate.txPowerLevel === null ||
      (typeof candidate.txPowerLevel === 'number' && Number.isFinite(candidate.txPowerLevel))) &&
    isUuidList(candidate.serviceUuids) &&
    isIpcManufacturerDataList(candidate.manufacturerData) &&
    isIpcServiceDataList(candidate.serviceData)
  )
}

function isIpcManufacturerDataList(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.every(
      entry =>
        typeof entry === 'object' &&
        entry !== null &&
        hasExactObjectKeys(entry, ['companyId', 'data']) &&
        Number.isSafeInteger(Reflect.get(entry, 'companyId')) &&
        Reflect.get(entry, 'companyId') >= 0 &&
        Reflect.get(entry, 'companyId') <= 0xffff &&
        Reflect.get(entry, 'data') instanceof Uint8Array
    )
  )
}

function isIpcServiceDataList(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.every(
      entry =>
        typeof entry === 'object' &&
        entry !== null &&
        hasExactObjectKeys(entry, ['uuid', 'data']) &&
        isUuidValue(Reflect.get(entry, 'uuid')) &&
        Reflect.get(entry, 'data') instanceof Uint8Array
    )
  )
}

function isUnscopedObservation(value: ScanObservation): boolean {
  return typeof value === 'object' && value !== null && !('device' in value) && !('peerId' in value)
}

function isNativeObservationShape(value: ScanObservation): boolean {
  if (typeof value !== 'object' || value === null || !('device' in value)) return false
  const required = [
    'device',
    'provenance',
    'sourceTimestamp',
    'receivedAtMonotonicMs',
    'ingressOrdinal',
    'scanSessionId',
    'localName',
    'rssi',
    'txPower',
    'connectable',
    'appearance',
    'serviceUuids',
    'solicitedServiceUuids',
    'overflowServiceUuids',
    'serviceData',
    'manufacturerData',
    'rawRecord',
    'scanResponseRecord'
  ]
  const keys = Object.keys(value)
    .filter(key => key !== 'peerReference')
    .sort()
  return (
    required
      .slice()
      .sort()
      .every((key, index) => keys[index] === key) &&
    keys.length === required.length &&
    isNativeObservationValues(value)
  )
}

function isNativeObservationValues(value: ScanObservation): boolean {
  if (typeof value !== 'object' || value === null || !('device' in value)) return false
  const native = value
  return (
    isDeviceIdentity(native.device) &&
    isObservationSource(native.provenance) &&
    isAdvertisementField(native.sourceTimestamp, isSourceTimestamp) &&
    Number.isFinite(native.receivedAtMonotonicMs) &&
    Number.isSafeInteger(native.ingressOrdinal) &&
    typeof native.scanSessionId === 'string' &&
    isAdvertisementField(native.localName, field => typeof field === 'string') &&
    isAdvertisementField(native.rssi, field => typeof field === 'number' && Number.isFinite(field)) &&
    isAdvertisementField(native.txPower, field => typeof field === 'number' && Number.isFinite(field)) &&
    isAdvertisementField(native.connectable, field => typeof field === 'boolean') &&
    isAdvertisementField(native.appearance, field => typeof field === 'number' && Number.isFinite(field)) &&
    isAdvertisementField(native.serviceUuids, isUuidList) &&
    isAdvertisementField(native.solicitedServiceUuids, isUuidList) &&
    isAdvertisementField(native.overflowServiceUuids, isUuidList) &&
    isAdvertisementField(native.serviceData, isServiceDataList) &&
    isAdvertisementField(native.manufacturerData, isManufacturerDataList) &&
    isAdvertisementField(native.rawRecord, field => field instanceof Uint8Array) &&
    isAdvertisementField(native.scanResponseRecord, field => field instanceof Uint8Array) &&
    (native.peerReference === undefined || isPeerReference(native.peerReference))
  )
}

function isAdvertisementField(field: unknown, isValue: (value: unknown) => boolean): boolean {
  if (typeof field !== 'object' || field === null || Array.isArray(field)) return false
  const state = Reflect.get(field, 'state')
  const provenance = Reflect.get(field, 'provenance')
  if (!isFieldProvenance(provenance)) return false
  if (state === 'present') {
    const keys = Object.keys(field).sort()
    return keys.join(',') === 'provenance,state,value' && 'value' in field && isValue(Reflect.get(field, 'value'))
  }
  if (state === 'absent' || state === 'unavailable') {
    const keys = Object.keys(field).sort()
    return keys.join(',') === 'provenance,reason,state' && typeof Reflect.get(field, 'reason') === 'string'
  }
  return false
}

function isFieldProvenance(value: unknown): boolean {
  return value === 'observed' || value === 'derived' || value === 'synthesized' || value === 'not-provided'
}

function isObservationSource(value: unknown): boolean {
  return value === 'platform-raw' || value === 'platform-derived' || value === 'core-merged'
}

function isSourceTimestamp(value: unknown): boolean {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  return (
    Object.keys(value).sort().join(',') === 'monotonicMs,origin' &&
    typeof Reflect.get(value, 'monotonicMs') === 'number' &&
    Number.isFinite(Reflect.get(value, 'monotonicMs')) &&
    (Reflect.get(value, 'origin') === 'platform' || Reflect.get(value, 'origin') === 'backend')
  )
}

function isUuidList(value: unknown): boolean {
  return Array.isArray(value) && value.every(uuid => isCanonicalUuid(uuid))
}

function isServiceDataList(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.every(
      entry =>
        typeof entry === 'object' &&
        entry !== null &&
        Object.keys(entry).sort().join(',') === 'serviceUuid,value' &&
        isCanonicalUuid(Reflect.get(entry, 'serviceUuid')) &&
        Reflect.get(entry, 'value') instanceof Uint8Array
    )
  )
}

function isManufacturerDataList(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.every(
      entry =>
        typeof entry === 'object' &&
        entry !== null &&
        Object.keys(entry).sort().join(',') === 'companyIdentifier,value' &&
        Number.isSafeInteger(Reflect.get(entry, 'companyIdentifier')) &&
        Reflect.get(entry, 'companyIdentifier') >= 0 &&
        Reflect.get(entry, 'companyIdentifier') <= 0xffff &&
        Reflect.get(entry, 'value') instanceof Uint8Array
    )
  )
}

function isDeviceIdentity(value: unknown): boolean {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const address = Reflect.get(value, 'address')
  const addressValid =
    address === null ||
    (typeof address === 'object' &&
      address !== null &&
      Object.keys(address).sort().join(',') === 'type,value' &&
      typeof Reflect.get(address, 'value') === 'string' &&
      (Reflect.get(address, 'type') === 'public' ||
        Reflect.get(address, 'type') === 'random' ||
        Reflect.get(address, 'type') === 'opaque'))
  return (
    Object.keys(value).sort().join(',') === 'address,backendInstanceId,id,scope,stableAcrossRestarts' &&
    typeof Reflect.get(value, 'id') === 'string' &&
    typeof Reflect.get(value, 'backendInstanceId') === 'string' &&
    (Reflect.get(value, 'scope') === 'session' ||
      Reflect.get(value, 'scope') === 'application' ||
      Reflect.get(value, 'scope') === 'backend') &&
    (typeof Reflect.get(value, 'stableAcrossRestarts') === 'boolean' ||
      Reflect.get(value, 'stableAcrossRestarts') === null) &&
    addressValid
  )
}

function isNormalizedObservation(value: ScanObservation): value is NormalizedScanObservation {
  if (typeof value !== 'object' || value === null || 'device' in value) return false
  if (
    !hasExactObservationKeys(
      value,
      ['localName', 'rssi', 'connectable', 'serviceUuids', 'manufacturerData', 'serviceData'],
      ['peerReference']
    )
  )
    return false
  const localName = Reflect.get(value, 'localName')
  const rssi = Reflect.get(value, 'rssi')
  const connectable = Reflect.get(value, 'connectable')
  const serviceUuids = Reflect.get(value, 'serviceUuids')
  const manufacturerData = Reflect.get(value, 'manufacturerData')
  const serviceData = Reflect.get(value, 'serviceData')
  return (
    (typeof localName === 'string' || localName === null) &&
    (typeof rssi === 'number' ? Number.isFinite(rssi) : rssi === null) &&
    (typeof connectable === 'boolean' || connectable === null) &&
    (serviceUuids === null || (Array.isArray(serviceUuids) && serviceUuids.every(uuid => isCanonicalUuid(uuid)))) &&
    (manufacturerData === null ||
      (Array.isArray(manufacturerData) && manufacturerData.every(isNormalizedManufacturerEntry))) &&
    (serviceData === null || (Array.isArray(serviceData) && serviceData.every(isNormalizedServiceEntry))) &&
    (value.peerReference === undefined || isPeerReference(value.peerReference))
  )
}

function isNormalizedManufacturerEntry(
  entry: { readonly companyId: number; readonly data: Uint8Array } | null
): boolean {
  return (
    entry !== null &&
    typeof entry === 'object' &&
    hasExactObjectKeys(entry, ['companyId', 'data']) &&
    Number.isSafeInteger(entry.companyId) &&
    entry.companyId >= 0 &&
    entry.companyId <= 0xffff &&
    entry.data instanceof Uint8Array
  )
}

function isNormalizedServiceEntry(entry: { readonly service: string; readonly data: Uint8Array } | null): boolean {
  return (
    entry !== null &&
    typeof entry === 'object' &&
    hasExactObjectKeys(entry, ['service', 'data']) &&
    isCanonicalUuid(entry.service) &&
    entry.data instanceof Uint8Array
  )
}

function isCanonicalUuid(value: string): boolean {
  if (typeof value !== 'string') return false
  try {
    return String(canonicalUuid(value)) === value
  } catch {
    return false
  }
}

function isUuidValue(value: unknown): boolean {
  return typeof value === 'string' && isCanonicalUuid(value)
}

function hasExactObservationKeys(
  value: object,
  requiredKeys: readonly string[],
  optionalKeys: readonly string[]
): boolean {
  const actualKeys = Object.keys(value)
  return (
    requiredKeys.every(key => actualKeys.includes(key)) &&
    actualKeys.every(key => requiredKeys.includes(key) || optionalKeys.includes(key))
  )
}

function hasExactObjectKeys(value: object, expectedKeys: readonly string[]): boolean {
  const actualKeys = Object.keys(value).sort()
  const sortedExpected = [...expectedKeys].sort()
  return actualKeys.length === sortedExpected.length && actualKeys.every((key, index) => key === sortedExpected[index])
}

function cloneNormalizedObservation(value: NormalizedScanObservation): NormalizedScanObservation {
  const peerReference =
    value.peerReference === undefined
      ? undefined
      : snapshotPeerReference(value.peerReference, 'scan.observation.peer-reference')
  return Object.freeze({
    ...(peerReference === undefined ? {} : { peerReference }),
    localName: value.localName,
    rssi: value.rssi,
    connectable: value.connectable,
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
