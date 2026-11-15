// src/ipc/ordered-codec.ts

import { contractError, type CleanupRecord, type NormalizedBleError } from '../backend-contract/errors'
import { byteLimit, ownBytes } from '../backend-contract/primitives'
import { isNotificationReceipt } from '../backend-contract/gatt'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationTerminal,
  OrderedSourceGeneration
} from '../core/ordered-notification-contract'
import { requiredTerminalError } from './manager'
import type { OrderedMarkerWriteStatus } from '../core/ordered-marker-wire'

function malformed(): never {
  throw contractError('protocol.malformed', 'ipc', 'ipc.ordered.record')
}
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}
function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) malformed()
}
function allowed(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).some(key => !keys.includes(key))) malformed()
}
function orderedError(value: unknown, operation: string): NormalizedBleError | null {
  if (value === null) return null
  if (!object(value)) return malformed()
  allowed(value, [
    'code',
    'domain',
    'operation',
    'platform',
    'retryability',
    'commit',
    'orderedFailure',
    'orderedFailureJson'
  ])
  if (object(value.platform)) allowed(value.platform, ['domain', 'code', 'safeMessage', 'metadata', 'metadataEntries'])
  return requiredTerminalError(value, operation)
}
function string(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0) return malformed()
  return value
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return malformed()
  return value
}
function nullableDecimal(value: unknown): string | null {
  return value === null ? null : decimal(value)
}
export function decodePublicMarkerWriteStatus(
  input: unknown
): OrderedMarkerWriteStatus | { readonly state: 'idle' } | { readonly state: 'retired' } {
  if (!object(input)) return malformed()
  if (input.state === 'idle' && Object.keys(input).length === 1) return { state: 'idle' }
  if (input.state === 'retired' && Object.keys(input).length === 1) return { state: 'retired' }
  if (input.state === 'pending' && Object.keys(input).length === 1) return { state: 'pending' }
  if (input.state === 'not-admitted' || input.state === 'rejected') {
    if (Object.keys(input).length !== 2 || !Object.hasOwn(input, 'error')) return malformed()
    const error = input.error === null ? null : orderedError(input.error, 'ipc.ordered.marker')
    if (input.state === 'rejected') {
      if (error === null) return malformed()
      return { state: 'rejected', error }
    }
    return { state: 'not-admitted', error }
  }
  if (input.state !== 'admitted' || Object.keys(input).length !== 3 || !object(input.write)) return malformed()
  const ordinal = decimal(input.ordinal)
  const write = input.write
  if (write.state === 'initiating' && Object.keys(write).length === 1)
    return { state: 'admitted', ordinal, write: { state: 'initiating' } }
  if (
    (write.state !== 'not-started' && write.state !== 'started' && write.state !== 'settled') ||
    Object.keys(write).length !== 2 ||
    !Object.hasOwn(write, 'error')
  )
    return malformed()
  return {
    state: 'admitted',
    ordinal,
    write: {
      state: write.state,
      error: write.error === null ? null : orderedError(write.error, 'ipc.ordered.marker')
    }
  }
}
export function decodeSourceGeneration(value: unknown): OrderedSourceGeneration | null {
  if (value === null) return null
  if (!object(value) || (value.kind !== 'counter' && value.kind !== 'opaque')) return malformed()
  exact(value, ['kind', 'value'])
  return { kind: value.kind, value: value.kind === 'counter' ? decimal(value.value) : string(value.value) }
}

function scope(value: unknown): OrderedNotificationLoss['scope'] {
  if (!object(value)) return malformed()
  if (value.kind === 'group' || value.kind === 'unknown') {
    exact(value, ['kind'])
    return { kind: value.kind }
  }
  if (value.kind === 'member') {
    exact(value, ['kind', 'memberId'])
    return { kind: 'member', memberId: string(value.memberId) }
  }
  return malformed()
}
function membership(value: unknown) {
  if (!object(value)) return malformed()
  exact(value, ['member', 'epoch'])
  return { member: decimal(value.member), epoch: decimal(value.epoch) }
}
function terminal(value: Record<string, unknown>): OrderedNotificationTerminal {
  allowed(value, [
    'kind',
    'reason',
    'uncertainTail',
    'error',
    'secondaryError',
    'fenceError',
    'rejectedValues',
    'additionalFailureCount',
    'failureCountOverflowed',
    'failureObservationsOmitted',
    'rejectedLoss',
    'sourceRejection',
    'sourceFailureMember'
  ])
  const reason = value.reason
  if (
    reason !== 'closed' &&
    reason !== 'overflow' &&
    reason !== 'source-failed' &&
    reason !== 'owner-released' &&
    reason !== 'connection-lost' &&
    reason !== 'service-changed' &&
    reason !== 'operation-aborted' &&
    reason !== 'operation-timed-out' &&
    reason !== 'ordinal-exhausted' &&
    reason !== 'host-reset'
  )
    return malformed()
  if (typeof value.uncertainTail !== 'boolean') return malformed()
  if (value.failureCountOverflowed !== undefined && typeof value.failureCountOverflowed !== 'boolean')
    return malformed()
  if (value.failureObservationsOmitted !== undefined && typeof value.failureObservationsOmitted !== 'boolean')
    return malformed()
  let rejectedLoss: OrderedNotificationTerminal['rejectedLoss']
  if (value.rejectedLoss === null) rejectedLoss = null
  else if (value.rejectedLoss !== undefined) {
    if (
      !object(value.rejectedLoss) ||
      (value.rejectedLoss.unit !== 'values' && value.rejectedLoss.unit !== 'source-records')
    )
      return malformed()
    exact(value.rejectedLoss, ['scope', 'unit', 'count', 'bytes'])
    rejectedLoss = {
      scope: scope(value.rejectedLoss.scope),
      unit: value.rejectedLoss.unit,
      count: nullableDecimal(value.rejectedLoss.count),
      bytes: nullableDecimal(value.rejectedLoss.bytes)
    }
  }
  let sourceRejection: OrderedNotificationTerminal['sourceRejection']
  if (value.sourceRejection === null) sourceRejection = null
  else if (value.sourceRejection !== undefined) {
    const r = value.sourceRejection
    if (!object(r)) return malformed()
    switch (r.kind) {
      case 'value':
      case 'ordinal-exhausted':
        exact(r, ['kind', 'bytes'])
        sourceRejection = { kind: r.kind, bytes: decimal(r.bytes) }
        break
      case 'loss':
        exact(r, ['kind', 'skippedRecords'])
        sourceRejection = { kind: r.kind, skippedRecords: decimal(r.skippedRecords) }
        break
      case 'fence':
        exact(r, ['kind', 'membership'])
        sourceRejection = { kind: r.kind, membership: membership(r.membership) }
        break
      case 'barrier':
        exact(r, ['kind', 'id'])
        sourceRejection = { kind: r.kind, id: decimal(r.id) }
        break
      case 'marker':
        exact(r, ['kind', 'markerId'])
        sourceRejection = { kind: r.kind, markerId: decimal(r.markerId) }
        break
      default:
        return malformed()
    }
  }
  return {
    kind: 'terminal',
    reason,
    uncertainTail: value.uncertainTail,
    ...(value.error === undefined ? {} : { error: orderedError(value.error, 'ipc.ordered') }),
    ...(value.secondaryError === undefined
      ? {}
      : { secondaryError: orderedError(value.secondaryError, 'ipc.ordered') }),
    ...(value.fenceError === undefined ? {} : { fenceError: orderedError(value.fenceError, 'ipc.ordered') }),
    ...(value.rejectedValues === undefined ? {} : { rejectedValues: nullableDecimal(value.rejectedValues) }),
    ...(value.additionalFailureCount === undefined
      ? {}
      : { additionalFailureCount: decimal(value.additionalFailureCount) }),
    ...(value.failureCountOverflowed === undefined
      ? {}
      : { failureCountOverflowed: value.failureCountOverflowed === true }),
    ...(value.failureObservationsOmitted === undefined
      ? {}
      : { failureObservationsOmitted: value.failureObservationsOmitted === true }),
    ...(rejectedLoss === undefined ? {} : { rejectedLoss }),
    ...(sourceRejection === undefined ? {} : { sourceRejection }),
    ...(value.sourceFailureMember === undefined
      ? {}
      : { sourceFailureMember: value.sourceFailureMember === null ? null : membership(value.sourceFailureMember) })
  }
}
export function decodeOrderedItem(value: unknown): OrderedNotificationItem {
  if (!object(value)) return malformed()
  if (value.kind === 'failure-observations-omitted') {
    exact(value, ['kind', 'count'])
    const count = nullableDecimal(value.count)
    if (count === '0') return malformed()
    return { kind: value.kind, count }
  }
  if (value.kind === 'failure-observation') {
    exact(value, ['kind', 'failure', 'sourceRejection', 'sourceFailureMember', 'rejectedLoss', 'tailUncertain'])
    if (typeof value.tailUncertain !== 'boolean') return malformed()
    if (value.failure !== null) {
      if (!object(value.failure)) return malformed()
      exact(value.failure, [
        'code',
        'domain',
        'operation',
        'platform',
        'retryability',
        ...(Object.hasOwn(value.failure, 'commit') ? ['commit'] : []),
        ...(Object.hasOwn(value.failure, 'orderedFailure') ? ['orderedFailure'] : []),
        ...(Object.hasOwn(value.failure, 'orderedFailureJson') ? ['orderedFailureJson'] : [])
      ])
      if (value.failure.platform !== null) {
        if (!object(value.failure.platform)) return malformed()
        exact(value.failure.platform, [
          'domain',
          'code',
          'safeMessage',
          'metadata',
          ...(Object.hasOwn(value.failure.platform, 'metadataEntries') ? ['metadataEntries'] : [])
        ])
      }
    }
    if (value.sourceFailureMember !== null) {
      if (!object(value.sourceFailureMember)) return malformed()
      exact(value.sourceFailureMember, ['member', 'epoch'])
    }
    if (value.sourceRejection !== null) {
      const r = value.sourceRejection
      if (!object(r)) return malformed()
      if (r.kind === 'value' || r.kind === 'ordinal-exhausted') exact(r, ['kind', 'bytes'])
      else if (r.kind === 'loss') exact(r, ['kind', 'skippedRecords'])
      else if (r.kind === 'fence') {
        exact(r, ['kind', 'membership'])
        if (!object(r.membership)) return malformed()
        exact(r.membership, ['member', 'epoch'])
      } else if (r.kind === 'barrier') exact(r, ['kind', 'id'])
      else if (r.kind === 'marker') exact(r, ['kind', 'markerId'])
      else return malformed()
    }
    if (value.rejectedLoss !== null) {
      if (!object(value.rejectedLoss) || !object(value.rejectedLoss.scope)) return malformed()
      exact(value.rejectedLoss, ['scope', 'unit', 'count', 'bytes'])
      exact(value.rejectedLoss.scope, value.rejectedLoss.scope.kind === 'member' ? ['kind', 'memberId'] : ['kind'])
    }
    const fields = terminal({
      kind: 'terminal',
      reason: 'closed',
      uncertainTail: value.tailUncertain,
      sourceRejection: value.sourceRejection,
      sourceFailureMember: value.sourceFailureMember,
      rejectedLoss: value.rejectedLoss
    })
    const failure = value.failure === null ? null : requiredTerminalError(value.failure, 'ipc.ordered.observation')
    const sourceRejection = fields.sourceRejection ?? null
    const sourceFailureMember = fields.sourceFailureMember ?? null
    const rejectedLoss = fields.rejectedLoss ?? null
    if (
      failure === null &&
      sourceRejection === null &&
      sourceFailureMember === null &&
      rejectedLoss === null &&
      !value.tailUncertain
    )
      return malformed()
    return {
      kind: value.kind,
      failure,
      sourceRejection,
      sourceFailureMember,
      rejectedLoss,
      tailUncertain: value.tailUncertain
    }
  }
  if (value.kind === 'terminal') return terminal(value)
  if (value.kind === 'marker') {
    allowed(value, [
      'kind',
      'collectionId',
      'connectionGeneration',
      'databaseGeneration',
      'markerId',
      'ordinal',
      'sourceGeneration',
      'sourceMembership'
    ])
    return {
      kind: 'marker',
      collectionId: string(value.collectionId),
      connectionGeneration: string(value.connectionGeneration),
      databaseGeneration: string(value.databaseGeneration),
      markerId: decimal(value.markerId),
      ordinal: decimal(value.ordinal),
      ...(value.sourceGeneration === undefined
        ? {}
        : { sourceGeneration: decodeSourceGeneration(value.sourceGeneration) }),
      ...(value.sourceMembership === undefined
        ? {}
        : { sourceMembership: value.sourceMembership === null ? null : membership(value.sourceMembership) })
    }
  }
  if (value.kind === 'loss') {
    exact(value, ['kind', 'reason', 'scope', 'lostValues', 'lostBytes', 'lostSourceRecords'])
    if (value.reason !== 'overflow' && value.reason !== 'source-loss' && value.reason !== 'control-loss')
      return malformed()
    return {
      kind: 'loss',
      reason: value.reason,
      scope: scope(value.scope),
      lostValues: nullableDecimal(value.lostValues),
      lostBytes: nullableDecimal(value.lostBytes),
      lostSourceRecords: nullableDecimal(value.lostSourceRecords)
    }
  }
  if (
    value.kind !== 'value' ||
    !Array.isArray(value.value) ||
    !value.value.every(x => Number.isInteger(x) && x >= 0 && x <= 255)
  )
    return malformed()
  allowed(value, [
    'kind',
    'collectionId',
    'sourceGeneration',
    'sourceMembership',
    'connectionGeneration',
    'databaseGeneration',
    'memberId',
    'ordinal',
    'value',
    'delivery',
    'provenance',
    'receipt'
  ])
  if (value.delivery !== 'notification' && value.delivery !== 'indication' && value.delivery !== 'unknown')
    return malformed()
  if (
    value.provenance !== 'notification' &&
    value.provenance !== 'read-or-notification' &&
    value.provenance !== 'unknown'
  )
    return malformed()
  if (value.receipt !== undefined && !isNotificationReceipt(value.receipt)) return malformed()
  return {
    kind: 'value',
    collectionId: string(value.collectionId),
    ...(value.sourceGeneration === undefined
      ? {}
      : { sourceGeneration: decodeSourceGeneration(value.sourceGeneration) }),
    ...(value.sourceMembership === undefined
      ? {}
      : { sourceMembership: value.sourceMembership === null ? null : membership(value.sourceMembership) }),
    connectionGeneration: string(value.connectionGeneration),
    databaseGeneration: string(value.databaseGeneration),
    memberId: string(value.memberId),
    ordinal: decimal(value.ordinal),
    value: ownBytes(new Uint8Array(value.value), byteLimit(value.value.length)),
    delivery: value.delivery,
    provenance: value.provenance,
    ...(value.receipt === undefined ? {} : { receipt: value.receipt })
  }
}
function cleanup(value: unknown): CleanupRecord {
  if (
    !object(value) ||
    (value.state !== 'released' && value.state !== 'release-failed') ||
    !Array.isArray(value.failures)
  )
    return malformed()
  exact(value, ['state', 'failures'])
  const failures = value.failures.map(entry => {
    if (!object(entry)) return malformed()
    exact(entry, ['resourceKind', 'error'])
    const error = orderedError(entry.error, 'ipc.ordered.cleanup')
    if (error === null) return malformed()
    return { resourceKind: string(entry.resourceKind), error }
  })
  if ((value.state === 'released') !== (failures.length === 0)) return malformed()
  return { state: value.state, failures }
}
export function decodeOrderedCleanup(value: unknown): OrderedNotificationCleanup {
  if (!object(value) || !Array.isArray(value.members)) return malformed()
  allowed(value, ['group', 'members', 'discarded'])
  let discarded: OrderedNotificationCleanup['discarded']
  if (value.discarded !== undefined) {
    const report = value.discarded
    if (!object(report) || typeof report.uncertainTail !== 'boolean') return malformed()
    allowed(report, [
      'values',
      'bytes',
      'controls',
      'uncertainTail',
      'terminal',
      'losses',
      'trustedValues',
      'markers',
      'failureObservations',
      'failureObservationsOmitted'
    ])
    let terminalItem: OrderedNotificationTerminal | null | undefined
    if (report.terminal === null) terminalItem = null
    else if (report.terminal !== undefined) {
      const item = decodeOrderedItem(report.terminal)
      if (item.kind !== 'terminal') return malformed()
      terminalItem = item
    }
    let losses: OrderedNotificationLoss[] | undefined
    if (report.losses !== undefined) {
      if (!Array.isArray(report.losses)) return malformed()
      losses = report.losses.map(input => {
        const item = decodeOrderedItem(input)
        if (item.kind !== 'loss') return malformed()
        return item
      })
    }
    let trustedValues: NonNullable<NonNullable<OrderedNotificationCleanup['discarded']>['trustedValues']> | undefined
    if (report.trustedValues !== undefined) {
      if (!Array.isArray(report.trustedValues)) return malformed()
      trustedValues = report.trustedValues.map(entry => {
        if (!object(entry)) return malformed()
        exact(entry, ['memberId', 'ordinal', 'sourceMembership', 'bytes'])
        return {
          memberId: string(entry.memberId),
          ordinal: decimal(entry.ordinal),
          sourceMembership: membership(entry.sourceMembership),
          bytes: decimal(entry.bytes)
        }
      })
    }
    let markers: NonNullable<NonNullable<OrderedNotificationCleanup['discarded']>['markers']> | undefined
    if (report.markers !== undefined) {
      if (!Array.isArray(report.markers)) return malformed()
      markers = report.markers.map(input => {
        const item = decodeOrderedItem(input)
        if (item.kind !== 'marker') return malformed()
        return item
      })
    }
    let failureObservations:
      | NonNullable<NonNullable<OrderedNotificationCleanup['discarded']>['failureObservations']>
      | undefined
    if (report.failureObservations !== undefined) {
      if (!Array.isArray(report.failureObservations)) return malformed()
      failureObservations = report.failureObservations.map(input => {
        const item = decodeOrderedItem(input)
        if (item.kind !== 'failure-observation') return malformed()
        return item
      })
    }
    let failureObservationsOmitted:
      | NonNullable<NonNullable<OrderedNotificationCleanup['discarded']>['failureObservationsOmitted']>
      | undefined
    if (report.failureObservationsOmitted !== undefined) {
      if (!Array.isArray(report.failureObservationsOmitted)) return malformed()
      failureObservationsOmitted = report.failureObservationsOmitted.map(input => {
        const item = decodeOrderedItem(input)
        if (item.kind !== 'failure-observations-omitted') return malformed()
        return item
      })
    }
    discarded = {
      values: decimal(report.values),
      bytes: decimal(report.bytes),
      controls: decimal(report.controls),
      uncertainTail: report.uncertainTail,
      ...(terminalItem === undefined ? {} : { terminal: terminalItem }),
      ...(losses === undefined ? {} : { losses }),
      ...(trustedValues === undefined ? {} : { trustedValues }),
      ...(markers === undefined ? {} : { markers }),
      ...(failureObservations === undefined ? {} : { failureObservations }),
      ...(failureObservationsOmitted === undefined ? {} : { failureObservationsOmitted })
    }
  }
  return {
    group: cleanup(value.group),
    members: value.members.map(member => {
      if (!object(member)) return malformed()
      exact(member, ['memberId', 'cleanup'])
      return { memberId: string(member.memberId), cleanup: cleanup(member.cleanup) }
    }),
    ...(discarded === undefined ? {} : { discarded })
  }
}

interface TauriOrderedStatusMember {
  readonly memberId: string
  readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
  readonly effectiveDelivery: 'notification' | 'indication' | 'unknown'
  readonly cleanupReleased: boolean
  readonly cleanupError: NormalizedBleError | null
}

/** Tauri status retains normalized errors as structured data. */
export function decodeTauriOrderedStatus(input: string, members: readonly string[]) {
  let status: unknown
  try {
    status = JSON.parse(input)
  } catch {
    return malformed()
  }
  if (
    !object(status) ||
    !('setupError' in status) ||
    !('fenceError' in status) ||
    !Array.isArray(status.members) ||
    status.members.length !== members.length
  )
    return malformed()
  exact(status, ['members', 'sourceGeneration', 'setupError', 'fenceError'])
  const entries = status.members.map((entry, index): TauriOrderedStatusMember => {
    if (!object(entry) || !('cleanupError' in entry) || entry.memberId !== String(index)) return malformed()
    exact(entry, ['memberId', 'acquisition', 'effectiveDelivery', 'cleanupReleased', 'cleanupError'])
    if (
      entry.acquisition !== 'not-started' &&
      entry.acquisition !== 'acquired' &&
      entry.acquisition !== 'indeterminate'
    )
      return malformed()
    if (
      entry.effectiveDelivery !== 'notification' &&
      entry.effectiveDelivery !== 'indication' &&
      entry.effectiveDelivery !== 'unknown'
    )
      return malformed()
    if (typeof entry.cleanupReleased !== 'boolean') return malformed()
    const memberId = members[index]
    if (memberId === undefined) return malformed()
    return {
      memberId,
      acquisition: entry.acquisition,
      effectiveDelivery: entry.effectiveDelivery,
      cleanupReleased: entry.cleanupReleased,
      cleanupError: orderedError(entry.cleanupError, 'ipc.ordered.native-status')
    }
  })
  return {
    members: entries,
    sourceGeneration: decodeSourceGeneration(status.sourceGeneration),
    setupError: orderedError(status.setupError, 'ipc.ordered.native-status'),
    fenceError: orderedError(status.fenceError, 'ipc.ordered.native-status')
  }
}
