// One scan generation's advertiser evidence.
//
// WinRT delivers the advertising packet and the scan response as separate
// observations. A name, a service UUID, and manufacturer data that arrived
// in different packets of the same generation still belong to one peer.
// Each raw packet stays exactly what that packet carried. A conjunction
// that needs more than one packet is a new observation, marked
// `core-merged`, and it does not claim an origin the radio did not report.

import type { AdvertisementField, AdvertisementObservation, ManufacturerData, ServiceDataEntry } from './advertisement'
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

interface CarriedAdvertisement {
  atMs: number
  localName: string | null
  serviceUuids: readonly Uuid[] | null
  manufacturerData: readonly ManufacturerData[] | null
  serviceData: readonly ServiceDataEntry[] | null
  connectable: boolean | null
  rssi: number | null
}

interface CarriedIpc {
  atMs: number
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

function remember<Carried extends { atMs: number }>(
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
  // Expiration is driven by receipt progress, even for peers that never advertise again.
  for (const [peer, bucket] of peers) {
    if (newest - bucket.atMs > SCAN_EVIDENCE_WINDOW_MS) peers.delete(peer)
    else
      for (const [fact, value] of bucket.facts) {
        if (newest - value.atMs > SCAN_EVIDENCE_WINDOW_MS) bucket.facts.delete(fact)
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
    if (existing === undefined || value.atMs >= existing.atMs) facts.set(fact, value)
  }
  if (facts.size > SCAN_EVIDENCE_FACT_CAPACITY)
    throw contractError('stream.quota', 'scan', 'scan.evidence.fact-capacity')
  peers.set(key, { atMs: Math.max(previous?.atMs ?? atMs, atMs), facts })
  // An out-of-order packet must not borrow observations from its future.
  let carried = empty(atMs)
  for (const value of [...facts.values()].sort((left, right) => left.atMs - right.atMs)) {
    if (value.atMs <= atMs) carried = merge(carried, value)
  }
  return merge(carried, incoming)
}

function emptyEvidence(atMs: number) {
  return {
    atMs,
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
  const empty = emptyEvidence(incoming.atMs)
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

function derived<Value>(value: Value): AdvertisementField<Value> {
  return Object.freeze({ state: 'present', value, provenance: 'derived' })
}

function mergedAdvertisement<Attachment extends string>(
  observation: AdvertisementObservation<Attachment>,
  carried: CarriedAdvertisement
): AdvertisementObservation<Attachment> | null {
  const raw = carriedFromAdvertisement(observation)
  let added = false
  const localName =
    raw.localName === null && carried.localName !== null ? derived(carried.localName) : observation.localName
  const serviceUuids =
    carriedList(raw.serviceUuids, carried.serviceUuids) !== raw.serviceUuids && carried.serviceUuids !== null
      ? derived(carried.serviceUuids)
      : observation.serviceUuids
  const manufacturerData =
    carriedList(raw.manufacturerData, carried.manufacturerData) !== raw.manufacturerData &&
    carried.manufacturerData !== null
      ? derived(carried.manufacturerData)
      : observation.manufacturerData
  const serviceData =
    carriedList(raw.serviceData, carried.serviceData) !== raw.serviceData && carried.serviceData !== null
      ? derived(carried.serviceData)
      : observation.serviceData
  const connectable =
    raw.connectable === null && carried.connectable !== null ? derived(carried.connectable) : observation.connectable
  const rssi = raw.rssi === null && carried.rssi !== null ? derived(carried.rssi) : observation.rssi
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
    localName: incoming.localName ?? previous.localName,
    serviceUuids: union(previous.serviceUuids, incoming.serviceUuids, value => value),
    manufacturerData: union(previous.manufacturerData, incoming.manufacturerData, value => value.companyId),
    serviceData: union(previous.serviceData, incoming.serviceData, value => value.uuid),
    connectable: incoming.connectable ?? previous.connectable,
    rssi: incoming.rssi ?? previous.rssi
  }
}

function splitIpc(incoming: CarriedIpc): ReadonlyMap<string, CarriedIpc> {
  const empty = emptyEvidence(incoming.atMs)
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
  const localName = raw.localName === null && carried.localName !== null ? carried.localName : observation.localName
  const serviceUuids = carriedList(raw.serviceUuids, carried.serviceUuids) ?? observation.serviceUuids
  const manufacturerData = carriedList(raw.manufacturerData, carried.manufacturerData) ?? observation.manufacturerData
  const serviceData = carriedList(raw.serviceData, carried.serviceData) ?? observation.serviceData
  const connectable =
    raw.connectable === null && carried.connectable !== null ? carried.connectable : (observation.connectable ?? null)
  const rssi = raw.rssi === null && carried.rssi !== null ? carried.rssi : observation.rssi
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
