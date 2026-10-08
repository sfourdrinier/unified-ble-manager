// One scan generation's advertiser evidence.
//
// WinRT delivers the advertising packet and the scan response as separate
// observations. A name, a service UUID, and manufacturer data that arrived
// in different packets of the same generation still belong to one peer.
// Each raw packet stays exactly what that packet carried. A conjunction
// that needs more than one packet is a new observation, marked
// `core-merged`, and it does not claim an origin the radio did not report.

import type {
  AdvertisementField,
  AdvertisementObservation,
  ManufacturerData,
  ServiceDataEntry,
  SourceTimestamp
} from './advertisement'
import type { IpcAdvertisement } from '../ipc/manager'
import type { Uuid } from './primitives'
import { contractError } from './errors'

/** Each fact expires against the scan's advancing receipt clock. */
export const SCAN_EVIDENCE_WINDOW_MS = 10_000
export const SCAN_EVIDENCE_PEER_CAPACITY = 4096
const SCAN_EVIDENCE_FACT_CAPACITY = 1024

interface EvidenceBucket<Carried> {
  atMs: number
  facts: Map<string, Carried>
}

interface EvidenceTiming {
  atMs: number
  sourceTime: SourceTimestamp | null
  ingressOrdinal: number | null
}

interface CarriedAdvertisement extends EvidenceTiming {
  localName: string | null
  serviceUuids: readonly Uuid[] | null
  manufacturerData: readonly ManufacturerData[] | null
  serviceData: readonly ServiceDataEntry[] | null
  connectable: boolean | null
  rssi: number | null
}

interface CarriedIpc extends EvidenceTiming {
  localName: string | null
  serviceUuids: readonly string[] | null
  manufacturerData: IpcAdvertisement['manufacturerData'] | null
  serviceData: IpcAdvertisement['serviceData'] | null
  connectable: boolean | null
  rssi: number | null
}

/**
 * Evidence for one scan generation. Create one per scan and drop it when
 * that scan stops. A later generation id discards the previous generation.
 */
export class ScanEvidenceSession {
  private readonly advertisements = new Map<string, EvidenceBucket<CarriedAdvertisement>>()
  private readonly ipc = new Map<string, EvidenceBucket<CarriedIpc>>()

  /** Drop every peer. Call this when the scan generation ends. */
  clear(): void {
    this.advertisements.clear()
    this.ipc.clear()
  }

  /**
   * Record `observation` and return what a filter should see.
   * The raw packet wins when it already matches. Otherwise the in-window
   * union is tried, and only that union is marked `core-merged`.
   */
  matchAdvertisement<Attachment extends string>(
    observation: AdvertisementObservation<Attachment>,
    matches: (observation: AdvertisementObservation<Attachment>) => boolean
  ): AdvertisementObservation<Attachment> | null {
    // A merged snapshot was already qualified by an upstream cache. It can
    // satisfy this consumer, but its receipt is not another radio observation
    // of every carried fact. Re-caching it would renew borrowed evidence.
    if (observation.provenance === 'core-merged') return matches(observation) ? observation : null
    const atMs = Number(observation.receivedAtMonotonicMs)
    const key = advertisementKey(observation)
    const carried = remember(
      this.advertisements,
      key,
      atMs,
      carriedFromAdvertisement(observation),
      splitAdvertisement,
      mergeAdvertisement,
      emptyEvidence
    )
    if (matches(observation)) return observation
    const merged = mergedAdvertisement(observation, carried)
    if (merged !== null && matches(merged)) return merged
    return null
  }

  /**
   * The compact IPC advertisement shape. Empty service and data lists do
   * not erase a field an earlier packet in this generation carried.
   */
  matchIpc(
    observation: IpcAdvertisement,
    atMs: number,
    sessionId: string,
    matches: (observation: IpcAdvertisement) => boolean
  ): IpcAdvertisement | null {
    if (observation.provenance === 'core-merged') return matches(observation) ? observation : null
    const key = `${sessionId}\0${ipcKey(observation)}`
    const carried = remember(
      this.ipc,
      key,
      atMs,
      { ...carriedFromIpc(observation), atMs },
      splitIpc,
      mergeIpc,
      emptyEvidence
    )
    if (matches(observation)) return observation
    const merged = mergedIpc(observation, carried)
    if (merged !== null && matches(merged)) return merged
    return null
  }
}

function remember<Carried extends EvidenceTiming>(
  peers: Map<string, EvidenceBucket<Carried>>,
  key: string,
  atMs: number,
  incoming: Carried,
  split: (incoming: Carried) => ReadonlyMap<string, Carried>,
  merge: (previous: Carried, incoming: Carried) => Carried,
  empty: (atMs: number) => Carried
): Carried {
  if (!Number.isFinite(atMs) || atMs < 0)
    throw contractError('protocol.violation', 'scan', 'scan.evidence.receipt-clock')
  let newest = atMs
  for (const bucket of peers.values()) newest = Math.max(newest, bucket.atMs)
  // Expiration is driven by receipt progress. Keep bounded source ordering
  // watermarks for this active peer so late captures cannot renew old facts.
  // All other inactive peers are swept, including their ordering metadata.
  for (const [peer, bucket] of peers) {
    if (newest - bucket.atMs > SCAN_EVIDENCE_WINDOW_MS && peer !== key) peers.delete(peer)
    else
      for (const [fact, value] of bucket.facts) {
        if (newest - value.atMs > SCAN_EVIDENCE_WINDOW_MS) {
          if (!scopedSource(value.sourceTime)) bucket.facts.delete(fact)
          else
            bucket.facts.set(fact, {
              ...empty(value.atMs),
              sourceTime: value.sourceTime,
              ingressOrdinal: value.ingressOrdinal
            })
        }
      }
  }
  if (newest - atMs > SCAN_EVIDENCE_WINDOW_MS) return incoming
  const previous = peers.get(key)
  if (previous === undefined && peers.size >= SCAN_EVIDENCE_PEER_CAPACITY) {
    throw contractError('stream.quota', 'scan', 'scan.evidence.peer-capacity')
  }
  const facts = new Map(previous?.facts)
  for (const [fact, value] of split(incoming)) {
    const existing = facts.get(fact)
    const order = existing === undefined ? 1 : compareTiming(value, existing)
    // Compact IPC has no native ordinal. Preserve deterministic delivery
    // ordering when its receipt clock resolves consecutive packets equally.
    const receiptTie =
      existing !== undefined &&
      !comparable(value, existing) &&
      order === 0 &&
      value.ingressOrdinal === null &&
      existing.ingressOrdinal === null
    if (existing === undefined || order > 0 || receiptTie) {
      // A newer ingress ordinal on the same capture can replace its value,
      // but observing that capture twice does not make it fresh twice.
      const accepted =
        existing !== undefined && sameCapture(value, existing)
          ? { ...value, atMs: Math.min(existing.atMs, value.atMs) }
          : value
      facts.set(
        fact,
        newest - accepted.atMs > SCAN_EVIDENCE_WINDOW_MS
          ? { ...empty(accepted.atMs), sourceTime: accepted.sourceTime, ingressOrdinal: accepted.ingressOrdinal }
          : accepted
      )
    }
  }
  if (facts.size > SCAN_EVIDENCE_FACT_CAPACITY)
    throw contractError('stream.quota', 'scan', 'scan.evidence.fact-capacity')
  peers.set(key, { atMs: Math.max(previous?.atMs ?? atMs, atMs), facts })
  // The selected latest fact must be fresh and not from this packet's future.
  // Do not merge incoming again: that would undo the per-fact ordering choice.
  let carried = empty(atMs)
  for (const value of facts.values()) {
    if (newest - value.atMs <= SCAN_EVIDENCE_WINDOW_MS && compareTiming(value, incoming) <= 0)
      carried = merge(carried, value)
  }
  return carried
}

function scopedSource(source: SourceTimestamp | null): boolean {
  return source !== null && typeof source.clockScope === 'string' && source.clockScope.trim().length > 0
}

function comparable(left: EvidenceTiming, right: EvidenceTiming): boolean {
  return (
    scopedSource(left.sourceTime) &&
    scopedSource(right.sourceTime) &&
    left.sourceTime?.clockScope === right.sourceTime?.clockScope &&
    left.sourceTime?.origin === right.sourceTime?.origin
  )
}

function sameCapture(left: EvidenceTiming, right: EvidenceTiming): boolean {
  return comparable(left, right) && left.sourceTime?.monotonicMs === right.sourceTime?.monotonicMs
}

function compareTiming(left: EvidenceTiming, right: EvidenceTiming): number {
  if (comparable(left, right) && left.sourceTime !== null && right.sourceTime !== null) {
    const capture = Number(left.sourceTime.monotonicMs) - Number(right.sourceTime.monotonicMs)
    if (capture !== 0) return capture
    const ordinal = (left.ingressOrdinal ?? 0) - (right.ingressOrdinal ?? 0)
    if (ordinal !== 0) return ordinal
    return 0
  }
  const receipt = left.atMs - right.atMs
  return receipt !== 0 ? receipt : (left.ingressOrdinal ?? 0) - (right.ingressOrdinal ?? 0)
}

function emptyEvidence(atMs: number) {
  return {
    atMs,
    sourceTime: null,
    ingressOrdinal: null,
    localName: null,
    serviceUuids: null,
    manufacturerData: null,
    serviceData: null,
    connectable: null,
    rssi: null
  }
}

function advertisementKey<Attachment extends string>(observation: AdvertisementObservation<Attachment>): string {
  const device = observation.device
  return JSON.stringify([
    device.backendInstanceId,
    observation.scanSessionId,
    device.id,
    device.scope,
    device.address?.type,
    device.address?.value
  ])
}

function ipcKey(observation: IpcAdvertisement): string {
  return JSON.stringify([observation.peerId, observation.addressType, observation.address])
}

function presentValue<Value>(field: AdvertisementField<Value>): Value | null {
  return field.state === 'present' ? field.value : null
}

function carriedFromAdvertisement<Attachment extends string>(
  observation: AdvertisementObservation<Attachment>
): CarriedAdvertisement {
  const services = presentValue(observation.serviceUuids)
  const manufacturer = presentValue(observation.manufacturerData)
  const serviceData = presentValue(observation.serviceData)
  const name = presentValue(observation.localName)
  return {
    atMs: Number(observation.receivedAtMonotonicMs),
    sourceTime: presentValue(observation.sourceTimestamp),
    ingressOrdinal: observation.ingressOrdinal,
    localName: name !== null && name.length > 0 ? name : null,
    serviceUuids: services !== null && services.length > 0 ? services : null,
    manufacturerData: manufacturer !== null && manufacturer.length > 0 ? manufacturer : null,
    serviceData: serviceData !== null && serviceData.length > 0 ? serviceData : null,
    connectable: presentValue(observation.connectable),
    rssi: presentValue(observation.rssi)
  }
}

function mergeAdvertisement(previous: CarriedAdvertisement, incoming: CarriedAdvertisement): CarriedAdvertisement {
  return {
    atMs: incoming.atMs,
    sourceTime: incoming.sourceTime,
    ingressOrdinal: incoming.ingressOrdinal,
    localName: incoming.localName ?? previous.localName,
    serviceUuids: union(previous.serviceUuids, incoming.serviceUuids, value => value),
    manufacturerData: union(previous.manufacturerData, incoming.manufacturerData, value => value.companyIdentifier),
    serviceData: union(previous.serviceData, incoming.serviceData, value => value.serviceUuid),
    connectable: incoming.connectable ?? previous.connectable,
    rssi: incoming.rssi ?? previous.rssi
  }
}

function union<Value>(
  previous: readonly Value[] | null,
  incoming: readonly Value[] | null,
  key: (value: Value) => string | number
): readonly Value[] | null {
  if (previous === null) return incoming
  if (incoming === null) return previous
  const values = new Map([...previous, ...incoming].map(value => [key(value), value]))
  return Object.freeze([...values.values()])
}

function splitAdvertisement(incoming: CarriedAdvertisement): ReadonlyMap<string, CarriedAdvertisement> {
  const empty = {
    ...emptyEvidence(incoming.atMs),
    sourceTime: incoming.sourceTime,
    ingressOrdinal: incoming.ingressOrdinal
  }
  const facts = new Map<string, CarriedAdvertisement>()
  if (incoming.localName !== null) facts.set('name', { ...empty, localName: incoming.localName })
  if (incoming.connectable !== null) facts.set('connectable', { ...empty, connectable: incoming.connectable })
  if (incoming.rssi !== null) facts.set('rssi', { ...empty, rssi: incoming.rssi })
  for (const value of incoming.serviceUuids ?? []) facts.set(`service:${value}`, { ...empty, serviceUuids: [value] })
  for (const value of incoming.manufacturerData ?? [])
    facts.set(`manufacturer:${value.companyIdentifier}`, { ...empty, manufacturerData: [value] })
  for (const value of incoming.serviceData ?? [])
    facts.set(`data:${value.serviceUuid}`, { ...empty, serviceData: [value] })
  return facts
}

function carriedList<Value>(raw: readonly Value[] | null, carried: readonly Value[] | null): readonly Value[] | null {
  if (carried === null) return raw
  return raw !== null && raw.length === carried.length && carried.every(value => raw.includes(value)) ? raw : carried
}

function selectedField<Value>(
  field: AdvertisementField<Value>,
  raw: Value | null,
  selected: Value | null
): AdvertisementField<Value> {
  if (raw === selected) return field
  return selected === null
    ? Object.freeze({
        state: 'absent',
        provenance: 'derived',
        reason: 'No fresh source-ordered evidence for this packet'
      })
    : derived(selected)
}

function selectedList<Value>(
  field: AdvertisementField<readonly Value[]>,
  raw: readonly Value[] | null,
  selected: readonly Value[] | null
): AdvertisementField<readonly Value[]> {
  if (carriedList(raw, selected) === raw && selected !== null) return field
  if (selected === null) {
    if (raw === null || raw.length === 0) return field
    return Object.freeze({
      state: 'absent',
      provenance: 'derived',
      reason: 'No fresh source-ordered evidence for this packet'
    })
  }
  return derived(selected)
}

function derived<Value>(value: Value): AdvertisementField<Value> {
  return Object.freeze({ state: 'present', value, provenance: 'derived' })
}

function mergedAdvertisement<Attachment extends string>(
  observation: AdvertisementObservation<Attachment>,
  carried: CarriedAdvertisement
): AdvertisementObservation<Attachment> | null {
  const raw = carriedFromAdvertisement(observation)
  let added = false
  const localName = selectedField(observation.localName, raw.localName, carried.localName)
  const serviceUuids = selectedList(observation.serviceUuids, raw.serviceUuids, carried.serviceUuids)
  const manufacturerData = selectedList(observation.manufacturerData, raw.manufacturerData, carried.manufacturerData)
  const serviceData = selectedList(observation.serviceData, raw.serviceData, carried.serviceData)
  const connectable = selectedField(observation.connectable, raw.connectable, carried.connectable)
  const rssi = selectedField(observation.rssi, raw.rssi, carried.rssi)
  added =
    localName !== observation.localName ||
    serviceUuids !== observation.serviceUuids ||
    manufacturerData !== observation.manufacturerData ||
    serviceData !== observation.serviceData ||
    connectable !== observation.connectable ||
    rssi !== observation.rssi
  if (!added) return null
  const { origin: _origin, ...rest } = observation
  return Object.freeze({
    ...rest,
    provenance: 'core-merged',
    localName,
    serviceUuids,
    manufacturerData,
    serviceData,
    connectable,
    rssi
  })
}

function carriedFromIpc(observation: IpcAdvertisement): CarriedIpc {
  return {
    atMs: 0,
    sourceTime: null,
    ingressOrdinal: null,
    localName: observation.localName !== null && observation.localName.length > 0 ? observation.localName : null,
    serviceUuids: observation.serviceUuids.length > 0 ? observation.serviceUuids : null,
    manufacturerData: observation.manufacturerData.length > 0 ? observation.manufacturerData : null,
    serviceData: observation.serviceData.length > 0 ? observation.serviceData : null,
    connectable: observation.connectable ?? null,
    rssi: observation.rssi
  }
}

function mergeIpc(previous: CarriedIpc, incoming: CarriedIpc): CarriedIpc {
  return {
    atMs: incoming.atMs,
    sourceTime: incoming.sourceTime,
    ingressOrdinal: incoming.ingressOrdinal,
    localName: incoming.localName ?? previous.localName,
    serviceUuids: union(previous.serviceUuids, incoming.serviceUuids, value => value),
    manufacturerData: union(previous.manufacturerData, incoming.manufacturerData, value => value.companyId),
    serviceData: union(previous.serviceData, incoming.serviceData, value => value.uuid),
    connectable: incoming.connectable ?? previous.connectable,
    rssi: incoming.rssi ?? previous.rssi
  }
}

function splitIpc(incoming: CarriedIpc): ReadonlyMap<string, CarriedIpc> {
  const empty = {
    ...emptyEvidence(incoming.atMs),
    sourceTime: incoming.sourceTime,
    ingressOrdinal: incoming.ingressOrdinal
  }
  const facts = new Map<string, CarriedIpc>()
  if (incoming.localName !== null) facts.set('name', { ...empty, localName: incoming.localName })
  if (incoming.connectable !== null) facts.set('connectable', { ...empty, connectable: incoming.connectable })
  if (incoming.rssi !== null) facts.set('rssi', { ...empty, rssi: incoming.rssi })
  for (const value of incoming.serviceUuids ?? []) facts.set(`service:${value}`, { ...empty, serviceUuids: [value] })
  for (const value of incoming.manufacturerData ?? [])
    facts.set(`manufacturer:${value.companyId}`, { ...empty, manufacturerData: [value] })
  for (const value of incoming.serviceData ?? []) facts.set(`data:${value.uuid}`, { ...empty, serviceData: [value] })
  return facts
}

function mergedIpc(observation: IpcAdvertisement, carried: CarriedIpc): IpcAdvertisement | null {
  const raw = carriedFromIpc(observation)
  const localName = carried.localName
  const serviceUuids = carried.serviceUuids === null ? [] : (carriedList(raw.serviceUuids, carried.serviceUuids) ?? [])
  const manufacturerData =
    carried.manufacturerData === null ? [] : (carriedList(raw.manufacturerData, carried.manufacturerData) ?? [])
  const serviceData = carried.serviceData === null ? [] : (carriedList(raw.serviceData, carried.serviceData) ?? [])
  const connectable = carried.connectable
  const rssi = carried.rssi
  if (
    localName === observation.localName &&
    serviceUuids === observation.serviceUuids &&
    manufacturerData === observation.manufacturerData &&
    serviceData === observation.serviceData &&
    connectable === (observation.connectable ?? null) &&
    rssi === observation.rssi
  ) {
    return null
  }
  const { origin: _origin, ...rest } = observation
  return Object.freeze({
    ...rest,
    provenance: 'core-merged',
    localName,
    serviceUuids,
    manufacturerData,
    serviceData,
    connectable,
    rssi
  })
}
