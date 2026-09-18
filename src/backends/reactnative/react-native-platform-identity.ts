// src/backends/reactnative/react-native-platform-identity.ts
//
// The registered identities and compatibility offers of the React Native
// hosts. They are host facts, independent of which implementation serves the
// host, so the production Rust route and the legacy reference providers read
// them from here.

import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { opaqueId, version, versionRange, type NativeCompatibilityOffer } from '../../backend-contract/primitives'

export const REACT_NATIVE_ANDROID_BACKEND_ID = 'unified-ble:react-native-android'
export const REACT_NATIVE_ANDROID_PLATFORM_ID = 'unified-ble:android-gatt'
export const REACT_NATIVE_ANDROID_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION
export const REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID = 'android-default-adapter'

export const REACT_NATIVE_APPLE_BACKEND_ID = 'unified-ble:react-native-apple'
export const REACT_NATIVE_APPLE_PLATFORM_ID = 'unified-ble:apple-corebluetooth'
export const REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION
export const REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID = 'apple-corebluetooth-default-adapter'

const reactNativeCompatibility: NativeCompatibilityOffer = Object.freeze({
  backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
  capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
  eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
  traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1)),
  nativeProtocol: versionRange(version('native-protocol', 2), version('native-protocol', 2))
})

export const reactNativeAndroidCompatibility: NativeCompatibilityOffer = reactNativeCompatibility
export const reactNativeAppleCompatibility: NativeCompatibilityOffer = reactNativeCompatibility

export function reactNativeAndroidDefaultAdapterId() {
  return opaqueId(REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID, 'adapter', 'react-native-android')
}

export function reactNativeAppleDefaultAdapterId() {
  return opaqueId(REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID, 'adapter', 'react-native-apple')
}
