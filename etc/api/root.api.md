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
  readonly controls: BleConnectionControls
  discover(options?: OperationOptions): Promise<GattDatabase>
  rediscoverGatt(options: RediscoverGattOptions): Promise<GattDatabase>
  disconnect(): Promise<CleanupRecord>
  release(): Promise<CleanupRecord>
}
export interface BleConnectionControls {
  readRssi(options?: OperationOptions): Promise<RssiObservation>
  effectiveMtu(): Promise<MtuObservation>
  requestMtu(mtu: number, options?: OperationOptions): Promise<MtuNegotiation>
  maximumWriteLength(mode: WriteMode): Promise<MaximumWriteLengthObservation>
  requestPriority(priority: ConnectionPriority, options?: OperationOptions): Promise<ConnectionPriorityResult>
  readPhy(options?: OperationOptions): Promise<PhyObservation>
  requestPhy(preference: PhyPreference, options?: OperationOptions): Promise<PhyUpdateResult>
  parameters(): Promise<ConnectionParametersObservation>
  parameterEvents(): AsyncIterable<ConnectionParametersObservation>
  requestSubrate(mode: SubrateMode, options?: OperationOptions): Promise<SubrateResult>
  writeReadiness(mode: 'without-response'): AsyncIterable<WriteReadinessEvent>
}
export interface RediscoverGattOptions extends OperationOptions { readonly reason: 'service-changed' | 'manual' }
export type ConnectionPriority = 'low-power' | 'balanced' | 'high-throughput'
export type WriteMode = 'with-response' | 'without-response'
export interface RssiObservation extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly rssi: number | null }
export interface MtuObservation extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly attMtu: number | null; readonly payloadBytes: number | null; readonly platformPduBytes: number | null }
export interface MtuNegotiation extends BleControlObservationMetadata { readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'; readonly requestedMtu: number; readonly observation: MtuObservation | null }
export interface MaximumWriteLengthObservation extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly mode: WriteMode; readonly maximumWriteLength: number | null }
export interface ConnectionPriorityResult extends BleControlObservationMetadata { readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'; readonly requested: ConnectionPriority }
export interface PhyPreference { readonly tx?: BlePhy; readonly rx?: BlePhy }
export interface PhyObservation extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly tx: BlePhy | null; readonly rx: BlePhy | null }
export interface PhyUpdateResult extends BleControlObservationMetadata { readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'; readonly requested: PhyPreference; readonly observation: PhyObservation | null }
export interface ConnectionParametersObservation extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly intervalMs: number | null; readonly peripheralLatency: number | null; readonly supervisionTimeoutMs: number | null; readonly subrateFactor: number | null; readonly connectionEventLengthMs: number | null }
export type SubrateMode = 'default' | 'low-latency' | 'low-power'
export interface SubrateResult extends BleControlObservationMetadata { readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'; readonly requested: SubrateMode; readonly observation: ConnectionParametersObservation | null }
export interface WriteReadinessEvent extends BleControlObservationMetadata { readonly state: 'measured' | 'unavailable' | 'unsupported'; readonly mode: 'without-response'; readonly ready: boolean | null }
export interface BleControlObservationMetadata { readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }
export type BleObservationSource = 'backend' | 'platform' | 'core' | 'unknown'
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
export function createConnectionSupervisor<Session = undefined>(manager: BleManager, peer: BlePeer | string | PeerReference, options: ConnectionSupervisorOptions<Session>): ConnectionSupervisor<Session>
export type {
  ConnectionGate,
  ConnectionGateContext,
  ConnectionGateDecision,
  ConnectionSupervisor,
  ConnectionSupervisorEvent,
  ConnectionSupervisorOptions,
  ConnectionSupervisorSnapshot,
  ConnectionSupervisorState,
  RetryPolicy
}
```

All BLE payloads are bytes. Application operations use `AbortSignal` and
`timeoutMs`; cleanup is awaited and remains retryable when it reports
`release-failed`. The root package selects no radio backend.
## Verified exported symbols
<!-- This section is generated by scripts/docs/check-api-reports.js. -->
<!-- entrypoint: .; source: src/index.ts -->

- `AdapterReadinessOptions`
- `BUILT_IN_FEATURE_CATALOG`
- `BUILT_IN_FEATURE_IDS`
- `BleAdapter`
- `BleAdapterState`
- `BleCapabilities`
- `BleConnection`
- `BleConnectionControls`
- `BleConnectionEvent`
- `BleControlObservationMetadata`
- `BleControlObservationState`
- `BleDiagnostics`
- `BleDiagnosticsSnapshot`
- `BleDiscoveryInfo`
- `BleError`
- `BleManager`
- `BleManagerCreateOptions`
- `BleObservationSource`
- `BlePeer`
- `BlePeerDirectory`
- `BlePeerState`
- `BlePhy`
- `BleRecovery`
- `BleRecoveryDisposition`
- `BleResourceCounters`
- `BleSecurity`
- `BuiltInFeatureId`
- `CapabilityDescriptor`
- `ChooseFilter`
- `ChooseOptions`
- `CleanupFailure`
- `CleanupRecord`
- `ConnectOptions`
- `ConnectionGate`
- `ConnectionGateContext`
- `ConnectionGateDecision`
- `ConnectionIntent`
- `ConnectionParametersObservation`
- `ConnectionPriority`
- `ConnectionPriorityResult`
- `ConnectionSupervisor`
- `ConnectionSupervisorEvent`
- `ConnectionSupervisorOptions`
- `ConnectionSupervisorSnapshot`
- `ConnectionSupervisorState`
- `CustomStreamBudget`
- `DescriptorWriteOptions`
- `DiagnosticsOptions`
- `FeatureId`
- `FindOptions`
- `GattAccessRequirements`
- `GattCharacteristic`
- `GattCharacteristicProperties`
- `GattDatabase`
- `GattDatabaseChangedEvent`
- `GattDatabaseSnapshot`
- `GattDescriptor`
- `GattLongWriteReceipt`
- `GattPathSelector`
- `GattService`
- `GattServiceReference`
- `GattSubscribeOptions`
- `GattSubscription`
- `GattSubscriptionValue`
- `GattValueEvent`
- `GattValueStream`
- `GattWriteOptions`
- `GattWriteReceipt`
- `KnownPeerQuery`
- `LongWriteOptions`
- `ManufacturerDataPattern`
- `MaximumWriteLengthObservation`
- `MtuNegotiation`
- `MtuNegotiationState`
- `MtuObservation`
- `OccurrenceSelector`
- `OperationOptions`
- `PairCancelResult`
- `PairOptions`
- `PairResult`
- `PairingAgent`
- `PairingChallenge`
- `PairingResponse`
- `PeerDirectoryRecord`
- `PeerReference`
- `PeerReferenceScope`
- `PeerSecurityEvent`
- `PeerSecurityState`
- `PeerSource`
- `PhyObservation`
- `PhyPreference`
- `PhyUpdateResult`
- `PublicScanObservation`
- `RediscoverGattOptions`
- `RequiredSecurityOptions`
- `RetryPolicy`
- `RssiObservation`
- `ScanClause`
- `ScanOptions`
- `ScanQuery`
- `ScanSession`
- `ScanStateEvent`
- `SecureConnectionsState`
- `SecurityAuthenticationState`
- `SecurityBondState`
- `SecurityEncryptionState`
- `SecurityPeer`
- `SecurityRequirement`
- `ServiceDataPattern`
- `StreamPolicy`
- `StreamPreset`
- `SubrateMode`
- `SubrateResult`
- `UnpairResult`
- `UuidInput`
- `WriteMode`
- `WriteReadinessEvent`
- `createConnectionSupervisor`
- `decodePeerReference`
- `encodePeerReference`
- `mergePeerDirectoryRecords`
- `withRequiredSecurity`
