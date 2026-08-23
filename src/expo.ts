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

export interface ExpoBackgroundRequest {
  readonly kind: 'connected-device'
  readonly reason: string
}

export interface ExpoBackgroundLease {
  readonly release: () => Promise<void>
}

export interface ExpoCompanionAssociationRequest {
  readonly name?: string
  readonly serviceUuid?: string
}

export interface ExpoCompanionAssociationResult {
  readonly source: 'associated'
  readonly associationId: number
  readonly peerId: string | null
  readonly displayName: string | null
}

export interface ExpoRestoredRecord {
  readonly kind: 'adapter' | 'connection'
  readonly ordinal: number
  readonly peerId: string | null
}

export interface ExpoRestorationClaimResult {
  readonly outcome:
    | 'adopted'
    | 'already-consumed'
    | 'attachment-mismatch'
    | 'backend-mismatch'
    | 'namespace-mismatch'
    | 'epoch-mismatch'
  readonly replayRecordCount: number
  readonly records: readonly ExpoRestoredRecord[]
}

export interface ExpoBleManager extends BleManager {
  readonly readiness: () => Promise<BleReadiness>
  readonly permissions: {
    readonly request: (request: ExpoPermissionRequest) => Promise<ExpoPermissionResult>
  }
  readonly openSettings: (target: ExpoSettingsTarget) => Promise<void>
  readonly background: {
    readonly acquire: (request: ExpoBackgroundRequest) => Promise<ExpoBackgroundLease>
  }
  readonly association: {
    readonly associate: (request?: ExpoCompanionAssociationRequest) => Promise<ExpoCompanionAssociationResult>
  }
  readonly restoration: {
    readonly claim: () => Promise<ExpoRestorationClaimResult>
  }
}

export interface ExpoSettingsBridge {
  (target: ExpoSettingsTarget): Promise<void>
}

export interface ExpoPermissionBridge {
  (request: ExpoPermissionRequest): Promise<ExpoPermissionResult>
}

export interface ExpoRuntimeConfiguration {
  readonly executionEnvironment?: 'expo-go' | 'development-build' | 'production'
  readonly nativeModuleAvailable?: boolean
  readonly nativeConfiguration?: { readonly digest: string }
  readonly expectedConfiguration?: { readonly digest: string }
  readonly settingsBridge?: ExpoSettingsBridge
  readonly permissionBridge?: ExpoPermissionBridge
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
    return withExpoRuntime(
      await createReactNativeBleManager(options),
      undefined,
      undefined,
      nativeBackgroundControl(),
      nativeAssociationControl(),
      nativeRestorationControl()
    )
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
    return withExpoRuntime(
      await createPublicBleManager(internal, environment.now),
      expo?.settingsBridge,
      expo?.permissionBridge,
      environment.control,
      environment.control,
      environment.control
    )
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

function withExpoRuntime(
  manager: BleManager,
  settingsBridge?: ExpoSettingsBridge,
  permissionBridge?: ExpoPermissionBridge,
  backgroundControl?: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'acquireBackground' | 'releaseBackground'>,
  associationControl?: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'associateCompanionDevice'>,
  restorationControl?: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'claimRestoration'>
): ExpoBleManager {
  return Object.assign(manager, {
    readiness: () => getExpoBleReadiness(manager),
    permissions: Object.freeze({
      request: (request: ExpoPermissionRequest) => requestExpoPermissions(request, permissionBridge)
    }),
    openSettings: (target: ExpoSettingsTarget) => openExpoSettings(target, settingsBridge),
    background: Object.freeze({
      acquire: (request: ExpoBackgroundRequest) => acquireExpoBackground(request, backgroundControl)
    }),
    association: Object.freeze({
      associate: (request: ExpoCompanionAssociationRequest = {}) =>
        associateExpoCompanionDevice(request, associationControl)
    }),
    restoration: Object.freeze({
      claim: () => claimExpoRestoration(restorationControl)
    })
  })
}

async function requestExpoPermissions(
  request: ExpoPermissionRequest,
  permissionBridge: ExpoPermissionBridge | undefined
): Promise<ExpoPermissionResult> {
  if (request.purpose !== 'scan-and-connect') {
    throw rehydratePublicError(contractError('argument.invalid', 'capability', 'expo.permissions.purpose'))
  }
  if (permissionBridge === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.permissions.request',
      'No trusted native permission bridge is available; invoke the host permission flow explicitly.'
    )
  }
  try {
    return await permissionBridge(request)
  } catch (error) {
    throwExpoRuntimeError('platform.failure', 'expo.permissions.request', errorMessage(error))
  }
}

async function acquireExpoBackground(
  request: ExpoBackgroundRequest,
  control: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'acquireBackground' | 'releaseBackground'> | undefined
): Promise<ExpoBackgroundLease> {
  if (request.kind !== 'connected-device' || request.reason.trim().length === 0) {
    throwExpoRuntimeError('argument.invalid', 'expo.background.acquire', 'A non-empty background reason is required.')
  }
  if (control === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.background.acquire',
      'Connected-device background execution is unavailable until the native Expo service is configured and rebuilt.'
    )
  }
  let result: import('./NativeUnifiedBleProtocolControl').NativeBackgroundLeaseResult
  try {
    result = await control.acquireBackground({ kind: request.kind, reason: request.reason })
  } catch (error) {
    const nativeCode = errorCode(error)
    throwExpoRuntimeError(
      normalizedBackgroundErrorCode(nativeCode),
      'expo.background.acquire',
      errorMessage(error),
      nativeCode
    )
  }
  let released = false
  return Object.freeze({
    release: async () => {
      if (released) return
      try {
        await control.releaseBackground({ leaseId: result.leaseId })
        released = true
      } catch (error) {
        const nativeCode = errorCode(error)
        throwExpoRuntimeError(
          normalizedBackgroundErrorCode(nativeCode),
          'expo.background.release',
          errorMessage(error),
          nativeCode
        )
      }
    }
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

async function associateExpoCompanionDevice(
  request: ExpoCompanionAssociationRequest,
  control: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'associateCompanionDevice'> | undefined
): Promise<ExpoCompanionAssociationResult> {
  if (request.name !== undefined && (request.name.trim().length === 0 || request.name.length > 128)) {
    throwExpoRuntimeError(
      'argument.invalid',
      'expo.association.associate',
      'Association name must be non-empty and bounded.'
    )
  }
  if (request.serviceUuid !== undefined && request.serviceUuid.trim().length === 0) {
    throwExpoRuntimeError(
      'argument.invalid',
      'expo.association.associate',
      'Association serviceUuid must be non-empty.'
    )
  }
  if (control === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.association.associate',
      'Companion Device Manager association is unavailable on this Expo host.'
    )
  }
  try {
    return await control.associateCompanionDevice(request)
  } catch (error) {
    throwExpoRuntimeError('capability.unavailable', 'expo.association.associate', errorMessage(error), errorCode(error))
  }
}

async function claimExpoRestoration(
  control: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'claimRestoration'> | undefined
): Promise<ExpoRestorationClaimResult> {
  if (control === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.restoration.claim',
      'Native restoration adoption is unavailable on this Expo host.'
    )
  }
  try {
    const result = await control.claimRestoration()
    return Object.freeze({
      outcome: restorationOutcome(result.outcome),
      replayRecordCount: result.replayRecordCount,
      records: Object.freeze(
        result.records.map(record =>
          Object.freeze({ kind: record.kind, ordinal: record.ordinal, peerId: record.peerId })
        )
      )
    })
  } catch (error) {
    throwExpoRuntimeError('capability.unavailable', 'expo.restoration.claim', errorMessage(error), errorCode(error))
  }
}

function restorationOutcome(
  outcome: import('./NativeUnifiedBleProtocolControl').NativeRestorationOutcome
): ExpoRestorationClaimResult['outcome'] {
  switch (outcome) {
    case 'alreadyConsumed':
      return 'already-consumed'
    case 'attachmentMismatch':
      return 'attachment-mismatch'
    case 'backendMismatch':
      return 'backend-mismatch'
    case 'namespaceMismatch':
      return 'namespace-mismatch'
    case 'epochMismatch':
      return 'epoch-mismatch'
    case 'adopted':
      return 'adopted'
  }
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

function nativeBackgroundControl():
  | Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'acquireBackground' | 'releaseBackground'>
  | undefined {
  try {
    return getNativeUnifiedBleProtocolControl()
  } catch {
    return undefined
  }
}

function nativeAssociationControl():
  | Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'associateCompanionDevice'>
  | undefined {
  try {
    return getNativeUnifiedBleProtocolControl()
  } catch {
    return undefined
  }
}

function nativeRestorationControl():
  | Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'claimRestoration'>
  | undefined {
  try {
    return getNativeUnifiedBleProtocolControl()
  } catch {
    return undefined
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

function throwExpoRuntimeError(
  code: BleErrorCode,
  operation: string,
  safeMessage: string,
  platformCode = operation
): never {
  throw rehydratePublicError(
    contractError(code, 'capability', operation, {
      domain: 'expo',
      code: platformCode,
      safeMessage,
      metadata: {}
    })
  )
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : 'The native Expo operation failed.'
}

function errorCode(error: unknown): string {
  if (typeof error === 'object' && error !== null) {
    const code = Reflect.get(error, 'code')
    if (typeof code === 'string' && code.length > 0) return code
  }
  return 'native-failure'
}

function normalizedBackgroundErrorCode(nativeCode: string): BleErrorCode {
  switch (nativeCode) {
    case 'foregroundServiceNotConfigured':
      return 'capability.unavailable'
    case 'foregroundServicePermissionDenied':
      return 'permission.denied'
    case 'invalidBackgroundRequest':
      return 'argument.invalid'
    case 'invalidBackgroundLease':
      return 'lifecycle.invalid-state'
    case 'unsupportedBackground':
      return 'capability.unsupported'
    default:
      return 'platform.failure'
  }
}
