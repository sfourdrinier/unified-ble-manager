// src/expo.ts — thin Expo-aware composition over the React Native factory

import { contractError } from './backend-contract/errors'
import type { BleErrorCode } from './backend-contract/errors'
import { Platform } from 'react-native'
import { rehydratePublicError } from './public/error-bridge'
import type { BleAdapterState } from './public/ble-adapter'
import { createPublicBleManager, type BleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions, type BleManagerCreateOptions } from './public/host-identity'
import {
  createReactNativeBleManager,
  createReactNativeBleManagerWithEnvironment,
  getNativeUnifiedBleProtocolControl
} from './react-native'
import { getNativeUnifiedBleExpoRuntime } from './expo-native-runtime'
import type {
  NativeExpoPermissionRequest,
  NativeExpoRuntimeConfiguration,
  NativeExpoSettingsRequest,
  Spec as NativeExpoRuntime
} from './NativeUnifiedBleExpoRuntime'
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
  /** Trusted host platform used to project platform-specific readiness prerequisites. */
  readonly platform?: 'android' | 'apple'
  readonly executionEnvironment?: 'expo-go' | 'development-build' | 'production'
  readonly nativeModuleAvailable?: boolean
  readonly nativeConfiguration?: { readonly digest: string }
  readonly expectedConfiguration?: { readonly digest: string }
  /** Trusted Android API level used to project pre-Android-12 scan prerequisites. */
  readonly androidApiLevel?: number
  readonly permissions?: {
    readonly android?: {
      readonly legacyLocation?: 'auto' | 'required' | 'none'
    }
  }
  readonly settingsBridge?: ExpoSettingsBridge
  readonly permissionBridge?: ExpoPermissionBridge
}

export type ExpoBleManagerEnvironment = ReactNativeBleManagerOptions & {
  readonly expo?: ExpoRuntimeConfiguration
}

const EXPO_GO_MESSAGE =
  'Expo Go is not supported; create an Expo development build that includes UnifiedBleProtocolControl.'

/** Creates the same RN manager and adds only Expo host ergonomics to it. */
export async function createExpoBleManager(
  options: BleManagerCreateOptions = {},
  runtimeConfiguration?: ExpoRuntimeConfiguration
): Promise<ExpoBleManager> {
  try {
    normalizeBleManagerCreateOptions(options)
    assertExpoRuntimeConfiguration(runtimeConfiguration)
    assertDirectExpoRuntime()
    const nativeRuntime = resolveNativeExpoRuntime()
    const nativeConfiguration = await readNativeExpoRuntimeConfiguration(nativeRuntime)
    const readinessConfiguration = directExpoRuntimeConfiguration({
      ...runtimeConfiguration,
      ...nativeConfiguration,
      ...(runtimeConfiguration?.expectedConfiguration === undefined
        ? {}
        : { expectedConfiguration: runtimeConfiguration.expectedConfiguration }),
      ...(runtimeConfiguration?.settingsBridge === undefined
        ? {}
        : { settingsBridge: runtimeConfiguration.settingsBridge }),
      ...(runtimeConfiguration?.permissionBridge === undefined
        ? {}
        : { permissionBridge: runtimeConfiguration.permissionBridge })
    })
    assertExpoRuntimeConfiguration(readinessConfiguration)
    return withExpoRuntime(
      await createReactNativeBleManager(options),
      readinessConfiguration?.settingsBridge ?? nativeSettingsBridge(nativeRuntime),
      readinessConfiguration?.permissionBridge ?? nativePermissionBridge(nativeRuntime),
      nativeBackgroundControl(),
      nativeAssociationControl(),
      nativeRestorationControl(),
      readinessConfiguration
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
    const readinessConfiguration = environmentExpoRuntimeConfiguration(environment.platform, expo)
    const internal = await createReactNativeBleManagerWithEnvironment(environment)
    return withExpoRuntime(
      await createPublicBleManager(internal, environment.now),
      expo?.settingsBridge,
      expo?.permissionBridge,
      environment.control,
      environment.control,
      environment.control,
      readinessConfiguration
    )
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

/**
 * A boundary that has not yet received an adapter-state event reports every
 * field as unknown, with a safeReason saying so. That shape is *pending*, not
 * authoritative, and must not be mapped to a readiness state - an unknown
 * availability would otherwise read as "there is no usable radio" for a radio
 * that is simply still starting up. The conjunction is what makes this safe:
 * a backend with no authorization concept (BlueZ) reports an unknown
 * authorization on an adapter whose availability and power ARE measured, so it
 * cannot be mistaken for pending.
 */
function isPendingAdapterState(adapter: BleAdapterState): boolean {
  return (
    adapter.availability === 'unknown' &&
    adapter.power === 'unknown' &&
    adapter.authorization === 'unknown' &&
    adapter.safeReason !== null &&
    adapter.safeReason !== undefined
  )
}

/**
 * Bounded wait for the first authoritative snapshot; ~2s at 100ms steps.
 *
 * Fixed rather than caller-tunable. `getExpoBleReadiness` is a synchronous-feeling
 * readiness probe with no `OperationOptions` of its own, and its contract is to
 * return deterministic guidance promptly rather than to block: a caller that
 * needs to wait for the adapter uses `adapter.waitUntilReady()`, which does take
 * a caller-supplied deadline. These two numbers only decide how long the probe
 * tolerates the pre-authoritative `unknown/unknown` snapshot a native module
 * reports before its first callback lands, and both ends are already covered --
 * an early exit as soon as the snapshot becomes authoritative, and a hard cap so
 * the probe cannot hang.
 */
const PENDING_ADAPTER_STATE_ATTEMPTS = 20
const PENDING_ADAPTER_STATE_INTERVAL_MS = 100

const sleep = (milliseconds: number): Promise<void> => new Promise<void>(resolve => setTimeout(resolve, milliseconds))

/** Reads one trusted adapter snapshot and derives deterministic Expo guidance. */
export async function getExpoBleReadiness(
  manager: Pick<BleManager, 'adapter'>,
  configuration?: ExpoRuntimeConfiguration
): Promise<BleReadiness> {
  try {
    let adapter = await manager.adapter.state()
    for (let attempt = 0; attempt < PENDING_ADAPTER_STATE_ATTEMPTS && isPendingAdapterState(adapter); attempt++) {
      await sleep(PENDING_ADAPTER_STATE_INTERVAL_MS)
      adapter = await manager.adapter.state()
    }
    return mapExpoReadiness(adapter, configuration)
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

/** Pure readiness mapping shared by both Expo factory forms. */
export function mapExpoReadiness(adapter: BleAdapterState, configuration?: ExpoRuntimeConfiguration): BleReadiness {
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
  const legacyLocation = configuration?.permissions?.android?.legacyLocation
  const androidPath =
    configuration?.platform === 'android' ||
    configuration?.androidApiLevel !== undefined ||
    legacyLocation !== undefined
  if (androidPath && configuration?.androidApiLevel === undefined) {
    return readiness(adapter, 'action-required', [{ kind: 'open-settings', target: 'location-services' }])
  }
  if (configuration?.androidApiLevel !== undefined && configuration.androidApiLevel < 31) {
    const legacyPolicy = legacyLocation ?? 'none'
    if (legacyPolicy === 'none') {
      return readiness(adapter, 'unavailable', [
        {
          kind: 'rebuild-native-app',
          reason: 'Android API 24-30 BLE scanning requires legacy location permission, but legacyLocation is none.'
        }
      ])
    }
    return readiness(adapter, 'action-required', [{ kind: 'open-settings', target: 'location-services' }])
  }
  if (legacyLocation === 'required') {
    return readiness(adapter, 'action-required', [{ kind: 'open-settings', target: 'location-services' }])
  }
  return readiness(adapter, 'ready', [])
}

function directExpoRuntimeConfiguration(
  configuration: ExpoRuntimeConfiguration | undefined
): ExpoRuntimeConfiguration | undefined {
  const platform = expoPlatform(Platform.OS)
  if (platform === undefined) return configuration
  const androidApiLevel =
    platform === 'android' && configuration?.androidApiLevel === undefined && typeof Platform.Version === 'number'
      ? Platform.Version
      : configuration?.androidApiLevel
  return {
    ...configuration,
    platform,
    ...(androidApiLevel === undefined ? {} : { androidApiLevel })
  }
}

function resolveNativeExpoRuntime(): NativeExpoRuntime {
  try {
    return getNativeUnifiedBleExpoRuntime()
  } catch (error) {
    const message = errorMessage(error)
    if (/UnifiedBleExpoRuntime|TurboModuleRegistry|NativeModules/.test(message)) {
      throwExpoRuntimeError('capability.unavailable', 'expo.runtime.native-module', EXPO_GO_MESSAGE)
    }
    throw error
  }
}

async function readNativeExpoRuntimeConfiguration(runtime: NativeExpoRuntime): Promise<ExpoRuntimeConfiguration> {
  let value: NativeExpoRuntimeConfiguration
  try {
    value = await runtime.getRuntimeConfiguration()
  } catch (error) {
    throwExpoRuntimeError('capability.unavailable', 'expo.runtime.configuration', errorMessage(error), errorCode(error))
  }
  const result = parseNativeExpoRuntimeConfiguration(value)
  return {
    platform: result.platform,
    nativeModuleAvailable: true,
    nativeConfiguration: { digest: result.configurationDigest },
    ...(result.legacyLocationPolicy === undefined
      ? {}
      : { permissions: { android: { legacyLocation: result.legacyLocationPolicy } } })
  }
}

function parseNativeExpoRuntimeConfiguration(value: unknown): NativeExpoRuntimeConfiguration {
  const result = expoRecord(value, 'expo.runtime.configuration.result')
  if (
    (result.platform !== 'android' && result.platform !== 'apple') ||
    !nonEmptyString(result.configurationDigest) ||
    (result.legacyLocationPolicy !== undefined &&
      result.legacyLocationPolicy !== 'auto' &&
      result.legacyLocationPolicy !== 'required' &&
      result.legacyLocationPolicy !== 'none')
  ) {
    throwExpoMalformedResult('expo.runtime.configuration.result')
  }
  return {
    platform: result.platform,
    configurationDigest: result.configurationDigest,
    ...(result.legacyLocationPolicy === undefined ? {} : { legacyLocationPolicy: result.legacyLocationPolicy })
  }
}

function nativePermissionBridge(runtime: NativeExpoRuntime): ExpoPermissionBridge {
  return (request: ExpoPermissionRequest) => {
    const nativeRequest: NativeExpoPermissionRequest = { purpose: request.purpose }
    return runtime.requestPermissions(nativeRequest).then(value => parseExpoPermissionResult(value))
  }
}

function nativeSettingsBridge(runtime: NativeExpoRuntime): ExpoSettingsBridge {
  return (target: ExpoSettingsTarget) => {
    const request: NativeExpoSettingsRequest = { target }
    return runtime.openSettings(request)
  }
}

function environmentExpoRuntimeConfiguration(
  platform: ReactNativeBleManagerOptions['platform'],
  configuration: ExpoRuntimeConfiguration | undefined
): ExpoRuntimeConfiguration {
  return { ...configuration, platform }
}

function expoPlatform(platform: string): ExpoRuntimeConfiguration['platform'] {
  if (platform === 'android') return 'android'
  if (platform === 'ios') return 'apple'
  return undefined
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
  restorationControl?: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'claimRestoration'>,
  runtimeConfiguration?: ExpoRuntimeConfiguration
): ExpoBleManager {
  return Object.assign(manager, {
    readiness: () => getExpoBleReadiness(manager, runtimeConfiguration),
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
    return parseExpoPermissionResult(await permissionBridge(request))
  } catch (error) {
    if (isExpoBoundaryError(error, 'expo.permissions.result')) throw error
    const nativeCode = errorCode(error)
    throwExpoRuntimeError(
      normalizedPermissionErrorCode(nativeCode),
      'expo.permissions.request',
      errorMessage(error),
      nativeCode
    )
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
  try {
    const result = parseNativeBackgroundLeaseResult(
      await control.acquireBackground({ kind: request.kind, reason: request.reason })
    )
    return backgroundLease(control, result)
  } catch (error) {
    if (isExpoBoundaryError(error, 'expo.background.acquire.result')) throw error
    const nativeCode = errorCode(error)
    throwExpoRuntimeError(
      normalizedBackgroundErrorCode(nativeCode),
      'expo.background.acquire',
      errorMessage(error),
      nativeCode
    )
  }
}

function backgroundLease(
  control: Pick<import('./NativeUnifiedBleProtocolControl').Spec, 'acquireBackground' | 'releaseBackground'>,
  result: import('./NativeUnifiedBleProtocolControl').NativeBackgroundLeaseResult
): ExpoBackgroundLease {
  let releasePromise: Promise<void> | undefined
  return Object.freeze({
    release: () => {
      if (releasePromise !== undefined) return releasePromise
      releasePromise = (async () => {
        try {
          await control.releaseBackground({ leaseId: result.leaseId })
        } catch (error) {
          releasePromise = undefined
          const nativeCode = errorCode(error)
          throwExpoRuntimeError(
            normalizedBackgroundErrorCode(nativeCode),
            'expo.background.release',
            errorMessage(error),
            nativeCode
          )
        }
      })()
      return releasePromise
    }
  })
}

async function openExpoSettings(target: ExpoSettingsTarget, settingsBridge?: ExpoSettingsBridge): Promise<void> {
  if (!isExpoSettingsTarget(target)) {
    throw rehydratePublicError(contractError('argument.invalid', 'capability', 'expo.open-settings.target'))
  }
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
    const nativeCode = errorCode(error)
    throwExpoRuntimeError(
      normalizedSettingsErrorCode(nativeCode),
      'expo.open-settings',
      errorMessage(error),
      nativeCode
    )
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
    return parseExpoAssociationResult(await control.associateCompanionDevice(request))
  } catch (error) {
    if (isExpoBoundaryError(error, 'expo.association.result')) throw error
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
    return parseExpoRestorationClaimResult(await control.claimRestoration())
  } catch (error) {
    if (
      isExpoBoundaryError(error, 'expo.restoration.result') ||
      isExpoBoundaryError(error, 'expo.restoration.native-outcome')
    ) {
      throw error
    }
    throwExpoRuntimeError('capability.unavailable', 'expo.restoration.claim', errorMessage(error), errorCode(error))
  }
}

function restorationOutcome(outcome: unknown): ExpoRestorationClaimResult['outcome'] {
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
    default:
      throw rehydratePublicError(contractError('protocol.malformed', 'restoration', 'expo.restoration.native-outcome'))
  }
}

function parseExpoPermissionResult(value: unknown): ExpoPermissionResult {
  const result = expoRecord(value, 'expo.permissions.result')
  const requested = expoPermissionList(result.requested, 'expo.permissions.result')
  const granted = expoPermissionList(result.granted, 'expo.permissions.result')
  const denied = expoPermissionList(result.denied, 'expo.permissions.result')
  const recommendedSettingsTarget = result.recommendedSettingsTarget
  if (recommendedSettingsTarget !== null && !isExpoSettingsTarget(recommendedSettingsTarget)) {
    throwExpoMalformedResult('expo.permissions.result')
  }
  return Object.freeze({
    requested: Object.freeze(requested),
    granted: Object.freeze(granted),
    denied: Object.freeze(denied),
    recommendedSettingsTarget
  })
}

function parseNativeBackgroundLeaseResult(
  value: unknown
): import('./NativeUnifiedBleProtocolControl').NativeBackgroundLeaseResult {
  const result = expoRecord(value, 'expo.background.acquire.result')
  if (!nonEmptyString(result.leaseId)) throwExpoMalformedResult('expo.background.acquire.result')
  return Object.freeze({ leaseId: result.leaseId })
}

function parseExpoAssociationResult(value: unknown): ExpoCompanionAssociationResult {
  const result = expoRecord(value, 'expo.association.result')
  if (
    result.source !== 'associated' ||
    !isSafePositiveInteger(result.associationId) ||
    !nullableString(result.peerId) ||
    !nullableString(result.displayName)
  ) {
    throwExpoMalformedResult('expo.association.result')
  }
  return Object.freeze({
    source: 'associated',
    associationId: result.associationId,
    peerId: result.peerId,
    displayName: result.displayName
  })
}

function parseExpoRestorationClaimResult(value: unknown): ExpoRestorationClaimResult {
  const result = expoRecord(value, 'expo.restoration.result')
  const outcome = restorationOutcome(result.outcome)
  const replayRecordCount = result.replayRecordCount
  if (!isSafeNonNegativeInteger(replayRecordCount) || !Array.isArray(result.records)) {
    throwExpoMalformedResult('expo.restoration.result')
  }
  if (replayRecordCount !== result.records.length) throwExpoMalformedResult('expo.restoration.result')
  const records = result.records.map(record => parseExpoRestoredRecord(record))
  return Object.freeze({
    outcome,
    replayRecordCount,
    records: Object.freeze(records)
  })
}

function parseExpoRestoredRecord(value: unknown): ExpoRestoredRecord {
  const record = expoRecord(value, 'expo.restoration.result')
  if (
    (record.kind !== 'adapter' && record.kind !== 'connection') ||
    !isSafeNonNegativeInteger(record.ordinal) ||
    !nullableString(record.peerId)
  ) {
    throwExpoMalformedResult('expo.restoration.result')
  }
  return Object.freeze({ kind: record.kind, ordinal: record.ordinal, peerId: record.peerId })
}

function expoPermissionList(value: unknown, operation: string): BlePermission[] {
  if (!Array.isArray(value)) throwExpoMalformedResult(operation)
  const permissions: BlePermission[] = []
  for (const permission of value) {
    if (permission !== 'bluetooth' || permissions.includes(permission)) throwExpoMalformedResult(operation)
    permissions.push(permission)
  }
  return permissions
}

function expoRecord(value: unknown, operation: string): Record<string, unknown> {
  if (!isRecord(value)) throwExpoMalformedResult(operation)
  return value
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0
}

function nullableString(value: unknown): value is string | null {
  return value === null || nonEmptyString(value)
}

function isSafeNonNegativeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

function isSafePositiveInteger(value: unknown): value is number {
  return isSafeNonNegativeInteger(value) && value > 0
}

function isExpoSettingsTarget(value: unknown): value is ExpoSettingsTarget {
  return value === 'app' || value === 'bluetooth' || value === 'location-services'
}

function throwExpoMalformedResult(operation: string): never {
  throw rehydratePublicError(contractError('protocol.malformed', 'capability', operation))
}

function isExpoBoundaryError(error: unknown, operation: string): boolean {
  if (typeof error !== 'object' || error === null) return false
  if ('operation' in error && typeof error.operation === 'string') return error.operation === operation
  if (!('normalized' in error) || typeof error.normalized !== 'object' || error.normalized === null) return false
  return 'operation' in error.normalized && error.normalized.operation === operation
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
  const expectedDigest = configuration.expectedConfiguration?.digest
  const actualDigest = configuration.nativeConfiguration?.digest
  if (expectedDigest !== undefined && actualDigest === undefined) {
    throwExpoRuntimeError(
      'protocol.incompatible',
      'expo.runtime.configuration',
      'The native Expo configuration digest is unavailable; rebuild the native app before starting BLE.'
    )
  }
  if (expectedDigest !== undefined && actualDigest !== expectedDigest) {
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
  if (error instanceof Error) return error.message
  if (typeof error === 'object' && error !== null) {
    const message = Reflect.get(error, 'message')
    if (typeof message === 'string' && message.length > 0) return message
  }
  return 'The native Expo operation failed.'
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

function normalizedPermissionErrorCode(nativeCode: string): BleErrorCode {
  switch (nativeCode) {
    case 'unsupportedPermissionPrompt':
      return 'capability.unsupported'
    case 'permissionNotDeclared':
      return 'capability.unavailable'
    case 'permissionDenied':
      return 'permission.denied'
    default:
      return 'platform.failure'
  }
}

function normalizedSettingsErrorCode(nativeCode: string): BleErrorCode {
  switch (nativeCode) {
    case 'settingsUnsupported':
      return 'capability.unsupported'
    case 'settingsUnavailable':
      return 'capability.unavailable'
    default:
      return 'platform.failure'
  }
}
