// src/backends/reactnative/react-native-providers.ts
//
// The public React Native provider factories. Both run the process-owned Rust
// mobile owner through the `UnifiedBleRustCore` TurboModule; there is no other
// route. `rustCore` substitutes the native module (tests and embedding hosts)
// and is still admitted and identity-checked like the production module.

import { Platform } from 'react-native'

import type { ReactNativeRustCoreBinding } from './react-native-rust-core'
import { createReactNativeRustCoreBinding } from './react-native-rust-core-binding'
import {
  createReactNativeRustCoreBackendProvider,
  type ReactNativeRustCoreBackendProvider
} from './react-native-rust-core-provider'
import type { ReactNativeRestorationAuthority } from './react-native-rust-core-restoration'

export interface ReactNativeAndroidBackendProviderOptions {
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /** Substitute native Rust core binding; absent, the `UnifiedBleRustCore` TurboModule. */
  readonly rustCore?: ReactNativeRustCoreBinding
  /** The Android API level; absent, React Native's `Platform.Version`. */
  readonly androidApiLevel?: number
}

export interface ReactNativeAppleBackendProviderOptions {
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /** Substitute native Rust core binding; absent, the `UnifiedBleRustCore` TurboModule. */
  readonly rustCore?: ReactNativeRustCoreBinding
  /** The app-declared restoration authority (`restorationIdentity`), when the app configured one. */
  readonly restorationAuthority?: ReactNativeRestorationAuthority
}

/** The host's Android API level as React Native reports it; `null` when it reports none. */
export function hostAndroidApiLevel(): number | null {
  const version: unknown = Platform.Version
  return typeof version === 'number' && Number.isSafeInteger(version) ? version : null
}

/** The Android provider over the Rust mobile owner. */
export function createReactNativeAndroidBackendProvider(
  options: ReactNativeAndroidBackendProviderOptions
): ReactNativeRustCoreBackendProvider {
  return createReactNativeRustCoreBackendProvider({
    platform: 'android',
    binding: options.rustCore ?? createReactNativeRustCoreBinding({ platform: 'android' }),
    owner: 'react-native-android',
    now: options.now,
    runtime: { androidApiLevel: options.androidApiLevel ?? hostAndroidApiLevel() },
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
}

/** The Apple provider over the Rust mobile owner, with CoreBluetooth state-restoration adoption. */
export function createReactNativeAppleBackendProvider(
  options: ReactNativeAppleBackendProviderOptions
): ReactNativeRustCoreBackendProvider {
  const authority = options.restorationAuthority ?? null
  return createReactNativeRustCoreBackendProvider({
    platform: 'apple',
    binding: options.rustCore ?? createReactNativeRustCoreBinding({ platform: 'apple' }),
    owner: 'react-native-apple',
    now: options.now,
    runtime: { androidApiLevel: null },
    restorationAuthority: () => authority,
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
}
