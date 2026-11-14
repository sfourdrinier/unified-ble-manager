// src/electron/ordered-marker-registry.ts

import { contractError } from '../backend-contract/errors'
import type { CharacteristicPath } from '../backend-contract/gatt'
import type { PublicOperationOptions } from '../backend-contract/operations'
import type { SerializableRecord } from '../backend-contract/primitives'
import { serializableRecordsEqual } from '../backend-contract/serializable'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import type { OrderedMarkerWriteHandle, OrderedMarkerWriteStatus } from '../core/ordered-marker-wire'
import { orderedWireRecord } from '../ipc/ordered-wire'

type Target = CharacteristicPath<string, string, string, string, string, 'current'>
type Handle = OrderedMarkerWriteHandle<Target>
interface Slot {
  readonly id: string
  readonly token: string
  readonly lease: string
  handle: Handle | null
  fingerprint: SerializableRecord | null
  pending: Promise<OrderedMarkerWriteStatus> | null
  cancelled: boolean
  started: boolean
  failure: { readonly error: unknown } | null
}
interface Owner {
  readonly slots: Map<string, Slot>
  retiredHighWater: bigint
}

/** Holds one source-owned marker reservation and transaction until evidence is consumed. */
export class ElectronOrderedMarkerRegistry {
  private readonly owners = new Map<string, Owner>()
  private nextId = 1n

  private owner(key: string): Owner {
    let owner = this.owners.get(key)
    if (!owner) {
      owner = { slots: new Map(), retiredHighWater: -1n }
      this.owners.set(key, owner)
    }
    return owner
  }

  reserve(key: string, token: string, lease = ''): SerializableRecord {
    if (!token || token.length > 256) throw contractError('argument.invalid', 'ipc', 'electron.ordered-marker.token')
    const owner = this.owner(key)
    for (const slot of owner.slots.values()) if (slot.token === token) return { markerHandle: slot.id }
    if (owner.slots.size !== 0) throw contractError('stream.quota', 'ipc', 'electron.ordered-marker.credit')
    if (this.nextId > 18446744073709551615n)
      throw contractError('stream.quota', 'ipc', 'electron.ordered-marker.identifiers')
    const id = String(this.nextId++)
    owner.slots.set(id, {
      id,
      token,
      lease,
      handle: null,
      fingerprint: null,
      pending: null,
      cancelled: false,
      started: false,
      failure: null
    })
    return { markerHandle: id }
  }

  retireUnusedForLease(lease: string): void {
    for (const [key, owner] of this.owners) {
      for (const [id, slot] of owner.slots) if (slot.lease === lease && slot.handle === null) owner.slots.delete(id)
      if (owner.slots.size === 0 && owner.retiredHighWater < 0n) this.owners.delete(key)
    }
  }

  private slot(key: string, id: string): Slot {
    if (!isCanonicalIngressOrdinal(id)) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.id')
    const slot = this.owners.get(key)?.slots.get(id)
    if (!slot) throw contractError('ownership.denied', 'ipc', 'electron.ordered-marker.owner')
    return slot
  }

  async start(
    key: string,
    id: string,
    fingerprint: SerializableRecord,
    create: () => Handle,
    target: Target,
    bytes: Readonly<Uint8Array>,
    withResponse: boolean,
    options: PublicOperationOptions
  ): Promise<SerializableRecord> {
    const slot = this.slot(key, id)
    if (slot.cancelled) return { statusFormat: 'public', status: { state: 'not-admitted', error: null } }
    if (slot.fingerprint !== null && !serializableRecordsEqual(slot.fingerprint, fingerprint))
      throw contractError('argument.invalid', 'ipc', 'electron.ordered-marker.retry')
    if (slot.pending !== null) return orderedWireRecord({ statusFormat: 'public', status: await slot.pending })
    if (slot.failure !== null) throw slot.failure.error
    if (slot.started && slot.handle !== null)
      return orderedWireRecord({ statusFormat: 'public', status: await slot.handle.status() })
    if (slot.handle === null) slot.handle = create()
    slot.fingerprint = fingerprint
    slot.started = true
    let work: Promise<OrderedMarkerWriteStatus>
    try {
      work = slot.handle.start(target, bytes, withResponse ? 'with-response' : 'without-response', options)
    } catch (error) {
      slot.failure = { error }
      throw error
    }
    slot.pending = work
    try {
      return orderedWireRecord({ statusFormat: 'public', status: await work })
    } catch (error) {
      slot.failure = { error }
      throw error
    } finally {
      if (slot.pending === work) slot.pending = null
    }
  }

  async status(key: string, id: string, cancel: boolean): Promise<SerializableRecord> {
    const slot = this.slot(key, id)
    if (slot.handle === null) {
      if (cancel) slot.cancelled = true
      return slot.cancelled
        ? { statusFormat: 'public', status: { state: 'not-admitted', error: null } }
        : { statusFormat: 'public', status: { state: 'idle' } }
    }
    return orderedWireRecord({
      statusFormat: 'public',
      status: cancel ? await slot.handle.cancel() : await slot.handle.status()
    })
  }

  async forget(
    key: string,
    id: string,
    acknowledged: (ordinal: string) => boolean = () => true
  ): Promise<SerializableRecord> {
    if (!isCanonicalIngressOrdinal(id)) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.id')
    const owner = this.owners.get(key)
    if (!owner) throw contractError('ownership.denied', 'ipc', 'electron.ordered-marker.owner')
    const slot = owner.slots.get(id)
    if (!slot) {
      if (BigInt(id) <= owner.retiredHighWater) return { forgotten: true }
      throw contractError('ownership.denied', 'ipc', 'electron.ordered-marker.owner')
    }
    if (slot.handle !== null) {
      const status = await slot.handle.status()
      if (status.state === 'admitted' && !acknowledged(status.ordinal))
        throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered-marker.unacknowledged')
      await slot.handle.forget()
    }
    owner.slots.delete(id)
    if (BigInt(id) > owner.retiredHighWater) owner.retiredHighWater = BigInt(id)
    return { forgotten: true }
  }

  hasActive(key: string): boolean {
    return (this.owners.get(key)?.slots.size ?? 0) > 0
  }
  releaseOwner(key: string): void {
    if (this.hasActive(key)) throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered-marker.active')
    this.owners.delete(key)
  }
  async snapshots(key: string): Promise<SerializableRecord[]> {
    const result: SerializableRecord[] = []
    const owner = this.owners.get(key)
    if (!owner) return result
    for (const slot of owner.slots.values()) {
      if (slot.handle === null) continue
      const status = await slot.handle.status()
      if (
        status.state === 'pending' ||
        (status.state === 'admitted' && (status.write.state === 'initiating' || status.write.state === 'started'))
      )
        throw contractError('lifecycle.invalid-state', 'cleanup', 'electron.ordered-marker.pending')
      if (status.state === 'idle' || status.state === 'retired') continue
      result.push(orderedWireRecord({ markerHandle: slot.id, statusFormat: 'public', status }))
    }
    return result
  }

  async list(key: string): Promise<SerializableRecord> {
    const owner = this.owners.get(key)
    const markers: SerializableRecord[] = []
    if (owner)
      for (const slot of owner.slots.values()) {
        const status =
          slot.handle === null
            ? slot.cancelled
              ? { state: 'not-admitted', error: null }
              : { state: 'idle' }
            : await slot.handle.status()
        markers.push(orderedWireRecord({ markerHandle: slot.id, statusFormat: 'public', status }))
      }
    return { markers }
  }

  async cancelAll(key: string): Promise<void> {
    const owner = this.owners.get(key)
    if (!owner) return
    for (const slot of owner.slots.values()) {
      if (slot.handle !== null) await slot.handle.cancel()
      else slot.cancelled = true
    }
  }
  async forgetAll(key: string): Promise<void> {
    const owner = this.owners.get(key)
    if (!owner) return
    for (const slot of owner.slots.values()) await this.forget(key, slot.id)
    this.owners.delete(key)
  }
}
