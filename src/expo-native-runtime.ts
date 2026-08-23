import type { Spec as NativeExpoRuntime } from './NativeUnifiedBleExpoRuntime'

/** Lazily resolves the Expo-only runtime/configuration TurboModule in a React Native runtime. */
export function getNativeUnifiedBleExpoRuntime(): NativeExpoRuntime {
  const module: {
    readonly default: NativeExpoRuntime
  } = require('./NativeUnifiedBleExpoRuntime')
  return module.default
}
