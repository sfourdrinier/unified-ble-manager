// src/react-native-manager.ts

import type { NativeBackendIdentity } from './backend-contract/identity'
import { contractError } from './backend-contract/errors'
import { byteLimit, opaqueId } from './backend-contract/primitives'
import {
  createReactNativeAndroidBackendProvider,
  reactNativeAndroidCompatibility,
  reactNativeAndroidDefaultAdapterId,
  type ReactNativeAndroidBackendProviderOptions
} from './backends/reactnative/react-native-android-provider'
import {
  createReactNativeAppleBackendProvider,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId,
  type ReactNativeAppleBackendProviderOptions
} from './backends/reactnative/react-native-apple-provider'
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS, type BleManager } from './manager/ble-manager'
import {
  createReactNativeRustCoreBackendProvider,
  type ReactNativeRustCorePlatform
} from './backends/reactnative/react-native-rust-core-provider'
import type { ReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core'
import { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
import { createReactNativeRustCoreManager } from './backends/reactnative/react-native-rust-core-manager'
import { rehydratePublicPromise } from './public/error-bridge'
import type { Spec as NativeUnifiedBleProtocolControl } from './NativeUnifiedBleProtocolControl'
import type { ReactNativeRestorationBackendProvider } from './backends/reactnative/react-native-restoration'
import type { DiagnosticsOptions } from './public/host-identity'

export type ReactNativeBlePlatform = 'android' | 'apple'

/** Inputs that bind one React Native application manager to one selected native adapter. */
export interface ReactNativeBleManagerOptions {
  readonly platform: ReactNativeBlePlatform
  readonly control: NativeUnifiedBleProtocolControl
  readonly now: () => number
  readonly clientId: string
  readonly managerId: string
  /** Host authentication scope bound to the one native restoration adopter. */
  readonly hostSessionScope: string
  readonly adapterId?: string
  readonly diagnostics?: DiagnosticsOptions
  readonly createOwnerId?: () => string
  /**
   * R01 shared-core binding override. When absent (the ordinary case), the
   * factory resolves the production `UnifiedBleRustCore` TurboModule binding
   * and every BLE data-path operation executes the native Rust core through
   * the binding-backed provider. Inject a binding only to substitute the
   * native module (tests); a missing native module rejects with
   * `capability.unsupported` before any BLE effect, never a silent fallback.
   */
  readonly rustCore?: ReactNativeRustCoreBinding
  /**
   * R01 isolated legacy route. The TypeScript protocol providers survive
   * ONLY behind this exact, separately authorized value — never as a
   * default, never inferred, never a hidden fallback. Test/reference use.
   */
  readonly legacyTypeScriptCore?: 'isolated-test-reference'
}

/**
 * Creates one owning 4.0 manager from the generated React Native protocol control.
 * The application must retain and destroy the returned manager before replacing it.
 * Apple restoration adoption additionally requires the app-owned Info.plist values
 * UnifiedBleProtocolRestorationNamespace, UnifiedBleProtocolRestorationEpoch,
 * UnifiedBleProtocolRestorationClientId, and UnifiedBleProtocolRestorationHostSessionScope.
 */
export async function createReactNativeBleManagerWithEnvironment(
  options: ReactNativeBleManagerOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  return rehydratePublicPromise(createReactNativeBleManagerWithEnvironmentInternal(options))
}

async function createReactNativeBleManagerWithEnvironmentInternal(
  options: ReactNativeBleManagerOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  if (options.hostSessionScope.length === 0) {
    throw contractError('argument.invalid', 'restoration', 'react-native-manager.host-session-scope')
  }
  const expectedAdapterId =
    options.platform === 'android' ? reactNativeAndroidDefaultAdapterId() : reactNativeAppleDefaultAdapterId()
  if (options.adapterId !== undefined && options.adapterId !== String(expectedAdapterId)) {
    throw contractError('adapter.unavailable', 'adapter', 'react-native-manager.adapter')
  }
  if (options.legacyTypeScriptCore === 'isolated-test-reference') {
    return createTypeScriptCoreManager(options)
  }
  const binding = options.rustCore ?? createReactNativeRustCoreBinding()
  return createRustCoreManager(options, binding)
}

/**
 * R01 isolated legacy route: the pre-cutover TypeScript protocol providers.
 * Reachable ONLY via the explicit `legacyTypeScriptCore` authorization.
 */
async function createTypeScriptCoreManager(
  options: ReactNativeBleManagerOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  const provider = providerFor(options)
  const managerOptions = managerOptionsFor(options)
  const scope: `${string}:${string}` = `react-native:${options.platform}`
  const clientId = opaqueId(options.clientId, 'client', scope)
  return createBleManagerFromProvider(
    {
      provider,
      selection: { selectedAdapterId: adapterIdFor(options.platform) },
      coreCompatibility: compatibilityFor(options.platform),
      manager: {
        clientId,
        managerId: opaqueId(options.managerId, 'manager', scope),
        ownerMode: 'owning',
        restoration: Object.freeze({
          client: Object.freeze({ clientId, hostSessionScope: options.hostSessionScope }),
          coordinator: provider.restoration
        })
      }
    },
    managerOptions
  )
}

/**
 * R01 production path: the manager builds on the binding-backed provider,
 * so every BLE data-path operation dispatches through the admitted native
 * Rust core session. The generated protocol control is used only for
 * restoration identity, never for BLE work.
 */
async function createRustCoreManager(
  options: ReactNativeBleManagerOptions,
  binding: ReactNativeRustCoreBinding
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  if (options.hostSessionScope.length === 0) {
    throw contractError('argument.invalid', 'restoration', 'react-native-manager.host-session-scope')
  }
  const platform: ReactNativeRustCorePlatform = options.platform
  const provider = createReactNativeRustCoreBackendProvider({
    platform,
    binding,
    owner: `${options.clientId}/${options.managerId}`,
    now: options.now,
    control: options.control,
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
  const backend = await provider.create({ selectedAdapterId: adapterIdFor(options.platform) })
  const scope: `${string}:${string}` = `react-native:${options.platform}`
  const clientId = opaqueId(options.clientId, 'client', scope)
  try {
    return await createReactNativeRustCoreManager({
      backend,
      coreCompatibility: compatibilityFor(options.platform),
      clientId,
      managerId: opaqueId(options.managerId, 'manager', scope),
      ownerMode: 'owning',
      restoration: Object.freeze({
        client: Object.freeze({ clientId, hostSessionScope: options.hostSessionScope }),
        coordinator: provider.restoration
      }),
      now: options.now,
      maximumValueBytes: managerOptionsFor(options).maximumValueBytes
    })
  } catch (error) {
    await backend.destroy().catch(() => undefined)
    throw error
  }
}

function managerOptionsFor(options: ReactNativeBleManagerOptions) {
  return {
    ...DEFAULT_BLE_MANAGER_OPTIONS,
    now: options.now,
    maximumValueBytes:
      options.diagnostics?.maximumValueBytes === undefined
        ? DEFAULT_BLE_MANAGER_OPTIONS.maximumValueBytes
        : byteLimit(options.diagnostics.maximumValueBytes),
    traceMaximumRecords: options.diagnostics?.traceMaximumRecords ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumRecords,
    traceMaximumBytes: options.diagnostics?.traceMaximumBytes ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumBytes
  }
}

function providerFor(options: ReactNativeBleManagerOptions): ReactNativeRestorationBackendProvider {
  if (options.platform === 'android') {
    return createReactNativeAndroidBackendProvider(androidProviderOptions(options))
  }
  return createReactNativeAppleBackendProvider(appleProviderOptions(options))
}

function androidProviderOptions(options: ReactNativeBleManagerOptions): ReactNativeAndroidBackendProviderOptions {
  if (options.createOwnerId === undefined) {
    return { control: options.control, now: options.now }
  }
  return { control: options.control, now: options.now, createOwnerId: options.createOwnerId }
}

function appleProviderOptions(options: ReactNativeBleManagerOptions): ReactNativeAppleBackendProviderOptions {
  if (options.createOwnerId === undefined) {
    return { control: options.control, now: options.now }
  }
  return { control: options.control, now: options.now, createOwnerId: options.createOwnerId }
}

function adapterIdFor(platform: ReactNativeBlePlatform) {
  return platform === 'android' ? reactNativeAndroidDefaultAdapterId() : reactNativeAppleDefaultAdapterId()
}

function compatibilityFor(platform: ReactNativeBlePlatform) {
  return platform === 'android' ? reactNativeAndroidCompatibility : reactNativeAppleCompatibility
}
