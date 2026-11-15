// src/electron/ordered-recording-recovery.ts
import { randomBytes } from 'node:crypto'
import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'
import type {
  OrderedRecordingRecoveryHandle,
  OrderedRecordingReclaimHandle,
  OrderedRecordingReclaimReceipt
} from '../core/ordered-recording-reclaim'
import { orderedWireRecord } from '../ipc/ordered-wire'

type Reclaim = OrderedRecordingReclaimHandle<OrderedRecordingReclaimReceipt>
interface Capability {
  readonly owner: string
  readonly startHandle: string
  readonly token: string
  readonly peer: string
  readonly recovery: OrderedRecordingRecoveryHandle
}
interface Transaction {
  readonly owner: string
  readonly id: string
  readonly token: string
  readonly reservation: string
  readonly database: string
  readonly lease: string
  readonly capability: Capability
  readonly handle: Reclaim
  pending: Promise<SerializableRecord> | null
  retired: boolean
  acknowledged: boolean
  forgotten: boolean
  committed: boolean
  revoked: boolean
}
const randomToken = () => randomBytes(32).toString('hex')

/** Process-scoped capabilities and exact transactions, scoped to authenticated client and window. */
export class ElectronOrderedRecordingRecovery {
  private nextId = 1n
  private readonly capabilities = new Map<string, Capability>()
  private readonly transactions = new Map<string, Transaction>()
  private readonly finished = new Set<string>()
  private readonly mappings = new Map<string, { owner: string; database: string; lease: string; startHandle: string }>()
  constructor(private readonly capacity: number) {}

  issue(
    owner: string,
    startHandle: string,
    peer: string,
    recovery: () => OrderedRecordingRecoveryHandle
  ): SerializableRecord {
    for (const capability of this.capabilities.values())
      if (capability.owner === owner && capability.startHandle === startHandle)
        return { recoveryToken: capability.token }
    if (this.capabilities.size >= this.capacity)
      throw contractError('stream.quota', 'ipc', 'electron.recording-recovery.capacity')
    const capability = { owner, startHandle, peer, recovery: recovery(), token: randomToken() }
    this.capabilities.set(capability.token, capability)
    return { recoveryToken: capability.token }
  }

  reserve(
    owner: string,
    token: string,
    reservation: string,
    database: string,
    lease: string,
    peer: string,
    create: (handle: OrderedRecordingRecoveryHandle) => Reclaim
  ): SerializableRecord {
    const capability = this.capabilities.get(token)
    if (!capability || capability.owner !== owner || capability.peer !== peer)
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.owner')
    if (!reservation || reservation.length > 256)
      throw contractError('argument.invalid', 'ipc', 'electron.recording-recovery.reservation')
    for (const transaction of this.transactions.values()) {
      if (transaction.owner !== owner || transaction.reservation !== reservation) continue
      if (transaction.token !== token || transaction.database !== database || transaction.lease !== lease)
        throw contractError('argument.invalid', 'ipc', 'electron.recording-recovery.retry')
      return { reclaimHandle: transaction.id }
    }
    if (this.transactions.size + this.mappings.size >= this.capacity)
      throw contractError('stream.quota', 'ipc', 'electron.recording-recovery.capacity')
    if (this.nextId > 18446744073709551615n)
      throw contractError('stream.quota', 'ipc', 'electron.recording-recovery.identifiers')
    const id = String(this.nextId++)
    const transaction: Transaction = {
      owner,
      id,
      token,
      reservation,
      database,
      lease,
      capability,
      handle: create(capability.recovery),
      pending: null,
      retired: false,
      acknowledged: false,
      forgotten: false,
      committed: false,
      revoked: false
    }
    this.transactions.set(id, transaction)
    return { reclaimHandle: id }
  }

  private transaction(owner: string, id: string): Transaction {
    const transaction = this.transactions.get(id)
    if (!transaction || transaction.owner !== owner)
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.owner')
    return transaction
  }

  async action(
    owner: string,
    id: string,
    lease: string,
    action: 'start' | 'status' | 'commit' | 'cancel' | 'release' | 'retire'
  ): Promise<SerializableRecord> {
    const transaction = this.transaction(owner, id)
    if (transaction.lease !== lease && (!transaction.revoked || action === 'start' || action === 'commit'))
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.lease')
    if (transaction.pending) {
      await transaction.pending
      return this.action(owner, id, lease, action)
    }
    if (transaction.retired) {
      if (action === 'retire') return { retired: true }
      if (action === 'status') return { status: { state: 'retired' } }
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.recording-recovery.retired')
    }
    const run = async (): Promise<SerializableRecord> => {
      if (action === 'retire') {
        await transaction.handle.retire()
        transaction.retired = true
        return { retired: true }
      }
      const status = await transaction.handle[action]()
      if ('mappingCommitted' in status && status.mappingCommitted && !transaction.revoked) {
        transaction.committed = true
        this.mappings.set(id, {
          owner,
          database: transaction.database,
          lease,
          startHandle: transaction.capability.startHandle
        })
      }
      return orderedWireRecord({ status })
    }
    const pending = run()
    transaction.pending = pending
    try {
      return await pending
    } finally {
      if (transaction.pending === pending) transaction.pending = null
    }
  }

  markerOwner(owner: string, id: string, database: string, lease: string): string {
    const mapping = this.mappings.get(id)
    if (!mapping || mapping.owner !== owner || mapping.database !== database || mapping.lease !== lease)
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.mapping')
    return mapping.startHandle
  }

  acknowledge(owner: string, id: string, lease: string): SerializableRecord {
    const transaction = this.transaction(owner, id)
    if (transaction.lease !== lease && !transaction.revoked)
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.lease')
    if (!transaction.retired)
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.recording-recovery.retirement')
    transaction.acknowledged = true
    return { acknowledged: true }
  }

  forget(owner: string, id: string, lease: string): SerializableRecord {
    const transaction = this.transactions.get(id)
    if (!transaction) return { forgotten: true }
    if (transaction.owner !== owner) throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.owner')
    if (transaction.lease !== lease && !transaction.revoked)
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.lease')
    if (!transaction.acknowledged)
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.recording-recovery.acknowledge')
    transaction.forgotten = true
    return { forgotten: true }
  }

  confirmForgotten(owner: string, id: string, lease: string): SerializableRecord {
    const transaction = this.transactions.get(id)
    if (!transaction) return { confirmed: true }
    if (transaction.owner !== owner || (transaction.lease !== lease && !transaction.revoked))
      throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.owner')
    if (!transaction.forgotten)
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.recording-recovery.forget')
    this.transactions.delete(id)
    if (this.finished.has(transaction.capability.token)) this.forgetRecording(owner, transaction.capability.startHandle)
    return { confirmed: true }
  }

  hasMapping(owner: string, startHandle: string, lease: string, database?: string): boolean {
    for (const mapping of this.mappings.values())
      if (
        mapping.owner === owner &&
        mapping.startHandle === startHandle &&
        mapping.lease === lease &&
        (database === undefined || mapping.database === database)
      )
        return true
    return false
  }

  pending(owner: string, after: string | null, limit: number): SerializableRecord {
    if (after !== null && (!/^(0|[1-9][0-9]*)$/.test(after) || BigInt(after) > 18446744073709551615n))
      throw contractError('argument.invalid', 'ipc', 'electron.recording-recovery.cursor')
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 64)
      throw contractError('argument.invalid', 'ipc', 'electron.recording-recovery.limit')
    const transactions: SerializableRecord[] = []
    for (const transaction of this.transactions.values()) {
      if (
        transaction.owner !== owner ||
        !transaction.revoked ||
        (after !== null && BigInt(transaction.id) <= BigInt(after))
      )
        continue
      transactions.push({ reclaimHandle: transaction.id, state: 'revoked' })
      if (transactions.length > limit) break
    }
    const truncated = transactions.length > limit
    if (truncated) transactions.pop()
    const last = transactions[transactions.length - 1]
    return { transactions, truncated, nextCursor: truncated && last ? (last.reclaimHandle ?? null) : null }
  }

  releaseLease(lease: string): void {
    for (const [id, mapping] of this.mappings) if (mapping.lease === lease) this.mappings.delete(id)
    for (const transaction of this.transactions.values()) if (transaction.lease === lease) transaction.revoked = true
  }

  forgetRecording(owner: string, startHandle: string): void {
    for (const [id, mapping] of this.mappings)
      if (mapping.owner === owner && mapping.startHandle === startHandle) this.mappings.delete(id)
    for (const [token, capability] of this.capabilities) {
      if (capability.owner !== owner || capability.startHandle !== startHandle) continue
      if ([...this.transactions.values()].some(transaction => transaction.capability === capability)) {
        this.finished.add(token)
        continue
      }
      this.finished.delete(token)
      this.capabilities.delete(token)
    }
  }
}
