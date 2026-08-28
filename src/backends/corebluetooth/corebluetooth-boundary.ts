// src/backends/corebluetooth/corebluetooth-boundary.ts

import type {
  BlePhy,
  ConnectionControlCapabilities,
  ConnectionPriority,
  PhyPreference
} from '../../backend-contract/connection-controls'
import type { ConnectionIntent } from '../../backend-contract/backend'

/**
 * Typed, bytes-first boundary between the CoreBluetooth addon and the shared
 * backend. Native peripheral identifiers remain inside this boundary; callers
 * only receive backend-issued opaque identities.
 *
 * `peer:address-targeting` (the `addresses` scan clause and the address form of
 * `connect()`) deliberately does not revise this decision for CoreBluetooth:
 * the platform cannot bootstrap a connection from a radio address at all, so
 * this backend keeps the capability unregistered and fails closed with
 * `capability.unsupported`. Re-entry to a known peer on Apple platforms remains
 * the durable `PeerReference`/restoration path, which never exports the native
 * CBPeripheral identifier across this boundary.
 */
export interface CoreBluetoothScanPlatformOptions {
  readonly kind: 'android' | 'corebluetooth' | 'winrt' | 'web' | 'electron' | 'tauri'
  readonly mode?: 'low-power' | 'balanced' | 'low-latency' | 'opportunistic'
  readonly callbackType?: 'all-matches' | 'first-match' | 'match-lost'
  readonly reportDelayMs?: number
  readonly legacy?: boolean
  readonly phy?: 'all-supported' | '1m' | 'coded'
}

export interface CoreBluetoothAdapterSnapshot {
  readonly availability: 'available' | 'unavailable' | 'unsupported' | 'unknown'
  /**
   * `'unknown'` when the platform exposes no per-application Bluetooth
   * authorization concept at all, or when this host did not query one. It is
   * the absence of a measurement and never a denial: `'not-determined'`
   * asserts a pending user decision and `'unavailable'` asserts the platform
   * withheld access, so a host that did not measure reports `'unknown'`,
   * exactly as `availability` and `power` already do. `safeReason` states why.
   */
  readonly authorization: 'granted' | 'denied' | 'restricted' | 'not-determined' | 'unavailable' | 'unknown'
  readonly power: 'on' | 'off' | 'resetting' | 'unsupported' | 'unknown'
  readonly safeReason: string | null
}

export interface CoreBluetoothAdvertisement {
  readonly nativePeerId: string
  readonly localName: string | null
  readonly rssi: number | null
  readonly serviceUuids: readonly string[] | null
  readonly txPower?: number | null
  readonly connectable?: boolean | null
  readonly appearance?: number | null
  readonly solicitedServiceUuids?: readonly string[] | null
  readonly overflowServiceUuids?: readonly string[] | null
  readonly serviceData?: readonly CoreBluetoothServiceDataEntry[] | null
  readonly manufacturerData?: readonly CoreBluetoothManufacturerData[] | null
  readonly rawRecord?: Readonly<Uint8Array> | null
  readonly scanResponseRecord?: Readonly<Uint8Array> | null
}

export interface CoreBluetoothServiceDataEntry {
  readonly serviceUuid: string
  readonly value: Readonly<Uint8Array>
}

export interface CoreBluetoothManufacturerData {
  readonly companyIdentifier: number
  readonly value: Readonly<Uint8Array>
}

export interface CoreBluetoothCharacteristicRecord {
  readonly uuid: string
  readonly occurrence: number
  readonly readable: boolean
  readonly writableWithResponse: boolean
  readonly writableWithoutResponse: boolean
  readonly notifiable: boolean
  readonly indicatable?: boolean
  readonly descriptors: readonly CoreBluetoothDescriptorRecord[]
}

export interface CoreBluetoothDescriptorRecord {
  readonly uuid: string
  readonly occurrence: number
}

export interface CoreBluetoothServiceRecord {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: readonly CoreBluetoothCharacteristicRecord[]
}

export interface CoreBluetoothGattSnapshot {
  readonly services: readonly CoreBluetoothServiceRecord[]
}

export interface CoreBluetoothWriteReadinessSnapshot {
  readonly nativePeerId: string
  readonly connectionGeneration: string
  readonly ready: boolean
  readonly ordinal: number
}

export interface CoreBluetoothWriteReadinessEvent {
  readonly nativePeerId: string
  readonly connectionGeneration: string
  readonly ready: boolean
  readonly ordinal: number
}

export interface CoreBluetoothCharacteristicAddress {
  readonly nativePeerId: string
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence: number
}

export interface CoreBluetoothDescriptorAddress extends CoreBluetoothCharacteristicAddress {
  readonly descriptorUuid: string
  readonly descriptorOccurrence: number
}

export interface CoreBluetoothPhyObservation {
  readonly txPhy: BlePhy
  readonly rxPhy: BlePhy
}

export interface CoreBluetoothPhyRequestResult {
  readonly accepted: boolean
  readonly observation: CoreBluetoothPhyObservation | null
}

export interface CoreBluetoothBoundary {
  /** A platform declares an unavailable control before the core submits any native command. */
  readonly connectionControlCapabilities?: ConnectionControlCapabilities
  /** True only when this concrete native boundary can execute descriptor reads and writes. */
  readonly descriptorOperationsAvailable?: boolean
  adapterSnapshot(): CoreBluetoothAdapterSnapshot
  startScan(
    onAdvertisement: (advertisement: CoreBluetoothAdvertisement) => void,
    serviceUuids: readonly string[],
    deviceAddresses?: readonly string[],
    platform?: CoreBluetoothScanPlatformOptions
  ): Promise<void>
  stopScan(): Promise<void>
  connect(nativePeerId: string, intent?: ConnectionIntent): Promise<void>
  disconnect(nativePeerId: string): Promise<void>
  connectionState(nativePeerId: string): 'connecting' | 'connected' | 'disconnected'
  readRssi?(nativePeerId: string): Promise<number>
  /** Reports the current CoreBluetooth write length for the selected response mode. */
  maximumWriteValueLength?(nativePeerId: string, withResponse: boolean): Promise<number>
  requestMtu?(nativePeerId: string, requestedMtu: number): Promise<number>
  effectiveMtu?(nativePeerId: string): Promise<number | null>
  requestPriority?(nativePeerId: string, priority: ConnectionPriority): Promise<boolean>
  readPhy?(nativePeerId: string): Promise<CoreBluetoothPhyObservation>
  requestPhy?(nativePeerId: string, preference: PhyPreference): Promise<CoreBluetoothPhyRequestResult>
  canSendWriteWithoutResponse?(nativePeerId: string): Promise<CoreBluetoothWriteReadinessSnapshot>
  discover(nativePeerId: string): Promise<CoreBluetoothGattSnapshot>
  read(address: CoreBluetoothCharacteristicAddress): Promise<Uint8Array>
  write(address: CoreBluetoothCharacteristicAddress, bytes: Uint8Array, withResponse: boolean): Promise<void>
  readDescriptor?(address: CoreBluetoothDescriptorAddress): Promise<Uint8Array>
  writeDescriptor?(address: CoreBluetoothDescriptorAddress, bytes: Uint8Array): Promise<void>
  startNotify(address: CoreBluetoothCharacteristicAddress, onValue: (bytes: Uint8Array) => void): Promise<void>
  stopNotify(address: CoreBluetoothCharacteristicAddress): Promise<void>
  onDisconnect(listener: (nativePeerId: string, safeMessage: string | null) => void): () => void
  /** Emits when the peer's GATT Services Changed indication invalidates the discovered database. */
  onDatabaseChanged?(listener: (nativePeerId: string) => void): () => void
  onWriteWithoutResponseReadiness?(listener: (event: CoreBluetoothWriteReadinessEvent) => void): () => void
  /** Android may report a terminal scanner failure after scan-start has already succeeded. */
  onScanFailure?(listener: (safeMessage: string) => void): () => void
  onAdapterState(listener: (state: CoreBluetoothAdapterSnapshot) => void): () => void
  destroy(): Promise<void>
}
