// src/expo.ts — thin Expo-aware composition over the React Native factory

import { BackendContractError, contractError } from './backend-contract/errors'
import type { BleErrorCode } from './backend-contract/errors'
import type { RestorationAdoptionResult } from './backend-contract/restoration'
import { Platform, TurboModuleRegistry } from 'react-native'
import { rehydratePublicError } from './public/error-bridge'
import { BleError } from './public/errors'
import type { BleAdapterState } from './public/ble-adapter'
import { createPublicBleManager, type BleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions, type BleManagerCreateOptions } from './public/host-identity'
import { createReactNativeApplicationHost } from './react-native-app-manager'
import { createReactNativeManagerHost, type ReactNativeManagerHost } from './react-native-manager'
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
  /**
   * Bounds the wait for the user's decision. Without one the request waits
   * until the platform answers or the signal aborts: the prompt is
   * user-facing UI, like bonding, and legacy waited it out too.
   */
  readonly timeoutMs?: number
  /** Aborts a pending request; the late platform answer is then discarded. */
  readonly signal?: AbortSignal
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

export interface ExpoBackgroundNotificationUpdate {
  readonly title: string
  readonly body?: string
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

export interface ExpoPresenceObservationRequest {
  readonly peerId: string
}

export interface ExpoPresenceObservationResult {
  readonly state: 'observing'
}

export interface ExpoPresenceReleaseResult {
  readonly state: 'idle'
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
    readonly updateNotification: (request: ExpoBackgroundNotificationUpdate) => Promise<void>
  }
  readonly association: {
    readonly associate: (request?: ExpoCompanionAssociationRequest) => Promise<ExpoCompanionAssociationResult>
  }
  readonly restoration: {
    readonly claim: () => Promise<ExpoRestorationClaimResult>
  }
  readonly presence: {
    readonly observe: (request: ExpoPresenceObservationRequest) => Promise<ExpoPresenceObservationResult>
    readonly unobserve: (request: ExpoPresenceObservationRequest) => Promise<ExpoPresenceReleaseResult>
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

const EXPO_GO_MESSAGE = 'Expo Go is not supported; create an Expo development build that includes UnifiedBleRustCore.'

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
    const host = await createReactNativeApplicationHost(options)
    return withExpoRuntime(
      await createPublicBleManager(host.manager, () => performance.now()),
      host,
      readinessConfiguration?.settingsBridge ?? nativeSettingsBridge(nativeRuntime),
      readinessConfiguration?.permissionBridge ?? nativePermissionBridge(nativeRuntime),
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
    const host = await createReactNativeManagerHost(environment)
    return withExpoRuntime(
      await createPublicBleManager(host.manager, environment.now),
      host,
      expo?.settingsBridge,
      expo?.permissionBridge,
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
  host: ReactNativeManagerHost,
  settingsBridge?: ExpoSettingsBridge,
  permissionBridge?: ExpoPermissionBridge,
  runtimeConfiguration?: ExpoRuntimeConfiguration
): ExpoBleManager {
  const activeBackgroundLeases = new Set<string>()
  return Object.assign(manager, {
    readiness: () => getExpoBleReadiness(manager, runtimeConfiguration),
    permissions: Object.freeze({
      request: (request: ExpoPermissionRequest) => requestExpoPermissions(request, permissionBridge)
    }),
    openSettings: (target: ExpoSettingsTarget) => openExpoSettings(target, settingsBridge),
    background: Object.freeze({
      acquire: async (request: ExpoBackgroundRequest) => {
        const lease = await acquireExpoBackground(request, host)
        activeBackgroundLeases.add(lease.leaseId)
        return Object.freeze({
          release: async () => {
            await lease.release()
            activeBackgroundLeases.delete(lease.leaseId)
          }
        })
      },
      updateNotification: (request: ExpoBackgroundNotificationUpdate) =>
        updateExpoBackgroundNotification(request, host, activeBackgroundLeases)
    }),
    association: Object.freeze({
      associate: (request: ExpoCompanionAssociationRequest = {}) => associateExpoCompanionDevice(request, host)
    }),
    restoration: Object.freeze({
      claim: () => claimExpoRestoration(host)
    }),
    presence: Object.freeze({
      observe: (request: ExpoPresenceObservationRequest) => observeExpoPresence(request, host),
      unobserve: (request: ExpoPresenceObservationRequest) => unobserveExpoPresence(request, host)
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
  assertValidPermissionTimeout(request.timeoutMs)
  assertValidPermissionSignal(request.signal)
  if (request.signal?.aborted === true) {
    throwExpoRuntimeError('operation.aborted', 'expo.permissions.request', 'The permission request was aborted.')
  }
  if (permissionBridge === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      'expo.permissions.request',
      'No trusted native permission bridge is available; invoke the host permission flow explicitly.'
    )
  }
  try {
    return parseExpoPermissionResult(await racePermissionBridge(request, permissionBridge))
  } catch (error) {
    if (error instanceof BleError) throw error
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

/**
 * Races the platform prompt against the caller's timeout and signal. The
 * bridge promise always settles — a late answer after a timeout or abort is
 * discarded, with a no-op rejection guard so it never surfaces as an
 * unhandled rejection. The result still reports what happened: the platform
 * answer on success, `operation.timed-out` or `operation.aborted` otherwise.
 */
function racePermissionBridge(
  request: ExpoPermissionRequest,
  permissionBridge: ExpoPermissionBridge
): Promise<ExpoPermissionResult> {
  const pending = permissionBridge(request)
  // A settled race must not turn a late bridge failure into an unhandled
  // rejection: the operation already reported its outcome.
  pending.then(undefined, () => undefined)
  if (request.signal === undefined && request.timeoutMs === undefined) return pending
  return new Promise<ExpoPermissionResult>((resolve, reject) => {
    let settled = false
    const settle = (): boolean => {
      if (settled) return false
      settled = true
      if (timer !== undefined) clearTimeout(timer)
      request.signal?.removeEventListener('abort', onAbort)
      return true
    }
    const fail = (code: BleErrorCode, message: string): void => {
      if (!settle()) return
      try {
        throwExpoRuntimeError(code, 'expo.permissions.request', message)
      } catch (error) {
        reject(error)
      }
    }
    const timer =
      request.timeoutMs === undefined
        ? undefined
        : setTimeout(() => {
            fail(
              'operation.timed-out',
              `The permission request was not answered within ${String(request.timeoutMs)}ms.`
            )
          }, request.timeoutMs)
    const onAbort = (): void => {
      fail('operation.aborted', 'The permission request was aborted.')
    }
    request.signal?.addEventListener('abort', onAbort, { once: true })
    pending.then(
      value => {
        if (settle()) resolve(value)
      },
      error => {
        if (settle()) reject(error)
      }
    )
  })
}

/** Mirrors the shared operation-options timeout bounds for this entrypoint. */
function assertValidPermissionTimeout(value: unknown): void {
  if (value === undefined) return
  if (typeof value !== 'number' || !Number.isFinite(value) || !Number.isSafeInteger(value) || value <= 0) {
    throw rehydratePublicError(contractError('argument.invalid', 'capability', 'expo.permissions.timeout'))
  }
}

function assertValidPermissionSignal(value: unknown): void {
  if (value === undefined) return
  const abortLike =
    typeof value === 'object' &&
    value !== null &&
    'aborted' in value &&
    typeof value.aborted === 'boolean' &&
    'addEventListener' in value &&
    typeof value.addEventListener === 'function' &&
    'removeEventListener' in value &&
    typeof value.removeEventListener === 'function'
  if (!abortLike) {
    throw rehydratePublicError(contractError('argument.invalid', 'capability', 'expo.permissions.signal'))
  }
}

async function acquireExpoBackground(
  request: ExpoBackgroundRequest,
  host: ReactNativeManagerHost
): Promise<ExpoBackgroundLease & { readonly leaseId: string }> {
  if (request.kind !== 'connected-device' || request.reason.trim().length === 0) {
    throwExpoRuntimeError('argument.invalid', 'expo.background.acquire', 'A non-empty background reason is required.')
  }
  let result: { readonly leaseId: string }
  try {
    result = await host.services.acquireBackground({ kind: request.kind, reason: request.reason })
  } catch (error) {
    throwBackgroundOwnerError(error, 'expo.background.acquire')
  }
  return backgroundLease(host, result.leaseId)
}

function backgroundLease(
  host: ReactNativeManagerHost,
  leaseId: string
): ExpoBackgroundLease & { readonly leaseId: string } {
  let releasePromise: Promise<void> | undefined
  return Object.freeze({
    leaseId,
    release: () => {
      if (releasePromise !== undefined) return releasePromise
      releasePromise = (async () => {
        try {
          const cleanup = await host.services.releaseBackground(leaseId)
          const failure = cleanup.failures[0]
          if (cleanup.state !== 'released' || failure !== undefined) {
            throw failure === undefined
              ? contractError('lifecycle.invariant-violation', 'cleanup', 'expo.background.release')
              : new BackendContractError(failure.error)
          }
        } catch (error) {
          releasePromise = undefined
          throwBackgroundOwnerError(error, 'expo.background.release')
        }
      })()
      return releasePromise
    }
  })
}

const MAXIMUM_NOTIFICATION_TEXT_LENGTH = 256

async function updateExpoBackgroundNotification(
  request: ExpoBackgroundNotificationUpdate,
  host: ReactNativeManagerHost,
  activeLeases: ReadonlySet<string>
): Promise<void> {
  const operation = 'expo.background.update-notification'
  if (
    !isRecord(request) ||
    !boundedNonEmptyString(request.title) ||
    (request.body !== undefined && !boundedNonEmptyString(request.body))
  ) {
    throwExpoRuntimeError('argument.invalid', operation, 'Notification title and body must be non-empty and bounded.')
  }
  const leaseId = activeLeases.values().next().value
  if (leaseId === undefined) {
    throwExpoRuntimeError(
      'capability.unavailable',
      operation,
      'An active connected-device background lease is required to update its notification.'
    )
  }
  try {
    await host.services.updateBackgroundNotification({
      leaseId,
      title: request.title,
      ...(request.body === undefined ? {} : { body: request.body })
    })
  } catch (error) {
    throwBackgroundOwnerError(error, operation)
  }
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
  host: ReactNativeManagerHost
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
  let result: unknown
  try {
    result = await host.services.associateCompanion(request)
  } catch (error) {
    // Legacy Expo reported every association failure as capability.unavailable.
    throwUnavailableOwnerError(error, 'expo.association.associate')
  }
  return parseExpoAssociationResult(result)
}

function expoPresencePeerId(request: ExpoPresenceObservationRequest, operation: string): string {
  if (!isRecord(request) || typeof request.peerId !== 'string' || request.peerId.length === 0) {
    throwExpoRuntimeError('argument.invalid', operation, 'A known peer id is required to observe its presence.')
  }
  return request.peerId
}

async function observeExpoPresence(
  request: ExpoPresenceObservationRequest,
  host: ReactNativeManagerHost
): Promise<ExpoPresenceObservationResult> {
  const operation = 'expo.presence.observe'
  const peerId = expoPresencePeerId(request, operation)
  let result: unknown
  try {
    result = await host.services.observePresence({ peerId })
  } catch (error) {
    // Presence is new in 5.0 with no legacy Expo code to preserve: the
    // owner's answer keeps its code (capability.unsupported on Apple, with
    // the platform reason), never a wrapped fake.
    throwOwnerError(error, operation)
  }
  const record = expoRecord(result, 'expo.presence.result')
  if (record.state !== 'observing') throwExpoMalformedResult('expo.presence.result')
  return Object.freeze({ state: 'observing' })
}

async function unobserveExpoPresence(
  request: ExpoPresenceObservationRequest,
  host: ReactNativeManagerHost
): Promise<ExpoPresenceReleaseResult> {
  const operation = 'expo.presence.unobserve'
  const peerId = expoPresencePeerId(request, operation)
  let result: unknown
  try {
    result = await host.services.unobservePresence({ peerId })
  } catch (error) {
    throwOwnerError(error, operation)
  }
  const record = expoRecord(result, 'expo.presence.result')
  if (record.state !== 'idle') throwExpoMalformedResult('expo.presence.result')
  return Object.freeze({ state: 'idle' })
}

async function claimExpoRestoration(host: ReactNativeManagerHost): Promise<ExpoRestorationClaimResult> {
  let result: RestorationAdoptionResult<string>
  try {
    result = await host.claimRestoration()
  } catch (error) {
    // Legacy Expo wrapped every restoration claim failure as capability.unavailable.
    throwUnavailableOwnerError(error, 'expo.restoration.claim')
  }
  return Object.freeze({
    outcome: result.outcome,
    replayRecordCount: result.replayedRecords.length,
    records: Object.freeze(
      result.replayedRecords.map(record => {
        if (record.kind !== 'adapter' && record.kind !== 'connection') {
          throwExpoMalformedResult('expo.restoration.result')
        }
        return Object.freeze({
          kind: record.kind,
          ordinal: record.ordinal,
          peerId: record.peerId === null ? null : String(record.peerId)
        })
      })
    )
  })
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

function boundedNonEmptyString(value: unknown): value is string {
  return nonEmptyString(value) && value.length <= MAXIMUM_NOTIFICATION_TEXT_LENGTH
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
  if (TurboModuleRegistry.get('UnifiedBleRustCore') == null) {
    throwExpoRuntimeError('capability.unavailable', 'expo.runtime.development-build', EXPO_GO_MESSAGE)
  }
}

/**
 * Re-raises an owner failure under the Expo operation: the owner's code
 * (`capability.unsupported`, `permission.denied`, ...) is kept, with its
 * operation and detail as platform data.
 */
/**
 * Legacy Expo's code for a native foreground-service failure (4.x
 * `src/expo.ts` `normalizedBackgroundErrorCode`, finding 133).
 */
function normalizedBackgroundErrorCode(nativeCode: string): BleErrorCode {
  switch (nativeCode) {
    case 'foregroundServiceNotConfigured':
    case 'foregroundServiceNotRunning':
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

/**
 * The native code legacy's background module would have rejected with: the
 * Android registry's own code, or the owner's refusal of an unknown lease,
 * a malformed request or a platform without the service. `null` for an
 * owner failure legacy had no native code for (a timeout, a destroyed
 * session), which keeps its own contract code.
 */
function backgroundNativeCode(error: BackendContractError): string | null {
  const normalized = error.normalized
  if (normalized.platform?.domain === 'android') return normalized.platform.code
  switch (normalized.code) {
    case 'ownership.denied':
      return 'invalidBackgroundLease'
    case 'argument.invalid':
      return 'invalidBackgroundRequest'
    case 'capability.unsupported':
      return 'unsupportedBackground'
    default:
      return null
  }
}

function throwBackgroundOwnerError(error: unknown, operation: string): never {
  if (error instanceof BackendContractError) {
    const nativeCode = backgroundNativeCode(error)
    if (nativeCode !== null) {
      throwExpoRuntimeError(
        normalizedBackgroundErrorCode(nativeCode),
        operation,
        error.normalized.platform?.safeMessage ?? error.normalized.operation,
        nativeCode
      )
    }
  }
  throwOwnerError(error, operation)
}

/** `capability.unavailable` carrying the native (or owner) code, as legacy Expo reported it. */
function throwUnavailableOwnerError(error: unknown, operation: string): never {
  if (error instanceof BackendContractError) {
    const normalized = error.normalized
    throwExpoRuntimeError(
      'capability.unavailable',
      operation,
      normalized.platform?.safeMessage ?? normalized.operation,
      normalized.platform?.domain === 'android' ? normalized.platform.code : normalized.code
    )
  }
  throwExpoRuntimeError('capability.unavailable', operation, errorMessage(error), errorCode(error))
}

function throwOwnerError(error: unknown, operation: string): never {
  if (error instanceof BackendContractError) {
    const normalized = error.normalized
    throwExpoRuntimeError(
      normalized.code,
      operation,
      normalized.platform?.safeMessage ?? normalized.operation,
      normalized.operation
    )
  }
  throwExpoRuntimeError('platform.failure', operation, errorMessage(error), errorCode(error))
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

function normalizedPermissionErrorCode(nativeCode: string): BleErrorCode {
  switch (nativeCode) {
    case 'unsupportedPermissionPrompt':
      return 'capability.unsupported'
    case 'permissionRestricted':
      // iOS parental/MDM restrictions: the user cannot change this, so it is
      // unsupported with the platform reason rather than a denial that would
      // send the app to settings it cannot fix (finding 179).
      return 'capability.unsupported'
    case 'permissionNotDeclared':
    case 'permissionUnavailable':
      return 'capability.unavailable'
    case 'permissionDenied':
      return 'permission.denied'
    case 'permissionTimeout':
      return 'operation.timed-out'
    case 'permissionInvalidPurpose':
      return 'argument.invalid'
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
