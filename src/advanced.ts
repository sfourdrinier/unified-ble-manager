// src/advanced.ts
//
// Expert entrypoint: exact deadlines, stream budgets, raw paths/receipts,
// ownership, and trace controls. Wraps the same implementation; does not fork
// behavior from the application façade.

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
  PortableConnectionPath,
  PortableDatabasePath,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableGattDatabaseSnapshot,
  PortableOperationOptions,
  PortableSubscriptionOptions,
  PortableWritePolicy,
  PortableCleanupRecord,
  PortableCleanupFailure,
  PortableLongWriteReceipt
} from './manager/consumer-handles'

export type {
  CharacteristicPath,
  DescriptorPath,
  ServicePath,
  DatabasePath,
  DevicePath,
  GattDatabaseSnapshot,
  Service,
  Characteristic,
  Descriptor
} from './backend-contract/gatt'

export type {
  BackendIdentity,
  AdapterSelection,
  BackendProvider,
  AdapterStateSnapshot
} from './backend-contract/identity'
export type { ManagerConstruction, AttachedBackend } from './backend-contract/backend'
export type { CapabilityDescriptor, FeatureId } from './backend-contract/capabilities'
export type { CleanupRecord, CleanupFailure } from './backend-contract/errors'

export {
  createBleManager,
  createBleManagerFromProvider,
  createBleManagerFromBackend,
  attachBleBackend,
  createManagerOwnershipAuthority
} from './manager/ble-manager'
export type {
  BleManagerOptions,
  ProviderBleManagerConstruction,
  BackendBleManagerConstruction
} from './manager/ble-manager'
export { ManagerOwnershipAuthority } from './manager/manager-ownership-authority'
export type { OwnershipTransferGrant } from './manager/manager-ownership-authority'

export type { DiagnosticTraceDocument, DiagnosticTraceRecord } from './diagnostics/trace-format'

// Re-export façade utilities for expert callers that need exact control.
export { normalizeOperationOptions } from './public/operation-options'
export type { NormalizedOperationOptions } from './public/operation-options'
export { resolveStreamPreset } from './public/stream-presets'
export type { StreamPreset, StreamBudget } from './public/stream-presets'
export { deriveRestorationIdentity, createEphemeralHostIdentity } from './public/host-identity'
export type { EphemeralHostIdentity, RestorationHostIdentity } from './public/host-identity'
