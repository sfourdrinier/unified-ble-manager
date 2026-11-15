// src/electron/ordered-recording-registry.ts

import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'
import { serializableRecordsEqual } from '../backend-contract/serializable'
import type { DiscoveredGattDatabase } from '../manager/ble-manager'
import type { HostNeutralBackendIdentity } from '../backend-contract/identity'
import type { OrderedRecordingStartResult } from '../core/ordered-recording-start'
import { orderedWireRecord } from '../ipc/ordered-wire'

type Database = DiscoveredGattDatabase<string, HostNeutralBackendIdentity<string>>
type RecordingHandle = ReturnType<Database['reserveOrderedRecordingStart']>
interface Reservation {
  readonly owner: string
  readonly token: string
  readonly lease: string
  readonly id: string
  handle: RecordingHandle | null
  fingerprint: SerializableRecord | null
  pending: Promise<OrderedRecordingStartResult> | null
  result: OrderedRecordingStartResult | null
  recordingId: string | null
  peerId: string | null
  retired: boolean
  failed: boolean
  finished: boolean
}

/** Process-owned start receipts remain addressable after renderer revocation. */
export class ElectronOrderedRecordingRegistry {
  private readonly slots = new Map<string, Reservation>()
  private nextId = 1n

  constructor(private readonly capacity: number) {}

  reserve(owner: string, lease: string, token: string): SerializableRecord {
    if (!owner || !token || token.length > 256)
      throw contractError('argument.invalid', 'ipc', 'electron.ordered-recording.token')
    for (const slot of this.slots.values())
      if (slot.owner === owner && slot.token === token) return { startHandle: slot.id }
    if (this.slots.size >= this.capacity)
      throw contractError('stream.quota', 'ipc', 'electron.ordered-recording.capacity')
    if (this.nextId > 18446744073709551615n)
      throw contractError('stream.quota', 'ipc', 'electron.ordered-recording.identifiers')
    const id = String(this.nextId++)
    this.slots.set(id, {
      owner,
      lease,
      token,
      id,
      handle: null,
      fingerprint: null,
      pending: null,
      result: null,
      recordingId: null,
      peerId: null,
      retired: false,
      failed: false,
      finished: false
    })
    return { startHandle: id }
  }

  private slot(owner: string, id: string): Reservation {
    const slot = this.slots.get(id)
    if (slot === undefined || slot.owner !== owner)
      throw contractError('ownership.denied', 'ipc', 'electron.ordered-recording.owner')
    return slot
  }

  async start(
    owner: string,
    id: string,
    recordingId: string,
    peerId: string,
    fingerprint: SerializableRecord,
    create: () => RecordingHandle,
    start: (handle: RecordingHandle) => Promise<OrderedRecordingStartResult>
  ): Promise<SerializableRecord> {
    const slot = this.slot(owner, id)
    if (slot.retired) throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered-recording.retired')
    if (slot.fingerprint !== null && !serializableRecordsEqual(slot.fingerprint, fingerprint))
      throw contractError('argument.invalid', 'ipc', 'electron.ordered-recording.retry')
    if (slot.result !== null) return orderedWireRecord(slot.result)
    if (slot.pending !== null) return orderedWireRecord(await slot.pending)
    if (slot.handle === null) slot.handle = create()
    slot.fingerprint = fingerprint
    slot.recordingId = recordingId
    slot.peerId = peerId
    const work = start(slot.handle)
    slot.pending = work
    try {
      const result = await work
      slot.result = result
      return orderedWireRecord(result)
    } catch (error) {
      slot.failed = true
      throw error
    } finally {
      if (slot.pending === work) slot.pending = null
    }
  }

  assertOwned(owner: string, id: string): void {
    this.slot(owner, id)
  }

  assertMarkerOwner(owner: string, id: string): void {
    const slot = this.slot(owner, id)
    if (
      slot.finished ||
      slot.handle === null ||
      slot.result?.kind !== 'started' ||
      slot.result.receipt.state !== 'active'
    )
      throw contractError('lifecycle.invalid-state', 'gatt', 'electron.ordered-recording.marker-owner')
  }

  isOriginLease(owner: string, id: string, lease: string): boolean {
    return this.slot(owner, id).lease === lease
  }

  recovery(
    owner: string,
    id: string
  ): { peer: string; handle: import('../core/ordered-recording-reclaim').OrderedRecordingRecoveryHandle } {
    const slot = this.slot(owner, id)
    this.assertMarkerOwner(owner, id)
    const issue = slot.handle?.recoveryHandle
    if (!issue || slot.peerId === null)
      throw contractError('capability.unsupported', 'gatt', 'electron.ordered-recording.recovery')
    return { peer: slot.peerId, handle: issue.call(slot.handle) }
  }

  markerOwner(owner: string, recordingId: string, peerId: string): SerializableRecord {
    for (const slot of this.slots.values()) {
      if (slot.owner !== owner || slot.recordingId !== recordingId) continue
      this.markerRecordingId(owner, slot.id, peerId)
      return { startHandle: slot.id }
    }
    throw contractError('ownership.denied', 'ipc', 'electron.ordered-recording.marker-owner')
  }

  markerRecordingId(owner: string, id: string, peerId: string): string {
    const slot = this.slot(owner, id)
    if (slot.peerId !== peerId) throw contractError('ownership.denied', 'ipc', 'electron.ordered-recording.marker-peer')
    if (
      slot.finished ||
      slot.recordingId === null ||
      slot.handle === null ||
      slot.result?.kind !== 'started' ||
      slot.result.receipt.state !== 'active'
    )
      throw contractError('lifecycle.invalid-state', 'gatt', 'electron.ordered-recording.marker-owner')
    return slot.recordingId
  }

  async status(owner: string, id: string): Promise<SerializableRecord> {
    const slot = this.slots.get(id)
    if (slot === undefined) return { state: 'unknown' }
    if (slot.owner !== owner) throw contractError('ownership.denied', 'ipc', 'electron.ordered-recording.owner')
    if (slot.retired) return { state: 'retired' }
    if (slot.pending !== null) return { state: 'pending' }
    if (slot.result !== null) return orderedWireRecord({ state: 'result', result: slot.result })
    if (slot.handle === null) return { state: 'unknown' }
    const status = await slot.handle.status()
    const response = orderedWireRecord(status)
    if (
      status.state === 'result' &&
      this.slots.get(id) === slot &&
      !slot.retired &&
      slot.pending === null &&
      slot.result === null
    ) {
      slot.result = status.result
      slot.failed = false
    }
    return response
  }

  async retire(owner: string, id: string, canReclaim = true): Promise<SerializableRecord> {
    const slot = this.slots.get(id)
    if (slot === undefined) return { retired: true }
    if (slot.owner !== owner) throw contractError('ownership.denied', 'ipc', 'electron.ordered-recording.owner')
    if (slot.retired) {
      if (slot.finished && canReclaim) this.slots.delete(id)
      return { retired: true }
    }
    if (slot.pending !== null) await slot.pending
    if (slot.handle !== null) await slot.handle.retire()
    if (
      slot.recordingId === null ||
      (slot.finished && canReclaim) ||
      (slot.result?.kind === 'prepare-failed' && slot.result.leaseCleanup?.state === 'released')
    )
      this.slots.delete(id)
    else slot.retired = true
    return { retired: true }
  }

  retireUnused(owner: string, lease: string): void {
    for (const [id, slot] of this.slots)
      if (slot.owner === owner && slot.lease === lease && slot.handle === null) this.slots.delete(id)
  }

  list(owner: string, after: string | null, limit: number): SerializableRecord {
    if (after !== null && (!/^(0|[1-9][0-9]*)$/.test(after) || BigInt(after) > 18446744073709551615n))
      throw contractError('argument.invalid', 'ipc', 'electron.ordered-recording.cursor')
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 64)
      throw contractError('argument.invalid', 'ipc', 'electron.ordered-recording.limit')
    const entries: SerializableRecord[] = []
    for (const slot of this.slots.values()) {
      if (slot.owner !== owner || slot.recordingId === null || (after !== null && BigInt(slot.id) <= BigInt(after)))
        continue
      const state = slot.retired
        ? 'retired'
        : slot.pending !== null
          ? 'pending'
          : slot.result !== null
            ? 'result'
            : slot.failed
              ? 'failed'
              : 'pending'
      entries.push({ startHandle: slot.id, recordingId: slot.recordingId, state })
      if (entries.length > limit) break
    }
    const truncated = entries.length > limit
    if (truncated) entries.pop()
    const last = entries[entries.length - 1]
    return {
      starts: entries,
      truncated,
      nextCursor: truncated && last !== undefined ? (last.startHandle ?? null) : null
    }
  }

  has(owner: string, id: string): boolean {
    return this.slots.get(id)?.owner === owner
  }

  /** Trusted process owner calls this only after recording stop and host cleanup are confirmed. */
  confirmFinished(
    recordingId: string,
    canReclaim: (owner: string, id: string) => boolean = () => true,
    onFinished: (owner: string, id: string) => void = () => undefined
  ): void {
    for (const [id, slot] of this.slots) {
      if (slot.recordingId !== recordingId) continue
      slot.finished = true
      onFinished(slot.owner, id)
      if (slot.retired && canReclaim(slot.owner, id)) this.slots.delete(id)
    }
  }
}
