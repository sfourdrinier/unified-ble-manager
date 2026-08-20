# API Report — unified-ble-manager (root)

> Do not edit directly. Generated via `pnpm exec tsc --project tsconfig.build.json` and `scripts/api/generate-api-reports.js`.
> Review changes to this file as you would review public API changes.

## Application façade (PR1)

```ts
// Host-neutral, non-generic
export class ApplicationBleManager implements BleManager { ... }
export function createPublicBleManager(...): Promise<BleManager>
export interface BleManager { readonly destroy(): Promise<CleanupRecord>; scan(options?: ScanOptions): Promise<ScanSession>; connect(peer: BlePeer | string, options?: OperationOptions): Promise<BleConnection>; withConnection<T>(peer: BlePeer | string, options: OperationOptions, use: (c: BleConnection) => Promise<T>): Promise<T> }
export interface BlePeer { readonly id: string; readonly name: string | null; readonly rssi: number | null }
export interface BleConnection { readonly peer: BlePeer; disconnect(): Promise<CleanupRecord>; release(): Promise<CleanupRecord> }
export interface ScanSession { readonly stop(): Promise<CleanupRecord>; readonly observations: AsyncIterable<AdvertisementObservation<string>> }
export interface GattDatabase { readonly peer: BlePeer }
export interface GattService { readonly uuid: string }
export interface GattCharacteristic { readonly uuid: string; read(options?: OperationOptions): Promise<Uint8Array>; write(value: Uint8Array, options?: OperationOptions): Promise<void> }
export interface BleAdapter { readonly id: string | null; readonly state: BleAdapterState }
export interface BleAdapterState { readonly available: boolean; readonly poweredOn: boolean }
export class BleError extends BackendContractError { readonly recovery: BleRecovery }
export interface BleRecovery { readonly disposition: BleRecoveryDisposition; readonly actions: readonly string[] }
export type BleRecoveryDisposition = 'none' | 'retry-immediately' | 'retry-with-backoff' | 'after-state-change' | 'after-user-action' | 'caller-policy'
export interface OperationOptions { readonly signal?: AbortSignal; readonly timeoutMs?: number }
export type StreamPreset = 'latest' | 'balanced' | 'lossless-bounded' | 'custom'
export interface StreamBudget { readonly itemCapacity: Capacity; readonly byteCapacity: Capacity; readonly reservedControlCapacity: Capacity; readonly overflowPolicy: OverflowPolicy }
export interface BleManagerCreateOptions { readonly instanceId?: string; readonly adapterId?: string; readonly diagnostics?: DiagnosticsOptions; readonly restoration?: { readonly applicationId: string; readonly restorationId: string; readonly generation?: string } }
```

## Preserved RC1 surface (still exported via root in phase 2 for test compatibility; will move to /advanced in final)

```ts
export { BleManager as LegacyBleManager, createBleManager, attachBleBackend, … } from './manager'
export { capacity, deadline, byteLimit, canonicalUuid } from './backend-contract/primitives'
export type { Capacity, Deadline, AttachmentId, ManagerId, PeerId, Uuid } from './backend-contract/primitives'
```

## Notes

- Root is host-neutral. No provider, no radio selection on import.
- After PR1 final, root will be **application-only**: no `attachBleBackend`, no `Capacity`/`Deadline` branded types, no `Portable*` paths. Those move to `/advanced` and `/backend-sdk`.
- Host factories (`/react-native`, `/expo`, `/web`, `/tauri`, `/node/*`) return the same `BleManager` type.
