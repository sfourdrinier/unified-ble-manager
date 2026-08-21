// src/public/index.ts — façade barrel (PR1 skeleton)

export type { OperationOptions } from './operation-options'
export { normalizeOperationOptions, composeAbortSignal } from './operation-options'
export type { StreamPreset, StreamBudget, StreamPresetInput } from './stream-presets'
export { resolveStreamPreset, STREAM_PRESET_DEFAULTS } from './stream-presets'
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
