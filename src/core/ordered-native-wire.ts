// src/core/ordered-native-wire.ts
import {
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  assertPlatformFailureEnvelopeBound,
  contractError,
  snapshotPlatformMetadataEntries
} from '../backend-contract/errors'
import { byteLimit, ownBytes } from '../backend-contract/primitives'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import { parseOrderedRecordingFailure } from './ordered-recording'
import type {
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationTerminal,
  OrderedSourceGeneration
} from './ordered-notification-contract'

function malformed(): never {
  throw contractError('protocol.malformed', 'boundary', 'ordered.native.wire')
}
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) malformed()
}
function strictMembership(value: unknown) {
  if (!object(value)) return malformed()
  exact(value, ['member', 'epoch'])
  return membership(value)
}
function strictRejection(value: unknown): NonNullable<OrderedNotificationTerminal['sourceRejection']> {
  if (!object(value)) return malformed()
  switch (value.kind) {
    case 'value':
    case 'ordinal-exhausted':
      exact(value, ['kind', 'bytes'])
      return { kind: value.kind, bytes: decimal(value.bytes) }
    case 'loss':
      exact(value, ['kind', 'skippedRecords'])
      return { kind: 'loss', skippedRecords: decimal(value.skippedRecords) }
    case 'fence':
      exact(value, ['kind', 'membership'])
      return { kind: 'fence', membership: strictMembership(value.membership) }
    case 'barrier':
      exact(value, ['kind', 'id'])
      return { kind: 'barrier', id: decimal(value.id) }
    case 'marker':
      exact(value, ['kind', 'markerId'])
      return { kind: 'marker', markerId: decimal(value.markerId) }
    default:
      return malformed()
  }
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return malformed()
  return value
}
function optionalDecimal(value: unknown): string | null {
  return value === null ? null : decimal(value)
}
function membership(input: unknown) {
  if (!object(input)) return malformed()
  return { member: decimal(input.member), epoch: decimal(input.epoch) }
}
function scope(value: unknown, members: readonly string[]): OrderedNotificationLoss['scope'] {
  if (!object(value)) return malformed()
  if (value.kind === 'group' || value.kind === 'unknown') return { kind: value.kind }
  if (value.kind !== 'member') return malformed()
  const id = decimal(value.memberId)
  const index = Number(id)
  if (!Number.isSafeInteger(index) || String(index) !== id || members[index] === undefined) return malformed()
  return { kind: 'member', memberId: members[index] }
}
function loss(value: unknown, members: readonly string[]): OrderedNotificationLoss {
  if (!object(value) || (value.unit !== 'values' && value.unit !== 'source-records')) return malformed()
  const count = optionalDecimal(value.count)
  const bytes = optionalDecimal(value.bytes)
  return {
    kind: 'loss',
    scope: scope(value.scope, members),
    reason: value.unit === 'values' ? 'overflow' : 'source-loss',
    lostValues: value.unit === 'values' ? count : null,
    lostBytes: bytes,
    lostSourceRecords: value.unit === 'source-records' ? count : null
  }
}
/** The metadata revision must be negotiated by the caller before accepting v2. */
export function decodeOrderedNativeFailure(value: unknown, metadataVersion: 1 | 2 = 1) {
  if (metadataVersion !== 1 && metadataVersion !== 2) return malformed()
  if (
    !object(value) ||
    typeof value.code !== 'string' ||
    typeof value.domain !== 'string' ||
    typeof value.operation !== 'string' ||
    value.operation.length === 0 ||
    (value.detail !== null && typeof value.detail !== 'string')
  )
    return malformed()
  exact(value, [
    'code',
    'domain',
    'operation',
    'detail',
    'platformDomain',
    'platformCode',
    'platformMessage',
    'platformMetadata',
    ...(metadataVersion === 2 && Object.hasOwn(value, 'platformMetadataEntries') ? ['platformMetadataEntries'] : []),
    ...(metadataVersion === 2 && Object.hasOwn(value, 'platformCleanup') ? ['platformCleanup'] : []),
    ...(metadataVersion === 2 && Object.hasOwn(value, 'platformCleanupFailures') ? ['platformCleanupFailures'] : [])
  ])
  assertPlatformFailureEnvelopeBound(value, 'ordered.native.failure')
  const code = BLE_ERROR_CODES.find(candidate => candidate === value.code)
  const domain = BLE_ERROR_DOMAINS.find(candidate => candidate === value.domain)
  if (code === undefined || domain === undefined || !object(value.platformMetadata)) return malformed()
  const recordingMetadata: Record<string, { kind: unknown; value: unknown }> = Object.create(null)
  const metadata: Record<string, string | boolean> = Object.create(null)
  for (const [key, entry] of Object.entries(value.platformMetadata)) {
    if (
      !object(entry) ||
      Object.keys(entry).length !== 2 ||
      !Object.hasOwn(entry, 'type') ||
      !Object.hasOwn(entry, 'value')
    )
      return malformed()
    recordingMetadata[key] = { kind: entry.type, value: entry.value }
    const safeKey = key !== '__proto__' && key !== 'constructor' && key !== 'prototype'
    if (!safeKey && !Object.hasOwn(value, 'platformMetadataEntries')) return malformed()
    if (entry.type === 'text' && typeof entry.value === 'string') {
      if (safeKey) metadata[key] = entry.value
    } else if (
      entry.type === 'int' &&
      typeof entry.value === 'string' &&
      /^(0|-?[1-9][0-9]*)$/.test(entry.value) &&
      entry.value.length <= 20 &&
      BigInt(entry.value) >= -9223372036854775808n &&
      BigInt(entry.value) <= 9223372036854775807n
    ) {
      if (safeKey) metadata[key] = entry.value
    } else if (entry.type === 'bool' && typeof entry.value === 'boolean') {
      if (safeKey) metadata[key] = entry.value
    } else return malformed()
  }
  const orderedFailure =
    metadataVersion === 2
      ? parseOrderedRecordingFailure({ ...value, platformMetadata: recordingMetadata }, 2)
      : undefined
  if (
    value.platformDomain === null &&
    value.platformCode === null &&
    value.platformMessage === null &&
    value.detail === null &&
    Object.keys(metadata).length === 0 &&
    !Object.hasOwn(value, 'platformMetadataEntries') &&
    !Object.hasOwn(value, 'platformCleanup') &&
    !Object.hasOwn(value, 'platformCleanupFailures')
  ) {
    const normalized = contractError(code, domain, value.operation).normalized
    return orderedFailure === undefined ? normalized : { ...normalized, orderedFailure }
  }
  if (
    (value.platformDomain !== null && typeof value.platformDomain !== 'string') ||
    (value.platformCode !== null && typeof value.platformCode !== 'string') ||
    (value.platformMessage !== null && typeof value.platformMessage !== 'string')
  )
    return malformed()
  const normalized = contractError(code, domain, value.operation, {
    domain: value.platformDomain ?? 'desktop-rust-core',
    code: value.platformCode ?? 'source-detail',
    safeMessage: value.platformMessage ?? value.detail ?? '',
    metadata,
    ...(Object.hasOwn(value, 'platformMetadataEntries')
      ? {
          metadataEntries: snapshotPlatformMetadataEntries(
            value.platformMetadataEntries,
            'ordered.native.metadata.entries'
          )
        }
      : {})
  }).normalized
  return orderedFailure === undefined ? normalized : { ...normalized, orderedFailure }
}
function terminal(
  value: Record<string, unknown>,
  members: readonly string[],
  metadataVersion: 1 | 2
): OrderedNotificationTerminal {
  if (!object(value.reason) || typeof value.reason.kind !== 'string' || typeof value.uncertainTail !== 'boolean')
    return malformed()
  const kind = value.reason.kind
  let reason: OrderedNotificationTerminal['reason']
  switch (kind) {
    case 'closed':
      reason = 'closed'
      break
    case 'setup-failed':
    case 'source-failed':
      reason = 'source-failed'
      break
    case 'connection-lost':
      reason = 'connection-lost'
      break
    case 'service-changed':
      reason = 'service-changed'
      break
    case 'owner-released':
      reason = 'owner-released'
      break
    case 'host-reset':
      reason = 'host-reset'
      break
    case 'capacity-exceeded':
    case 'source-overflow':
    case 'ordinal-invalid':
      reason = 'overflow'
      break
    case 'ordinal-exhausted':
      reason = 'ordinal-exhausted'
      break
    default:
      return malformed()
  }
  if (!Object.hasOwn(value, 'failureSummaryVersion')) {
    exact(value, ['kind', 'reason', 'uncertainTail'])
    exact(
      value.reason,
      kind === 'source-failed' || kind === 'setup-failed' || kind === 'host-reset' ? ['kind', 'error'] : ['kind']
    )
    return {
      kind: 'terminal',
      reason,
      uncertainTail: value.uncertainTail,
      ...(kind === 'source-failed' || kind === 'setup-failed' || kind === 'host-reset'
        ? { error: decodeOrderedNativeFailure(value.reason.error, metadataVersion) }
        : {})
    }
  }
  if (metadataVersion === 2) {
    exact(value, [
      'kind',
      'reason',
      'uncertainTail',
      'rejectedValues',
      'rejectedLoss',
      'sourceRejection',
      'sourceFailureMember',
      'secondaryFailure',
      'additionalFailureCount',
      'failureSummaryVersion',
      'failureCountOverflowed',
      'failureObservationsOmitted'
    ])
    exact(
      value.reason,
      kind === 'source-failed' || kind === 'setup-failed' || kind === 'host-reset' ? ['kind', 'error'] : ['kind']
    )
  }
  const rejectedValues = optionalDecimal(value.rejectedValues)
  let rejectedLoss: OrderedNotificationTerminal['rejectedLoss'] = null
  if (value.rejectedLoss !== null) {
    if (
      !object(value.rejectedLoss) ||
      (value.rejectedLoss.unit !== 'values' && value.rejectedLoss.unit !== 'source-records')
    )
      return malformed()
    if (metadataVersion === 2) {
      exact(value.rejectedLoss, ['scope', 'unit', 'count', 'bytes'])
      if (!object(value.rejectedLoss.scope)) return malformed()
      exact(value.rejectedLoss.scope, value.rejectedLoss.scope.kind === 'member' ? ['kind', 'memberId'] : ['kind'])
    }
    rejectedLoss = {
      scope: scope(value.rejectedLoss.scope, members),
      unit: value.rejectedLoss.unit,
      count: optionalDecimal(value.rejectedLoss.count),
      bytes: optionalDecimal(value.rejectedLoss.bytes)
    }
  }
  let sourceRejection: OrderedNotificationTerminal['sourceRejection'] = null
  if (value.sourceRejection !== null) {
    const rejected = value.sourceRejection
    if (!object(rejected)) return malformed()
    if (metadataVersion === 2) sourceRejection = strictRejection(rejected)
    else
      switch (rejected.kind) {
        case 'value':
          sourceRejection = { kind: 'value', bytes: decimal(rejected.bytes) }
          break
        case 'loss':
          sourceRejection = { kind: 'loss', skippedRecords: decimal(rejected.skippedRecords) }
          break
        case 'fence':
          sourceRejection = { kind: 'fence', membership: membership(rejected.membership) }
          break
        case 'barrier':
          sourceRejection = { kind: 'barrier', id: decimal(rejected.id) }
          break
        case 'marker':
          sourceRejection = { kind: 'marker', markerId: decimal(rejected.markerId) }
          break
        case 'ordinal-exhausted':
          sourceRejection = { kind: 'ordinal-exhausted', bytes: decimal(rejected.bytes) }
          break
        default:
          return malformed()
      }
  }
  if (
    value.failureSummaryVersion !== 1 ||
    typeof value.failureCountOverflowed !== 'boolean' ||
    typeof value.failureObservationsOmitted !== 'boolean'
  )
    return malformed()
  return {
    kind: 'terminal',
    reason,
    uncertainTail: value.uncertainTail,
    rejectedValues,
    rejectedLoss,
    sourceRejection,
    sourceFailureMember:
      value.sourceFailureMember === null
        ? null
        : metadataVersion === 2
          ? strictMembership(value.sourceFailureMember)
          : membership(value.sourceFailureMember),
    secondaryError:
      value.secondaryFailure === null ? null : decodeOrderedNativeFailure(value.secondaryFailure, metadataVersion),
    additionalFailureCount: decimal(value.additionalFailureCount),
    failureCountOverflowed: value.failureCountOverflowed === true,
    failureObservationsOmitted: value.failureObservationsOmitted === true,
    ...(kind === 'source-failed' || kind === 'setup-failed' || kind === 'host-reset'
      ? { error: decodeOrderedNativeFailure(value.reason.error, metadataVersion) }
      : {})
  }
}
/** Decode one native record; callers retain the returned value's ordinal for the next call. */
export function decodeOrderedNativeItem(
  input: unknown,
  context: {
    readonly collectionId: string
    readonly connectionGeneration: string
    readonly databaseGeneration: string
    readonly sourceGeneration?: OrderedSourceGeneration | null
    readonly members: readonly string[]
    readonly byteCapacity: number
    readonly lastOrdinal: string | null
    readonly metadataVersion?: 1 | 2
  }
): OrderedNotificationItem {
  if (!object(input)) return malformed()
  const metadataVersion = context.metadataVersion ?? 1
  if (metadataVersion !== 1 && metadataVersion !== 2) return malformed()
  if (input.kind === 'terminal' || input.kind === 'failure-observation')
    assertPlatformFailureEnvelopeBound(input, 'ordered.native.delivery')
  if (input.kind === 'failure-observations-omitted') {
    exact(input, ['kind', 'count'])
    const count = optionalDecimal(input.count)
    if (count === '0') return malformed()
    return { kind: input.kind, count }
  }
  if (input.kind === 'failure-observation') {
    exact(input, ['kind', 'failure', 'sourceRejection', 'sourceFailureMember', 'rejectedLoss', 'tailUncertain'])
    if (typeof input.tailUncertain !== 'boolean') return malformed()
    const failure = input.failure === null ? null : decodeOrderedNativeFailure(input.failure, metadataVersion)
    const sourceRejection = input.sourceRejection === null ? null : strictRejection(input.sourceRejection)
    const sourceFailureMember = input.sourceFailureMember === null ? null : strictMembership(input.sourceFailureMember)
    let rejectedLoss: OrderedNotificationTerminal['rejectedLoss'] = null
    if (input.rejectedLoss !== null) {
      if (!object(input.rejectedLoss)) return malformed()
      exact(input.rejectedLoss, ['scope', 'unit', 'count', 'bytes'])
      const rawScope = input.rejectedLoss.scope
      if (!object(rawScope)) return malformed()
      exact(rawScope, rawScope.kind === 'member' ? ['kind', 'memberId'] : ['kind'])
      if (input.rejectedLoss.unit !== 'values' && input.rejectedLoss.unit !== 'source-records') return malformed()
      rejectedLoss = {
        scope: scope(rawScope, context.members),
        unit: input.rejectedLoss.unit,
        count: optionalDecimal(input.rejectedLoss.count),
        bytes: optionalDecimal(input.rejectedLoss.bytes)
      }
    }
    if (
      failure === null &&
      sourceRejection === null &&
      sourceFailureMember === null &&
      rejectedLoss === null &&
      !input.tailUncertain
    )
      return malformed()
    return {
      kind: input.kind,
      failure,
      sourceRejection,
      sourceFailureMember,
      rejectedLoss,
      tailUncertain: input.tailUncertain
    }
  }
  if (input.kind === 'loss') return loss(input.loss, context.members)
  if (input.kind === 'terminal') return terminal(input, context.members, metadataVersion)
  if (input.kind === 'marker') {
    const ordinal = decimal(input.ordinal)
    if (
      context.lastOrdinal !== null &&
      (ordinal.length < context.lastOrdinal.length ||
        (ordinal.length === context.lastOrdinal.length && ordinal <= context.lastOrdinal))
    )
      return malformed()
    return {
      kind: 'marker',
      collectionId: context.collectionId,
      connectionGeneration: context.connectionGeneration,
      databaseGeneration: context.databaseGeneration,
      sourceGeneration: context.sourceGeneration ?? null,
      sourceMembership: null,
      markerId: decimal(input.markerId),
      ordinal
    }
  }
  if (
    input.kind !== 'value' ||
    !Array.isArray(input.bytes) ||
    !input.bytes.every(x => Number.isInteger(x) && x >= 0 && x <= 255)
  )
    return malformed()
  const nativeId = decimal(input.memberId)
  const index = Number(nativeId)
  const memberId = context.members[index]
  if (!Number.isSafeInteger(index) || String(index) !== nativeId || memberId === undefined) return malformed()
  if (input.delivery !== 'notification' && input.delivery !== 'indication' && input.delivery !== 'unknown')
    return malformed()
  if (
    input.provenance !== 'notification' &&
    input.provenance !== 'read-or-notification' &&
    input.provenance !== 'unknown'
  )
    return malformed()
  const ordinal = decimal(input.ordinal)
  if (
    context.lastOrdinal !== null &&
    (ordinal.length < context.lastOrdinal.length ||
      (ordinal.length === context.lastOrdinal.length && ordinal <= context.lastOrdinal))
  )
    return malformed()
  return {
    kind: 'value',
    collectionId: context.collectionId,
    connectionGeneration: context.connectionGeneration,
    databaseGeneration: context.databaseGeneration,
    sourceGeneration: context.sourceGeneration ?? null,
    sourceMembership: membership(input.sourceMembership),
    memberId,
    ordinal,
    value: ownBytes(new Uint8Array(input.bytes), byteLimit(context.byteCapacity)),
    delivery: input.delivery,
    provenance: input.provenance
  }
}
