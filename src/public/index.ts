// src/public/index.ts — façade barrel (PR1 skeleton)

export type { OperationOptions } from './operation-options'
export { normalizeOperationOptions, composeAbortSignal } from './operation-options'
export type { StreamPreset, StreamPolicy, StreamBudget, StreamPresetInput, CustomStreamBudget } from './stream-presets'
export { resolveStreamPolicy, resolveStreamPreset, STREAM_PRESET_DEFAULTS } from './stream-presets'
export type {
  CleanupFailure,
  CleanupRecord,
  NormalizedBleError,
  PublicPlatformErrorDetail,
  PublicSerializableRecord,
  PublicSerializableValue
} from './cleanup'
export type {
  PublicBoundedAsyncStream,
  PublicBoundedAsyncStreamIterator,
  PublicStreamItem,
  PublicStreamLimits,
  PublicStreamOverflowNotice,
  PublicStreamOverflowPolicy,
  PublicStreamTerminalNotice,
  PublicStreamTerminalReason,
  PublicStreamValue
} from './streams'
export type {
  BleManagerCreateOptions,
  DiagnosticsOptions,
  EphemeralHostIdentity,
  RestorationHostIdentity
} from './host-identity'
export {
  deriveRestorationIdentity,
  createEphemeralHostIdentity,
  normalizeBleManagerCreateOptions
} from './host-identity'

export type {
  GattDatabase,
  GattDatabaseSnapshot,
  GattService,
  GattCharacteristic,
  GattDescriptor,
  GattSubscription,
  GattValueEvent,
  GattValueStream,
  GattDatabaseChangedEvent,
  GattWriteReceipt,
  GattLongWriteReceipt,
  GattWriteOptions,
  LongWriteOptions,
  DescriptorWriteOptions,
  GattSubscribeOptions,
  GattCharacteristicProperties,
  GattAccessRequirements,
  GattServiceReference,
  OccurrenceSelector,
  GattPathSelector,
  UuidInput
} from './gatt'
export type {
  BleConnectionEvent,
  BleConnectionControls,
  BleControlObservationMetadata,
  BleControlObservationState,
  BleObservationSource,
  RssiObservation,
  MtuObservation,
  MtuNegotiation,
  MtuNegotiationState,
  MaximumWriteLengthObservation,
  ConnectionPriority,
  ConnectionPriorityResult,
  PhyPreference,
  PhyObservation,
  PhyUpdateResult,
  ConnectionParametersObservation,
  SubrateMode,
  SubrateResult,
  WriteMode,
  WriteReadinessEvent,
  RediscoverGattOptions,
  PublicScanObservation,
  ScanPlan,
  ConnectionIntent,
  BlePhy,
  ConnectOptions,
  FindOptions,
  ChooseOptions,
  ChooseFilter,
  BleDiscoveryInfo,
  ScanQuery,
  ScanClause,
  ManufacturerDataPattern,
  ServiceDataPattern,
  NormalizedScanQuery,
  NormalizedScanObservation
} from './ble-manager'
export type { BleDiagnostics, BleDiagnosticsSnapshot, BleResourceCounters } from './diagnostics'
export type { BlePeerDirectory, BlePeerState, KnownPeerQuery, PeerSource } from './peer-directory'
export type { PeerDirectoryRecord } from './peer-directory'
export { mergePeerDirectoryRecords } from './peer-directory'
export type { PeerReference, PeerReferenceScope } from './peer-reference'
export { encodePeerReference, decodePeerReference } from './peer-reference'
export type {
  BleSecurity,
  PairCancelResult,
  PairingAgent,
  PairingChallenge,
  PairingResponse,
  PairOptions,
  RequiredSecurityOptions,
  PairResult,
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityAuthenticationState,
  SecurityBondState,
  SecurityEncryptionState,
  SecureConnectionsState,
  SecurityPeer,
  UnpairResult,
  SecurityRequirement
} from './security'
export { withRequiredSecurity } from './security'
export { createConnectionSupervisor } from './connection-supervisor'
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
} from './connection-supervisor'
