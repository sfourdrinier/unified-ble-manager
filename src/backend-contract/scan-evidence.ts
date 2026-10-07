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
  ServiceDataEntry
} from './advertisement'
import type { IpcAdvertisement } from '../ipc/manager'
import type { Uuid } from './primitives'

/** Packets older than this, measured from the newest packet of that peer, leave the generation. */
export const SCAN_EVIDENCE_WINDOW_MS = 10_000

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
  private readonly advertisements = new Map<string, CarriedAdvertisement>()
  private readonly ipc = new Map<string, CarriedIpc>()

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
    const atMs = Number(observation.receivedAtMonotonicMs)
    const key = advertisementKey(observation)
    const carried = remember(this.advertisements, key, atMs, carriedFromAdvertisement(observation), mergeAdvertisement)
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
    const key = `${sessionId}\0${ipcKey(observation)}`
    const carried = remember(this.ipc, key, atMs, carriedFromIpc(observation), mergeIpc)
    if (matches(observation)) return observation
    const merged = mergedIpc(observation, carried)
    if (merged !== null && matches(merged)) return merged
    return null
  }

}

function remember<Carried extends { atMs: number }>(
  peers: Map<string, Carried>,
  key: string,
  atMs: number,
  incoming: Carried,
  merge: (previous: Carried, incoming: Carried) => Carried
): Carried {
  const previous = peers.get(key)
  if (previous === undefined || !Number.isFinite(atMs) || !Number.isFinite(previous.atMs)) {
    const stored = { ...incoming, atMs: Number.isFinite(atMs) ? atMs : incoming.atMs }
    peers.set(key, stored)
    return stored
  }
  const gap = Math.abs(atMs - previous.atMs)
  if (gap > SCAN_EVIDENCE_WINDOW_MS) {
    // This packet cannot borrow the other one. Keep the newer packet for
    // whoever arrives inside its window next.
    if (atMs >= previous.atMs) peers.set(key, { ...incoming, atMs })
    return { ...incoming, atMs }
  }
  const stored = atMs >= previous.atMs ? merge(previous, incoming) : merge({ ...incoming, atMs }, previous)
  const stamped = { ...stored, atMs: Math.max(atMs, previous.atMs) }
  peers.set(key, stamped)
  return stamped
}

function advertisementKey<Attachment extends string>(observation: AdvertisementObservation<Attachment>): string {
  const address = observation.device.address
  if (address !== null && address.value.length > 0) return `addr:${address.value}`
  return `peer:${String(observation.device.id)}`
}

function ipcKey(observation: IpcAdvertisement): string {
  if (typeof observation.address === 'string' && observation.address.length > 0) return `addr:${observation.address}`
  return `peer:${observation.peerId}`
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
    serviceUuids: incoming.serviceUuids ?? previous.serviceUuids,
    manufacturerData: incoming.manufacturerData ?? previous.manufacturerData,
    serviceData: incoming.serviceData ?? previous.serviceData,
    connectable: incoming.connectable ?? previous.connectable,
    rssi: incoming.rssi ?? previous.rssi
  }
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
    raw.serviceUuids === null && carried.serviceUuids !== null
      ? derived(carried.serviceUuids)
      : observation.serviceUuids
  const manufacturerData =
    raw.manufacturerData === null && carried.manufacturerData !== null
      ? derived(carried.manufacturerData)
      : observation.manufacturerData
  const serviceData =
    raw.serviceData === null && carried.serviceData !== null ? derived(carried.serviceData) : observation.serviceData
  const connectable =
    raw.connectable === null && carried.connectable !== null
      ? derived(carried.connectable)
      : observation.connectable
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
    serviceUuids: incoming.serviceUuids ?? previous.serviceUuids,
    manufacturerData: incoming.manufacturerData ?? previous.manufacturerData,
    serviceData: incoming.serviceData ?? previous.serviceData,
    connectable: incoming.connectable ?? previous.connectable,
    rssi: incoming.rssi ?? previous.rssi
  }
}

function mergedIpc(observation: IpcAdvertisement, carried: CarriedIpc): IpcAdvertisement | null {
  const raw = carriedFromIpc(observation)
  const localName = raw.localName === null && carried.localName !== null ? carried.localName : observation.localName
  const serviceUuids =
    raw.serviceUuids === null && carried.serviceUuids !== null ? carried.serviceUuids : observation.serviceUuids
  const manufacturerData =
    raw.manufacturerData === null && carried.manufacturerData !== null
      ? carried.manufacturerData
      : observation.manufacturerData
  const serviceData =
    raw.serviceData === null && carried.serviceData !== null ? carried.serviceData : observation.serviceData
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
