// src/index.ts

/**
 * Host-neutral public API for unified-ble-manager 4.0.
 *
 * Hosts select and construct a backend explicitly. Backend implementation and
 * deterministic controls are intentionally isolated behind `backend-sdk` and
 * `testing`; importing this entrypoint does not select a host or radio.
 */
export {
  attachBleBackend,
  BleManager,
  Connection,
  collectNotifications,
  connectAndDiscover,
  createBleManager,
  createBleManagerFromProvider,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS,
  defaultScanDelivery,
  DiscoveredGattDatabase,
  find,
  firstNotification,
  ScanSession,
  scanForServices,
  scanUntil,
  Subscription,
  throwIfCleanupFailed,
  withConnection,
  withDiscoveredConnection
} from './manager'
export type {
  BleConnectionHandle,
  BleManagerLifetime,
  BleManagerOptions,
  CollectNotificationsOptions,
  ConnectedGattDatabase,
  DeadlineHandle,
  DiscoveredGattDatabaseHandle,
  PortableAttachmentRecord,
  PortableBoundedAsyncStream,
  PortableBoundedAsyncStreamIterator,
  PortableCleanupFailure,
  PortableCleanupRecord,
  PortableConnectionLifecycleEvent,
  PortableConnectionPath,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableDatabasePath,
  PortableGattDatabaseSnapshot,
  PortableLongWriteChunkProgress,
  PortableLongWriteReceipt,
  PortableMaximumWriteLengthObservation,
  PortableNotificationValue,
  PortableOperationOptions,
  PortableOperationTerminalRecord,
  PortableSubscriptionOptions,
  PortableWritePolicy,
  PortableWriteReceipt,
  ProviderBleManagerConstruction,
  ScanUntilOptions,
  SubscriptionHandle
} from './manager'

export { BackendContractError } from './backend-contract/errors'
export type {
  BleErrorCode,
  BleErrorDomain,
  CleanupFailure,
  CleanupRecord,
  NormalizedBleError,
  PlatformErrorDetail
} from './backend-contract/errors'

export type {
  AdvertisementField,
  AdvertisementInput,
  AdvertisementObservation,
  DeviceAddress,
  DeviceIdentity,
  FieldProvenance,
  JoinScanSharing,
  OwnerScanSharing,
  ManufacturerDataFilter,
  ScanFilter,
  ScanOptions,
  ScanSharing,
  SourceTimestamp
} from './backend-contract/advertisement'
export type {
  EvidenceLevel,
  EvidenceReceipt,
  CapabilityDescriptor,
  CapabilityLimit,
  CapabilityLimits,
  FeatureId,
  FeatureRegistry,
  FeatureState,
  Limitation
} from './backend-contract/capabilities'
export type {
  Characteristic,
  CharacteristicProperties,
  CharacteristicPath,
  DatabasePath,
  Descriptor,
  DescriptorPath,
  DevicePath,
  GattDatabaseSnapshot,
  MaximumWriteLengthObservation,
  NotificationValue,
  PathValidity,
  Service,
  ServicePath
} from './backend-contract/gatt'
export type { ManagerState, OwnerMode, ResourceCounters } from './backend-contract/backend'
export type { RestorationAdoptionRequest, RestorationAdoptionResult } from './backend-contract/restoration'
export { MAXIMUM_REQUESTED_ATT_MTU, MINIMUM_ATT_MTU } from './backend-contract/connection-controls'
export type {
  ConnectionControlCapabilities,
  ConnectionControlSupport,
  MtuNegotiation,
  RssiMeasurement
} from './backend-contract/connection-controls'
export type { ConnectionLifecycleCause, ConnectionLifecycleEvent } from './backend-contract/connection-lifecycle'
export type { BackendIdentity } from './backend-contract/identity'
export { isAuthorizationBlocking } from './backend-contract/identity'
export type {
  DiagnosticTraceDocument,
  DiagnosticTraceKind,
  DiagnosticTraceRecord,
  TraceValidationFailure,
  TraceValidationResult
} from './diagnostics/trace-format'
export type {
  LongWriteChunkProgress,
  LongWriteNotPlannedReceipt,
  LongWritePlannedReceipt,
  LongWritePolicy,
  LongWriteReceipt,
  OperationTerminalOutcome,
  OperationTerminalRecord,
  PublicOperationOptions,
  SubscriptionOptions,
  WriteMode,
  WritePolicy,
  WriteReceipt
} from './backend-contract/operations'
export { byteLimit, canonicalUuid, capacity, deadline } from './backend-contract/primitives'
export type {
  AttachmentId,
  ByteLimit,
  Capacity,
  Deadline,
  ManagerId,
  MonotonicTimestamp,
  OwnedBytes,
  PeerId,
  ResourceCount,
  Uuid
} from './backend-contract/primitives'
export type {
  BoundedAsyncStream,
  BoundedAsyncStreamIterator,
  OverflowPolicy,
  StreamItem,
  StreamLimits,
  StreamOverflowNotice,
  StreamTerminalNotice,
  StreamValue
} from './backend-contract/streams'

// PR1 façade — non-generic application surface (additive in phase 1, will become sole root export in phase 2)
export { BleManagerImpl as ApplicationBleManager, createPublicBleManager } from './public/ble-manager'
export type {
  BlePeer,
  BleConnection as PublicBleConnection,
  ScanSession as PublicScanSession
} from './public/ble-manager'
export type { BleAdapter, BleAdapterState } from './public/ble-adapter'
export { BleError as PublicBleError } from './public/errors'
export type { BleRecovery, BleRecoveryDisposition } from './public/errors'
export type { OperationOptions } from './public/operation-options'
export { normalizeOperationOptions, composeAbortSignal } from './public/operation-options'
export type { StreamPreset, StreamBudget } from './public/stream-presets'
export { resolveStreamPreset, STREAM_PRESET_DEFAULTS } from './public/stream-presets'
export type { BleManagerCreateOptions, DiagnosticsOptions } from './public/host-identity'
export {
  deriveRestorationIdentity,
  createEphemeralHostIdentity,
  normalizeBleManagerCreateOptions
} from './public/host-identity'
