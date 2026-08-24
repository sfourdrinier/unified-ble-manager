// src/advanced.ts
//
// Expert entrypoint: exact deadlines, stream budgets, raw paths/receipts,
// ownership, and trace controls. Wraps the same implementation; does not fork
// behavior from the application façade.

import { createPublicBleManager } from './public/ble-manager'
import type { BleManager as PublicBleManager } from './public/ble-manager'
import type { BackendIdentity } from './backend-contract/identity'
import type { BleManager as AdvancedBleManager } from './manager/ble-manager'

/** Projects an already-owned advanced manager into the stable public façade. */
export function createPublicBleManagerFacade(
  internal: AdvancedBleManager<string, BackendIdentity<string>>,
  now: () => number
): Promise<PublicBleManager> {
  return createPublicBleManager(internal, now)
}

export { deadline, capacity, byteLimit, canonicalUuid } from './backend-contract/primitives'
export type {
  Deadline,
  Capacity,
  ByteLimit,
  Uuid,
  AttachmentId,
  ManagerId,
  ClientId,
  PeerId,
  ConnectionId,
  LeaseId,
  ScanSessionId,
  ScanShareToken,
  BackendInstanceId,
  GenerationId,
  OwnedBytes,
  MonotonicTimestamp,
  OpaqueId
} from './backend-contract/primitives'

export type {
  PublicOperationOptions,
  SubscriptionOptions,
  WritePolicy,
  LongWritePolicy
} from './backend-contract/operations'
export type {
  OperationOptions,
  WriteReceipt,
  LongWriteReceipt,
  LongWritePlannedReceipt,
  LongWriteNotPlannedReceipt,
  LongWriteChunkProgress,
  OperationTerminalRecord,
  SubscriptionOptions as AdvancedSubscriptionOptions
} from './backend-contract/operations'

export type {
  BoundedAsyncStream,
  BoundedAsyncStreamIterator,
  StreamLimits,
  StreamItem,
  StreamValue,
  StreamOverflowNotice,
  StreamTerminalNotice,
  OverflowPolicy
} from './backend-contract/streams'

export type {
  PortableAttachmentRecord,
  PortableBoundedAsyncStream,
  PortableBoundedAsyncStreamIterator,
  PortableConnectionLifecycleEvent,
  PortableConnectionPath,
  PortableDatabasePath,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableGattDatabaseSnapshot,
  PortableLongWriteReceipt,
  PortableMaximumWriteLengthObservation,
  PortableNotificationValue,
  PortableOperationOptions,
  PortableOperationTerminalRecord,
  PortableSubscriptionOptions,
  PortableWritePolicy,
  PortableWriteReceipt,
  PortableCleanupRecord,
  PortableCleanupFailure
} from './manager/consumer-handles'

export type {
  CharacteristicPath,
  CharacteristicProperties,
  GattAccessRequirements,
  GattCharacteristicPropertyAvailability,
  GattDatabaseChangedEvent,
  GattDescriptorProperties,
  GattServiceReference,
  DescriptorPath,
  ServicePath,
  DatabasePath,
  DevicePath,
  GattDatabaseSnapshot,
  Service,
  Characteristic,
  Descriptor,
  MaximumWriteLengthObservation
} from './backend-contract/gatt'

export type {
  BackendIdentity,
  AdapterSelection,
  BackendProvider,
  AdapterStateSnapshot
} from './backend-contract/identity'
export type { ConnectionLifecycleCause, ConnectionLifecycleEvent } from './backend-contract/connection-lifecycle'
export type { ScanOptions, AdvertisementField, AdvertisementObservation } from './backend-contract/advertisement'
export type {
  AttachedBackend,
  BackendPeerRecord,
  BackendPeerQuery,
  ManagerConstruction,
  PeerDirectoryBackend
} from './backend-contract/backend'
export type { CapabilityDescriptor, FeatureId, FeatureRegistry } from './backend-contract/capabilities'
export type { CleanupRecord, CleanupFailure, NormalizedBleError } from './backend-contract/errors'

export {
  BleManager,
  createBleManager,
  createBleManagerFromProvider,
  createBleManagerFromBackend,
  attachBleBackend,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} from './manager/ble-manager'
export type {
  BleConnectionHandle,
  BleManagerLifetime,
  DiscoveredGattDatabaseHandle,
  SubscriptionHandle
} from './manager/consumer-handles'
export type {
  BleManagerOptions,
  ProviderBleManagerConstruction,
  BackendBleManagerConstruction
} from './manager/ble-manager'
export { ManagerOwnershipAuthority } from './manager/manager-ownership-authority'
export type { OwnershipTransferGrant } from './manager/manager-ownership-authority'

export type { DiagnosticTraceDocument, DiagnosticTraceRecord } from './diagnostics/trace-format'

// Public helpers that operate on low-level handles — advanced because they use
// branded deadlines, capacities, and portable paths. Application code should
// prefer the façade's OperationOptions/timeoutMs and StreamPreset.
export {
  collectNotifications,
  connectAndDiscover,
  defaultScanDelivery,
  find,
  firstNotification,
  scanForServices,
  scanUntil,
  throwIfCleanupFailed,
  withConnection,
  withDiscoveredConnection
} from './manager/public-helpers'
export type { ScanUntilOptions, ConnectedGattDatabase, CollectNotificationsOptions } from './manager/public-helpers'
export type {
  ManufacturerDataPattern,
  NormalizedManufacturerDataPattern,
  NormalizedScanClause,
  NormalizedScanObservation,
  NormalizedScanQuery,
  NormalizedServiceDataPattern,
  ScanClause,
  ScanQuery,
  ServiceDataPattern
} from './public/scan-query'
export { normalizeScanObservation, normalizeScanQuery, observationMatchesScanQuery } from './public/scan-query'
export type { BlePeerDirectory, BlePeerState, KnownPeerQuery, PeerSource } from './public/peer-directory'
export type { PeerDirectoryRecord } from './public/peer-directory'
export { mergePeerDirectoryRecords } from './public/peer-directory'
export type { PeerReference, PeerReferenceScope } from './public/peer-reference'
export { encodePeerReference, decodePeerReference } from './public/peer-reference'

// Re-export façade utilities for expert callers that need exact control.
export { normalizeOperationOptions } from './public/operation-options'
export type { NormalizedOperationOptions } from './public/operation-options'
export { resolveStreamPreset } from './public/stream-presets'
export type { StreamPreset, StreamBudget } from './public/stream-presets'
export { deriveRestorationIdentity, createEphemeralHostIdentity } from './public/host-identity'
export type { EphemeralHostIdentity, RestorationHostIdentity } from './public/host-identity'
export { withRequiredSecurity } from './public/security'
export type {
  PairingAgent,
  PairingChallenge,
  PairingResponse,
  RequiredSecurityOptions,
  SecurityRequirement
} from './public/security'
