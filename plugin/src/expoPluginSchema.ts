// Schema fingerprinting only. This is Node-only build-time code and must not be
// imported from runtime modules. The hash is not a security primitive.
import { createHash } from 'node:crypto'

export type LegacyLocationPolicy = 'auto' | 'required' | 'none'
export type NativeLoggingLevel = 'off' | 'errors' | 'events'

export interface UnifiedBleExpoAndroidPermissions {
  readonly neverForLocation?: boolean
  readonly legacyLocation?: LegacyLocationPolicy
}

export interface UnifiedBleExpoPermissions {
  readonly bluetoothAlways?: string | false
  readonly android?: UnifiedBleExpoAndroidPermissions
}

export type BackgroundContinuationStrategy = 'record-only' | 'native' | 'headless-task' | 'foreground-service'

export interface UnifiedBleExpoContinuationResubscribe {
  readonly serviceUuid: string
  readonly serviceOccurrence?: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence?: number
}

export interface UnifiedBleExpoBackgroundContinuation {
  readonly onAppearance?: BackgroundContinuationStrategy
  readonly peerId?: string
  readonly resubscribe?: readonly UnifiedBleExpoContinuationResubscribe[]
  readonly headlessTaskName?: string
  readonly foregroundService?: {
    readonly notification: {
      readonly channelId: string
      readonly channelName: string
      readonly title: string
      readonly body?: string
      readonly icon?: string
    }
  }
}

export interface UnifiedBleExpoPluginOptions {
  readonly requiredHardware?: boolean
  readonly permissions?: UnifiedBleExpoPermissions
  readonly background?: {
    readonly ios?: {
      readonly mode: 'central'
      readonly restoration?: {
        readonly id: string
        readonly generation?: string
      }
      readonly showPowerAlert?: boolean
    }
    readonly android?: AndroidBackgroundOptions
    readonly continuation?: UnifiedBleExpoBackgroundContinuation
  }
  readonly diagnostics?: {
    readonly nativeLogging?: NativeLoggingLevel
  }
}

export type AndroidBackgroundOptions =
  | { readonly mode: 'none' }
  | {
      readonly mode: 'connected-device-foreground-service'
      readonly notification: {
        readonly channelId: string
        readonly channelName: string
        readonly title: string
        readonly body?: string
        readonly icon?: string
      }
      readonly restart?: 'never' | 'while-session-intent-exists'
    }

export interface IosNativeProtocolRestoration {
  readonly identifier: string
  readonly namespace: string
  readonly epoch: string
  readonly clientId: string
  readonly hostSessionScope: string
}

const ROOT_KEYS = Object.freeze(['requiredHardware', 'permissions', 'background', 'diagnostics'])
const PERMISSIONS_KEYS = Object.freeze(['bluetoothAlways', 'android'])
const ANDROID_PERMISSIONS_KEYS = Object.freeze(['neverForLocation', 'legacyLocation'])
const BACKGROUND_KEYS = Object.freeze(['ios', 'android', 'continuation'])
const CONTINUATION_KEYS = Object.freeze([
  'onAppearance',
  'peerId',
  'resubscribe',
  'headlessTaskName',
  'foregroundService'
])
const CONTINUATION_RESUBSCRIBE_KEYS = Object.freeze([
  'serviceUuid',
  'serviceOccurrence',
  'characteristicUuid',
  'characteristicOccurrence'
])
const CONTINUATION_FOREGROUND_SERVICE_KEYS = Object.freeze(['notification'])
const IOS_BACKGROUND_KEYS = Object.freeze(['mode', 'restoration', 'showPowerAlert'])
const RESTORATION_KEYS = Object.freeze(['id', 'generation'])
const ANDROID_NOTIFICATION_KEYS = Object.freeze(['channelId', 'channelName', 'title', 'body', 'icon'])
const DIAGNOSTICS_KEYS = Object.freeze(['nativeLogging'])

function isConfigurationObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function configurationObject(value: unknown, label: string): Record<string, unknown> {
  if (!isConfigurationObject(value)) {
    throw new Error(`${label} must be an object when configured`)
  }
  return value
}

function rejectUnknownProperties(value: Record<string, unknown>, expected: readonly string[], label: string): void {
  const unknown = Object.keys(value)
    .filter(key => !expected.includes(key))
    .sort()
  if (unknown.length > 0) {
    throw new Error(`${label} contains unsupported properties: ${unknown.join(', ')}`)
  }
}

function requireNonEmptyProperties(value: Record<string, unknown>, label: string): void {
  if (Object.keys(value).length === 0) {
    throw new Error(`${label} must contain at least one configured property`)
  }
}

function booleanOption(value: unknown, label: string): boolean | undefined {
  if (value === undefined) return undefined
  if (typeof value !== 'boolean') throw new Error(`${label} must be a boolean when configured`)
  return value
}

function nonEmptyString(value: unknown, label: string): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new Error(`${label} must be a non-empty string`)
  }
  return value
}

const RESTORATION_TOKEN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/

function restorationToken(value: unknown, label: string, maxBytes: number): string {
  const token = nonEmptyString(value, label)
  if (!RESTORATION_TOKEN.test(token) || Buffer.byteLength(token, 'utf8') > maxBytes) {
    throw new Error(`${label} must match [A-Za-z0-9][A-Za-z0-9._-]* and be at most ${maxBytes} bytes`)
  }
  return token
}

function optionalString(value: unknown, label: string): string | undefined {
  if (value === undefined) return undefined
  return nonEmptyString(value, label)
}

function legacyLocationPolicy(value: unknown): LegacyLocationPolicy | undefined {
  if (value === undefined) return undefined
  if (value === 'auto' || value === 'required' || value === 'none') return value
  throw new Error('permissions.android.legacyLocation must be auto, required, or none')
}

function nativeLoggingLevel(value: unknown): NativeLoggingLevel | undefined {
  if (value === undefined) return undefined
  if (value === 'off' || value === 'errors' || value === 'events') return value
  throw new Error('diagnostics.nativeLogging must be off, errors, or events')
}

function freezeObject<T extends object>(value: T): T {
  return Object.freeze(value)
}

function validatePermissions(value: unknown): UnifiedBleExpoPluginOptions['permissions'] {
  const permissions = configurationObject(value, 'permissions')
  rejectUnknownProperties(permissions, PERMISSIONS_KEYS, 'permissions')
  requireNonEmptyProperties(permissions, 'permissions')

  const bluetoothAlways: string | false | undefined =
    permissions.bluetoothAlways === undefined
      ? undefined
      : permissions.bluetoothAlways === false
        ? false
        : nonEmptyString(permissions.bluetoothAlways, 'permissions.bluetoothAlways')

  let android: UnifiedBleExpoAndroidPermissions | undefined
  if (permissions.android !== undefined) {
    const androidPermissions = configurationObject(permissions.android, 'permissions.android')
    rejectUnknownProperties(androidPermissions, ANDROID_PERMISSIONS_KEYS, 'permissions.android')
    requireNonEmptyProperties(androidPermissions, 'permissions.android')
    const neverForLocation = booleanOption(androidPermissions.neverForLocation, 'permissions.android.neverForLocation')
    const legacyLocation = legacyLocationPolicy(androidPermissions.legacyLocation)
    android = freezeObject({
      ...(neverForLocation === undefined ? {} : { neverForLocation }),
      ...(legacyLocation === undefined ? {} : { legacyLocation })
    })
  }

  return freezeObject({
    ...(bluetoothAlways === undefined ? {} : { bluetoothAlways }),
    ...(android === undefined ? {} : { android })
  })
}

function validateRestoration(
  value: unknown
): NonNullable<NonNullable<UnifiedBleExpoPluginOptions['background']>['ios']>['restoration'] {
  const restoration = configurationObject(value, 'background.ios.restoration')
  rejectUnknownProperties(restoration, RESTORATION_KEYS, 'background.ios.restoration')
  const id = restorationToken(restoration.id, 'background.ios.restoration.id', 128)
  const generation =
    restoration.generation === undefined
      ? undefined
      : restorationToken(restoration.generation, 'background.ios.restoration.generation', 64)
  return freezeObject({ ...(generation === undefined ? {} : { generation }), id })
}

function validateIosBackground(
  value: unknown
): NonNullable<NonNullable<UnifiedBleExpoPluginOptions['background']>['ios']> {
  const ios = configurationObject(value, 'background.ios')
  rejectUnknownProperties(ios, IOS_BACKGROUND_KEYS, 'background.ios')
  if (ios.mode !== 'central') {
    throw new Error('background.ios.mode must be central')
  }
  const restoration = ios.restoration === undefined ? undefined : validateRestoration(ios.restoration)
  const showPowerAlert = booleanOption(ios.showPowerAlert, 'background.ios.showPowerAlert')
  return freezeObject({
    mode: 'central',
    ...(restoration === undefined ? {} : { restoration }),
    ...(showPowerAlert === undefined ? {} : { showPowerAlert })
  })
}

function validateNotification(
  value: unknown
): NonNullable<Extract<AndroidBackgroundOptions, { mode: 'connected-device-foreground-service' }>['notification']> {
  const notification = configurationObject(value, 'background.android.notification')
  rejectUnknownProperties(notification, ANDROID_NOTIFICATION_KEYS, 'background.android.notification')
  return freezeObject({
    channelId: nonEmptyString(notification.channelId, 'background.android.notification.channelId'),
    channelName: nonEmptyString(notification.channelName, 'background.android.notification.channelName'),
    title: nonEmptyString(notification.title, 'background.android.notification.title'),
    ...(optionalString(notification.body, 'background.android.notification.body') === undefined
      ? {}
      : { body: optionalString(notification.body, 'background.android.notification.body') }),
    ...(optionalString(notification.icon, 'background.android.notification.icon') === undefined
      ? {}
      : { icon: optionalString(notification.icon, 'background.android.notification.icon') })
  })
}

function validateAndroidBackground(value: unknown): AndroidBackgroundOptions {
  const android = configurationObject(value, 'background.android')
  if (android.mode === 'none') {
    if (Object.keys(android).length !== 1) {
      throw new Error('background.android mode none cannot contain foreground-service properties')
    }
    return freezeObject({ mode: 'none' })
  }
  if (android.mode !== 'connected-device-foreground-service') {
    throw new Error('background.android.mode must be none or connected-device-foreground-service')
  }
  const notification = validateNotification(android.notification)
  const restart = android.restart
  if (restart !== undefined && restart !== 'never' && restart !== 'while-session-intent-exists') {
    throw new Error('background.android.restart must be never or while-session-intent-exists')
  }
  rejectUnknownProperties(android, ['mode', 'notification', 'restart'], 'background.android')
  return freezeObject({
    mode: 'connected-device-foreground-service',
    notification,
    ...(restart === undefined ? {} : { restart })
  })
}

const MAC_PATTERN = /^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$/
const UUID_PATTERN = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/

function continuationOccurrence(value: unknown, label: string): number {
  if (value === undefined) return 1
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) {
    throw new Error(`${label} must be a positive integer when configured`)
  }
  return value
}

function continuationUuid(value: unknown, label: string): string {
  const text = nonEmptyString(value, label)
  if (!UUID_PATTERN.test(text)) {
    throw new Error(`${label} must be a canonical 128-bit UUID`)
  }
  return text.toLowerCase()
}

function validateContinuationResubscribe(
  value: unknown
): NonNullable<UnifiedBleExpoBackgroundContinuation['resubscribe']>[number] {
  const entry = configurationObject(value, 'background.continuation.resubscribe entry')
  rejectUnknownProperties(entry, CONTINUATION_RESUBSCRIBE_KEYS, 'background.continuation.resubscribe entry')
  return freezeObject({
    serviceUuid: continuationUuid(entry.serviceUuid, 'background.continuation.resubscribe.serviceUuid'),
    serviceOccurrence: continuationOccurrence(
      entry.serviceOccurrence,
      'background.continuation.resubscribe.serviceOccurrence'
    ),
    characteristicUuid: continuationUuid(
      entry.characteristicUuid,
      'background.continuation.resubscribe.characteristicUuid'
    ),
    characteristicOccurrence: continuationOccurrence(
      entry.characteristicOccurrence,
      'background.continuation.resubscribe.characteristicOccurrence'
    )
  })
}

function validateContinuationForegroundService(
  value: unknown
): NonNullable<UnifiedBleExpoBackgroundContinuation['foregroundService']> {
  const service = configurationObject(value, 'background.continuation.foregroundService')
  rejectUnknownProperties(service, CONTINUATION_FOREGROUND_SERVICE_KEYS, 'background.continuation.foregroundService')
  if (service.notification === undefined) {
    throw new Error('background.continuation.foregroundService.notification is required')
  }
  const notification = configurationObject(
    service.notification,
    'background.continuation.foregroundService.notification'
  )
  rejectUnknownProperties(
    notification,
    ANDROID_NOTIFICATION_KEYS,
    'background.continuation.foregroundService.notification'
  )
  return freezeObject({
    notification: freezeObject({
      channelId: nonEmptyString(
        notification.channelId,
        'background.continuation.foregroundService.notification.channelId'
      ),
      channelName: nonEmptyString(
        notification.channelName,
        'background.continuation.foregroundService.notification.channelName'
      ),
      title: nonEmptyString(notification.title, 'background.continuation.foregroundService.notification.title'),
      ...(optionalString(notification.body, 'background.continuation.foregroundService.notification.body') === undefined
        ? {}
        : {
            body: optionalString(notification.body, 'background.continuation.foregroundService.notification.body')
          }),
      ...(optionalString(notification.icon, 'background.continuation.foregroundService.notification.icon') === undefined
        ? {}
        : {
            icon: optionalString(notification.icon, 'background.continuation.foregroundService.notification.icon')
          })
    })
  })
}

function continuationStrategy(value: unknown): BackgroundContinuationStrategy {
  if (value === 'record-only' || value === 'native' || value === 'headless-task' || value === 'foreground-service') {
    return value
  }
  throw new Error(
    'background.continuation.onAppearance must be record-only, native, headless-task, or foreground-service'
  )
}

function continuationPeerId(value: unknown): string {
  const text = nonEmptyString(value, 'background.continuation.peerId')
  if (!MAC_PATTERN.test(text)) {
    throw new Error('background.continuation.peerId must be a MAC address (AA:BB:CC:DD:EE:FF)')
  }
  return text.toUpperCase()
}

function continuationResubscribeList(
  value: unknown
): readonly NonNullable<UnifiedBleExpoBackgroundContinuation['resubscribe']>[number][] {
  if (!Array.isArray(value)) {
    throw new Error('background.continuation.resubscribe must be an array when configured')
  }
  return Object.freeze(value.map(validateContinuationResubscribe))
}

function validateContinuation(value: unknown): UnifiedBleExpoBackgroundContinuation {
  const continuation = configurationObject(value, 'background.continuation')
  rejectUnknownProperties(continuation, CONTINUATION_KEYS, 'background.continuation')
  const strategy =
    continuation.onAppearance === undefined ? 'record-only' : continuationStrategy(continuation.onAppearance)
  const peerId = continuation.peerId === undefined ? undefined : continuationPeerId(continuation.peerId)
  const resubscribe =
    continuation.resubscribe === undefined ? undefined : continuationResubscribeList(continuation.resubscribe)
  const headlessTaskName =
    continuation.headlessTaskName === undefined
      ? undefined
      : nonEmptyString(continuation.headlessTaskName, 'background.continuation.headlessTaskName')
  if (strategy === 'headless-task' && headlessTaskName === undefined) {
    throw new Error('background.continuation.headlessTaskName is required for headless-task')
  }
  if (strategy !== 'headless-task' && headlessTaskName !== undefined) {
    throw new Error('background.continuation.headlessTaskName applies only to headless-task')
  }
  const foregroundService =
    continuation.foregroundService === undefined
      ? undefined
      : validateContinuationForegroundService(continuation.foregroundService)
  if (strategy === 'foreground-service' && foregroundService === undefined) {
    throw new Error('background.continuation.foregroundService is required for foreground-service')
  }
  if (strategy !== 'foreground-service' && foregroundService !== undefined) {
    throw new Error('background.continuation.foregroundService applies only to foreground-service')
  }
  return freezeObject({
    onAppearance: strategy,
    ...(peerId === undefined ? {} : { peerId }),
    ...(resubscribe === undefined ? {} : { resubscribe }),
    ...(headlessTaskName === undefined ? {} : { headlessTaskName }),
    ...(foregroundService === undefined ? {} : { foregroundService })
  })
}

function validateBackground(value: unknown): UnifiedBleExpoPluginOptions['background'] {
  const background = configurationObject(value, 'background')
  rejectUnknownProperties(background, BACKGROUND_KEYS, 'background')
  requireNonEmptyProperties(background, 'background')
  const ios = background.ios === undefined ? undefined : validateIosBackground(background.ios)
  const android = background.android === undefined ? undefined : validateAndroidBackground(background.android)
  const continuation = background.continuation === undefined ? undefined : validateContinuation(background.continuation)
  return freezeObject({
    ...(ios === undefined ? {} : { ios }),
    ...(android === undefined ? {} : { android }),
    ...(continuation === undefined ? {} : { continuation })
  })
}

function validateDiagnostics(value: unknown): UnifiedBleExpoPluginOptions['diagnostics'] {
  const diagnostics = configurationObject(value, 'diagnostics')
  rejectUnknownProperties(diagnostics, DIAGNOSTICS_KEYS, 'diagnostics')
  requireNonEmptyProperties(diagnostics, 'diagnostics')
  const nativeLogging = nativeLoggingLevel(diagnostics.nativeLogging)
  return freezeObject({ ...(nativeLogging === undefined ? {} : { nativeLogging }) })
}

export function validateUnifiedBleExpoPluginOptions(value: unknown): UnifiedBleExpoPluginOptions {
  if (value === undefined) return freezeObject({})
  const options = configurationObject(value, 'unified-ble-manager Expo plugin options')
  rejectUnknownProperties(options, ROOT_KEYS, 'unified-ble-manager Expo plugin options')

  const requiredHardware = booleanOption(options.requiredHardware, 'requiredHardware')
  const permissions = options.permissions === undefined ? undefined : validatePermissions(options.permissions)
  const background = options.background === undefined ? undefined : validateBackground(options.background)
  const diagnostics = options.diagnostics === undefined ? undefined : validateDiagnostics(options.diagnostics)

  return freezeObject({
    ...(requiredHardware === undefined ? {} : { requiredHardware }),
    ...(permissions === undefined ? {} : { permissions }),
    ...(background === undefined ? {} : { background }),
    ...(diagnostics === undefined ? {} : { diagnostics })
  })
}

export interface RestorationDerivationInput {
  readonly applicationId: string
  readonly restorationId: string
  readonly generation?: string
}

function sha256(value: Buffer): Buffer {
  return createHash('sha256').update(value).digest()
}

function base64Url(value: Buffer): string {
  return value.toString('base64').replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')
}

function lengthPrefixed(value: string): Buffer {
  const encoded = Buffer.from(value, 'utf8')
  const length = Buffer.allocUnsafe(4)
  length.writeUInt32BE(encoded.length, 0)
  return Buffer.concat([length, encoded])
}

export function deriveIosNativeProtocolRestoration(input: RestorationDerivationInput): IosNativeProtocolRestoration {
  const applicationId = nonEmptyString(input.applicationId, 'applicationId')
  const restorationId = restorationToken(input.restorationId, 'restorationId', 128)
  const generation = input.generation === undefined ? '1' : restorationToken(input.generation, 'generation', 64)
  const root = sha256(
    Buffer.concat([
      Buffer.from('ubm-restoration-v1', 'utf8'),
      lengthPrefixed(applicationId),
      lengthPrefixed(restorationId),
      lengthPrefixed(generation)
    ])
  )
  const derive = (label: string) =>
    base64Url(sha256(Buffer.concat([root, Buffer.from([0]), Buffer.from(label, 'utf8')])))
  return {
    identifier: `${applicationId}.ubm.${derive('restore').slice(0, 22)}`,
    namespace: `ubm-ns:${derive('namespace')}`,
    epoch: generation,
    clientId: `ubm-client:${derive('client')}`,
    hostSessionScope: `ubm-host:${derive('host')}`
  }
}
