import type { PeerReference } from './peer-reference'

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
