// src/index.ts — PR1 final: application-only root
//
// Host-neutral public API for unified-ble-manager 4.0.
// Do not import a backend here — hosts select via explicit factories
// (/react-native, /expo, /web, /tauri, /node/*). Low-level contracts live in
// /advanced and backend authoring in /backend-sdk.

// --- Non-generic application façade ---
export type {
  BleManager,
  BlePeer,
  BleConnection,
  ScanSession,
  GattDatabase,
  GattService,
  GattCharacteristic,
  GattDescriptor,
  GattSubscriptionValue,
  ScanOptions
} from './public/ble-manager'
export { BUILT_IN_FEATURE_CATALOG, BUILT_IN_FEATURE_IDS } from './backend-contract/capabilities'
export type { BleCapabilities, BuiltInFeatureId, CapabilityDescriptor, FeatureId } from './public/capabilities'
export type { BleAdapter, BleAdapterState } from './public/ble-adapter'

// --- Operation options & stream presets (public types only) ---
export type { OperationOptions } from './public/operation-options'
export type { StreamPreset } from './public/stream-presets'

// --- Host identity (ephemeral vs restoration) ---
export type { BleManagerCreateOptions, DiagnosticsOptions } from './public/host-identity'

// --- Errors & recovery (public) ---
export { BleError } from './public/errors'
export type { BleRecovery, BleRecoveryDisposition } from './public/errors'

// Re-export cleanup evidence types that are safe for app code (read-only)
export type { CleanupRecord, CleanupFailure } from './backend-contract/errors'
