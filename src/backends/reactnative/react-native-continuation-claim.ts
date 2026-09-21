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
  type BackgroundContinuationDeclaration,
  type BackgroundContinuationResubscribeSelector
} from '../../backend-contract/background-continuation'
import { contractError } from '../../backend-contract/errors'
import { parseDrainText, type WireDrainRecord, type WireDelivery } from './rust-core-wire'

/** The native claim shape (`sessions.claimContinuation`): verbatim batches. */
export interface ContinuationClaimPayload {
  readonly batches: readonly string[]
  readonly disposed: boolean
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
  readonly values: readonly ContinuationBacklogValue[]
  readonly streamEnds: readonly ContinuationBacklogStreamEnd[]
  /** Other control records (link, db-changed, restored, …) for the caller to reconcile. */
  readonly control: readonly WireDrainRecord[]
  /** Cumulative control loss: an increase means run `session.reconcile`. */
  readonly controlLost: number
  readonly disposed: boolean
}

function assertClaimPayload(value: unknown): asserts value is ContinuationClaimPayload {
  if (!isRecord(value)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.payload')
  }
  const batches = value.batches
  const disposed = value.disposed
  // Empty batches are the valid no-wake answer (no continuation session alive).
  if (!Array.isArray(batches) || batches.some(batch => typeof batch !== 'string')) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.batches')
  }
  if (typeof disposed !== 'boolean') {
    throw contractError('protocol.malformed', 'restoration', 'continuation-claim.disposed')
  }
}

/**
 * Maps a backlog consumer to the declared selector that subscribed it
 * (`ubm-continuation-{index}` → `resubscribe[index]`), or null when the
 * consumer is not a wake subscription.
 */
export function continuationConsumerSelector(
  consumer: string,
  declaration: BackgroundContinuationDeclaration
): BackgroundContinuationResubscribeSelector | null {
  if (!consumer.startsWith(CONTINUATION_CONSUMER_PREFIX)) return null
  const index = Number(consumer.slice(CONTINUATION_CONSUMER_PREFIX.length))
  if (!Number.isSafeInteger(index) || index < 0) return null
  return declaration.resubscribe[index] ?? null
}

/** One wake outcome in the status answer (null fields stay null, never invented). */
export interface ContinuationWakeStatus {
  readonly observedAtMs: number
  readonly event: 'continuation.completed' | 'continuation.failed'
  readonly strategy: string
  readonly peerAddress: string | null
  readonly code: string | null
  readonly reason: string | null
}

/** The continuation posture (`sessions.continuationStatus`). */
export interface ContinuationStatus {
  readonly strategy: string
  readonly peerId: string | null
  readonly resubscribe: number
  readonly malformedDeclarations: number
  readonly lastWake: ContinuationWakeStatus | null
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
    ['strategy', 'peerId', 'resubscribe', 'malformedDeclarations', 'lastWake'],
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
    typeof parsed.malformedDeclarations !== 'number' ||
    !Number.isSafeInteger(parsed.malformedDeclarations)
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.counts')
  }
  return Object.freeze({
    strategy: parsed.strategy,
    peerId: parsed.peerId,
    resubscribe: parsed.resubscribe,
    malformedDeclarations: parsed.malformedDeclarations,
    lastWake: parseWakeStatus(parsed.lastWake)
  })
}

function parseWakeStatus(value: unknown): ContinuationWakeStatus | null {
  if (value === null || value === undefined) return null
  if (!isRecord(value)) {
    throw contractError('protocol.malformed', 'restoration', 'continuation-status.last-wake')
  }
  unexpectedKeys(
    value,
    ['observedAtMs', 'event', 'strategy', 'peerAddress', 'code', 'reason'],
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
  return Object.freeze({
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
function assertDeclaredConsumer(consumer: string, declaration: BackgroundContinuationDeclaration): void {
  if (continuationConsumerSelector(consumer, declaration) === null) {
    throw contractError('protocol.violation', 'restoration', 'continuation-claim.undeclared-consumer')
  }
}

/** Parses and aggregates one native claim; fails closed on any gap. */
export function aggregateContinuationClaim(
  claim: unknown,
  declaration: BackgroundContinuationDeclaration
): ContinuationBacklog {
  assertClaimPayload(claim)
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
        assertDeclaredConsumer(record.consumer, declaration)
        values.push(Object.freeze({ consumer: record.consumer, value: record.value, delivery: record.delivery }))
      } else if (record.t === 'stream-end') {
        assertDeclaredConsumer(record.consumer, declaration)
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
    values: Object.freeze(values),
    streamEnds: Object.freeze(streamEnds),
    control: Object.freeze(control),
    controlLost,
    disposed: claim.disposed
  })
}
