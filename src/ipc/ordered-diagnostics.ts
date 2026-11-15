// src/ipc/ordered-diagnostics.ts

import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'
import { decodeDesktopOrderedItem } from '../backends/desktop/desktop-ordered-group'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import type {
  OrderedNotificationLoss,
  OrderedNotificationMarker,
  OrderedNotificationFailureObservation,
  OrderedNotificationFailureObservationsOmitted,
  OrderedNotificationTerminal
} from '../core/ordered-notification-contract'
import { decodeOrderedItem, decodePublicMarkerWriteStatus, decodeTauriOrderedStatus } from './ordered-codec'
import { decodeOrderedMarkerWriteStatus, type OrderedMarkerWriteStatus } from '../core/ordered-marker-wire'
import { parseOrderedRecordingStartResult } from '../core/ordered-recording-start'
import { decodeReclaimReceipt } from './manager'
import type { OrderedRecordingReclaimReceipt } from '../core/ordered-recording-reclaim'

export interface DesktopOrderedRecordingStartSummary {
  readonly startHandle: string
  readonly recordingId: string
  readonly state: 'pending' | 'result' | 'failed' | 'retired'
}

export interface DesktopOrderedOrphanSummary {
  readonly receiptId: string
  readonly values: string
  readonly bytes: string
  readonly controls: string
  readonly uncertainTail: boolean
}

export interface DesktopOrderedOrphanReceipt extends DesktopOrderedOrphanSummary {
  readonly receiptVersion: 1 | 2
  /** Exact payload-free receipt as received, including native status evidence. */
  readonly wireReceipt: string
  readonly terminal: OrderedNotificationTerminal | null
  readonly losses: readonly OrderedNotificationLoss[]
  readonly markers?: readonly OrderedNotificationMarker[]
  readonly markerWrites?: readonly DesktopOrderedMarkerWriteEvidence[]
  readonly failureObservations?: readonly OrderedNotificationFailureObservation[]
  readonly failureObservationsOmitted?: readonly OrderedNotificationFailureObservationsOmitted[]
  readonly trustedValues?: readonly {
    readonly memberId: string
    readonly ordinal: string
    readonly sourceMembership: { readonly member: string; readonly epoch: string }
    readonly bytes: string
  }[]
}
export interface DesktopOrderedMarkerWriteEvidence {
  readonly markerHandle: string
  readonly status: OrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }
}

export interface DesktopOrderedDiagnosticClient {
  request(request: {
    readonly command: string
    readonly payload: SerializableRecord
    readonly binaryPayload: null
    readonly signal: null
  }): Promise<{ readonly payload: SerializableRecord }>
}

function malformed(): never {
  throw contractError('protocol.malformed', 'ipc', 'ipc.ordered.orphan-receipt')
}
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}
function string(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0) return malformed()
  return value
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return malformed()
  return value
}
function markerWrite(value: unknown, metadataVersion: 1 | 2): DesktopOrderedMarkerWriteEvidence {
  if (!object(value) || Object.keys(value).some(key => !['markerHandle', 'statusFormat', 'status'].includes(key)))
    return malformed()
  const status =
    value.statusFormat === 'native'
      ? decodeOrderedMarkerWriteStatus(value.status, metadataVersion)
      : value.statusFormat === 'public'
        ? decodePublicMarkerWriteStatus(value.status)
        : malformed()
  return { markerHandle: decimal(value.markerHandle), status }
}

function decodeSummary(value: unknown): DesktopOrderedOrphanSummary {
  if (
    !object(value) ||
    (value.format !== 'public' && value.format !== 'native') ||
    typeof value.uncertainTail !== 'boolean'
  )
    return malformed()
  return {
    receiptId: string(value.receiptId),
    values: decimal(value.values),
    bytes: decimal(value.bytes),
    controls: decimal(value.controls),
    uncertainTail: value.uncertainTail
  }
}

function decodeReceipt(value: unknown): Omit<DesktopOrderedOrphanReceipt, 'wireReceipt'> {
  if (
    !object(value) ||
    (value.format !== 'public' && value.format !== 'native') ||
    typeof value.uncertainTail !== 'boolean' ||
    !Array.isArray(value.losses)
  )
    return malformed()
  // Legacy sealed receipts predate explicit versioning. Unknown versions or
  // fields must remain retained until a reader understands all their evidence.
  const rawVersion = value.receiptVersion === undefined ? 1 : value.receiptVersion
  if (rawVersion !== 1 && rawVersion !== 2) return malformed()
  const receiptVersion: 1 | 2 = rawVersion
  const keys = [
    'format',
    'receiptVersion',
    'receiptId',
    'values',
    'bytes',
    'controls',
    'uncertainTail',
    'terminal',
    'losses',
    'markers',
    'markerWrites',
    'trustedValues',
    ...(value.format === 'native'
      ? ['memberIds', 'nativeStatus', 'collectionId', 'connectionGeneration', 'databaseGeneration']
      : []),
    ...(receiptVersion === 2 ? ['failureObservations', 'failureObservationsOmitted'] : [])
  ]
  if (Object.keys(value).some(key => !keys.includes(key))) return malformed()
  if (
    receiptVersion === 2 &&
    (!Array.isArray(value.failureObservations) || !Array.isArray(value.failureObservationsOmitted))
  )
    return malformed()
  let decode: (input: unknown) => ReturnType<typeof decodeOrderedItem>
  let nativeMembers: readonly string[] | null = null
  if (value.format === 'public') decode = decodeOrderedItem
  else {
    if (
      !Array.isArray(value.memberIds) ||
      !value.memberIds.every(member => typeof member === 'string' && member.length > 0) ||
      typeof value.nativeStatus !== 'string'
    )
      return malformed()
    nativeMembers = value.memberIds
    const status = decodeTauriOrderedStatus(value.nativeStatus, value.memberIds)
    const context = {
      collectionId: decimal(value.collectionId),
      connectionGeneration: string(value.connectionGeneration),
      databaseGeneration: string(value.databaseGeneration),
      sourceGeneration: status.sourceGeneration,
      members: value.memberIds,
      byteCapacity: 0,
      metadataVersion: receiptVersion,
      lastOrdinal: null
    }
    decode = input => decodeDesktopOrderedItem(input, context)
  }
  const terminal = value.terminal === null ? null : decode(value.terminal)
  if (terminal !== null && terminal.kind !== 'terminal') return malformed()
  const losses = value.losses.map(input => {
    const item = decode(input)
    if (item.kind !== 'loss') return malformed()
    return item
  })
  let markers: OrderedNotificationMarker[] | undefined
  if (value.markers !== undefined) {
    if (!Array.isArray(value.markers)) return malformed()
    markers = value.markers.map(input => {
      const item = decode(input)
      if (item.kind !== 'marker') return malformed()
      return item
    })
  }
  let failureObservations: OrderedNotificationFailureObservation[] | undefined
  let failureObservationsOmitted: OrderedNotificationFailureObservationsOmitted[] | undefined
  if (receiptVersion === 2) {
    if (!Array.isArray(value.failureObservations) || !Array.isArray(value.failureObservationsOmitted))
      return malformed()
    failureObservations = value.failureObservations.map(input => {
      const item = decode(input)
      if (item.kind !== 'failure-observation') return malformed()
      return item
    })
    failureObservationsOmitted = value.failureObservationsOmitted.map(input => {
      const item = decode(input)
      if (item.kind !== 'failure-observations-omitted') return malformed()
      return item
    })
  }
  let markerWrites: DesktopOrderedMarkerWriteEvidence[] | undefined
  if (value.markerWrites !== undefined) {
    if (!Array.isArray(value.markerWrites) || value.markerWrites.length > 1) return malformed()
    markerWrites = value.markerWrites.map(entry => markerWrite(entry, receiptVersion))
  }
  let trustedValues: DesktopOrderedOrphanReceipt['trustedValues']
  if (value.trustedValues !== undefined) {
    if (!Array.isArray(value.trustedValues)) return malformed()
    trustedValues = value.trustedValues.map(entry => {
      if (!object(entry) || !object(entry.sourceMembership)) return malformed()
      if (
        Object.keys(entry).some(key => !['memberId', 'ordinal', 'sourceMembership', 'bytes'].includes(key)) ||
        Object.keys(entry.sourceMembership).some(key => !['member', 'epoch'].includes(key))
      )
        return malformed()
      let memberId = string(entry.memberId)
      if (nativeMembers !== null) {
        const nativeId = decimal(memberId)
        const index = Number(nativeId)
        const mapped = nativeMembers[index]
        if (!Number.isSafeInteger(index) || String(index) !== nativeId || mapped === undefined) return malformed()
        memberId = mapped
      }
      return {
        memberId,
        ordinal: decimal(entry.ordinal),
        sourceMembership: {
          member: decimal(entry.sourceMembership.member),
          epoch: decimal(entry.sourceMembership.epoch)
        },
        bytes: decimal(entry.bytes)
      }
    })
  }
  return {
    receiptVersion,
    receiptId: string(value.receiptId),
    values: decimal(value.values),
    bytes: decimal(value.bytes),
    controls: decimal(value.controls),
    uncertainTail: value.uncertainTail,
    terminal,
    losses,
    ...(failureObservations === undefined ? {} : { failureObservations }),
    ...(failureObservationsOmitted === undefined ? {} : { failureObservationsOmitted }),
    ...(markers === undefined ? {} : { markers }),
    ...(markerWrites === undefined ? {} : { markerWrites }),
    ...(trustedValues === undefined ? {} : { trustedValues })
  }
}

/** Desktop-only diagnostics for the authenticated client/window bound to this IPC client. */
export function createDesktopOrderedDiagnostics(client: DesktopOrderedDiagnosticClient) {
  const reclaimRetirementProgress = new Map<string, number>()
  const decodedReceipts = new Set<string>()
  let lastAcknowledgedReceipt: string | null = null
  return {
    async revokedRecordingReclaims(
      after: string | null = null,
      limit = 64
    ): Promise<{
      readonly transactions: readonly { readonly reclaimHandle: string; readonly state: 'revoked' }[]
      readonly truncated: boolean
      readonly nextCursor: string | null
    }> {
      if (!Number.isInteger(limit) || limit < 1 || limit > 64) return malformed()
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.reclaim.pending',
        payload: { after: after === null ? null : decimal(after), limit },
        binaryPayload: null,
        signal: null
      })
      if (
        !Array.isArray(payload.transactions) ||
        payload.transactions.length > limit ||
        typeof payload.truncated !== 'boolean'
      )
        return malformed()
      const transactions = payload.transactions.map(value => {
        if (!object(value) || value.state !== 'revoked') return malformed()
        return { reclaimHandle: decimal(value.reclaimHandle), state: 'revoked' as const }
      })
      const nextCursor = payload.nextCursor === null ? null : decimal(payload.nextCursor)
      if (payload.truncated !== (nextCursor !== null)) return malformed()
      return { transactions, truncated: payload.truncated, nextCursor }
    },
    async reclaimCleanup(
      reclaimHandle: string,
      action: 'status' | 'cancel' | 'release'
    ): Promise<OrderedRecordingReclaimReceipt> {
      const { payload } = await client.request({
        command: `gatt.ordered-recording.reclaim.${action}`,
        payload: { reclaimHandle: decimal(reclaimHandle) },
        binaryPayload: null,
        signal: null
      })
      return decodeReclaimReceipt(payload.status)
    },
    async retireRevokedReclaim(reclaimHandle: string): Promise<void> {
      const handle = decimal(reclaimHandle)
      const phases = [
        ['retire', 'retired'],
        ['ack-retirement', 'acknowledged'],
        ['forget-retirement', 'forgotten'],
        ['confirm-forgotten', 'confirmed']
      ] as const
      let phase = reclaimRetirementProgress.get(handle) ?? 0
      while (phase < phases.length) {
        const entry = phases[phase]
        if (entry === undefined) return malformed()
        const [action, field] = entry
        const { payload } = await client.request({
          command: `gatt.ordered-recording.reclaim.${action}`,
          payload: { reclaimHandle: handle },
          binaryPayload: null,
          signal: null
        })
        if (payload[field] !== true) return malformed()
        phase += 1
        reclaimRetirementProgress.set(handle, phase)
      }
      reclaimRetirementProgress.delete(handle)
    },
    async recordingMarkers(startHandle: string): Promise<readonly DesktopOrderedMarkerWriteEvidence[]> {
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.markers',
        payload: { startHandle: decimal(startHandle) },
        binaryPayload: null,
        signal: null
      })
      if (!Array.isArray(payload.markers) || payload.markers.length > 1) return malformed()
      return payload.markers.map(entry => markerWrite(entry, 1))
    },
    async recordingMarkerStatus(
      startHandle: string,
      markerHandle: string
    ): Promise<DesktopOrderedMarkerWriteEvidence['status']> {
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.marker.status',
        payload: { startHandle: decimal(startHandle), markerHandle: decimal(markerHandle) },
        binaryPayload: null,
        signal: null
      })
      return markerWrite({ ...payload, markerHandle }, 1).status
    },
    async cancelRecordingMarker(
      startHandle: string,
      markerHandle: string
    ): Promise<DesktopOrderedMarkerWriteEvidence['status']> {
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.marker.cancel',
        payload: { startHandle: decimal(startHandle), markerHandle: decimal(markerHandle) },
        binaryPayload: null,
        signal: null
      })
      return markerWrite({ ...payload, markerHandle }, 1).status
    },
    async forgetRecordingMarker(startHandle: string, markerHandle: string): Promise<void> {
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.marker.forget',
        payload: { startHandle: decimal(startHandle), markerHandle: decimal(markerHandle) },
        binaryPayload: null,
        signal: null
      })
      if (payload.forgotten !== true) return malformed()
    },
    async recordingStarts(
      after: string | null = null,
      limit = 64
    ): Promise<{
      readonly starts: readonly DesktopOrderedRecordingStartSummary[]
      readonly truncated: boolean
      readonly nextCursor: string | null
    }> {
      if (after !== null) decimal(after)
      if (!Number.isSafeInteger(limit) || limit < 1 || limit > 64) return malformed()
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.starts',
        payload: { after, limit },
        binaryPayload: null,
        signal: null
      })
      if (!Array.isArray(payload.starts) || payload.starts.length > limit || typeof payload.truncated !== 'boolean')
        return malformed()
      const starts = payload.starts.map((entry): DesktopOrderedRecordingStartSummary => {
        if (
          !object(entry) ||
          (entry.state !== 'pending' &&
            entry.state !== 'result' &&
            entry.state !== 'failed' &&
            entry.state !== 'retired')
        )
          return malformed()
        return { startHandle: decimal(entry.startHandle), recordingId: string(entry.recordingId), state: entry.state }
      })
      const nextCursor = payload.nextCursor === null ? null : decimal(payload.nextCursor)
      if (
        payload.truncated !== (nextCursor !== null) ||
        (nextCursor !== null && starts.at(-1)?.startHandle !== nextCursor)
      )
        return malformed()
      return { starts, truncated: payload.truncated, nextCursor }
    },
    async recordingStartStatus(startHandle: string, recordingId: string, memberCount: number) {
      if (!Number.isSafeInteger(memberCount) || memberCount < 0 || memberCount > 128) return malformed()
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.status',
        payload: { startHandle: decimal(startHandle) },
        binaryPayload: null,
        signal: null
      })
      if (payload.state === 'pending') return { state: 'pending' as const }
      if (payload.state === 'unknown') return { state: 'unknown' as const }
      if (payload.state === 'failed') return { state: 'failed' as const }
      if (payload.state === 'retired') return { state: 'retired' as const }
      if (payload.state !== 'result') return malformed()
      return {
        state: 'result' as const,
        result: parseOrderedRecordingStartResult(payload.result, string(recordingId), memberCount)
      }
    },
    async retireRecordingStart(startHandle: string): Promise<void> {
      const { payload } = await client.request({
        command: 'gatt.ordered-recording.retire',
        payload: { startHandle: decimal(startHandle) },
        binaryPayload: null,
        signal: null
      })
      if (payload.retired !== true) return malformed()
    },
    async inspect(): Promise<{
      readonly receipts: readonly DesktopOrderedOrphanSummary[]
      readonly truncated: boolean
    }> {
      const { payload } = await client.request({
        command: 'gatt.ordered.orphans',
        payload: {},
        binaryPayload: null,
        signal: null
      })
      if (!Array.isArray(payload.receipts) || typeof payload.truncated !== 'boolean') return malformed()
      return { receipts: payload.receipts.map(decodeSummary), truncated: payload.truncated }
    },
    async read(receiptId: string): Promise<DesktopOrderedOrphanReceipt> {
      const id = string(receiptId)
      if (lastAcknowledgedReceipt === id) lastAcknowledgedReceipt = null
      decodedReceipts.delete(id)
      let offset = 0
      let encoded = ''
      for (;;) {
        const { payload } = await client.request({
          command: 'gatt.ordered.orphan-page',
          payload: { receiptId: id, offset },
          binaryPayload: null,
          signal: null
        })
        if (
          typeof payload.chunk !== 'string' ||
          typeof payload.nextOffset !== 'number' ||
          !Number.isSafeInteger(payload.nextOffset) ||
          typeof payload.done !== 'boolean'
        )
          return malformed()
        if (payload.done) {
          if (payload.chunk !== '' || payload.nextOffset !== offset) return malformed()
          let parsed: unknown
          try {
            parsed = JSON.parse(encoded)
          } catch {
            return malformed()
          }
          const receipt = decodeReceipt(parsed)
          if (receipt.receiptId !== id) return malformed()
          decodedReceipts.add(id)
          return { ...receipt, wireReceipt: encoded }
        }
        const count = Array.from(payload.chunk).length
        if (count === 0 || payload.nextOffset !== offset + count) return malformed()
        encoded += payload.chunk
        if (encoded.length > 16 * 1024 * 1024) throw contractError('stream.quota', 'ipc', 'ipc.ordered.orphan-size')
        offset = payload.nextOffset
      }
    },
    async acknowledge(receiptId: string): Promise<void> {
      const id = string(receiptId)
      if (lastAcknowledgedReceipt === id) return
      if (!decodedReceipts.has(id)) throw contractError('lifecycle.invalid-state', 'ipc', 'ipc.ordered.orphan-unread')
      const { payload } = await client.request({
        command: 'gatt.ordered.orphan-ack',
        payload: { receiptId: id },
        binaryPayload: null,
        signal: null
      })
      if (payload.state !== 'forgotten') return malformed()
      decodedReceipts.delete(id)
      lastAcknowledgedReceipt = id
    }
  }
}
