// src/backends/reactnative/react-native-continuation-claim.ts
//
// Drains the continuation backlog the native wake queued with no JavaScript
// (BGS4). The native claim returns verbatim drain batches; this module parses
// them through the existing drain codec (`parseDrainText`, chained ordinals)
// and aggregates values plus loss accounting — stream-end carries
// droppedItems/droppedBytes, controlLost is cumulative and tells the reader
// to run `session.reconcile` instead of inferring. A broken chain or a
// malformed batch fails closed: no partial backlog is ever delivered as if
// complete.

import {
  CONTINUATION_CONSUMER_PREFIX,
  type BackgroundContinuationResubscribeSelector
} from '../../backend-contract/background-continuation'
import { BLE_RETRYABILITIES, contractError, type BleRetryability } from '../../backend-contract/errors'
import { parseContinuationLinkOutcome, type ContinuationLinkOutcome } from '../../core/native-continuation-link'
import {
  parseDrainText,
  parseRemoteFailureText,
  type WireRemoteFailure,
  type WirePlatformDetail,
  type WireDrainRecord,
  type WireDelivery
} from './rust-core-wire'

/** The native prepared-claim shape (`sessions.prepareContinuationClaim`): verbatim batches. */
export interface ContinuationClaimPayload {
  readonly recording?: { readonly id: string }
  /** Opaque native ownership token; it is acknowledged only after this payload decodes. */
  readonly claimToken: string
  /** Consumer count captured from the exact native session being claimed. */
  readonly consumerCount: number
  /** Immutable selector identity for each numeric continuation consumer. */
  readonly selectors: readonly BackgroundContinuationResubscribeSelector[]
  readonly batches: readonly string[]
  readonly disposed: boolean
  /** Data admissions observed after the native cutoff, never hidden as backlog. */
  readonly afterCutoffLoss: { readonly items: number; readonly bytes: number }
  /**
   * Why the session is still alive (absent or null when disposed or when no
   * wake existed): a release-failed dispose or an incomplete drain the next
   * claim retries, never an abandoned session.
   */
  readonly disposeFailure?: string | null
}

export interface ContinuationBacklogValue {
  readonly consumer: string
  readonly value: Uint8Array
  readonly delivery: WireDelivery
}

export interface ContinuationBacklogStreamEnd {
  readonly consumer: string
  readonly reason: 'overflow' | 'invalidated' | 'closed'
  readonly droppedItems: number
  readonly droppedBytes: number
}

export interface ContinuationBacklog {
  /** Separate durable cursor: claiming radio ownership never acknowledges or deletes this recording. */
  readonly recording?: { readonly id: string }
  /** Immutable selector identity from the session that produced this backlog. */
  readonly selectors: readonly BackgroundContinuationResubscribeSelector[]
  readonly values: readonly ContinuationBacklogValue[]
  readonly streamEnds: readonly ContinuationBacklogStreamEnd[]
  /** Other control records (link, db-changed, restored, …) for the caller to reconcile. */
  readonly control: readonly WireDrainRecord[]
  /** Cumulative control loss: an increase means run `session.reconcile`. */
  readonly controlLost: number
  /** Native intake observed after the handoff cutoff. */
  readonly afterCutoffLoss: { readonly items: number; readonly bytes: number }
  readonly disposed: boolean
  /** Why the session is still alive (null when disposed or no wake existed). */
  readonly disposeFailure: string | null
}

export interface ContinuationClaimAcknowledgement {
  readonly disposed: boolean
  readonly afterCutoffLoss: { readonly items: number; readonly bytes: number }
  readonly disposeFailure: string | null
}

function assertClaimPayload(value: unknown): asserts value is ContinuationClaimPayload {
  if (!isRecord(value)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.payload')
  }
  if (
    value.recording !== undefined &&
    (!isRecord(value.recording) ||
      Object.keys(value.recording).length !== 1 ||
      typeof value.recording.id !== 'string' ||
      !/^[A-Za-z0-9_-]{1,64}$/.test(value.recording.id))
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.recording')
  }
  const batches = value.batches
  const claimToken = value.claimToken
  const disposed = value.disposed
  const consumerCount = value.consumerCount
  const selectors = value.selectors
  const afterCutoffLoss = value.afterCutoffLoss
  // Empty batches are the valid no-wake answer (no continuation session alive).
  if (!Array.isArray(batches) || batches.some(batch => typeof batch !== 'string')) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.batches')
  }
  if (
    claimToken !== undefined &&
    (typeof claimToken !== 'string' || claimToken.length === 0 || claimToken.length > 256)
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.token')
  }
  if (typeof consumerCount !== 'number' || !Number.isSafeInteger(consumerCount) || consumerCount < 0) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.consumer-count')
  }
  if (
    !Array.isArray(selectors) ||
    selectors.length !== consumerCount ||
    selectors.some(selector => !isSelector(selector))
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.selectors')
  }
  if (typeof disposed !== 'boolean') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.disposed')
  }
  if (
    !isRecord(afterCutoffLoss) ||
    Object.keys(afterCutoffLoss).length !== 2 ||
    !Object.prototype.hasOwnProperty.call(afterCutoffLoss, 'items') ||
    !Object.prototype.hasOwnProperty.call(afterCutoffLoss, 'bytes') ||
    typeof afterCutoffLoss.items !== 'number' ||
    !Number.isSafeInteger(afterCutoffLoss.items) ||
    afterCutoffLoss.items < 0 ||
    typeof afterCutoffLoss.bytes !== 'number' ||
    !Number.isSafeInteger(afterCutoffLoss.bytes) ||
    afterCutoffLoss.bytes < 0
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.after-cutoff-loss')
  }
  const disposeFailure = value.disposeFailure
  if (disposeFailure !== undefined && disposeFailure !== null && typeof disposeFailure !== 'string') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.dispose-failure')
  }
}

/** Reads the opaque prepared-claim token after applying the same strict shape check as aggregation. */
export function continuationClaimToken(value: unknown): string {
  assertClaimPayload(value)
  if (typeof value.claimToken !== 'string' || value.claimToken.length === 0) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.token')
  }
  return value.claimToken
}

/** The no-wake answer has no token; every native prepared handoff has one. */
export function optionalContinuationClaimToken(value: unknown): string | null {
  assertClaimPayload(value)
  return typeof value.claimToken === 'string' ? value.claimToken : null
}

/** Validates the cleanup answer separately so a decode failure cannot authorize it. */
export function parseContinuationClaimAcknowledgement(value: unknown): ContinuationClaimAcknowledgement {
  if (!isRecord(value)) throw contractError('protocol.malformed', 'restoration', 'continuation-claim.ack')
  unexpectedKeys(value, ['disposed', 'afterCutoffLoss', 'disposeFailure'], 'continuation-claim.ack.keys')
  const afterCutoffLoss = value.afterCutoffLoss
  const items = isRecord(afterCutoffLoss) ? afterCutoffLoss.items : undefined
  const bytes = isRecord(afterCutoffLoss) ? afterCutoffLoss.bytes : undefined
  if (
    typeof value.disposed !== 'boolean' ||
    !isRecord(afterCutoffLoss) ||
    Object.keys(afterCutoffLoss).length !== 2 ||
    typeof items !== 'number' ||
    !Number.isSafeInteger(items) ||
    items < 0 ||
    typeof bytes !== 'number' ||
    !Number.isSafeInteger(bytes) ||
    bytes < 0 ||
    (value.disposeFailure !== null && typeof value.disposeFailure !== 'string')
  )
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.ack')
  return Object.freeze({
    disposed: value.disposed,
    afterCutoffLoss: Object.freeze({ items, bytes }),
    disposeFailure: value.disposeFailure
  })
}

/**
 * Maps a backlog consumer to the immutable selector list returned by the
 * exact native session that subscribed it, or null when the consumer is not
 * a wake subscription.
 */
export function continuationConsumerSelector(
  consumer: string,
  selectors: readonly BackgroundContinuationResubscribeSelector[]
): BackgroundContinuationResubscribeSelector | null {
  const index = continuationConsumerIndex(consumer)
  if (index === null) return null
  return selectors[index] ?? null
}

function isSelector(value: unknown): value is BackgroundContinuationResubscribeSelector {
  if (!isRecord(value)) return false
  const keys = Object.keys(value)
  const serviceOccurrence = value.serviceOccurrence
  const characteristicOccurrence = value.characteristicOccurrence
  if (
    keys.length !== 4 ||
    !keys.includes('serviceUuid') ||
    !keys.includes('serviceOccurrence') ||
    !keys.includes('characteristicUuid') ||
    !keys.includes('characteristicOccurrence')
  ) {
    return false
  }
  return (
    typeof value.serviceUuid === 'string' &&
    typeof value.characteristicUuid === 'string' &&
    typeof serviceOccurrence === 'number' &&
    Number.isSafeInteger(serviceOccurrence) &&
    serviceOccurrence > 0 &&
    typeof characteristicOccurrence === 'number' &&
    Number.isSafeInteger(characteristicOccurrence) &&
    characteristicOccurrence > 0
  )
}

function continuationConsumerIndex(consumer: string): number | null {
  if (!consumer.startsWith(CONTINUATION_CONSUMER_PREFIX)) return null
  const index = Number(consumer.slice(CONTINUATION_CONSUMER_PREFIX.length))
  return Number.isSafeInteger(index) && index >= 0 ? index : null
}

/** One wake outcome in the status answer (null fields stay null, never invented). */
export interface ContinuationWakeStatus {
  /** Acceptance stage, never proof the dispatched application task completed. */
  readonly stage?: 'task-dispatched' | 'foreground-service-started'
  readonly platform?: WirePlatformDetail
  readonly observedAtMs: number
  readonly event: 'continuation.completed' | 'continuation.failed'
  readonly strategy: string
  readonly peerAddress: string | null
  readonly code: string | null
  readonly reason: string | null
}

/** Latest autonomous recovery attempt, distinct from the OS wake that started collection. */
export type ContinuationRecoveryStatus =
  | {
      readonly event: 'continuation.completed'
      readonly strategy: 'native'
      readonly attempt: number
      readonly peerAddress: string
      readonly resubscribed: number
      readonly link?: ContinuationLinkOutcome
    }
  | {
      readonly event: 'continuation.failed'
      readonly strategy: 'native'
      readonly attempt: number
      readonly error: WireRemoteFailure
      readonly retryability: BleRetryability
    }

/** The continuation posture (`sessions.continuationStatus`). */
export interface ContinuationStatus {
  readonly strategy: string
  readonly peerId: string | null
  readonly resubscribe: number
  readonly malformedDeclarations: number
  readonly lastWake: ContinuationWakeStatus | null
  readonly lastRecovery: ContinuationRecoveryStatus | null
  /** Native accessory-startup refusal, not a wake/recovery outcome; absent on older hosts. */
  readonly startupFailure: WireRemoteFailure | null
  /**
   * The host's reason a declared mechanism is unavailable (for example an
   * Android-specific task/service strategy on Apple); null where it executes.
   */
  readonly detail: string | null
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function unexpectedKeys(value: Record<string, unknown>, expected: readonly string[], operation: string): void {
  const unknown = Object.keys(value).filter(key => !expected.includes(key))
  if (unknown.length > 0) {
    throw contractError('protocol.malformed', 'restoration', operation)
  }
}

/** Parses one native status answer; native drift is refused, never guessed. */
export function parseContinuationStatus(value: unknown): ContinuationStatus {
  let parsed: unknown = value
  if (typeof parsed === 'string') {
    try {
      parsed = JSON.parse(parsed)
    } catch {
      throw contractError('protocol.malformed', 'restoration', 'continuation-status.json')
    }
  }
  if (!isRecord(parsed)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.shape')
  }
  unexpectedKeys(
    parsed,
    [
      'strategy',
      'peerId',
      'resubscribe',
      'malformedDeclarations',
      'lastWake',
      'lastRecovery',
      'startupFailure',
      'detail'
    ],
    'continuation-status.keys'
  )
  if (typeof parsed.strategy !== 'string' || parsed.strategy.length === 0) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.strategy')
  }
  if (parsed.peerId !== null && typeof parsed.peerId !== 'string') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.peerId')
  }
  if (
    typeof parsed.resubscribe !== 'number' ||
    !Number.isSafeInteger(parsed.resubscribe) ||
    parsed.resubscribe < 0 ||
    typeof parsed.malformedDeclarations !== 'number' ||
    !Number.isSafeInteger(parsed.malformedDeclarations) ||
    parsed.malformedDeclarations < 0
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.counts')
  }
  const detail = parsed.detail ?? null
  if (detail !== null && typeof detail !== 'string') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.detail')
  }
  const startupFailure =
    parsed.startupFailure == null
      ? null
      : parseRemoteFailureText(JSON.stringify(parsed.startupFailure), 'continuation-status.startup-failure')
  if (startupFailure !== null && !startupFailure.ok) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.startup-failure')
  }
  return Object.freeze({
    strategy: parsed.strategy,
    peerId: parsed.peerId,
    resubscribe: parsed.resubscribe,
    malformedDeclarations: parsed.malformedDeclarations,
    lastWake: parseWakeStatus(parsed.lastWake),
    lastRecovery: parseContinuationRecoveryStatus(parsed.lastRecovery),
    startupFailure: startupFailure === null ? null : startupFailure.value,
    detail
  })
}

/** Shared native mobile/desktop recovery diagnostic decoder. */
export function parseContinuationRecoveryStatus(value: unknown): ContinuationRecoveryStatus | null {
  if (value === null || value === undefined) return null
  const operation = 'continuation-status.last-recovery'
  if (
    !isRecord(value) ||
    value.strategy !== 'native' ||
    typeof value.attempt !== 'number' ||
    !Number.isSafeInteger(value.attempt) ||
    value.attempt < 1
  ) {
    throw contractError('protocol.malformed', 'restoration', operation)
  }
  if (value.event === 'continuation.completed') {
    unexpectedKeys(value, ['event', 'strategy', 'attempt', 'peerAddress', 'resubscribed', 'link'], operation)
    if (
      typeof value.peerAddress !== 'string' ||
      value.peerAddress.length === 0 ||
      typeof value.resubscribed !== 'number' ||
      !Number.isSafeInteger(value.resubscribed) ||
      value.resubscribed < 0
    ) {
      throw contractError('protocol.malformed', 'restoration', operation)
    }
    const link = parseContinuationLinkOutcome(value.link)
    return Object.freeze({
      event: value.event,
      strategy: value.strategy,
      attempt: value.attempt,
      peerAddress: value.peerAddress,
      resubscribed: value.resubscribed,
      ...(link === undefined ? {} : { link })
    })
  }
  if (value.event === 'continuation.failed') {
    unexpectedKeys(value, ['event', 'strategy', 'attempt', 'error', 'retryability'], operation)
    const retryability = BLE_RETRYABILITIES.find(candidate => candidate === value.retryability)
    const error = parseRemoteFailureText(JSON.stringify(value.error), operation)
    if (retryability === undefined || !error.ok) throw contractError('protocol.malformed', 'restoration', operation)
    return Object.freeze({
      event: value.event,
      strategy: value.strategy,
      attempt: value.attempt,
      error: error.value,
      retryability
    })
  }
  throw contractError('protocol.malformed', 'restoration', operation)
}

function parseWakeStatus(value: unknown): ContinuationWakeStatus | null {
  if (value === null || value === undefined) return null
  if (!isRecord(value)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake')
  }
  unexpectedKeys(
    value,
    ['observedAtMs', 'event', 'strategy', 'peerAddress', 'code', 'reason', 'stage', 'platform'],
    'continuation-status.last-wake.keys'
  )
  if (typeof value.observedAtMs !== 'number' || !Number.isSafeInteger(value.observedAtMs)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.time')
  }
  if (value.event !== 'continuation.completed' && value.event !== 'continuation.failed') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.event')
  }
  if (typeof value.strategy !== 'string') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.strategy')
  }
  if (
    (value.peerAddress !== null && typeof value.peerAddress !== 'string') ||
    (value.code !== null && typeof value.code !== 'string') ||
    (value.reason !== null && typeof value.reason !== 'string')
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.detail')
  }
  const expectedStage =
    value.event !== 'continuation.completed'
      ? undefined
      : value.strategy === 'headless-task'
        ? 'task-dispatched'
        : value.strategy === 'foreground-service'
          ? 'foreground-service-started'
          : undefined
  if (value.stage !== expectedStage) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.stage')
  }
  let platform: WirePlatformDetail | undefined
  if (value.platform !== undefined) {
    const parsed = parseRemoteFailureText(
      JSON.stringify({
        code: 'platform.failure',
        domain: 'platform',
        operation: 'continuation.wake',
        detail: null,
        platform: value.platform
      }),
      'continuation.wake.platform'
    )
    if (value.event !== 'continuation.failed' || !parsed.ok || parsed.value.platform === null) {
      throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake.platform')
    }
    platform = parsed.value.platform
  }
  return Object.freeze({
    ...(expectedStage === undefined ? {} : { stage: expectedStage }),
    ...(platform === undefined ? {} : { platform }),
    observedAtMs: value.observedAtMs,
    event: value.event,
    strategy: value.strategy,
    peerAddress: value.peerAddress,
    code: value.code,
    reason: value.reason
  })
}

/**
 * The wake subscribes exactly the declared selectors, so a value or
 * stream-end for any other consumer is native/JS declaration drift —
 * refused, never merged quietly.
 */
function assertDeclaredConsumer(consumer: string, declaredConsumerCount: number): void {
  const index = continuationConsumerIndex(consumer)
  if (index === null || index >= declaredConsumerCount) {
    throw contractError('protocol.violation', 'restoration', 'continuation-claim.undeclared-consumer')
  }
}

/** Parses and aggregates one native claim; fails closed on any gap. */
export function aggregateContinuationClaim(claim: unknown): ContinuationBacklog {
  assertClaimPayload(claim)
  const declaredConsumerCount = claim.consumerCount
  const selectors = Object.freeze(
    claim.selectors.map(selector =>
      Object.freeze({
        serviceUuid: selector.serviceUuid,
        serviceOccurrence: selector.serviceOccurrence,
        characteristicUuid: selector.characteristicUuid,
        characteristicOccurrence: selector.characteristicOccurrence
      })
    )
  )
  const values: ContinuationBacklogValue[] = []
  const streamEnds: ContinuationBacklogStreamEnd[] = []
  const control: WireDrainRecord[] = []
  let controlLost = 0
  let lastOrdinal: number | null = null
  for (const text of claim.batches) {
    const parsed = parseDrainText(text, lastOrdinal)
    if (!parsed.ok) throw parsed.error
    const batch = parsed.value
    if (batch.controlLost < controlLost) {
      throw contractError('protocol.violation', 'restoration', 'continuation-claim.control-lost-regressed')
    }
    controlLost = batch.controlLost
    for (const record of batch.records) {
      if (record.t === 'value') {
        assertDeclaredConsumer(record.consumer, declaredConsumerCount)
        values.push(Object.freeze({ consumer: record.consumer, value: record.value, delivery: record.delivery }))
      } else if (record.t === 'stream-end') {
        assertDeclaredConsumer(record.consumer, declaredConsumerCount)
        streamEnds.push(
          Object.freeze({
            consumer: record.consumer,
            reason: record.reason,
            droppedItems: record.droppedItems,
            droppedBytes: record.droppedBytes
          })
        )
      } else {
        control.push(record)
      }
    }
    const last = batch.records[batch.records.length - 1]
    if (last !== undefined) lastOrdinal = last.ordinal
  }
  return Object.freeze({
    selectors,
    ...(claim.recording === undefined ? {} : { recording: Object.freeze({ id: claim.recording.id }) }),
    values: Object.freeze(values),
    streamEnds: Object.freeze(streamEnds),
    control: Object.freeze(control),
    controlLost,
    afterCutoffLoss: Object.freeze({ items: claim.afterCutoffLoss.items, bytes: claim.afterCutoffLoss.bytes }),
    disposed: claim.disposed,
    disposeFailure: claim.disposeFailure ?? null
  })
}
