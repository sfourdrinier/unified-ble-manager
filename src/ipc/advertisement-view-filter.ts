import { canonicalUuid } from '../backend-contract/primitives'

export interface AdvertisementViewFilter {
  readonly nameContains?: string
  readonly minRssi?: number
  readonly maxRssi?: number
  readonly serviceUuid?: string
  readonly manufacturerCompanyId?: number
  readonly namedOnly?: boolean
}

export interface AdvertisementViewRecord {
  readonly peerId: string
  readonly localName: string | null
  readonly rssi: number | null
  readonly serviceUuids?: readonly string[]
  readonly manufacturerData?: readonly { readonly companyId: number }[]
}

/** Host-neutral UI filter over IPC advertisements. Native ScanFilter remains AND of radio criteria. */
export function advertisementPassesViewFilter(
  advertisement: AdvertisementViewRecord,
  filter: AdvertisementViewFilter
): boolean {
  if (filter.namedOnly === true && !hasLocalName(advertisement.localName)) {
    return false
  }
  if (!nameContains(advertisement, filter.nameContains)) {
    return false
  }
  if (!rssiAtLeast(advertisement.rssi, filter.minRssi)) {
    return false
  }
  if (!rssiAtMost(advertisement.rssi, filter.maxRssi)) {
    return false
  }
  if (!hasService(advertisement.serviceUuids, filter.serviceUuid)) {
    return false
  }
  return hasManufacturer(advertisement.manufacturerData, filter.manufacturerCompanyId)
}

function hasLocalName(localName: string | null): boolean {
  return localName !== null && localName.trim().length > 0
}

function nameContains(advertisement: AdvertisementViewRecord, query: string | undefined): boolean {
  if (query === undefined) {
    return true
  }
  const needle = query.trim().toLowerCase()
  if (needle.length === 0) {
    return true
  }
  if (advertisement.peerId.toLowerCase().includes(needle)) {
    return true
  }
  return advertisement.localName !== null && advertisement.localName.toLowerCase().includes(needle)
}

function rssiAtLeast(rssi: number | null, minRssi: number | undefined): boolean {
  if (minRssi === undefined) {
    return true
  }
  return rssi !== null && rssi >= minRssi
}

function rssiAtMost(rssi: number | null, maxRssi: number | undefined): boolean {
  if (maxRssi === undefined) {
    return true
  }
  return rssi !== null && rssi <= maxRssi
}

function hasService(serviceUuids: readonly string[] | undefined, wanted: string | undefined): boolean {
  if (wanted === undefined || wanted.trim().length === 0) {
    return true
  }
  const canonical = tryCanonicalUuid(wanted)
  if (canonical === null) {
    return false
  }
  if (serviceUuids === undefined) {
    return false
  }
  return serviceUuids.some(uuid => tryCanonicalUuid(uuid) === canonical)
}

function hasManufacturer(
  manufacturerData: readonly { readonly companyId: number }[] | undefined,
  companyId: number | undefined
): boolean {
  if (companyId === undefined) {
    return true
  }
  if (manufacturerData === undefined) {
    return false
  }
  return manufacturerData.some(entry => entry.companyId === companyId)
}

function tryCanonicalUuid(value: string): string | null {
  try {
    return canonicalUuid(value.replace(/^0x/i, ''))
  } catch {
    return null
  }
}
