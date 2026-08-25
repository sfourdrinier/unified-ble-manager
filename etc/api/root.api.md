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
  connect(peer: BlePeer | string | PeerReference | PeerAddress, options?: ConnectOptions): Promise<BleConnection>
  withConnection<T>(peer: BlePeer | string | PeerReference | PeerAddress, options: ConnectOptions, action: (connection: BleConnection) => Promise<T>): Promise<T>
  withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T>
  withDiscoveredConnection<T>(peer: BlePeer | string | PeerReference | PeerAddress, options: ConnectOptions, action: (scope: { connection: BleConnection; gatt: GattDatabase }) => Promise<T>): Promise<T>
}

// peer:address-targeting entry form. Public/static addresses only; peers using
// resolvable private addresses re-enter through a durable PeerReference.
export interface PeerAddress { readonly address: string; readonly addressType?: 'public' | 'random' }
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
  | { readonly kind: 'presence-tracking-overflow'; readonly guarantee: 'reportLostAfterMs-completeness'; readonly droppedEntries: number; readonly droppedBytes: number }
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

- `AdapterReadinessOptions :: { readonly operation?: "scan" | "choose" | "connect" | "known-peers" | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `AndroidScanCallbackType :: "all-matches" | "first-match" | "match-lost"`
- `AndroidScanMode :: "low-power" | "balanced" | "low-latency" | "opportunistic"`
- `AndroidScanPhy :: "all-supported" | "1m" | "coded"`
- `AndroidScanPlatformOptions :: { readonly kind: "android"; readonly mode?: AndroidScanMode | undefined; readonly callbackType?: AndroidScanCallbackType | undefined; readonly reportDelayMs?: number | undefined; readonly legacy?: boolean | undefined; readonly phy?: AndroidScanPhy | undefined }`
- `BUILT_IN_FEATURE_CATALOG :: readonly BuiltInFeatureCatalogEntry[]`
- `BUILT_IN_FEATURE_IDS :: Readonly<{ discoveryContinuousScan: "discovery:continuous-scan"; discoverySystemChooser: "discovery:system-chooser"; discoveryAdvertisementWatch: "discovery:advertisement-watch"; peerResolveReference: "peer:resolve-reference"; peerAddressTargeting: "peer:address-targeting"; peerKnown: "peer:known"; peerSystemConnected: "peer:system-connected"; peerBonded: "peer:bonded"; peerOriginAuthorized: "peer:origin-authorized"; peerRestored: "peer:restored"; connectionDirect: "connection:direct"; connectionWhenAvailable: "connection:when-available"; connectionRssi: "connection:rssi"; connectionEffectiveMtu: "connection:effective-mtu"; connectionRequestMtu: "connection:request-mtu"; connectionPriority: "connection:priority"; connectionParameters: "connection:parameters"; connectionPhy: "connection:phy"; connectionSubrate: "connection:subrate"; securityState: "security:state"; securityPair: "security:pair"; securityCancelPairing: "security:cancel-pairing"; securityUnpair: "security:unpair"; securityCustomCeremony: "security:custom-ceremony"; gattDescriptors: "gatt:descriptors"; gattIndications: "gatt:indications"; gattServiceChanged: "gatt:service-changed"; maximumWriteLength: "gatt:maximum-write-length"; longWrite: "gatt:long-write"; reliableWrite: "gatt:reliable-write"; writeWithoutResponseReadiness: "gatt:write-without-response-readiness"; highThroughputAcquire: "gatt:high-throughput-acquire"; backgroundAppleRestoration: "background:apple-restoration"; backgroundAndroidConnectedDeviceService: "background:android-connected-device-service"; backgroundDesktopMaintainConnection: "background:desktop-maintain-connection"; lifecyclePagePersistence: "lifecycle:page-persistence"; }>`
- `BleAdapter :: { readonly id: string | null; state(): Promise<BleAdapterState>; waitUntilReady(options?: AdapterReadinessOptions | undefined): Promise<BleAdapterState>; watchState(options?: AdapterWatchOptions | undefined): Promise<BleAdapterStateWatch> }`
- `BleAdapterState :: Omit<AdapterStateSnapshot<string>, "backendGeneration" | "updatedAt"> & { readonly backendGeneration: string; readonly updatedAt: number; }`
- `BleCapabilities :: { supports(id: BuiltInFeatureId): boolean; get(id: '${string}:${string}'): CapabilityDescriptor | undefined; require(id: BuiltInFeatureId): CapabilityDescriptor; list(): readonly CapabilityDescriptor[] }`
- `BleConnection :: { readonly peer: BlePeer; readonly connectionGeneration: string; readonly lifecycleEvents: AsyncIterable<BleConnectionEvent>; readonly controls: BleConnectionControls; readonly discover: (options?: OperationOptions | undefined) => Promise<GattDatabase>; readonly rediscoverGatt: (options: RediscoverGattOptions) => Promise<GattDatabase>; readonly disconnect: () => Promise<CleanupRecord>; readonly release: () => Promise<CleanupRecord> }`
- `BleConnectionControls :: { readRssi(options?: OperationOptions | undefined): Promise<RssiObservation>; effectiveMtu(): Promise<MtuObservation>; requestMtu(mtu: number, options?: OperationOptions | undefined): Promise<MtuNegotiation>; maximumWriteLength(mode: WriteMode): Promise<MaximumWriteLengthObservation>; requestPriority(priority: ConnectionPriority, options?: OperationOptions | undefined): Promise<ConnectionPriorityResult>; readPhy(options?: OperationOptions | undefined): Promise<PhyObservation>; requestPhy(preference: Readonly<{ readonly tx?: BlePhy | undefined; readonly rx?: BlePhy | undefined; }>, options?: OperationOptions | undefined): Promise<PhyUpdateResult>; parameters(): Promise<ConnectionParametersObservation>; parameterEvents(): AsyncIterable<ConnectionParametersObservation>; requestSubrate(mode: SubrateMode, options?: OperationOptions | undefined): Promise<SubrateResult>; writeReadiness(mode: "without-response"): AsyncIterable<WriteReadinessEvent> }`
- `BleConnectionEvent :: { readonly kind: "connection-lifecycle"; readonly previous: ConnectionState; readonly current: ConnectionState; readonly cause: ConnectionLifecycleCause; readonly connectionGeneration: string; readonly sequence: number }`
- `BleControlObservationMetadata :: { readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `BleControlObservationState :: "measured" | "unavailable" | "unsupported"`
- `BleDiagnostics :: { snapshot(): BleDiagnosticsSnapshot; resourceCounters(): BleResourceCounters; startTrace(): { stop(): Promise<DiagnosticTraceDocument>; } }`
- `BleDiagnosticsSnapshot :: { readonly trace: DiagnosticTraceDocument; readonly resourceCounters: BleResourceCounters }`
- `BleDiscoveryInfo :: { readonly kind: "continuous-scan" | "system-chooser" | "hybrid" }`
- `BleError :: typeof BleError`
- `BleManager :: { readonly capabilities: BleCapabilities; readonly adapter: BleAdapter; readonly diagnostics: BleDiagnostics; readonly peers: BlePeerDirectory; readonly security: BleSecurity; readonly discovery: BleDiscoveryInfo; readonly destroy: () => Promise<CleanupRecord>; scan(options?: ScanOptions | undefined): Promise<ScanSession>; find(options?: FindOptions | undefined): Promise<BlePeer>; choose(options?: ChooseOptions | undefined): Promise<BlePeer>; connect(peer: string | BlePeer | PeerReference | PeerAddress, options?: ConnectOptions | undefined): Promise<BleConnection>; withConnection<T>(peer: string | BlePeer | PeerReference | PeerAddress, options: ConnectOptions, action: (connection: BleConnection) => Promise<T>): Promise<T>; withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T>; withDiscoveredConnection<T>(peer: string | BlePeer | PeerReference | PeerAddress, options: ConnectOptions, action: (scope: { readonly connection: BleConnection; readonly gatt: GattDatabase; }) => Promise<T>): Promise<T> }`
- `BleManagerCreateOptions :: { readonly instanceId?: string | undefined; readonly adapterId?: string | undefined; readonly diagnostics?: DiagnosticsOptions | undefined; readonly randomBytes?: ((length: number) => Uint8Array<ArrayBufferLike>) | undefined; readonly restoration?: { readonly restorationId: string; readonly generation?: string | undefined; } | undefined }`
- `BleObservationSource :: "backend" | "platform" | "core" | "unknown"`
- `BlePeer :: { readonly id: string; readonly name: string | null; readonly rssi: number | null; readonly reference: PeerReference | null; readonly sources: readonly PeerSource[]; readonly lastAdvertisement: NormalizedScanObservation | null; readonly state?: BlePeerState | undefined }`
- `BlePeerDirectory :: { resolve(reference: PeerReference, options?: OperationOptions | undefined): Promise<BlePeer | null>; known(options?: KnownPeerQuery | undefined): Promise<readonly BlePeer[]>; connected(options?: KnownPeerQuery | undefined): Promise<readonly BlePeer[]>; bonded(options?: KnownPeerQuery | undefined): Promise<readonly BlePeer[]>; authorized(options?: KnownPeerQuery | undefined): Promise<readonly BlePeer[]>; restored(options?: KnownPeerQuery | undefined): Promise<readonly BlePeer[]> }`
- `BlePeerState :: { readonly reachability: "unknown" | "reachable" | "unreachable"; readonly connection: "connected" | "disconnected" | "unknown"; readonly bond: "unknown" | "unsupported" | "bonded" | "not-bonded"; readonly lastSeenAtMonotonicMs: number | null }`
- `BlePhy :: "le-1m" | "le-2m" | "le-coded"`
- `BleRecovery :: { readonly disposition: BleRecoveryDisposition; readonly actions: readonly RecoveryAction[] }`
- `BleRecoveryDisposition :: "none" | "retry-immediately" | "retry-with-backoff" | "after-state-change" | "after-user-action" | "caller-policy"`
- `BleResourceCounters :: { readonly activeScanControllers: number; readonly scanConsumers: number; readonly chooserSessions: number; readonly connectionLeases: number; readonly physicalLinks: number; readonly databaseSnapshots: number; readonly physicalCccdEnablements: number; readonly subscriptionConsumers: number; readonly queuedOperations: number; readonly dispatchedOperations: number; readonly retainedByteBuffers: number; readonly restorationRecords: number; readonly orphanedIpcOwners: number }`
- `BleSecurity :: { state(peer: SecurityPeer, options?: OperationOptions | undefined): Promise<PeerSecurityState>; watch(peer: SecurityPeer): AsyncIterable<PeerSecurityEvent>; pair(peer: SecurityPeer, options?: PairOptions | undefined): Promise<PairResult>; cancelPairing(peer: SecurityPeer, options?: OperationOptions | undefined): Promise<SecurityCancelPairingResult>; unpair(peer: SecurityPeer, options?: OperationOptions | undefined): Promise<SecurityUnpairResult> }`
- `BuiltInFeatureId :: "discovery:continuous-scan" | "discovery:system-chooser" | "discovery:advertisement-watch" | "peer:resolve-reference" | "peer:address-targeting" | "peer:known" | "peer:system-connected" | "peer:bonded" | "peer:origin-authorized" | "peer:restored" | "connection:direct" | "connection:when-available" | "connection:rssi" | "connection:effective-mtu" | "connection:request-mtu" | "connection:priority" | "connection:parameters" | "connection:phy" | "connection:subrate" | "security:state" | "security:pair" | "security:cancel-pairing" | "security:unpair" | "security:custom-ceremony" | "gatt:descriptors" | "gatt:indications" | "gatt:service-changed" | "gatt:maximum-write-length" | "gatt:long-write" | "gatt:reliable-write" | "gatt:write-without-response-readiness" | "gatt:high-throughput-acquire" | "background:apple-restoration" | "background:android-connected-device-service" | "background:desktop-maintain-connection" | "lifecycle:page-persistence"`
- `CapabilityDescriptor :: { readonly id: '${string}:${string}'; readonly state: FeatureState; readonly selectedSchemaRange: VersionRange<"capability-schema">; readonly implementationOrigin: "backend-native" | "core-emulated"; readonly tck: TckBinding; readonly evidence: EvidenceReceipt; readonly limitations: readonly Limitation[]; readonly limits: Readonly<Record<string, CapabilityLimit>> }`
- `ChooseFilter :: { readonly serviceUuids?: readonly (string | number)[] | undefined; readonly manufacturerData?: readonly { readonly companyIdentifier: number; readonly dataPrefix?: Readonly<Uint8Array<ArrayBufferLike>> | undefined; }[] | undefined; readonly localNamePrefix?: string | undefined }`
- `ChooseOptions :: { readonly filters?: readonly ChooseFilter[] | undefined; readonly optionalServices?: readonly (string | number)[] | undefined; readonly acceptAllDevices?: boolean | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `CleanupFailure :: { readonly resourceKind: string; readonly error: NormalizedBleError }`
- `CleanupRecord :: { readonly state: "released" | "release-failed"; readonly failures: readonly CleanupFailure[] }`
- `ConnectOptions :: { readonly intent?: ConnectionIntent | undefined; readonly transport?: "le" | "auto" | undefined; readonly preferredPhy?: readonly BlePhy[] | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `ConnectionGate :: { (context: ConnectionGateContext) => ConnectionGateDecision | Promise<ConnectionGateDecision> }`
- `ConnectionGateContext :: { readonly attempt: number; readonly adapter: BleAdapterState; readonly lastError: BleError | null; readonly lastDisconnect: BleConnectionEvent | null }`
- `ConnectionGateDecision :: "allow" | "pause" | "stop"`
- `ConnectionIntent :: "direct" | "when-available"`
- `ConnectionParametersObservation :: { readonly state: BleControlObservationState; readonly intervalMs: number | null; readonly peripheralLatency: number | null; readonly supervisionTimeoutMs: number | null; readonly subrateFactor: number | null; readonly connectionEventLengthMs: number | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `ConnectionPriority :: "low-power" | "balanced" | "high-throughput"`
- `ConnectionPriorityResult :: { readonly state: "unavailable" | "unsupported" | "accepted" | "rejected"; readonly requested: ConnectionPriority; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `ConnectionSupervisor :: { readonly events: BoundedAsyncStream<ConnectionSupervisorEvent<Session>>; readonly snapshot: ConnectionSupervisorSnapshot<Session>; start(): void; pause(reason?: string | undefined): Promise<void>; resume(): void; reconnectNow(): void; stop(): Promise<CleanupRecord> }`
- `ConnectionSupervisorEvent :: { readonly kind: "state"; readonly supervisorId: string; readonly previous: ConnectionSupervisorState; readonly state: ConnectionSupervisorState; readonly attempt: number; readonly connectionGeneration: string | null; readonly timestamp: number; readonly delayMs: number | null; readonly gateDecision: ConnectionGateDecision | null; readonly error: BleError | null; readonly session: Session | null; readonly cleanup?: CleanupRecord | undefined }`
- `ConnectionSupervisorOptions :: { readonly connection?: Omit<ConnectOptions, "signal"> | undefined; readonly retry: RetryPolicy; readonly gate?: ConnectionGate | undefined; readonly configure?: ((connection: BleConnection) => Promise<Session>) | undefined; readonly disposeSession?: ((session: Session) => Promise<void>) | undefined; readonly resetBackoffAfterConnectedMs?: number | undefined; readonly now?: (() => number) | undefined; readonly random?: (() => number) | undefined; readonly setTimeout?: ((callback: () => void, delayMs: number) => unknown) | undefined; readonly clearTimeout?: ((handle: unknown) => void) | undefined }`
- `ConnectionSupervisorSnapshot :: { readonly supervisorId: string; readonly state: ConnectionSupervisorState; readonly attempt: number; readonly connectionGeneration: string | null; readonly session: Session | null; readonly lastError: BleError | null; readonly lastDisconnect: BleConnectionEvent | null }`
- `ConnectionSupervisorState :: "connecting" | "connected" | "disconnecting" | "stopped" | "idle" | "waiting-for-gate" | "configuring" | "backoff" | "cleanup-failed"`
- `CustomStreamBudget :: { readonly itemCapacity: number; readonly byteCapacity: number; readonly reservedControlCapacity?: number | undefined; readonly overflowPolicy?: OverflowPolicy | undefined }`
- `DescriptorWriteOptions :: { readonly response?: "required" | "automatic" | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `DiagnosticsOptions :: { readonly traceMaximumRecords?: number | undefined; readonly traceMaximumBytes?: number | undefined; readonly maximumValueBytes?: number | undefined }`
- `DiscoveryEvent :: { readonly kind: "observed"; readonly peer: BlePeer; } | { readonly kind: "lost"; readonly peer: BlePeer; readonly lastObservedAt: number; readonly derivedAt: number; readonly reason: "observation-timeout"; } | { readonly kind: "presence-tracking-overflow"; readonly guarantee: "reportLostAfterMs-completeness"; readonly droppedEntries: number; readonly droppedBytes: number; }`
- `FeatureId :: '${Namespace}:${Name}'`
- `FindOptions :: { readonly query?: ScanQuery | undefined; readonly select?: "first" | ((peer: BlePeer) => boolean) | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `GattAccessRequirements :: { readonly read: "unknown" | "none" | "encrypted" | "authenticated" | "authorized"; readonly write: "unknown" | "none" | "encrypted" | "authenticated" | "authorized" }`
- `GattCharacteristic :: { readonly uuid: string; readonly occurrence: number; readonly service: GattService; readonly properties: GattCharacteristicProperties; readonly access: GattAccessRequirements; readonly descriptors: readonly GattDescriptor[]; read(options?: OperationOptions | undefined): Promise<Uint8Array<ArrayBufferLike>>; write(value: Uint8Array<ArrayBufferLike>, options?: GattWriteOptions | undefined): Promise<PortableWriteReceipt>; writeWhenReady(value: Uint8Array<ArrayBufferLike>, options?: OperationOptions | undefined): Promise<PortableWriteReceipt>; writeLong(value: Uint8Array<ArrayBufferLike>, options?: LongWriteOptions | undefined): Promise<PortableLongWriteReceipt>; subscribe(options?: GattSubscribeOptions | undefined): Promise<GattSubscription>; withSubscription<T>(options: GattSubscribeOptions, action: (subscription: GattSubscription) => Promise<T>): Promise<T>; descriptor(uuid: UuidInput, selector?: OccurrenceSelector | undefined): GattDescriptor }`
- `GattCharacteristicProperties :: { readonly broadcast: boolean; readonly read: boolean; readonly writeWithResponse: boolean; readonly writeWithoutResponse: boolean; readonly authenticatedSignedWrites: boolean; readonly notify: boolean; readonly indicate: boolean; readonly extendedProperties: boolean; readonly reliableWrite: boolean; readonly writableAuxiliaries: boolean; readonly availability: GattCharacteristicPropertyAvailability }`
- `GattDatabase :: { readonly generation: string; readonly services: readonly GattService[]; readonly changed: AsyncIterable<StreamItem<GattDatabaseChangedEvent>>; service(uuid: UuidInput, selector?: OccurrenceSelector | undefined): GattService; servicesByUuid(uuid: UuidInput): readonly GattService[]; characteristic(serviceUuid: UuidInput, characteristicUuid: UuidInput, selector?: GattPathSelector | undefined): GattCharacteristic; snapshot(): GattDatabaseSnapshot }`
- `GattDatabaseChangedEvent :: { readonly previousGeneration: string; readonly reason: "service-changed" | "reconnect" | "backend-reset" | "manual-rediscovery"; readonly affectedHandleRange: { readonly start: number; readonly end: number; } | null }`
- `GattDatabaseSnapshot :: { readonly generation: string; readonly services: readonly GattService[]; readonly characteristics: readonly GattCharacteristic[]; readonly descriptors: readonly GattDescriptor[] }`
- `GattDescriptor :: { readonly uuid: string; readonly occurrence: number; readonly characteristic: GattCharacteristic; readonly properties: GattDescriptorProperties; read(options?: OperationOptions | undefined): Promise<Uint8Array<ArrayBufferLike>>; write(value: Uint8Array<ArrayBufferLike>, options?: DescriptorWriteOptions | undefined): Promise<PortableWriteReceipt> }`
- `GattLongWriteReceipt :: PortableLongWriteNotPlannedReceipt | PortableLongWritePlannedReceipt`
- `GattPathSelector :: { readonly serviceOccurrence?: number | undefined; readonly characteristicOccurrence?: number | undefined }`
- `GattService :: { readonly uuid: string; readonly occurrence: number; readonly primary: boolean; readonly includedServices: readonly GattServiceReference[]; readonly characteristics: readonly GattCharacteristic[]; characteristic(uuid: UuidInput, selector?: OccurrenceSelector | undefined): GattCharacteristic; characteristicsByUuid(uuid: UuidInput): readonly GattCharacteristic[] }`
- `GattServiceReference :: { readonly uuid: string; readonly occurrence: number }`
- `GattSubscribeOptions :: { readonly delivery?: "prefer-notification" | "prefer-indication" | "require-notification" | "require-indication" | undefined; readonly stream?: StreamPolicy | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `GattSubscription :: { readonly requestedDelivery: "prefer-notification" | "prefer-indication" | "require-notification" | "require-indication" | undefined; readonly effectiveDelivery: GattDelivery; readonly values: GattValueStream; remove(): Promise<PortableCleanupRecord> }`
- `GattSubscriptionValue :: { readonly value: Uint8Array<ArrayBufferLike>; readonly delivery: GattDelivery; readonly observedAtMonotonicMs: number; readonly sequence: number }`
- `GattValueEvent :: { readonly value: Uint8Array<ArrayBufferLike>; readonly delivery: GattDelivery; readonly observedAtMonotonicMs: number; readonly sequence: number }`
- `GattValueStream :: { readonly limits: { readonly itemCapacity: number; readonly byteCapacity: number; readonly reservedControlCapacity: number; }; readonly overflowPolicy: "latest" | "drop-oldest" | "drop-newest" | "error"; close(): Promise<PortableCleanupRecord>; __@asyncIterator@169(): AsyncIterator<StreamItem<GattValueEvent>, any, any> }`
- `GattWriteOptions :: { readonly response?: "required" | "not-required" | "automatic" | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `GattWriteReceipt :: { readonly terminal: PortableOperationTerminalRecord; readonly commitState: "unknown" | "confirmed" }`
- `KnownPeerQuery :: { readonly sources?: readonly PeerSource[] | undefined; readonly services?: readonly (string | number)[] | undefined; readonly references?: readonly PeerReference[] | undefined; readonly includeUnavailable?: boolean | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `LongWriteOptions :: { readonly chunkSize?: number | undefined; readonly response?: "required" | "not-required" | "automatic" | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `ManufacturerDataPattern :: { readonly companyId: number; readonly dataPrefix?: Readonly<Uint8Array<ArrayBufferLike>> | undefined; readonly mask?: Readonly<Uint8Array<ArrayBufferLike>> | undefined }`
- `MaximumWriteLengthObservation :: { readonly state: BleControlObservationState; readonly mode: WriteMode; readonly maximumWriteLength: number | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `MtuNegotiation :: { readonly state: MtuNegotiationState; readonly requestedMtu: number; readonly observation: MtuObservation | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `MtuNegotiationState :: "unavailable" | "unsupported" | "accepted" | "rejected"`
- `MtuObservation :: { readonly state: BleControlObservationState; readonly attMtu: number | null; readonly payloadBytes: number | null; readonly platformPduBytes: number | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `OccurrenceSelector :: { readonly occurrence?: number | undefined }`
- `OperationOptions :: { readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `PairCancelResult :: { readonly outcome: "cancelled" | "not-pairing" }`
- `PairOptions :: { readonly transport?: "le" | "auto" | undefined; readonly protection?: "encrypted" | "authenticated" | "system-default" | undefined; readonly ceremony?: PairingAgent | "system" | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `PairResult :: { readonly outcome: "paired"; readonly state: PeerSecurityState; } | { readonly outcome: "already-paired"; readonly state: PeerSecurityState; } | { readonly outcome: "repaired"; readonly state: PeerSecurityState; } | { readonly outcome: "rejected"; readonly reason: string | null; } | { readonly outcome: "cancelled"; }`
- `PairingAgent :: { onChallenge(challenge: PairingChallenge): Promise<PairingResponse> }`
- `PairingChallenge :: { readonly kind: "confirm"; readonly peer: BlePeer; readonly challengeId: string; readonly deadlineMonotonicMs: number; } | { readonly kind: "confirm-passkey"; readonly peer: BlePeer; readonly challengeId: string; readonly passkey: number; readonly deadlineMonotonicMs: number; } | { readonly kind: "display-passkey"; readonly peer: BlePeer; readonly challengeId: string; readonly passkey: number; readonly deadlineMonotonicMs: number; } | { readonly kind: "provide-pin"; readonly peer: BlePeer; readonly challengeId: string; readonly deadlineMonotonicMs: number; } | { readonly kind: "provide-passkey"; readonly peer: BlePeer; readonly challengeId: string; readonly deadlineMonotonicMs: number; }`
- `PairingResponse :: { readonly kind: "confirm"; readonly confirmed: boolean; } | { readonly kind: "confirm-passkey"; readonly confirmed: boolean; } | { readonly kind: "display-passkey"; readonly acknowledged: boolean; } | { readonly kind: "provide-pin"; readonly pin: string; } | { readonly kind: "provide-passkey"; readonly passkey: string; }`
- `PeerDirectoryRecord :: { readonly reference: PeerReference; readonly peer: BlePeer; readonly source: PeerSource; readonly state: BlePeerState; readonly services?: readonly string[] | undefined; readonly clockScope?: string | undefined }`
- `PeerReference :: { readonly version: 1; readonly backendId: string; readonly scope: PeerReferenceScope; readonly opaqueId: string }`
- `PeerReferenceScope :: "system" | "application" | "origin"`
- `PeerSecurityEvent :: { readonly kind: "state"; readonly peerId: string; readonly sequence: number; readonly state: PeerSecurityState }`
- `PeerSecurityState :: { readonly bond: SecurityBondState; readonly encryption: SecurityEncryptionState; readonly authentication: SecurityAuthenticationState; readonly secureConnections: SecureConnectionsState; readonly pairingPossible: boolean | null; readonly measuredAtMonotonicMs: number; readonly limitations: readonly Limitation[] }`
- `PeerSource :: "scan-observed" | "app-reference" | "system-connected" | "system-bonded" | "origin-authorized" | "restored" | "backend-cache"`
- `PhyObservation :: { readonly state: BleControlObservationState; readonly tx: BlePhy | null; readonly rx: BlePhy | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `PhyPreference :: { readonly tx?: BlePhy | undefined; readonly rx?: BlePhy | undefined }`
- `PhyUpdateResult :: { readonly state: "unavailable" | "unsupported" | "accepted" | "rejected"; readonly requested: Readonly<{ readonly tx?: BlePhy | undefined; readonly rx?: BlePhy | undefined; }>; readonly observation: PhyObservation | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `PublicScanObservation :: { readonly peer: BlePeer; readonly observedAtMonotonicMs: number | null; readonly peerReference?: PeerReference | undefined; readonly address?: NormalizedObservationAddress | undefined; readonly localName: string | null; readonly rssi: number | null; readonly connectable: boolean | null; readonly serviceUuids: readonly string[] | null; readonly manufacturerData: readonly { readonly companyId: number; readonly data: Uint8Array<ArrayBufferLike>; }[] | null; readonly serviceData: readonly { readonly service: string; readonly data: Uint8Array<ArrayBufferLike>; }[] | null }`
- `RediscoverGattOptions :: { readonly reason: "service-changed" | "manual"; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `RequiredSecurityOptions :: { readonly state?: OperationOptions | undefined; readonly pair?: PairOptions | undefined }`
- `RetryPolicy :: { readonly initialDelayMs: number; readonly maximumDelayMs: number; readonly multiplier: number; readonly jitter: number; readonly maximumAttempts?: number | undefined; readonly maximumElapsedMs?: number | undefined }`
- `RssiObservation :: { readonly state: BleControlObservationState; readonly rssi: number | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `ScanClause :: { readonly peers?: readonly PeerReference[] | undefined; readonly addresses?: readonly string[] | undefined; readonly services?: { readonly any?: readonly (string | number)[] | undefined; readonly all?: readonly (string | number)[] | undefined; } | undefined; readonly names?: { readonly exact?: readonly string[] | undefined; readonly prefixes?: readonly string[] | undefined; } | undefined; readonly manufacturerData?: { readonly any?: readonly ManufacturerDataPattern[] | undefined; readonly all?: readonly ManufacturerDataPattern[] | undefined; } | undefined; readonly serviceData?: { readonly any?: readonly ServiceDataPattern[] | undefined; readonly all?: readonly ServiceDataPattern[] | undefined; } | undefined; readonly rssi?: { readonly minimum?: number | undefined; readonly maximum?: number | undefined; } | undefined; readonly connectable?: boolean | undefined }`
- `ScanOptions :: { readonly query?: ScanQuery | undefined; readonly duplicates?: "coalesced" | "all" | undefined; readonly delivery?: StreamPolicy | undefined; readonly observation?: { readonly reportLostAfterMs?: number | undefined; readonly includeRawAdvertisement?: boolean | undefined; } | undefined; readonly platform?: ScanPlatformOptions | undefined; readonly signal?: AbortSignal | undefined; readonly timeoutMs?: number | undefined }`
- `ScanPlan :: { readonly sourceQuery: NormalizedScanQuery; readonly queryDigest: string; readonly residualQueryDigest: string; readonly nativeGuarantee: "exact" | "safe-superset"; readonly native: ScanPlanProjection; readonly residual: ScanPlanResidualProjection; readonly unavailable: readonly ScanPredicateDescription[]; readonly limitations: readonly ScanPlanLimitation[]; readonly estimatedCost: "native-only" | "low" | "moderate" | "high" }`
- `ScanPlatformOptions :: AndroidScanPlatformOptions | { readonly kind: "corebluetooth"; } | { readonly kind: "winrt"; } | { readonly kind: "web"; } | { readonly kind: "electron"; } | { readonly kind: "tauri"; }`
- `ScanQuery :: { readonly anyOf?: readonly ScanClause[] | undefined; readonly exclude?: readonly ScanClause[] | undefined }`
- `ScanSession :: { readonly plan: ScanPlan | null; readonly stop: () => Promise<CleanupRecord>; readonly observations: BoundedAsyncStream<PublicScanObservation>; readonly events?: AsyncIterable<DiscoveryEvent> | undefined; readonly state: AsyncIterable<ScanStateEvent> }`
- `ScanStateEvent :: { readonly state: "starting" | "active" | "stopping" | "stopped" | "failed"; readonly reason?: string | undefined }`
- `SecureConnectionsState :: "unknown" | "unsupported" | "yes" | "no"`
- `SecurityAuthenticationState :: "unknown" | "unsupported" | "authenticated" | "unauthenticated"`
- `SecurityBondState :: "unknown" | "unsupported" | "bonded" | "not-bonded" | "bonding"`
- `SecurityEncryptionState :: "unknown" | "unsupported" | "encrypted" | "not-encrypted"`
- `SecurityPeer :: BlePeer | PeerReference`
- `SecurityRequirement :: "encrypted" | "authenticated"`
- `ServiceDataPattern :: { readonly service: string | number; readonly dataPrefix?: Readonly<Uint8Array<ArrayBufferLike>> | undefined; readonly mask?: Readonly<Uint8Array<ArrayBufferLike>> | undefined }`
- `StreamPolicy :: "balanced" | "latest" | "lossless-bounded" | { readonly preset: "custom"; readonly budget: CustomStreamBudget; }`
- `StreamPreset :: "balanced" | "latest" | "lossless-bounded" | "custom"`
- `SubrateMode :: "low-power" | "default" | "low-latency"`
- `SubrateResult :: { readonly state: "unavailable" | "unsupported" | "accepted" | "rejected"; readonly requested: SubrateMode; readonly observation: ConnectionParametersObservation | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `UnpairResult :: { readonly outcome: "unsupported" | "unpaired" | "already-unpaired" }`
- `UuidInput :: string | number`
- `WriteMode :: "with-response" | "without-response"`
- `WriteReadinessEvent :: { readonly state: BleControlObservationState; readonly mode: "without-response"; readonly ready: boolean | null; readonly connectionGeneration: string; readonly observedAtMonotonicMs: number; readonly source: BleObservationSource; readonly authority: string; readonly limitations: readonly Limitation[] }`
- `createConnectionSupervisor :: <Session = undefined>(manager: BleManager, peer: string | BlePeer | PeerReference, options: ConnectionSupervisorOptions<Session>) => ConnectionSupervisor<Session>`
- `decodePeerReference :: (value: string) => PeerReference`
- `encodePeerReference :: (reference: PeerReference) => string`
- `mergePeerDirectoryRecords :: (records: readonly PeerDirectoryRecord[]) => readonly BlePeer[]`
- `withRequiredSecurity :: <Value>(security: BleSecurity, peer: SecurityPeer, requirement: SecurityRequirement, action: () => Promise<Value>, options?: RequiredSecurityOptions) => Promise<Value>`
