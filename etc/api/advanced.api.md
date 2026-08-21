# API Report — unified-ble-manager/advanced

```ts
export { deadline, capacity, byteLimit, canonicalUuid } from './backend-contract/primitives'
export type { Deadline, Capacity, ByteLimit, Uuid, AttachmentId, ManagerId, PeerId, ClientId, ConnectionId, LeaseId } from './backend-contract/primitives'
export { BleManager, createBleManager, createBleManagerFromProvider, createBleManagerFromBackend, attachBleBackend, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } from './manager/ble-manager'
export { ManagerOwnershipAuthority } from './manager/manager-ownership-authority'
export { collectNotifications, connectAndDiscover, defaultScanDelivery, find, firstNotification, scanForServices, scanUntil, throwIfCleanupFailed, withConnection, withDiscoveredConnection } from './manager/public-helpers'
export { normalizeOperationOptions, resolveStreamPreset, deriveRestorationIdentity, createEphemeralHostIdentity } from './public/*'
export type { NormalizedBleError, FeatureRegistry, MaximumWriteLengthObservation, ConnectionLifecycleEvent, ScanOptions } from './backend-contract/*'
```
