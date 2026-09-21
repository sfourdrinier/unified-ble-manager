// src/react-native-manager.ts

import {
  normalizeBackgroundContinuation,
  DEFAULT_BACKGROUND_CONTINUATION,
  type BackgroundContinuationDeclaration
} from './backend-contract/background-continuation'
import type { NativeBackendIdentity } from './backend-contract/identity'
import { contractError } from './backend-contract/errors'
import { byteLimit, opaqueId } from './backend-contract/primitives'
import {
  reactNativeAndroidCompatibility,
  reactNativeAndroidDefaultAdapterId,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId
} from './backends/reactnative/react-native-platform-identity'
import type { BleManager } from './manager/ble-manager'
import { DEFAULT_CORE_MAXIMUM_VALUE_BYTES } from './core/unified-ble-core-helpers'
import { CoreTraceRecorder } from './core/trace-recorder'
import {
  createReactNativeRustCoreBackendProvider,
  type ReactNativeRustCoreBackend,
  type ReactNativeRustCoreHostServices
} from './backends/reactnative/react-native-rust-core-provider'
import type { ReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core'
import { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
import { createReactNativeRustCoreManager } from './backends/reactnative/react-native-rust-core-manager'
import type { ReactNativeRestorationAuthority } from './backends/reactnative/react-native-rust-core-restoration'
import { hostAndroidApiLevel } from './backends/reactnative/react-native-providers'
import { rehydratePublicPromise } from './public/error-bridge'
import type { DiagnosticsOptions } from './public/host-identity'

export type ReactNativeBlePlatform = 'android' | 'apple'

/** Trace retention defaults (the legacy `DEFAULT_BLE_MANAGER_OPTIONS`). */
const DEFAULT_TRACE_MAXIMUM_RECORDS = 256
const DEFAULT_TRACE_MAXIMUM_BYTES = 512 * 1024

/** Options of the removed legacy route (4.x TypeScript core over the protocol control). */
const REMOVED_LEGACY_OPTIONS = Object.freeze(['legacyTypeScriptCore', 'control'])

/** Inputs that bind one React Native application manager to one selected native adapter. */
export interface ReactNativeBleManagerOptions {
  readonly platform: ReactNativeBlePlatform
  readonly now: () => number
  readonly clientId: string
  readonly managerId: string
  /** Host authentication scope bound to the one native restoration adopter. */
  readonly hostSessionScope: string
  readonly adapterId?: string
  readonly diagnostics?: DiagnosticsOptions
  readonly createOwnerId?: () => string
  /**
   * Substitute native Rust core binding. When absent (the ordinary case), the
   * factory resolves the production `UnifiedBleRustCore` TurboModule. Every
   * binding is identity-checked and admitted before any radio work; a missing
   * native module rejects with `capability.unsupported`. There is no other
   * route.
   */
  readonly rustCore?: ReactNativeRustCoreBinding
  /** Android API level; absent, React Native's `Platform.Version`. */
  readonly androidApiLevel?: number
  /**
   * The app-declared restoration authority (the `restorationIdentity`
   * answer), when the app configured restoration. Adoption is refused without
   * one, exactly as the native journal refused an unconfigured app.
   */
  readonly restorationAuthority?: ReactNativeRestorationAuthority
  /**
   * Declared background standing order (`background.continuation`, BGS4).
   * Absent means `record-only`. A non-`record-only` order without a
   * persisting native owner fails host creation with
   * `capability.unsupported` — never a silent record-only.
   */
  readonly background?: {
    readonly continuation?: unknown
  }
}

/** What an Expo host reaches besides the public manager. */
export interface ReactNativeManagerHost {
  readonly manager: BleManager<string, NativeBackendIdentity<string>>
  readonly services: ReactNativeRustCoreHostServices
  /** Adopts restoration with the configured authority (Expo `restoration.claim`). */
  readonly claimRestoration: () => ReturnType<BleManager<string, NativeBackendIdentity<string>>['adoptRestoration']>
  /** The normalized declared standing order (defaults to `record-only`). */
  readonly continuation: BackgroundContinuationDeclaration
}

/**
 * Creates one owning manager over the process-owned Rust mobile owner. The
 * application must retain and destroy the returned manager before replacing
 * it. Apple restoration adoption additionally requires the app-owned
 * Info.plist values UnifiedBleProtocolRestorationNamespace,
 * UnifiedBleProtocolRestorationEpoch, UnifiedBleProtocolRestorationClientId,
 * and UnifiedBleProtocolRestorationHostSessionScope.
 */
export async function createReactNativeBleManagerWithEnvironment(
  options: ReactNativeBleManagerOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  return rehydratePublicPromise(createReactNativeManagerHost(options).then(host => host.manager))
}

/** The manager plus its session services (Expo composition). Not a public entrypoint. */
export async function createReactNativeManagerHost(
  options: ReactNativeBleManagerOptions
): Promise<ReactNativeManagerHost> {
  for (const removed of REMOVED_LEGACY_OPTIONS) {
    if (Object.prototype.hasOwnProperty.call(options, removed)) {
      // 5.0 has one route: a caller still asking for the removed legacy route
      // is told so, never silently given another one.
      throw contractError('argument.invalid', 'core', `react-native-manager.${removed}`)
    }
  }
  if (options.hostSessionScope.length === 0) {
    throw contractError('argument.invalid', 'restoration', 'react-native-manager.host-session-scope')
  }
  const expectedAdapterId = adapterIdFor(options.platform)
  if (options.adapterId !== undefined && options.adapterId !== String(expectedAdapterId)) {
    throw contractError('adapter.unavailable', 'adapter', 'react-native-manager.adapter')
  }
  // Built before any session opens, so out-of-range bounds are refused
  // without an effect (the legacy manager's defaults: 256 records / 512 KiB).
  const trace = new CoreTraceRecorder(
    options.diagnostics?.traceMaximumRecords ?? DEFAULT_TRACE_MAXIMUM_RECORDS,
    options.diagnostics?.traceMaximumBytes ?? DEFAULT_TRACE_MAXIMUM_BYTES
  )
  const binding = options.rustCore ?? createReactNativeRustCoreBinding({ platform: options.platform })
  const authority = options.restorationAuthority ?? null
  // Only an explicitly passed order reaches the owner: an absent option
  // leaves the native store alone, so a build-time manifest declaration
  // stands until the app overrides it at runtime (or clears it with an
  // explicit record-only). The host still reports the record-only default.
  const continuation =
    options.background === undefined
      ? DEFAULT_BACKGROUND_CONTINUATION
      : normalizeBackgroundContinuation(options.background.continuation)
  const provider = createReactNativeRustCoreBackendProvider({
    platform: options.platform,
    binding,
    owner: `${options.clientId}/${options.managerId}`,
    now: options.now,
    runtime: {
      androidApiLevel: options.platform === 'android' ? (options.androidApiLevel ?? hostAndroidApiLevel()) : null
    },
    restorationAuthority: () => authority,
    trace,
    ...(options.background === undefined ? {} : { backgroundContinuation: continuation }),
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
  const backend: ReactNativeRustCoreBackend = await provider.create({ selectedAdapterId: expectedAdapterId })
  const scope: `${string}:${string}` = `react-native:${options.platform}`
  const clientId = opaqueId(options.clientId, 'client', scope)
  let manager: BleManager<string, NativeBackendIdentity<string>>
  try {
    manager = await createReactNativeRustCoreManager({
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
      trace,
      maximumValueBytes:
        options.diagnostics?.maximumValueBytes === undefined
          ? DEFAULT_CORE_MAXIMUM_VALUE_BYTES
          : byteLimit(options.diagnostics.maximumValueBytes)
    })
  } catch (error) {
    const cleanup = await backend.destroy()
    if (cleanup.state !== 'released') {
      throw new AggregateError([error, ...cleanup.failures.map(failure => failure.error)], 'react-native-manager.open')
    }
    throw error
  }
  return Object.freeze({
    manager,
    services: backend.hostServices,
    continuation,
    claimRestoration: () => {
      if (authority === null) {
        throw contractError('capability.unavailable', 'restoration', 'react-native-manager.restoration.claim')
      }
      const identity = manager.identity
      return manager.adoptRestoration(
        Object.freeze({
          namespace: authority.namespaceValue,
          attachmentId: identity.attachment.attachmentId,
          expectedBackendInstanceId: identity.attachment.backendInstanceId,
          expectedEpoch: opaqueId(authority.adoptionEpoch, 'restoration-epoch', 'react-native-restoration'),
          expectedVersions: identity.versions
        })
      )
    }
  })
}

function adapterIdFor(platform: ReactNativeBlePlatform) {
  return platform === 'android' ? reactNativeAndroidDefaultAdapterId() : reactNativeAppleDefaultAdapterId()
}

function compatibilityFor(platform: ReactNativeBlePlatform) {
  return platform === 'android' ? reactNativeAndroidCompatibility : reactNativeAppleCompatibility
}
