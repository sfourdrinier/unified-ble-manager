# API Report — unified-ble-manager (root)

> Do not edit directly. Generated via `pnpm exec tsc --project tsconfig.build.json`.
> Review changes to this file as you would review public API changes.

## Application façade (PR1 final — application-only)

```ts
// Host-neutral, non-generic — no backend construction, no branded Deadline/Capacity, no Portable* paths
export interface BleManager { readonly capabilities: BleCapabilities; readonly destroy(): Promise<CleanupRecord>; scan(options?: ScanOptions): Promise<ScanSession>; connect(peer: BlePeer | string, options?: OperationOptions): Promise<BleConnection>; withConnection<T>(peer: BlePeer | string, options: OperationOptions, action: (c: BleConnection) => Promise<T>): Promise<T> }
export interface BlePeer { readonly id: string; readonly name: string | null; readonly rssi: number | null }
export interface BleConnection { readonly peer: BlePeer; disconnect(): Promise<CleanupRecord>; release(): Promise<CleanupRecord> }
export interface ScanSession { readonly stop(): Promise<CleanupRecord>; readonly observations: BoundedAsyncStream<PublicScanObservation> }
export interface GattDatabase { readonly peer: BlePeer }
export interface GattService { readonly uuid: string }
export interface GattCharacteristic { readonly uuid: string; read(options?: OperationOptions): Promise<Uint8Array>; write(value: Uint8Array, options?: OperationOptions): Promise<void> }
export interface GattDescriptor { readonly uuid: string; read(options?: OperationOptions): Promise<Uint8Array>; write(value: Uint8Array, options?: OperationOptions): Promise<void> }
export interface BleAdapter { readonly id: string | null; readonly state: BleAdapterState }
export interface BleAdapterState { readonly available: boolean; readonly poweredOn: boolean }
export class BleError extends Error { readonly code: BleErrorCode; readonly domain: BleErrorDomain; readonly operation: string; readonly platform: PlatformErrorDetail | null; readonly limitations: readonly Limitation[]; readonly recovery: BleRecovery }
export interface BleRecovery { readonly disposition: BleRecoveryDisposition; readonly actions: readonly RecoveryAction[] }
export interface BleCapabilities { supports(id: BuiltInFeatureId): boolean; get(id: FeatureId): CapabilityDescriptor | undefined; require(id: BuiltInFeatureId): CapabilityDescriptor; list(): readonly CapabilityDescriptor[] }
export type BleRecoveryDisposition = 'none' | 'retry-immediately' | 'retry-with-backoff' | 'after-state-change' | 'after-user-action' | 'caller-policy'
export interface OperationOptions { readonly signal?: AbortSignal; readonly timeoutMs?: number }
export type StreamPreset = 'latest' | 'balanced' | 'lossless-bounded' | 'custom'
export interface StreamBudget { readonly itemCapacity: Capacity; readonly byteCapacity: Capacity; readonly reservedControlCapacity: Capacity; readonly overflowPolicy: OverflowPolicy }
export interface BleManagerCreateOptions { readonly instanceId?: string; readonly adapterId?: string; readonly diagnostics?: DiagnosticsOptions; readonly restoration?: { readonly applicationId: string; readonly restorationId: string; readonly generation?: string } }
export function normalizeOperationOptions(options: OperationOptions | undefined, now: () => number, existingDeadline?: Deadline | null): NormalizedOperationOptions
export function composeAbortSignal(outer: AbortSignal | null, inner: AbortSignal | null): AbortSignal | null
export function resolveStreamPreset(input: StreamPresetInput): StreamBudget
export const STREAM_PRESET_DEFAULTS: Record<StreamPreset, StreamBudget>
```

## Notes

- Root is host-neutral. No provider, no radio selection on import. No `attachBleBackend`, no `Capacity`/`Deadline` branded types, no `Portable*` paths — those live in `/advanced` and `/backend-sdk`.
- All host factories (`/react-native`, `/expo`, `/web`, `/tauri`, `/node/*`) return the same `BleManager` type. Capability differences come from trusted-host bootstrap.
- Low-level helpers (`find`, `scanUntil`, `withConnection`, `defaultScanDelivery`, etc.) and branded primitives now live in `/advanced`.
