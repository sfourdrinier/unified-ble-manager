// src/core/ordered-recording-start.ts
import { contractError } from '../backend-contract/errors'
import { parseOrderedRecordingFailure, type OrderedRecordingFailure } from './ordered-recording'
import {
  parseOrderedRecordingStopFailure,
  parseOrderedRecordingHostCleanup,
  type OrderedRecordingStopFailure
} from './continuation-recording'

export interface OrderedRecordingStartReceipt {
  readonly version: 1 | 2
  readonly recordingId: string
  readonly collectionId: string
  readonly state: 'active' | 'setup-failed' | 'failed'
  readonly sourceGeneration: { readonly kind: 'counter' | 'opaque'; readonly value: string } | null
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
    readonly delivery: 'notification' | 'indication' | 'unknown'
    readonly cleanupReleased: boolean
    readonly cleanupError: OrderedRecordingFailure | null
  }[]
  readonly setupFailure: OrderedRecordingFailure | null
  readonly fenceFailure: OrderedRecordingFailure | null
  readonly error: OrderedRecordingStopFailure | null
  readonly inspectionError: OrderedRecordingStopFailure | null
  readonly driverError: OrderedRecordingStopFailure | null
}
function bad(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.recording.start-ordered')
}
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return bad()
  return Object.fromEntries(Object.entries(value))
}
function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) bad()
}
function decimal(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^(0|[1-9][0-9]*)$/.test(value) ||
    value.length > 20 ||
    BigInt(value) > 18446744073709551615n
  )
    return bad()
  return value
}
const failure = (value: unknown, version: 1 | 2) =>
  value === null ? null : parseOrderedRecordingFailure(value, version)
const stopFailure = (value: unknown, version: 1 | 2) =>
  value === null ? null : parseOrderedRecordingStopFailure(value, version)
export function parseOrderedRecordingStartReceipt(
  input: unknown,
  recordingId: string,
  memberCount: number,
  expectedVersion: 1 | 2 = 1
): OrderedRecordingStartReceipt {
  const value = object(input)
  exact(value, [
    'version',
    'recordingId',
    'collectionId',
    'state',
    'sourceGeneration',
    'members',
    'setupFailure',
    'fenceFailure',
    'error',
    'inspectionError',
    'driverError'
  ])
  if (
    (expectedVersion !== 1 && expectedVersion !== 2) ||
    value.version !== expectedVersion ||
    value.recordingId !== recordingId ||
    (value.state !== 'active' && value.state !== 'setup-failed' && value.state !== 'failed') ||
    !Array.isArray(value.members)
  )
    return bad()
  const inspectionError = stopFailure(value.inspectionError, expectedVersion)
  if (value.members.length !== memberCount && !(value.members.length === 0 && inspectionError !== null)) return bad()
  const members = value.members.map((item, index) => {
    const m = object(item)
    exact(m, ['memberId', 'acquisition', 'delivery', 'cleanupReleased', 'cleanupError'])
    if (
      decimal(m.memberId) !== String(index) ||
      (m.acquisition !== 'not-started' && m.acquisition !== 'acquired' && m.acquisition !== 'indeterminate') ||
      (m.delivery !== 'notification' && m.delivery !== 'indication' && m.delivery !== 'unknown') ||
      typeof m.cleanupReleased !== 'boolean'
    )
      return bad()
    return Object.freeze({
      memberId: String(index),
      acquisition: m.acquisition,
      delivery: m.delivery,
      cleanupReleased: m.cleanupReleased,
      cleanupError: failure(m.cleanupError, expectedVersion)
    })
  })
  let sourceGeneration: OrderedRecordingStartReceipt['sourceGeneration'] = null
  if (value.sourceGeneration !== null) {
    const generation = object(value.sourceGeneration)
    exact(generation, ['kind', 'value'])
    if (
      (generation.kind !== 'counter' && generation.kind !== 'opaque') ||
      typeof generation.value !== 'string' ||
      !generation.value.length ||
      generation.value.length > 256
    )
      return bad()
    sourceGeneration = Object.freeze({
      kind: generation.kind,
      value: generation.kind === 'counter' ? decimal(generation.value) : generation.value
    })
  }
  return Object.freeze({
    version: expectedVersion,
    recordingId,
    collectionId: decimal(value.collectionId),
    state: value.state,
    sourceGeneration,
    members: Object.freeze(members),
    setupFailure: failure(value.setupFailure, expectedVersion),
    fenceFailure: failure(value.fenceFailure, expectedVersion),
    error: stopFailure(value.error, expectedVersion),
    inspectionError,
    driverError: stopFailure(value.driverError, expectedVersion)
  })
}

export type OrderedRecordingStartResult =
  | { readonly kind: 'started'; readonly receipt: OrderedRecordingStartReceipt }
  | {
      readonly kind: 'prepare-failed'
      readonly leaseCleanup?: import('../backend-contract/errors').CleanupRecord | { readonly state: 'pending' }
      readonly receipt: {
        readonly version: 1 | 2
        readonly state: 'prepare-failed'
        readonly recordingId: string
        readonly sourceStarted: false
        readonly journalEffect: 'none' | 'unknown'
        readonly error: OrderedRecordingStopFailure
      }
    }
  | {
      readonly kind: 'owner-failed'
      readonly recordingId: string
      readonly collectionId: string
      readonly error: OrderedRecordingStopFailure
    }

export function parseOrderedRecordingStartResult(
  input: unknown,
  recordingId: string,
  memberCount: number,
  expectedVersion: 1 | 2 = 1
): OrderedRecordingStartResult {
  if (expectedVersion !== 1 && expectedVersion !== 2) return bad()
  const value = object(input)
  if (value.kind === 'started') {
    exact(value, ['kind', 'receipt'])
    return Object.freeze({
      kind: value.kind,
      receipt: parseOrderedRecordingStartReceipt(value.receipt, recordingId, memberCount, expectedVersion)
    })
  }
  if (value.kind === 'prepare-failed') {
    exact(value, Object.hasOwn(value, 'leaseCleanup') ? ['kind', 'receipt', 'leaseCleanup'] : ['kind', 'receipt'])
    const leaseCleanup = Object.hasOwn(value, 'leaseCleanup')
      ? parseOrderedRecordingHostCleanup(value.leaseCleanup)
      : undefined
    if (leaseCleanup?.state === 'not-applicable' || leaseCleanup?.state === 'uncertain') return bad()
    const receipt = object(value.receipt)
    exact(receipt, ['version', 'state', 'recordingId', 'sourceStarted', 'journalEffect', 'error'])
    if (
      receipt.version !== expectedVersion ||
      receipt.state !== 'prepare-failed' ||
      receipt.recordingId !== recordingId ||
      receipt.sourceStarted !== false ||
      (receipt.journalEffect !== 'none' && receipt.journalEffect !== 'unknown')
    )
      return bad()
    return Object.freeze({
      kind: value.kind,
      ...(leaseCleanup ? { leaseCleanup } : {}),
      receipt: Object.freeze({
        version: expectedVersion,
        state: 'prepare-failed',
        recordingId,
        sourceStarted: false,
        journalEffect: receipt.journalEffect,
        error: parseOrderedRecordingStopFailure(receipt.error, expectedVersion)
      })
    })
  }
  if (value.kind === 'owner-failed') {
    exact(value, ['kind', 'recordingId', 'collectionId', 'error'])
    if (value.recordingId !== recordingId) return bad()
    return Object.freeze({
      kind: value.kind,
      recordingId,
      collectionId: decimal(value.collectionId),
      error: parseOrderedRecordingStopFailure(value.error, expectedVersion)
    })
  }
  return bad()
}
