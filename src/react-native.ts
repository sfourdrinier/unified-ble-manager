// src/react-native.ts

/**
 * Explicit React Native host entrypoint. Importing the root package remains
 * host-neutral. Every factory here runs the process-owned Rust mobile owner
 * through the `UnifiedBleRustCore` TurboModule; there is no other route.
 */
export {
  reactNativeAndroidCompatibility,
  reactNativeAndroidDefaultAdapterId,
  REACT_NATIVE_ANDROID_BACKEND_ID,
  REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_ANDROID_IMPLEMENTATION_VERSION,
  REACT_NATIVE_ANDROID_PLATFORM_ID,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId,
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION,
  REACT_NATIVE_APPLE_PLATFORM_ID
} from './backends/reactnative/react-native-platform-identity'
export {
  createReactNativeAndroidBackendProvider,
  createReactNativeAppleBackendProvider
} from './backends/reactnative/react-native-providers'
export type {
  ReactNativeAndroidBackendProviderOptions,
  ReactNativeAppleBackendProviderOptions
} from './backends/reactnative/react-native-providers'
export { createReactNativeBleManagerWithEnvironment } from './react-native-manager'
export type { ReactNativeBleManagerOptions, ReactNativeBlePlatform } from './react-native-manager'
export {
  createReactNativeRustCoreBackendProvider,
  REACT_NATIVE_RUST_CORE_BACKEND_ID,
  REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION
} from './backends/reactnative/react-native-rust-core-provider'
export type {
  ReactNativeRustCoreBackendProvider,
  ReactNativeRustCorePlatform,
  ReactNativeRustCoreProviderOptions
} from './backends/reactnative/react-native-rust-core-provider'
export {
  RUST_CORE_CONTRACT_REVISION,
  resolveReactNativeRustCoreBinding
} from './backends/reactnative/react-native-rust-core'
export type {
  ReactNativeRustCoreBinding,
  ReactNativeRustCoreSession,
  RustCoreRestorationIdentityRequest
} from './backends/reactnative/react-native-rust-core'
export { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
export type {
  ReactNativeRustCoreBindingOptions,
  ReactNativeRustCoreBindingPlatform
} from './backends/reactnative/react-native-rust-core-binding'
export type { ReactNativeRestorationAuthority } from './backends/reactnative/react-native-rust-core-restoration'
export type { ReactNativeRustCoreRuntimeFacts } from './backends/reactnative/react-native-rust-core-features'
export { createReactNativeBleManager } from './react-native-app-manager'
export type { CreateReactNativeBleManagerOptions } from './react-native-app-manager'
export type { BleManagerCreateOptions } from './public/host-identity'
export {
  combineReactNativeFeatureRegistries,
  createReactNativeRestorationFeatureRegistry,
  ReactNativeRestorationActivation,
  ReactNativeRestorationCoordinator
} from './backends/reactnative/react-native-restoration'
export type {
  ReactNativeRestorationAdoptionRecord,
  ReactNativeRestorationAdoptionRequestRecord,
  ReactNativeRestorationBackendProvider,
  ReactNativeRestorationJournal,
  ReactNativeRestorationReplayRecord
} from './backends/reactnative/react-native-restoration'
export type { Spec as NativeUnifiedBleRustCore, RustCoreSessionWake } from './NativeUnifiedBleRustCore'
