# API Report — unified-ble-manager (root)

> Reviewed against the current `src/public` façade. Host construction lives in
> explicit host entrypoints; advanced/backend authoring types are not root API.

```ts
export interface BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly discovery: BleDiscoveryInfo
  destroy(): Promise<CleanupRecord>
  scan(options?: ScanOptions): Promise<ScanSession>
  find(options?: FindOptions): Promise<BlePeer>
  choose(options?: ChooseOptions): Promise<BlePeer>
  connect(peer: BlePeer | string | PeerReference, options?: ConnectOptions): Promise<BleConnection>
  withConnection<T>(peer: BlePeer | string | PeerReference, options: ConnectOptions, action: (connection: BleConnection) => Promise<T>): Promise<T>
  withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T>
  withDiscoveredConnection<T>(peer: BlePeer | string | PeerReference, options: ConnectOptions, action: (scope: { connection: BleConnection; gatt: GattDatabase }) => Promise<T>): Promise<T>
}

export interface BlePeer {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
  readonly reference: PeerReference | null
  readonly sources: readonly PeerSource[]
  readonly lastAdvertisement: NormalizedScanObservation | null
}
export interface BleConnection {
  readonly peer: BlePeer
  readonly connectionGeneration: string
  readonly lifecycleEvents: AsyncIterable<BleConnectionEvent>
  discover(options?: OperationOptions): Promise<GattDatabase>
  readRssi(options?: OperationOptions): Promise<number>
  requestMtu(requestedMtu: number, options?: OperationOptions): Promise<number>
  disconnect(): Promise<CleanupRecord>
  release(): Promise<CleanupRecord>
}
export interface ScanSession {
  readonly observations: BoundedAsyncStream<PublicScanObservation>
  readonly state: AsyncIterable<ScanStateEvent>
  stop(): Promise<CleanupRecord>
}
export interface BleAdapter { readonly id: string | null; state(): Promise<BleAdapterState>; waitUntilReady(options?: AdapterReadinessOptions): Promise<BleAdapterState> }
export interface BleDiscoveryInfo { readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid' }
export interface OperationOptions { readonly signal?: AbortSignal; readonly timeoutMs?: number }
export interface ScanOptions extends OperationOptions { readonly query?: ScanQuery; readonly duplicates?: 'coalesced' | 'all'; readonly delivery?: StreamPolicy }
export interface FindOptions extends OperationOptions { readonly query?: ScanQuery; readonly select?: 'first' | ((peer: BlePeer) => boolean) }
export interface ChooseOptions extends OperationOptions { readonly filters?: readonly ChooseFilter[]; readonly optionalServices?: readonly (string | number)[]; readonly acceptAllDevices?: boolean }
export type { GattDatabase, GattService, GattCharacteristic, GattDescriptor, GattSubscription, GattValueEvent, GattWriteReceipt, GattSubscribeOptions } from './gatt'
export type { BleCapabilities, CapabilityDescriptor, FeatureId } from './capabilities'
export type { BleDiagnostics, BleDiagnosticsSnapshot } from './diagnostics'
export type { BlePeerDirectory, PeerReference, PeerReferenceScope, PeerSource } from './peer-directory'
export class BleError extends Error { readonly code: BleErrorCode; readonly domain: BleErrorDomain; readonly operation: string; readonly platform: PlatformErrorDetail | null; readonly recovery: BleRecovery }
```

All BLE payloads are bytes. Application operations use `AbortSignal` and
`timeoutMs`; cleanup is awaited and remains retryable when it reports
`release-failed`. The root package selects no radio backend.
