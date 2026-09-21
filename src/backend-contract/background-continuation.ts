// src/backend-contract/background-continuation.ts
//
// `background.continuation`: the declared standing order the OS wake executes
// (BGS4, the 5.0 headline capability). The app declares while alive what a
// wake may do; the wake executes only what was declared — the library never
// invents a connect. `record-only` is today's behaviour and the default, so
// 5.0 changes nothing for an app that does not opt in.
//
// Strategies: `record-only` (install owner, record restored peer, stop),
// `native` (reconnect the declared known peer + resubscribe the declared
// characteristics through the Rust core, no JavaScript), `headless-task` (run
// the registered headless JS task, Android), `foreground-service` (start the
// configured connected-device foreground service from the wake). The deferred
// strategies keep their validated option shape here so rc.1 adds executors
// with no breaking change; until then they answer `capability.unsupported`
// with "not implemented in this release" — never "the platform cannot".

import { contractError } from './errors'

/** Wake strategies in declaration order. `record-only` is the default. */
export const CONTINUATION_STRATEGIES = Object.freeze([
  'record-only',
  'native',
  'headless-task',
  'foreground-service'
] as const)

export type BackgroundContinuationStrategy = (typeof CONTINUATION_STRATEGIES)[number]

/** One capability per strategy, reported at runtime by the instantiated backend. */
export const BACKGROUND_CONTINUATION_FEATURE_IDS = Object.freeze({
  wakeOnAppearance: 'background:wake-on-appearance',
  nativeResubscribe: 'background:native-resubscribe',
  headlessTask: 'background:headless-task',
  wakeNotification: 'background:wake-notification'
} as const)

export type BackgroundContinuationFeatureId =
  (typeof BACKGROUND_CONTINUATION_FEATURE_IDS)[keyof typeof BACKGROUND_CONTINUATION_FEATURE_IDS]

/**
 * Wake outcome vocabulary. ONE vocabulary on both hosts: the wake reports
 * `continuation.completed` or `continuation.failed` with the platform's own
 * detail underneath — the same event names and words on Android and iOS.
 * iOS parity arrives in rc.1; these names are designed so the Apple path
 * reports the same events.
 */
export const CONTINUATION_OUTCOME_EVENTS = Object.freeze(['continuation.completed', 'continuation.failed'] as const)

export type BackgroundContinuationOutcomeEvent = (typeof CONTINUATION_OUTCOME_EVENTS)[number]

/** One declared GATT resubscription for the `native` standing order. */
export interface BackgroundContinuationResubscribeSelector {
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence: number
}

/** Foreground-service configuration for the deferred `foreground-service` strategy. */
export interface BackgroundContinuationForegroundService {
  readonly notification: {
    readonly channelId: string
    readonly channelName: string
    readonly title: string
    readonly body?: string
    readonly icon?: string
  }
}

/** The declared standing order: what one OS wake may do, and to what. */
export interface BackgroundContinuationDeclaration {
  readonly onAppearance: BackgroundContinuationStrategy
  /** Uppercase MAC subject; absent scopes the order to whichever armed peer appears. */
  readonly peerId?: string
  readonly resubscribe: readonly BackgroundContinuationResubscribeSelector[]
  readonly headlessTaskName?: string
  readonly foregroundService?: BackgroundContinuationForegroundService
}

/** The default: today's behaviour — install owner, record restored peer, stop. */
export const DEFAULT_BACKGROUND_CONTINUATION: BackgroundContinuationDeclaration = Object.freeze({
  onAppearance: 'record-only' as const,
  resubscribe: Object.freeze([])
})

/**
 * Wake consumer name prefix. The native wake subscribes as
 * `ubm-continuation-{index}` (one per declared resubscription, in order), so
 * the claim maps each backlog value to the selector that subscribed it. The
 * Kotlin executor's `CONSUMER_PREFIX` must stay identical; both sides cite
 * this constant's spelling.
 */
export const CONTINUATION_CONSUMER_PREFIX = 'ubm-continuation-' as const

const DECLARATION_KEYS = Object.freeze([
  'onAppearance',
  'peerId',
  'resubscribe',
  'headlessTaskName',
  'foregroundService'
])
const SELECTOR_KEYS = Object.freeze([
  'serviceUuid',
  'serviceOccurrence',
  'characteristicUuid',
  'characteristicOccurrence'
])
const FOREGROUND_SERVICE_KEYS = Object.freeze(['notification'])
const NOTIFICATION_KEYS = Object.freeze(['channelId', 'channelName', 'title', 'body', 'icon'])

const MAC_PATTERN = /^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$/
const UUID_PATTERN = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function rejectUnknownKeys(value: Record<string, unknown>, expected: readonly string[], label: string): void {
  const unknown = Object.keys(value).filter(key => !expected.includes(key))
  if (unknown.length > 0) {
    throw contractError('argument.invalid', 'restoration', `${label}.unknown-key`)
  }
}

function nonEmptyString(value: unknown, label: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw contractError('argument.invalid', 'restoration', label)
  }
  return value
}

function occurrence(value: unknown, label: string): number {
  if (value === undefined) return 1
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) {
    throw contractError('argument.invalid', 'restoration', label)
  }
  return value
}

function canonicalUuid(value: unknown, label: string): string {
  const text = nonEmptyString(value, label)
  if (!UUID_PATTERN.test(text)) {
    throw contractError('argument.invalid', 'restoration', label)
  }
  return text.toLowerCase()
}

function selector(value: unknown): BackgroundContinuationResubscribeSelector {
  if (!isPlainRecord(value)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.resubscribe.entry')
  }
  rejectUnknownKeys(value, SELECTOR_KEYS, 'background.continuation.resubscribe.entry')
  return Object.freeze({
    serviceUuid: canonicalUuid(value.serviceUuid, 'background.continuation.resubscribe.serviceUuid'),
    serviceOccurrence: occurrence(value.serviceOccurrence, 'background.continuation.resubscribe.serviceOccurrence'),
    characteristicUuid: canonicalUuid(
      value.characteristicUuid,
      'background.continuation.resubscribe.characteristicUuid'
    ),
    characteristicOccurrence: occurrence(
      value.characteristicOccurrence,
      'background.continuation.resubscribe.characteristicOccurrence'
    )
  })
}

function notification(value: unknown): BackgroundContinuationForegroundService['notification'] {
  if (!isPlainRecord(value)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.foregroundService.notification')
  }
  rejectUnknownKeys(value, NOTIFICATION_KEYS, 'background.continuation.foregroundService.notification')
  const channelId = nonEmptyString(value.channelId, 'background.continuation.foregroundService.notification.channelId')
  const channelName = nonEmptyString(
    value.channelName,
    'background.continuation.foregroundService.notification.channelName'
  )
  const title = nonEmptyString(value.title, 'background.continuation.foregroundService.notification.title')
  const body =
    value.body === undefined
      ? undefined
      : nonEmptyString(value.body, 'background.continuation.foregroundService.notification.body')
  const icon =
    value.icon === undefined
      ? undefined
      : nonEmptyString(value.icon, 'background.continuation.foregroundService.notification.icon')
  return Object.freeze({
    channelId,
    channelName,
    title,
    ...(body === undefined ? {} : { body }),
    ...(icon === undefined ? {} : { icon })
  })
}

/**
 * Normalizes a caller-authored continuation declaration. Unknown strategies
 * and unknown keys are refused — never replaced with a quieter path.
 */
export function normalizeBackgroundContinuation(input: unknown): BackgroundContinuationDeclaration {
  if (input === undefined) return DEFAULT_BACKGROUND_CONTINUATION
  if (!isPlainRecord(input)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation')
  }
  rejectUnknownKeys(input, DECLARATION_KEYS, 'background.continuation')
  const onAppearance = input.onAppearance === undefined ? 'record-only' : input.onAppearance
  if (typeof onAppearance !== 'string' || !(CONTINUATION_STRATEGIES as readonly string[]).includes(onAppearance)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.onAppearance')
  }
  const strategy = onAppearance as BackgroundContinuationStrategy
  const peerId = input.peerId === undefined ? undefined : normalizePeerAddress(input.peerId)
  const resubscribe =
    input.resubscribe === undefined ? Object.freeze([]) : Object.freeze(normalizeResubscribe(input.resubscribe))
  const headlessTaskName =
    input.headlessTaskName === undefined
      ? undefined
      : nonEmptyString(input.headlessTaskName, 'background.continuation.headlessTaskName')
  if (strategy === 'headless-task' && headlessTaskName === undefined) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.headlessTaskName.required')
  }
  if (strategy !== 'headless-task' && headlessTaskName !== undefined) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.headlessTaskName.strategy')
  }
  const foregroundService =
    input.foregroundService === undefined ? undefined : normalizeForegroundService(input.foregroundService)
  if (strategy === 'foreground-service' && foregroundService === undefined) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.foregroundService.required')
  }
  if (strategy !== 'foreground-service' && foregroundService !== undefined) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.foregroundService.strategy')
  }
  return Object.freeze({
    onAppearance: strategy,
    ...(peerId === undefined ? {} : { peerId }),
    resubscribe,
    ...(headlessTaskName === undefined ? {} : { headlessTaskName }),
    ...(foregroundService === undefined ? {} : { foregroundService })
  })
}

function normalizePeerAddress(value: unknown): string {
  const text = nonEmptyString(value, 'background.continuation.peerId')
  if (!MAC_PATTERN.test(text)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.peerId')
  }
  return text.toUpperCase()
}

function normalizeResubscribe(value: unknown): BackgroundContinuationResubscribeSelector[] {
  if (!Array.isArray(value)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.resubscribe')
  }
  if (value.length > 64) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.resubscribe.too-many')
  }
  return value.map(selector)
}

/**
 * Canonical JSON of a normalized declaration for native persistence (the
 * shape `BackgroundContinuationStore` parses). One authority: the binding
 * serializes here, never hand-builds the payload.
 */
export function serializeBackgroundContinuation(declaration: BackgroundContinuationDeclaration): string {
  return JSON.stringify({
    onAppearance: declaration.onAppearance,
    ...(declaration.peerId === undefined ? {} : { peerId: declaration.peerId }),
    resubscribe: declaration.resubscribe.map(entry => ({
      serviceUuid: entry.serviceUuid,
      serviceOccurrence: entry.serviceOccurrence,
      characteristicUuid: entry.characteristicUuid,
      characteristicOccurrence: entry.characteristicOccurrence
    })),
    ...(declaration.headlessTaskName === undefined ? {} : { headlessTaskName: declaration.headlessTaskName }),
    ...(declaration.foregroundService === undefined ? {} : { foregroundService: declaration.foregroundService })
  })
}

function normalizeForegroundService(value: unknown): BackgroundContinuationForegroundService {
  if (!isPlainRecord(value)) {
    throw contractError('argument.invalid', 'restoration', 'background.continuation.foregroundService')
  }
  rejectUnknownKeys(value, FOREGROUND_SERVICE_KEYS, 'background.continuation.foregroundService')
  if (value.notification === undefined) {
    throw contractError(
      'argument.invalid',
      'restoration',
      'background.continuation.foregroundService.notification.required'
    )
  }
  return Object.freeze({ notification: notification(value.notification) })
}
