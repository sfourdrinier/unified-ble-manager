// src/expo.ts — thin Expo-aware composition over the React Native factory

import { contractError } from './backend-contract/errors'
import type { BleErrorCode } from './backend-contract/errors'
import { rehydratePublicError } from './public/error-bridge'
import type { BleAdapterState } from './public/ble-adapter'
import { createPublicBleManager, type BleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions, type BleManagerCreateOptions } from './public/host-identity'
import {
  createReactNativeBleManager,
  createReactNativeBleManagerWithEnvironment,
  getNativeUnifiedBleProtocolControl
} from './react-native'
import type { ReactNativeBleManagerOptions } from './react-native-manager'

export type { BleManagerCreateOptions } from './public/host-identity'
export { normalizeBleManagerCreateOptions } from './public/host-identity'

export type BlePermission = 'bluetooth'
export type ExpoSettingsTarget = 'app' | 'bluetooth' | 'location-services'

export type BleReadinessAction =
  | { readonly kind: 'request-permission'; readonly permission: BlePermission }
  | { readonly kind: 'open-settings'; readonly target: ExpoSettingsTarget }
  | { readonly kind: 'enable-bluetooth'; readonly systemUiOnly: true }
  | { readonly kind: 'create-development-build' }
  | { readonly kind: 'rebuild-native-app'; readonly reason: string }

export interface BleReadiness {
  readonly state: 'ready' | 'action-required' | 'unavailable'
  readonly adapter: BleAdapterState
  readonly actions: readonly BleReadinessAction[]
}

export interface ExpoPermissionRequest {
  readonly purpose: 'scan-and-connect'
}

export interface ExpoPermissionResult {
  readonly requested: readonly BlePermission[]
  readonly granted: readonly BlePermission[]
  readonly denied: readonly BlePermission[]
  readonly recommendedSettingsTarget: ExpoSettingsTarget | null
}

export interface ExpoBleManager extends BleManager {
  readonly readiness: () => Promise<BleReadiness>
  readonly permissions: {
    readonly request: (request: ExpoPermissionRequest) => Promise<ExpoPermissionResult>
  }
  readonly openSettings: (target: ExpoSettingsTarget) => Promise<void>
}

export interface ExpoSettingsBridge {
  (target: ExpoSettingsTarget): Promise<void>
}

export interface ExpoRuntimeConfiguration {
  readonly executionEnvironment?: 'expo-go' | 'development-build' | 'production'
  readonly nativeModuleAvailable?: boolean
  readonly nativeConfiguration?: { readonly digest: string }
  readonly expectedConfiguration?: { readonly digest: string }
  readonly settingsBridge?: ExpoSettingsBridge
}

export type ExpoBleManagerEnvironment = ReactNativeBleManagerOptions & {
  readonly expo?: ExpoRuntimeConfiguration
}

const EXPO_GO_MESSAGE =
  'Expo Go is not supported; create an Expo development build that includes UnifiedBleProtocolControl.'

/** Creates the same RN manager and adds only Expo host ergonomics to it. */
export async function createExpoBleManager(options: BleManagerCreateOptions = {}): Promise<ExpoBleManager> {
  try {
    normalizeBleManagerCreateOptions(options)
    assertDirectExpoRuntime()
    return withExpoRuntime(await createReactNativeBleManager(options))
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

export async function createExpoBleManagerWithEnvironment(
  environment: ExpoBleManagerEnvironment
): Promise<ExpoBleManager> {
  try {
    const expo = environment.expo
    assertExpoRuntimeConfiguration(expo)
    const internal = await createReactNativeBleManagerWithEnvironment(environment)
    return withExpoRuntime(await createPublicBleManager(internal, environment.now), expo?.settingsBridge)
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

/** Reads one trusted adapter snapshot and derives deterministic Expo guidance. */
export async function getExpoBleReadiness(manager: Pick<BleManager, 'adapter'>): Promise<BleReadiness> {
  try {
    return mapExpoReadiness(await manager.adapter.state())
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

/** Pure readiness mapping shared by both Expo factory forms. */
export function mapExpoReadiness(adapter: BleAdapterState): BleReadiness {
  if (
    adapter.availability !== 'available' ||
    adapter.authorization === 'restricted' ||
    adapter.authorization === 'unavailable' ||
    adapter.power === 'unsupported'
  ) {
    return readiness(adapter, 'unavailable', [])
  }
  if (adapter.authorization === 'denied') {
    return readiness(adapter, 'action-required', [{ kind: 'open-settings', target: 'app' }])
  }
  if (adapter.authorization === 'not-determined') {
    return readiness(adapter, 'action-required', [{ kind: 'request-permission', permission: 'bluetooth' }])
  }
  if (adapter.power === 'off') {
    return readiness(adapter, 'action-required', [{ kind: 'enable-bluetooth', systemUiOnly: true }])
  }
  if (adapter.power !== 'on' || adapter.authorization !== 'granted') {
    return readiness(adapter, 'action-required', [])
  }
  return readiness(adapter, 'ready', [])
}

function readiness(
  adapter: BleAdapterState,
  state: BleReadiness['state'],
  actions: readonly BleReadinessAction[]
): BleReadiness {
  return Object.freeze({ adapter, state, actions: Object.freeze([...actions]) })
}

function withExpoRuntime(manager: BleManager, settingsBridge?: ExpoSettingsBridge): ExpoBleManager {
  return Object.assign(manager, {
    readiness: () => getExpoBleReadiness(manager),
    permissions: Object.freeze({
      request: (request: ExpoPermissionRequest) => requestExpoPermissions(manager, request)
    }),
    openSettings: (target: ExpoSettingsTarget) => openExpoSettings(target, settingsBridge)
  })
}

async function requestExpoPermissions(
  manager: Pick<BleManager, 'adapter'>,
  request: ExpoPermissionRequest
): Promise<ExpoPermissionResult> {
  if (request.purpose !== 'scan-and-connect') {
    throw rehydratePublicError(contractError('argument.invalid', 'capability', 'expo.permissions.purpose'))
  }
  const adapter = await manager.adapter.state()
  const requested: readonly BlePermission[] = ['bluetooth']
  const granted: readonly BlePermission[] = adapter.authorization === 'granted' ? ['bluetooth'] : []
  const denied: readonly BlePermission[] =
    adapter.authorization === 'denied' || adapter.authorization === 'restricted' ? ['bluetooth'] : []
  return Object.freeze({
    requested,
    granted,
    denied,
    recommendedSettingsTarget: recommendedSettingsTarget(adapter)
  })
}

async function openExpoSettings(target: ExpoSettingsTarget, settingsBridge?: ExpoSettingsBridge): Promise<void> {
  if (settingsBridge === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.open-settings',
      'No trusted native settings bridge is available for this Expo host.'
    )
  }
  try {
    await settingsBridge(target)
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function recommendedSettingsTarget(adapter: BleAdapterState): ExpoSettingsTarget | null {
  if (adapter.authorization === 'denied' || adapter.authorization === 'restricted') return 'app'
  if (adapter.power === 'off') return 'bluetooth'
  return null
}

function assertDirectExpoRuntime(): void {
  if (typeof getNativeUnifiedBleProtocolControl !== 'function') return
  try {
    getNativeUnifiedBleProtocolControl()
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    if (/UnifiedBleProtocolControl|TurboModuleRegistry|NativeModules/.test(message)) {
      throwExpoRuntimeError('capability.unavailable', 'expo.runtime.development-build', EXPO_GO_MESSAGE)
    }
    throw error
  }
}

function assertExpoRuntimeConfiguration(configuration: ExpoRuntimeConfiguration | undefined): void {
  if (configuration === undefined) return
  if (configuration.executionEnvironment === 'expo-go') {
    throwExpoRuntimeError('capability.unavailable', 'expo.runtime.development-build', EXPO_GO_MESSAGE)
  }
  if (configuration.nativeModuleAvailable === false) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.runtime.native-module',
      'The native protocol module is absent; rebuild the Expo development build.'
    )
  }
  if (
    configuration.nativeConfiguration?.digest !== undefined &&
    configuration.expectedConfiguration?.digest !== undefined &&
    configuration.nativeConfiguration.digest !== configuration.expectedConfiguration.digest
  ) {
    throwExpoRuntimeError(
      'protocol.incompatible',
      'expo.runtime.configuration',
      'The native Expo configuration differs from the trusted application configuration; rebuild the native app.'
    )
  }
}

function throwExpoRuntimeError(code: BleErrorCode, operation: string, safeMessage: string): never {
  throw rehydratePublicError(
    contractError(code, 'capability', operation, {
      domain: 'expo',
      code: operation,
      safeMessage,
      metadata: {}
    })
  )
}
