// src/core/ordered-recording.ts

import {
  assertPlatformFailureEnvelopeBound,
  contractError,
  snapshotPlatformMetadataEntries,
  type PlatformMetadataEntry
} from '../backend-contract/errors'
import { utf8ByteLength } from '../backend-contract/serializable'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import { MAX_RECORDING_TOKEN_BYTES } from '../backend-contract/continuation-recording-bounds'

export interface OrderedRecordingMembership {
  readonly member: string
  readonly epoch: string
}
export type OrderedRecordingScope =
  | { readonly kind: 'member'; readonly member: string }
  | { readonly kind: 'group' | 'unknown' }
export interface OrderedRecordingLoss {
  readonly scope: OrderedRecordingScope
  readonly unit: 'values' | 'source-records'
  readonly count: string | null
  readonly bytes: string | null
}
export type OrderedRecordingFailureValue =
  | { readonly kind: 'int' | 'text'; readonly value: string }
  | { readonly kind: 'bool'; readonly value: boolean }
export interface OrderedRecordingFailure {
  readonly code: string
  readonly domain: string
  readonly operation: string
  readonly detail: string | null
  readonly platformDomain: string | null
  readonly platformCode: string | null
  readonly platformMessage: string | null
  readonly platformMetadata: Readonly<Record<string, OrderedRecordingFailureValue>>
  readonly platformMetadataEntries?: readonly PlatformMetadataEntry[]
  readonly platformCleanup?: readonly OrderedRecordingPlatformDetail[]
  readonly platformCleanupFailures?: readonly OrderedRecordingFailure[]
}
/** Independent native platform cleanup detail, preserving nested causes. */
export interface OrderedRecordingPlatformDetail {
  readonly domain: string
  readonly code: string
  readonly message: string | null
  readonly metadata: Readonly<Record<string, OrderedRecordingFailureValue>>
  readonly metadataEntries?: readonly PlatformMetadataEntry[]
  readonly cleanup?: readonly OrderedRecordingPlatformDetail[]
  readonly cleanupFailures?: readonly OrderedRecordingFailure[]
}
export type OrderedRecordingRejection =
  | { readonly kind: 'value' | 'ordinal-exhausted'; readonly bytes: string }
  | { readonly kind: 'loss'; readonly skippedRecords: string }
  | { readonly kind: 'fence'; readonly membership: OrderedRecordingMembership }
  | { readonly kind: 'barrier'; readonly id: string }
  | { readonly kind: 'marker'; readonly markerId: string }
export type OrderedRecordingReason =
  | { readonly kind: 'setup-failed' | 'source-failed' | 'host-reset'; readonly failure: OrderedRecordingFailure }
  | {
      readonly kind:
        | 'closed'
        | 'invalidated'
        | 'service-changed'
        | 'connection-lost'
        | 'owner-released'
        | 'capacity-exceeded'
        | 'ordinal-invalid'
        | 'source-overflow'
        | 'source-ordinal-exhausted'
    }
export type OrderedRecordingRecord =
  | {
      readonly kind: 'value'
      readonly ordinal: string
      readonly member: string
      readonly membership: OrderedRecordingMembership
      readonly bytes: Uint8Array
      readonly delivery: 'notification' | 'indication' | 'unknown'
      readonly provenance: 'notification' | 'read-or-notification' | 'unknown'
    }
  | { readonly kind: 'marker'; readonly markerId: string; readonly ordinal: string }
  | ({ readonly kind: 'loss' } & OrderedRecordingLoss)
  | {
      readonly kind: 'failure-observation'
      readonly failure: OrderedRecordingFailure | null
      readonly sourceRejection: OrderedRecordingRejection | null
      readonly sourceFailureMember: OrderedRecordingMembership | null
      readonly rejectedLoss: OrderedRecordingLoss | null
      readonly tailUncertain: boolean
    }
  | { readonly kind: 'failure-observations-omitted'; readonly count: string | null }
  | { readonly kind: 'terminal'; readonly reason: OrderedRecordingReason; readonly tailUncertain: boolean }
  | {
      readonly kind: 'terminal'
      readonly reason: OrderedRecordingReason
      readonly tailUncertain: boolean
      readonly rejectedValues: string | null
      readonly rejectedLoss: OrderedRecordingLoss | null
      readonly sourceRejection: OrderedRecordingRejection | null
      readonly sourceFailureMember: OrderedRecordingMembership | null
      readonly secondaryFailure: OrderedRecordingFailure | null
      readonly additionalFailureCount: string
      readonly failureSummaryVersion?: 1
      readonly failureCountOverflowed?: boolean
      readonly failureObservationsOmitted?: boolean
    }
  | {
      readonly kind: 'setup-terminal'
      readonly sourceCreated: false
      readonly tailUncertain: false
      readonly failure: OrderedRecordingFailure
      readonly setupFailure: OrderedRecordingFailure | null
      readonly fenceFailure: OrderedRecordingFailure | null
      readonly members: readonly {
        readonly member: string
        readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
        readonly delivery: 'notification' | 'indication' | 'unknown'
        readonly cleanupReleased: boolean
        readonly cleanupError: OrderedRecordingFailure | null
      }[]
    }
export interface OrderedRecordingMetadata {
  readonly kind: 'ordered'
  readonly version: 1 | 2
  readonly recordingId: string
  readonly peerId: string
  readonly collectionId: string
  readonly sourceGeneration: { readonly kind: 'counter' | 'opaque'; readonly value: string } | null
  readonly deliveryId: string | null
}
export interface OrderedRecordingBatch {
  readonly token: string | null
  readonly records: readonly {
    readonly ordinal: number
    readonly metadata: OrderedRecordingMetadata
    readonly record: OrderedRecordingRecord
  }[]
  readonly bytes: number
  readonly more: boolean
}

function bad(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.recording.ordered')
}
function obj(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return bad()
  return Object.fromEntries(Object.entries(value))
}
function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) bad()
}
function text(value: unknown, max = 65536): string {
  if (typeof value !== 'string' || value.length === 0 || value.length > max) return bad()
  return value
}
function optionalText(value: unknown): string | null {
  if (value === null) return null
  if (typeof value !== 'string' || value.length > 65536) return bad()
  return value
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return bad()
  return value
}
function optionalDecimal(value: unknown): string | null {
  return value === null ? null : decimal(value)
}
function bool(value: unknown): boolean {
  if (typeof value !== 'boolean') return bad()
  return value
}
function number(value: unknown, min: number, max: number): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < min || value > max) return bad()
  return value
}
function membership(input: unknown): OrderedRecordingMembership {
  const value = obj(input)
  exact(value, ['member', 'epoch'])
  return Object.freeze({ member: decimal(value.member), epoch: decimal(value.epoch) })
}
function scope(input: unknown): OrderedRecordingScope {
  const value = obj(input)
  if (value.kind === 'member') {
    exact(value, ['kind', 'member'])
    return Object.freeze({ kind: 'member', member: decimal(value.member) })
  }
  if (value.kind !== 'group' && value.kind !== 'unknown') return bad()
  exact(value, ['kind'])
  return Object.freeze({ kind: value.kind })
}
function lossFields(value: Record<string, unknown>): OrderedRecordingLoss {
  if (value.unit !== 'values' && value.unit !== 'source-records') return bad()
  return {
    scope: scope(value.scope),
    unit: value.unit,
    count: optionalDecimal(value.count),
    bytes: optionalDecimal(value.bytes)
  }
}
function loss(input: unknown): OrderedRecordingLoss {
  const value = obj(input)
  exact(value, ['scope', 'unit', 'count', 'bytes'])
  return Object.freeze(lossFields(value))
}
// Rust's v2 format bounds the combined tree at 32 edges and 1 MiB.
const MAX_FAILURE_DEPTH = 32
function boundedText(value: unknown, allowEmpty: boolean): string {
  if (
    typeof value !== 'string' ||
    (!allowEmpty && value.length === 0) ||
    value.length > 65536 ||
    utf8ByteLength(value) > 65536
  )
    return bad()
  return value
}
function parseMetadata(
  input: unknown,
  version: 1 | 2,
  hasEntries: boolean
): Readonly<Record<string, OrderedRecordingFailureValue>> {
  const raw = obj(input)
  const entries: [string, OrderedRecordingFailureValue][] = []
  for (const [key, item] of Object.entries(raw)) {
    if (key.length > 65536 || (version === 2 && utf8ByteLength(key) > 65536)) return bad()
    const v = obj(item)
    exact(v, ['kind', 'value'])
    const safeKey = key !== '__proto__' && key !== 'constructor' && key !== 'prototype'
    if (!safeKey && !hasEntries) return bad()
    let result: OrderedRecordingFailureValue
    if (v.kind === 'bool') result = Object.freeze({ kind: 'bool', value: bool(v.value) })
    else if (v.kind === 'int') {
      const n = text(v.value, 20)
      if (!/^(0|-?[1-9][0-9]*)$/.test(n) || BigInt(n) < -9223372036854775808n || BigInt(n) > 9223372036854775807n)
        return bad()
      result = Object.freeze({ kind: 'int', value: n })
    } else if (v.kind === 'text') {
      if (typeof v.value !== 'string' || v.value.length > 65536 || (version === 2 && utf8ByteLength(v.value) > 65536))
        return bad()
      result = Object.freeze({ kind: 'text', value: v.value })
    } else return bad()
    // Preserve v1 behavior; v2 retains all map keys safely as own data properties.
    if (safeKey || version === 2) entries.push([key, result])
  }
  return Object.freeze(Object.fromEntries(entries))
}
function array<T>(input: unknown, parse: (value: unknown) => T): readonly T[] {
  if (!Array.isArray(input) || input.length > 1048576) return bad()
  return Object.freeze(input.map(parse))
}
function platformDetail(input: unknown, depth: number): OrderedRecordingPlatformDetail {
  if (depth > MAX_FAILURE_DEPTH) return bad()
  const v = obj(input)
  exact(v, [
    'domain',
    'code',
    'message',
    'metadata',
    ...(Object.hasOwn(v, 'metadataEntries') ? ['metadataEntries'] : []),
    ...(Object.hasOwn(v, 'cleanup') ? ['cleanup'] : []),
    ...(Object.hasOwn(v, 'cleanupFailures') ? ['cleanupFailures'] : [])
  ])
  return Object.freeze({
    domain: boundedText(v.domain, true),
    code: boundedText(v.code, true),
    message: v.message === null ? null : boundedText(v.message, true),
    metadata: parseMetadata(v.metadata, 2, Object.hasOwn(v, 'metadataEntries')),
    ...(Object.hasOwn(v, 'metadataEntries')
      ? { metadataEntries: snapshotPlatformMetadataEntries(v.metadataEntries, 'ordered.recording.metadata.entries') }
      : {}),
    ...(Object.hasOwn(v, 'cleanup') ? { cleanup: array(v.cleanup, child => platformDetail(child, depth + 1)) } : {}),
    ...(Object.hasOwn(v, 'cleanupFailures')
      ? { cleanupFailures: array(v.cleanupFailures, child => recordingFailure(child, 2, depth + 1)) }
      : {})
  })
}
function recordingFailure(input: unknown, version: 1 | 2, depth: number): OrderedRecordingFailure {
  if (depth > MAX_FAILURE_DEPTH) return bad()
  const value = obj(input)
  exact(value, [
    'code',
    'domain',
    'operation',
    'detail',
    'platformDomain',
    'platformCode',
    'platformMessage',
    'platformMetadata',
    ...(version === 2 && Object.hasOwn(value, 'platformMetadataEntries') ? ['platformMetadataEntries'] : []),
    ...(version === 2 && Object.hasOwn(value, 'platformCleanup') ? ['platformCleanup'] : []),
    ...(version === 2 && Object.hasOwn(value, 'platformCleanupFailures') ? ['platformCleanupFailures'] : [])
  ])
  const readText = version === 2 ? (v: unknown) => boundedText(v, false) : text
  const readOptional = version === 2 ? (v: unknown) => (v === null ? null : boundedText(v, true)) : optionalText
  return Object.freeze({
    code: readText(value.code),
    domain: readText(value.domain),
    operation: readText(value.operation),
    detail: readOptional(value.detail),
    platformDomain: readOptional(value.platformDomain),
    platformCode: readOptional(value.platformCode),
    platformMessage: readOptional(value.platformMessage),
    platformMetadata: parseMetadata(value.platformMetadata, version, Object.hasOwn(value, 'platformMetadataEntries')),
    ...(Object.hasOwn(value, 'platformMetadataEntries')
      ? {
          platformMetadataEntries: snapshotPlatformMetadataEntries(
            value.platformMetadataEntries,
            'ordered.recording.metadata.entries'
          )
        }
      : {}),
    ...(Object.hasOwn(value, 'platformCleanup')
      ? { platformCleanup: array(value.platformCleanup, child => platformDetail(child, depth + 1)) }
      : {}),
    ...(Object.hasOwn(value, 'platformCleanupFailures')
      ? {
          platformCleanupFailures: array(value.platformCleanupFailures, child =>
            recordingFailure(child, version, depth + 1)
          )
        }
      : {})
  })
}
export function parseOrderedRecordingFailure(input: unknown, version: 1 | 2 = 1): OrderedRecordingFailure {
  if (version !== 1 && version !== 2) return bad()
  if (version === 2) assertPlatformFailureEnvelopeBound(input, 'ordered.recording.failure')
  return recordingFailure(input, version, 0)
}

function rejection(input: unknown): OrderedRecordingRejection {
  const v = obj(input)
  if (v.kind === 'value' || v.kind === 'ordinal-exhausted') {
    exact(v, ['kind', 'bytes'])
    return Object.freeze({ kind: v.kind, bytes: decimal(v.bytes) })
  }
  if (v.kind === 'loss') {
    exact(v, ['kind', 'skippedRecords'])
    return Object.freeze({ kind: 'loss', skippedRecords: decimal(v.skippedRecords) })
  }
  if (v.kind === 'fence') {
    exact(v, ['kind', 'membership'])
    return Object.freeze({ kind: 'fence', membership: membership(v.membership) })
  }
  if (v.kind === 'barrier') {
    exact(v, ['kind', 'id'])
    return Object.freeze({ kind: 'barrier', id: decimal(v.id) })
  }
  if (v.kind === 'marker') {
    exact(v, ['kind', 'markerId'])
    return Object.freeze({ kind: 'marker', markerId: decimal(v.markerId) })
  }
  return bad()
}
function reason(input: unknown, version: 1 | 2): OrderedRecordingReason {
  const v = obj(input)
  if (v.kind === 'setup-failed' || v.kind === 'source-failed' || v.kind === 'host-reset') {
    exact(v, ['kind', 'failure'])
    return Object.freeze({ kind: v.kind, failure: parseOrderedRecordingFailure(v.failure, version) })
  }
  if (
    v.kind !== 'closed' &&
    v.kind !== 'invalidated' &&
    v.kind !== 'service-changed' &&
    v.kind !== 'connection-lost' &&
    v.kind !== 'owner-released' &&
    v.kind !== 'capacity-exceeded' &&
    v.kind !== 'ordinal-invalid' &&
    v.kind !== 'source-overflow' &&
    v.kind !== 'source-ordinal-exhausted'
  )
    return bad()
  exact(v, ['kind'])
  return Object.freeze({ kind: v.kind })
}
function delivery(input: unknown): 'notification' | 'indication' | 'unknown' {
  if (input !== 'notification' && input !== 'indication' && input !== 'unknown') return bad()
  return input
}
function record(input: unknown, version: 1 | 2): OrderedRecordingRecord {
  const v = obj(input)
  if (v.kind === 'failure-observation') {
    if (version !== 2) return bad()
    exact(v, ['kind', 'failure', 'sourceRejection', 'sourceFailureMember', 'rejectedLoss', 'tailUncertain'])
    const failure = v.failure === null ? null : parseOrderedRecordingFailure(v.failure, version)
    const sourceRejection = v.sourceRejection === null ? null : rejection(v.sourceRejection)
    const sourceFailureMember = v.sourceFailureMember === null ? null : membership(v.sourceFailureMember)
    const rejectedLoss = v.rejectedLoss === null ? null : loss(v.rejectedLoss)
    const tailUncertain = bool(v.tailUncertain)
    if (
      failure === null &&
      sourceRejection === null &&
      sourceFailureMember === null &&
      rejectedLoss === null &&
      !tailUncertain
    )
      return bad()
    return Object.freeze({
      kind: 'failure-observation',
      failure,
      sourceRejection,
      sourceFailureMember,
      rejectedLoss,
      tailUncertain
    })
  }
  if (v.kind === 'failure-observations-omitted') {
    if (version !== 2) return bad()
    exact(v, ['kind', 'count'])
    const count = optionalDecimal(v.count)
    if (count === '0') return bad()
    return Object.freeze({ kind: 'failure-observations-omitted', count })
  }
  if (v.kind === 'value') {
    exact(v, ['kind', 'ordinal', 'member', 'membership', 'bytes', 'delivery', 'provenance'])
    if (!Array.isArray(v.bytes) || !v.bytes.every(x => Number.isInteger(x) && x >= 0 && x <= 255)) return bad()
    if (v.provenance !== 'notification' && v.provenance !== 'read-or-notification' && v.provenance !== 'unknown')
      return bad()
    return Object.freeze({
      kind: 'value',
      ordinal: decimal(v.ordinal),
      member: decimal(v.member),
      membership: membership(v.membership),
      bytes: Uint8Array.from(v.bytes),
      delivery: delivery(v.delivery),
      provenance: v.provenance
    })
  }
  if (v.kind === 'marker') {
    exact(v, ['kind', 'markerId', 'ordinal'])
    return Object.freeze({ kind: 'marker', markerId: decimal(v.markerId), ordinal: decimal(v.ordinal) })
  }
  if (v.kind === 'loss') {
    exact(v, ['kind', 'scope', 'unit', 'count', 'bytes'])
    return Object.freeze({ kind: 'loss', ...lossFields(v) })
  }
  if (v.kind === 'terminal') {
    if (version === 2) {
      exact(v, ['kind', 'reason', 'tailUncertain'])
      return Object.freeze({
        kind: 'terminal',
        reason: reason(v.reason, version),
        tailUncertain: bool(v.tailUncertain)
      })
    }
    const hasSummaryFlags = v.failureCountOverflowed !== undefined || v.failureObservationsOmitted !== undefined
    exact(v, [
      'kind',
      'reason',
      'tailUncertain',
      'rejectedValues',
      'rejectedLoss',
      'sourceRejection',
      'sourceFailureMember',
      'secondaryFailure',
      'additionalFailureCount',
      ...(v.failureSummaryVersion === undefined ? [] : ['failureSummaryVersion']),
      ...(hasSummaryFlags ? ['failureCountOverflowed', 'failureObservationsOmitted'] : [])
    ])
    if (v.failureSummaryVersion !== undefined && v.failureSummaryVersion !== 1) return bad()
    if (hasSummaryFlags && v.failureSummaryVersion !== 1) return bad()
    return Object.freeze({
      kind: 'terminal',
      reason: reason(v.reason, version),
      tailUncertain: bool(v.tailUncertain),
      rejectedValues: optionalDecimal(v.rejectedValues),
      rejectedLoss: v.rejectedLoss === null ? null : loss(v.rejectedLoss),
      sourceRejection: v.sourceRejection === null ? null : rejection(v.sourceRejection),
      sourceFailureMember: v.sourceFailureMember === null ? null : membership(v.sourceFailureMember),
      secondaryFailure: v.secondaryFailure === null ? null : parseOrderedRecordingFailure(v.secondaryFailure, version),
      additionalFailureCount: decimal(v.additionalFailureCount),
      ...(v.failureSummaryVersion === undefined ? {} : { failureSummaryVersion: 1 as const }),
      ...(hasSummaryFlags
        ? {
            failureCountOverflowed: bool(v.failureCountOverflowed),
            failureObservationsOmitted: bool(v.failureObservationsOmitted)
          }
        : {})
    })
  }
  if (v.kind === 'setup-terminal') {
    exact(v, ['kind', 'sourceCreated', 'tailUncertain', 'failure', 'setupFailure', 'fenceFailure', 'members'])
    if (v.sourceCreated !== false || v.tailUncertain !== false || !Array.isArray(v.members) || v.members.length > 128)
      return bad()
    const members = v.members.map(memberInput => {
      const m = obj(memberInput)
      exact(m, ['member', 'acquisition', 'delivery', 'cleanupReleased', 'cleanupError'])
      if (m.acquisition !== 'not-started' && m.acquisition !== 'acquired' && m.acquisition !== 'indeterminate')
        return bad()
      return Object.freeze({
        member: decimal(m.member),
        acquisition: m.acquisition,
        delivery: delivery(m.delivery),
        cleanupReleased: bool(m.cleanupReleased),
        cleanupError: m.cleanupError === null ? null : parseOrderedRecordingFailure(m.cleanupError, version)
      })
    })
    return Object.freeze({
      kind: 'setup-terminal',
      sourceCreated: false,
      tailUncertain: false,
      failure: parseOrderedRecordingFailure(v.failure, version),
      setupFailure: v.setupFailure === null ? null : parseOrderedRecordingFailure(v.setupFailure, version),
      fenceFailure: v.fenceFailure === null ? null : parseOrderedRecordingFailure(v.fenceFailure, version),
      members: Object.freeze(members)
    })
  }
  return bad()
}
function metadata(input: unknown, recordingId: string): OrderedRecordingMetadata {
  const v = obj(input)
  exact(v, ['kind', 'version', 'recordingId', 'peerId', 'collectionId', 'sourceGeneration', 'deliveryId'])
  if (v.kind !== 'ordered' || (v.version !== 1 && v.version !== 2) || v.recordingId !== recordingId) return bad()
  let generation: OrderedRecordingMetadata['sourceGeneration'] = null
  if (v.sourceGeneration !== null) {
    const g = obj(v.sourceGeneration)
    exact(g, ['kind', 'value'])
    if (g.kind !== 'counter' && g.kind !== 'opaque') return bad()
    generation = Object.freeze({ kind: g.kind, value: g.kind === 'counter' ? decimal(g.value) : text(g.value, 256) })
  }
  const deliveryId = optionalDecimal(v.deliveryId)
  if ((generation === null) !== (deliveryId === null)) return bad()
  return Object.freeze({
    kind: 'ordered',
    version: v.version,
    recordingId,
    peerId: text(v.peerId, 16384),
    collectionId: decimal(v.collectionId),
    sourceGeneration: generation,
    deliveryId
  })
}
/** Decode a prepared ordered prefix without acknowledging its shared journal cursor. */
export function parseOrderedRecordingBatch(
  input: unknown,
  recordingId: string,
  limits: { readonly maxItems: number; readonly maxBytes: number },
  serializedRecordBytes?: number
): OrderedRecordingBatch {
  const batch = obj(input)
  exact(batch, ['token', 'records', 'bytes', 'more'])
  const bytes = number(batch.bytes, 0, limits.maxBytes)
  const more = bool(batch.more)
  if (!Array.isArray(batch.records) || batch.records.length > limits.maxItems) return bad()
  const token = batch.token === null ? null : text(batch.token, MAX_RECORDING_TOKEN_BYTES)
  if (token !== null && utf8ByteLength(token) > MAX_RECORDING_TOKEN_BYTES) return bad()
  if ((batch.records.length === 0) !== (token === null) || (token === null && (bytes !== 0 || more))) return bad()
  let previous = 0
  let encoded = 0
  let origin: string | undefined
  const deliveryIds = new Set<string>()
  const records = batch.records.map(rowInput => {
    const row = obj(rowInput)
    exact(row, ['ordinal', 'metadata', 'record'])
    const rawRecord = obj(row.record)
    if (
      rawRecord.kind === 'terminal' ||
      rawRecord.kind === 'setup-terminal' ||
      rawRecord.kind === 'failure-observation'
    )
      assertPlatformFailureEnvelopeBound(row, 'ordered.recording.row')
    encoded += utf8ByteLength(JSON.stringify(row))
    const ordinal = number(row.ordinal, 1, Number.MAX_SAFE_INTEGER)
    if (previous !== 0 && ordinal !== previous + 1) return bad()
    previous = ordinal
    const context = metadata(row.metadata, recordingId)
    if (context.version === 2 && context.deliveryId !== null) {
      if (deliveryIds.has(context.deliveryId)) return bad()
      deliveryIds.add(context.deliveryId)
    }
    const source = JSON.stringify([context.version, context.peerId, context.collectionId, context.sourceGeneration])
    if (origin !== undefined && source !== origin) return bad()
    origin = source
    const value = record(row.record, context.version)
    if ((value.kind === 'setup-terminal') !== (context.sourceGeneration === null)) return bad()
    return Object.freeze({ ordinal, metadata: context, record: value })
  })
  if (
    !Number.isSafeInteger(encoded) ||
    (serializedRecordBytes === undefined ? encoded : number(serializedRecordBytes, 0, limits.maxBytes)) !== bytes
  )
    return bad()
  return Object.freeze({ token, records: Object.freeze(records), bytes, more })
}

export interface OrderedRecordingContext {
  readonly kind: 'ordered-recording'
  readonly version: 1 | 2
  readonly peerId: string
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly maxItems: number
  readonly maxBytes: number
  readonly maxControls: number
  readonly members: readonly {
    readonly memberId: string
    readonly publicMemberId: string
    readonly selector: {
      readonly serviceUuid: string
      readonly serviceOccurrence: string
      readonly characteristicUuid: string
      readonly characteristicOccurrence: string
    }
    readonly delivery: 'notification' | 'indication' | null
    readonly preference: 'notification' | 'indication' | null
  }[]
}

/** Validate the immutable source declaration independently of live radio state. */
export function parseOrderedRecordingContext(input: unknown): OrderedRecordingContext {
  const value = obj(input)
  exact(value, [
    'kind',
    'version',
    'peerId',
    'connectionGeneration',
    'databaseGeneration',
    'maxItems',
    'maxBytes',
    'maxControls',
    'members'
  ])
  if (
    value.kind !== 'ordered-recording' ||
    (value.version !== 1 && value.version !== 2) ||
    !Array.isArray(value.members) ||
    value.members.length < 1 ||
    value.members.length > 128
  )
    return bad()
  const ids = new Set<string>()
  const publicIds = new Set<string>()
  const members = value.members.map(memberInput => {
    const member = obj(memberInput)
    exact(member, ['memberId', 'publicMemberId', 'selector', 'delivery', 'preference'])
    const memberId = decimal(member.memberId)
    const publicMemberId = text(member.publicMemberId, 256)
    if (utf8ByteLength(publicMemberId) > 256 || ids.has(memberId) || publicIds.has(publicMemberId)) return bad()
    ids.add(memberId)
    publicIds.add(publicMemberId)
    const selector = obj(member.selector)
    exact(selector, ['serviceUuid', 'serviceOccurrence', 'characteristicUuid', 'characteristicOccurrence'])
    const selected = (mode: unknown): 'notification' | 'indication' | null => {
      if (mode !== null && mode !== 'notification' && mode !== 'indication') return bad()
      return mode
    }
    return Object.freeze({
      memberId,
      publicMemberId,
      selector: Object.freeze({
        serviceUuid: text(selector.serviceUuid),
        serviceOccurrence: decimal(selector.serviceOccurrence),
        characteristicUuid: text(selector.characteristicUuid),
        characteristicOccurrence: decimal(selector.characteristicOccurrence)
      }),
      delivery: selected(member.delivery),
      preference: selected(member.preference)
    })
  })
  return Object.freeze({
    kind: 'ordered-recording',
    version: value.version,
    peerId: text(value.peerId),
    connectionGeneration: text(value.connectionGeneration),
    databaseGeneration: text(value.databaseGeneration),
    maxItems: number(value.maxItems, 1, 65536),
    maxBytes: number(value.maxBytes, 1, 4194304),
    maxControls: number(value.maxControls, 1, 65536),
    members: Object.freeze(members)
  })
}
