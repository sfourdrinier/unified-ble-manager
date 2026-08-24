# API Report — unified-ble-manager (root)

> Reviewed against the current `src/public` façade. Host construction lives in
> explicit host entrypoints; advanced/backend authoring types are not root API.

```ts
export interface BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly security: BleSecurity
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
  readonly state?: BlePeerState
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
export interface GattCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly service: GattService
  readonly properties: GattCharacteristicProperties
  readonly access: GattAccessRequirements
  readonly descriptors: readonly GattDescriptor[]
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: GattWriteOptions): Promise<GattWriteReceipt>
  writeWhenReady(value: Uint8Array, options?: OperationOptions): Promise<GattWriteReceipt>
  writeLong(value: Uint8Array, options?: LongWriteOptions): Promise<GattLongWriteReceipt>
  subscribe(options?: GattSubscribeOptions): Promise<GattSubscription>
  withSubscription<T>(options: GattSubscribeOptions, action: (subscription: GattSubscription) => Promise<T>): Promise<T>
  descriptor(uuid: UuidInput, selector?: OccurrenceSelector): GattDescriptor
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
  readonly plan: ScanPlan | null
  readonly observations: BoundedAsyncStream<PublicScanObservation>
  readonly events?: AsyncIterable<DiscoveryEvent>
  readonly state: AsyncIterable<ScanStateEvent>
  stop(): Promise<CleanupRecord>
}
export type DiscoveryEvent =
  | { readonly kind: 'observed'; readonly peer: BlePeer }
  | { readonly kind: 'lost'; readonly peer: BlePeer; readonly lastObservedAt: number; readonly derivedAt: number; readonly reason: 'observation-timeout' }
export type ScanPlatformOptions = AndroidScanPlatformOptions | { readonly kind: 'corebluetooth' } | { readonly kind: 'winrt' } | { readonly kind: 'web' } | { readonly kind: 'electron' } | { readonly kind: 'tauri' }
export interface AndroidScanPlatformOptions { readonly kind: 'android'; readonly mode?: 'low-power' | 'balanced' | 'low-latency' | 'opportunistic'; readonly callbackType?: 'all-matches' | 'first-match' | 'match-lost'; readonly reportDelayMs?: number; readonly legacy?: boolean; readonly phy?: 'all-supported' | '1m' | 'coded' }
export interface BleAdapter { readonly id: string | null; state(): Promise<BleAdapterState>; waitUntilReady(options?: AdapterReadinessOptions): Promise<BleAdapterState> }
export interface BleDiscoveryInfo { readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid' }
export interface OperationOptions { readonly signal?: AbortSignal; readonly timeoutMs?: number }
export interface ScanOptions extends OperationOptions { readonly query?: ScanQuery; readonly duplicates?: 'coalesced' | 'all'; readonly delivery?: StreamPolicy; readonly observation?: { readonly reportLostAfterMs?: number; readonly includeRawAdvertisement?: boolean }; readonly platform?: ScanPlatformOptions }
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

- `AdapterReadinessOptions :: any`
- `AndroidScanCallbackType :: any`
- `AndroidScanMode :: any`
- `AndroidScanPhy :: any`
- `AndroidScanPlatformOptions :: any`
- `BUILT_IN_FEATURE_CATALOG :: readonly BuiltInFeatureCatalogEntry[]`
- `BUILT_IN_FEATURE_IDS :: Readonly<{ discoveryContinuousScan: "discovery:continuous-scan"; discoverySystemChooser: "discovery:system-chooser"; discoveryAdvertisementWatch: "discovery:advertisement-watch"; peerResolveReference: "peer:resolve-reference"; peerKnown: "peer:known"; peerSystemConnected: "peer:system-connected"; peerBonded: "peer:bonded"; peerOriginAuthorized: "peer:origin-authorized"; peerRestored: "peer:restored"; connectionDirect: "connection:direct"; connectionWhenAvailable: "connection:when-available"; connectionRssi: "connection:rssi"; connectionEffectiveMtu: "connection:effective-mtu"; connectionRequestMtu: "connection:request-mtu"; connectionPriority: "connection:priority"; connectionParameters: "connection:parameters"; connectionPhy: "connection:phy"; connectionSubrate: "connection:subrate"; securityState: "security:state"; securityPair: "security:pair"; securityCancelPairing: "security:cancel-pairing"; securityUnpair: "security:unpair"; securityCustomCeremony: "security:custom-ceremony"; gattDescriptors: "gatt:descriptors"; gattIndications: "gatt:indications"; gattServiceChanged: "gatt:service-changed"; maximumWriteLength: "gatt:maximum-write-length"; longWrite: "gatt:long-write"; reliableWrite: "gatt:reliable-write"; writeWithoutResponseReadiness: "gatt:write-without-response-readiness"; highThroughputAcquire: "gatt:high-throughput-acquire"; backgroundAppleRestoration: "background:apple-restoration"; backgroundAndroidConnectedDeviceService: "background:android-connected-device-service"; backgroundDesktopMaintainConnection: "background:desktop-maintain-connection"; lifecyclePagePersistence: "lifecycle:page-persistence"; }>`
- `BleAdapter :: any`
- `BleAdapterState :: any`
- `BleCapabilities :: any`
- `BleConnection :: any`
- `BleConnectionControls :: any`
- `BleConnectionEvent :: any`
- `BleControlObservationMetadata :: any`
- `BleControlObservationState :: any`
- `BleDiagnostics :: any`
- `BleDiagnosticsSnapshot :: any`
- `BleDiscoveryInfo :: any`
- `BleError :: typeof BleError`
- `BleManager :: any`
- `BleManagerCreateOptions :: any`
- `BleObservationSource :: any`
- `BlePeer :: any`
- `BlePeerDirectory :: any`
- `BlePeerState :: any`
- `BlePhy :: any`
- `BleRecovery :: any`
- `BleRecoveryDisposition :: any`
- `BleResourceCounters :: any`
- `BleSecurity :: any`
- `BuiltInFeatureId :: any`
- `CapabilityDescriptor :: any`
- `ChooseFilter :: any`
- `ChooseOptions :: any`
- `CleanupFailure :: any`
- `CleanupRecord :: any`
- `ConnectOptions :: any`
- `ConnectionGate :: any`
- `ConnectionGateContext :: any`
- `ConnectionGateDecision :: any`
- `ConnectionIntent :: any`
- `ConnectionParametersObservation :: any`
- `ConnectionPriority :: any`
- `ConnectionPriorityResult :: any`
- `ConnectionSupervisor :: any`
- `ConnectionSupervisorEvent :: any`
- `ConnectionSupervisorOptions :: any`
- `ConnectionSupervisorSnapshot :: any`
- `ConnectionSupervisorState :: any`
- `CustomStreamBudget :: any`
- `DescriptorWriteOptions :: any`
- `DiagnosticsOptions :: any`
- `DiscoveryEvent :: any`
- `FeatureId :: any`
- `FindOptions :: any`
- `GattAccessRequirements :: any`
- `GattCharacteristic :: any`
- `GattCharacteristicProperties :: any`
- `GattDatabase :: any`
- `GattDatabaseChangedEvent :: any`
- `GattDatabaseSnapshot :: any`
- `GattDescriptor :: any`
- `GattLongWriteReceipt :: any`
- `GattPathSelector :: any`
- `GattService :: any`
- `GattServiceReference :: any`
- `GattSubscribeOptions :: any`
- `GattSubscription :: any`
- `GattSubscriptionValue :: any`
- `GattValueEvent :: any`
- `GattValueStream :: any`
- `GattWriteOptions :: any`
- `GattWriteReceipt :: any`
- `KnownPeerQuery :: any`
- `LongWriteOptions :: any`
- `ManufacturerDataPattern :: any`
- `MaximumWriteLengthObservation :: any`
- `MtuNegotiation :: any`
- `MtuNegotiationState :: any`
- `MtuObservation :: any`
- `OccurrenceSelector :: any`
- `OperationOptions :: any`
- `PairCancelResult :: any`
- `PairOptions :: any`
- `PairResult :: any`
- `PairingAgent :: any`
- `PairingChallenge :: any`
- `PairingResponse :: any`
- `PeerDirectoryRecord :: any`
- `PeerReference :: any`
- `PeerReferenceScope :: any`
- `PeerSecurityEvent :: any`
- `PeerSecurityState :: any`
- `PeerSource :: any`
- `PhyObservation :: any`
- `PhyPreference :: any`
- `PhyUpdateResult :: any`
- `PublicScanObservation :: any`
- `RediscoverGattOptions :: any`
- `RequiredSecurityOptions :: any`
- `RetryPolicy :: any`
- `RssiObservation :: any`
- `ScanClause :: any`
- `ScanOptions :: any`
- `ScanPlan :: any`
- `ScanPlatformOptions :: any`
- `ScanQuery :: any`
- `ScanSession :: any`
- `ScanStateEvent :: any`
- `SecureConnectionsState :: any`
- `SecurityAuthenticationState :: any`
- `SecurityBondState :: any`
- `SecurityEncryptionState :: any`
- `SecurityPeer :: any`
- `SecurityRequirement :: any`
- `ServiceDataPattern :: any`
- `StreamPolicy :: any`
- `StreamPreset :: any`
- `SubrateMode :: any`
- `SubrateResult :: any`
- `UnpairResult :: any`
- `UuidInput :: any`
- `WriteMode :: any`
- `WriteReadinessEvent :: any`
- `createConnectionSupervisor :: <Session = undefined>(manager: BleManager, peer: string | BlePeer | PeerReference, options: ConnectionSupervisorOptions<Session>) => ConnectionSupervisor<Session>`
- `decodePeerReference :: (value: string) => PeerReference`
- `encodePeerReference :: (reference: PeerReference) => string`
- `mergePeerDirectoryRecords :: (records: readonly PeerDirectoryRecord[]) => readonly BlePeer[]`
- `withRequiredSecurity :: <Value>(security: BleSecurity, peer: SecurityPeer, requirement: SecurityRequirement, action: () => Promise<Value>, options?: RequiredSecurityOptions) => Promise<Value>`
