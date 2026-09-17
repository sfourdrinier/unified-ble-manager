// src/backends/reactnative/react-native-apple-provider.ts
//
// R02 Apple cutover: Rust authority for the Apple path.
//
// The Apple provider executes the native Rust core through an injected
// binding or fails LOUDLY. BLE admission (listAdapters/create) REQUIRES a
// `rustCore` binding: without one it rejects with `capability.unsupported`
// before any Swift control-surface work, and a foreign contract revision
// fails closed with `protocol.incompatible`. There is no silent
// Swift-only fallback and no synthetic success — the Swift JSI boundary
// below (Owned CoreBluetooth radio) is the OS effector the core authorizes
// in production, not a parallel BLE implementation this provider drives.
//
// With a binding, BLE dispatches through the binding-backed provider for
// platform 'apple' (see `./react-native-rust-core-provider`): the op name
// and args cross verbatim via the admitted core session and the raw core
// result returns. The generated protocol control below travels only as
// restoration identity (never BLE work), exactly as on the binding-backed
// path. Backend identity keeps the Apple backend/platform ids; the runtime
// diagnostics report the core-session transport wording that matches this
// reality.

import { contractError } from '../../backend-contract/errors'
import type { AdapterSelection } from '../../backend-contract/identity'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { opaqueId, version, versionRange, type NativeCompatibilityOffer } from '../../backend-contract/primitives'
import type { Spec as NativeProtocolControl } from '../../NativeUnifiedBleProtocolControl'
import { coreBluetoothCompatibility } from '../corebluetooth/corebluetooth-provider'
import {
  createReactNativeRustCoreBackendProvider,
  type ReactNativeRustCoreBackendProvider
} from './react-native-rust-core-provider'
import { resolveReactNativeRustCoreBinding, type ReactNativeRustCoreBinding } from './react-native-rust-core'
import type { ReactNativeRestorationBackendProvider } from './react-native-restoration'

export const REACT_NATIVE_APPLE_BACKEND_ID = 'unified-ble:react-native-apple'
export const REACT_NATIVE_APPLE_PLATFORM_ID = 'unified-ble:apple-corebluetooth'
export const REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION
export const REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID = 'apple-corebluetooth-default-adapter'

export const reactNativeAppleCompatibility: NativeCompatibilityOffer = Object.freeze({
  ...coreBluetoothCompatibility,
  nativeProtocol: versionRange(version('native-protocol', 2), version('native-protocol', 2))
})

let nextBoundaryOwner = 1

export interface ReactNativeAppleBackendProviderOptions {
  /** Generated control module carrying restoration identity (never BLE work). */
  readonly control: NativeProtocolControl
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /**
   * R02 authority: the injected native Rust core binding. BLE admission
   * requires it — without one, listAdapters/create fail loudly with
   * `capability.unsupported` and never open the Swift-only radio.
   */
  readonly rustCore?: ReactNativeRustCoreBinding
}

/** Creates the core-backed Apple provider without importing React Native from this public module. */
export function createReactNativeAppleBackendProvider(
  options: ReactNativeAppleBackendProviderOptions
): ReactNativeRestorationBackendProvider {
  const createOwnerId = options.createOwnerId ?? allocateBoundaryOwnerId
  // One binding-backed provider per Apple provider, so every backend shares
  // the single restoration coordinator exposed below (the probe path never
  // activates restoration; create does, against this same coordinator).
  let coreProvider: ReactNativeRustCoreBackendProvider | null = null
  const requireCoreProvider = (): ReactNativeRustCoreBackendProvider => {
    if (coreProvider === null) {
      if (options.rustCore === undefined) {
        throw contractError('capability.unsupported', 'capability', 'react-native-apple.provider.rust-core-missing')
      }
      coreProvider = createReactNativeRustCoreBackendProvider({
        platform: 'apple',
        binding: resolveReactNativeRustCoreBinding(options.rustCore),
        owner: 'react-native-apple',
        now: options.now,
        control: options.control,
        ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
      })
    }
    return coreProvider
  }
  return Object.freeze({
    descriptor: Object.freeze({
      providerId: 'unified-ble:react-native-apple-provider',
      hostKind: 'native-mobile',
      loadability: 'loadable',
      compatibility: reactNativeAppleCompatibility
    }),
    get restoration() {
      return requireCoreProvider().restoration
    },
    listAdapters: async () => {
      const ownerId = createOwnerId()
      if (ownerId.length === 0) {
        throw contractError('argument.invalid', 'core', 'react-native-apple.provider.owner-id')
      }
      return requireCoreProvider().listAdapters()
    },
    create: async (selection: AdapterSelection<string>) => {
      // Authority first: without a core there is no adapter to select, so
      // the missing-binding rejection precedes the selection check.
      const provider = requireCoreProvider()
      if (String(selection.selectedAdapterId) !== REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID) {
        throw contractError('adapter.unavailable', 'adapter', 'react-native-apple.provider.select-adapter')
      }
      const ownerId = createOwnerId()
      if (ownerId.length === 0) {
        throw contractError('argument.invalid', 'core', 'react-native-apple.provider.owner-id')
      }
      return provider.create(selection)
    }
  })
}

function allocateBoundaryOwnerId(): string {
  const ordinal = nextBoundaryOwner
  nextBoundaryOwner += 1
  return `react-native-apple-owner-${ordinal}`
}

export function reactNativeAppleDefaultAdapterId() {
  return opaqueId(REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID, 'adapter', 'react-native-apple')
}
