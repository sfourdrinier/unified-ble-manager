// src/core/ordered-native-stream.ts
import { contractError } from '../backend-contract/errors'
import type { BackendContractError, CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { StreamLimits } from '../backend-contract/streams'
import { capacity } from '../backend-contract/primitives'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import { accountOrderedDiscard } from './ordered-notification-stream'
import { decodeOrderedNativeItem } from './ordered-native-wire'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationStream,
  OrderedNotificationTerminal,
  OrderedSourceGeneration
} from './ordered-notification-contract'

export interface OrderedNativeStatus {
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
    readonly effectiveDelivery: 'notification' | 'indication' | 'unknown'
    readonly cleanupReleased: boolean
    readonly cleanupError: NormalizedBleError | null
  }[]
  readonly sourceGeneration: OrderedSourceGeneration | null
  readonly cleanupComplete: boolean
  readonly setupError: NormalizedBleError | null
  readonly fenceError: NormalizedBleError | null
}
export interface OrderedNativeDiscardReceipt {
  readonly receiptId: string
  readonly discardedDeliveryId: string
  readonly evidence: string
}
export interface OrderedNativeTransport {
  increaseByteCapacity?(
    generation: OrderedSourceGeneration,
    maxBytes: number
  ): Promise<import('./ordered-notification-contract').OrderedByteCapacityChange>
  pollOrderedGroup(options: {
    readonly peerId: string
    readonly collectionId: string
  }): Promise<{ readonly deliveryId?: string | null; readonly record: string }>
  acknowledgeOrderedDelivery(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<void>
  discardOrderedDelivery(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<OrderedNativeDiscardReceipt>
  pollOrderedDiscardReceipt(options: {
    readonly peerId: string
    readonly collectionId: string
  }): Promise<OrderedNativeDiscardReceipt | null | undefined>
  acknowledgeOrderedDiscardReceipt(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<void>
  closeOrderedGroup(options: {
    readonly peerId: string
    readonly collectionId: string
  }): Promise<'released' | 'pending-source' | 'pending-drain'>
  acknowledgeOrderedGroup(options: { readonly peerId: string; readonly collectionId: string }): Promise<void>
  forgetOrderedGroup(options: { readonly peerId: string; readonly collectionId: string }): Promise<void>
  status(): Promise<OrderedNativeStatus>
  normalizeError(error: unknown, operation: string): BackendContractError
}
function malformed(): never {
  throw contractError('protocol.malformed', 'boundary', 'ordered.native.wire')
}
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return malformed()
  return value
}
function membership(input: unknown) {
  if (!object(input)) return malformed()
  return { member: decimal(input.member), epoch: decimal(input.epoch) }
}
function parse(value: string): Record<string, unknown> {
  let result: unknown
  try {
    result = JSON.parse(value)
  } catch {
    return malformed()
  }
  if (!object(result)) return malformed()
  return result
}
export class OrderedNativeStream implements OrderedNotificationStream {
  private readonly queued: OrderedNotificationItem[] = []
  private claimed = false
  private ended = false
  private nativeEnded = false
  private serial: Promise<void> = Promise.resolve()
  private release: OrderedNotificationCleanup | null = null
  private finalReceipt: OrderedNotificationCleanup | null = null
  private groupAcknowledged = false
  private discardRequested = false
  private lastOrdinal: string | null = null
  private pendingDelivery: { readonly id: string; readonly item: OrderedNotificationItem } | null = null
  private unprocessedDeliveryId: string | null = null
  private pendingDiscard: {
    readonly receiptId: string
    readonly deliveryId: string
    readonly ordinal: string | null
    readonly evidence: NonNullable<OrderedNotificationCleanup['discarded']>
  } | null = null
  private readonly discardedNative: NonNullable<OrderedNotificationCleanup['discarded']>[] = []
  private sourceGeneration: OrderedSourceGeneration | null = null
  constructor(
    private currentLimits: StreamLimits,
    private readonly central: OrderedNativeTransport,
    private readonly peerId: string,
    private readonly collectionId: string,
    private readonly connectionGeneration: string,
    private readonly databaseGeneration: string,
    private readonly members: readonly string[],
    private readonly metadataVersion: 1 | 2 = 1
  ) {}
  get limits(): StreamLimits {
    return this.currentLimits
  }
  async increaseByteCapacity(
    maxBytes: number
  ): Promise<import('./ordered-notification-contract').OrderedByteCapacityChange> {
    return this.locked(async () => {
      if (this.sourceGeneration === null || this.central.increaseByteCapacity === undefined)
        throw contractError('capability.unsupported', 'gatt', 'ordered.native.increase-capacity')
      if (!Number.isSafeInteger(maxBytes) || maxBytes < this.currentLimits.byteCapacity)
        throw contractError('argument.invalid', 'gatt', 'ordered.native.increase-capacity')
      const result = await this.central.increaseByteCapacity(this.sourceGeneration, maxBytes)
      this.currentLimits = { ...this.currentLimits, byteCapacity: capacity(result.maxBytes) }
      return result
    })
  }
  setSourceGeneration(generation: OrderedSourceGeneration | null): void {
    this.sourceGeneration = generation
  }
  private async locked<T>(fn: () => Promise<T>): Promise<T> {
    const previous = this.serial
    let unlock: () => void = () => undefined
    this.serial = new Promise(resolve => {
      unlock = resolve
    })
    await previous
    try {
      return await fn()
    } finally {
      unlock()
    }
  }
  private async status(): Promise<OrderedNativeStatus> {
    try {
      return await this.central.status()
    } catch (error) {
      throw this.central.normalizeError(error, 'ordered.status')
    }
  }
  private discardReport(includeQueued = true): NonNullable<OrderedNotificationCleanup['discarded']> {
    const reports = includeQueued ? [...this.discardedNative, accountOrderedDiscard(this.queued)] : this.discardedNative
    let values = 0n
    let bytes = 0n
    let controls = 0n
    let retainedTerminal: OrderedNotificationTerminal | null = null
    const losses: OrderedNotificationLoss[] = []
    const markers: import('./ordered-notification-contract').OrderedNotificationMarker[] = []
    const failureObservations: import('./ordered-notification-contract').OrderedNotificationFailureObservation[] = []
    const failureObservationsOmitted: import('./ordered-notification-contract').OrderedNotificationFailureObservationsOmitted[] =
      []
    const trustedValues: NonNullable<NonNullable<OrderedNotificationCleanup['discarded']>['trustedValues']>[number][] =
      []
    for (const report of reports) {
      values += BigInt(report.values)
      bytes += BigInt(report.bytes)
      controls += BigInt(report.controls)
      if (report.terminal != null) retainedTerminal = report.terminal
      if (report.losses !== undefined) losses.push(...report.losses)
      if (report.markers !== undefined) markers.push(...report.markers)
      if (report.failureObservations !== undefined) failureObservations.push(...report.failureObservations)
      if (report.failureObservationsOmitted !== undefined)
        failureObservationsOmitted.push(...report.failureObservationsOmitted)
      if (report.trustedValues !== undefined) trustedValues.push(...report.trustedValues)
    }
    return {
      values: values.toString(),
      bytes: bytes.toString(),
      controls: controls.toString(),
      uncertainTail: retainedTerminal?.uncertainTail ?? true,
      terminal: retainedTerminal,
      losses,
      markers,
      ...(failureObservations.length === 0 ? {} : { failureObservations }),
      ...(failureObservationsOmitted.length === 0 ? {} : { failureObservationsOmitted }),
      trustedValues
    }
  }
  private async settleUnprocessedDelivery(): Promise<void> {
    const discard = this.central.discardOrderedDelivery
    const pollReceipt = this.central.pollOrderedDiscardReceipt
    const acknowledge = this.central.acknowledgeOrderedDiscardReceipt
    if (discard === undefined || pollReceipt === undefined || acknowledge === undefined) return malformed()
    if (this.pendingDiscard === null) {
      if (this.unprocessedDeliveryId === null) return
      let receipt
      try {
        receipt = await pollReceipt.call(this.central, { peerId: this.peerId, collectionId: this.collectionId })
        if (receipt == null)
          receipt = await discard.call(this.central, {
            peerId: this.peerId,
            collectionId: this.collectionId,
            deliveryId: this.unprocessedDeliveryId
          })
      } catch (error) {
        throw this.central.normalizeError(error, 'ordered.native.discard')
      }
      if (!object(receipt) || typeof receipt.evidence !== 'string') return malformed()
      const receiptId = decimal(receipt.receiptId)
      const deliveryId = decimal(receipt.discardedDeliveryId)
      if (deliveryId !== this.unprocessedDeliveryId) return malformed()
      const evidence = parse(receipt.evidence)
      let report: NonNullable<OrderedNotificationCleanup['discarded']>
      let discardedOrdinal: string | null = null
      if (evidence.kind === 'value') {
        const member = decimal(evidence.memberId)
        const index = Number(member)
        if (!Number.isSafeInteger(index) || String(index) !== member || this.members[index] === undefined)
          return malformed()
        const ordinal = decimal(evidence.ordinal)
        const sourceMembership = membership(evidence.sourceMembership)
        if (
          this.lastOrdinal !== null &&
          (ordinal.length < this.lastOrdinal.length ||
            (ordinal.length === this.lastOrdinal.length && ordinal <= this.lastOrdinal))
        )
          return malformed()
        const byteCount = decimal(evidence.bytes)
        report = {
          values: '1',
          bytes: byteCount,
          controls: '0',
          uncertainTail: true,
          trustedValues: [{ memberId: this.members[index], ordinal, sourceMembership, bytes: byteCount }]
        }
        discardedOrdinal = ordinal
      } else {
        let item = decodeOrderedNativeItem(evidence, {
          collectionId: this.collectionId,
          connectionGeneration: this.connectionGeneration,
          databaseGeneration: this.databaseGeneration,
          sourceGeneration: this.sourceGeneration,
          members: this.members,
          metadataVersion: this.metadataVersion,
          byteCapacity: this.limits.byteCapacity,
          lastOrdinal: this.lastOrdinal
        })
        if (item.kind === 'value') return malformed()
        if (item.kind === 'marker') discardedOrdinal = item.ordinal
        if (item.kind === 'terminal') {
          const inventory = await this.status()
          if (inventory.fenceError !== null)
            item = {
              ...item,
              uncertainTail: true,
              fenceError: inventory.fenceError
            }
        }
        report = accountOrderedDiscard([item])
      }
      this.pendingDiscard = { receiptId, deliveryId, ordinal: discardedOrdinal, evidence: report }
    }
    const pending = this.pendingDiscard
    try {
      await acknowledge.call(this.central, {
        peerId: this.peerId,
        collectionId: this.collectionId,
        deliveryId: pending.receiptId
      })
    } catch (error) {
      throw this.central.normalizeError(error, 'ordered.native.discard-ack')
    }
    this.discardedNative.push(pending.evidence)
    if (pending.ordinal !== null) this.lastOrdinal = pending.ordinal
    if (pending.evidence.terminal != null) this.nativeEnded = true
    this.pendingDiscard = null
    this.unprocessedDeliveryId = null
  }
  private async poll(): Promise<OrderedNotificationItem | null> {
    const poll = this.central.pollOrderedGroup
    if (poll === undefined) return malformed()
    if (this.pendingDelivery === null) {
      let envelope: { readonly deliveryId?: string | null; readonly record: string }
      try {
        envelope = await poll.call(this.central, { peerId: this.peerId, collectionId: this.collectionId })
      } catch (error) {
        throw this.central.normalizeError(error, 'ordered.native.poll')
      }
      if (!object(envelope)) return malformed()
      const id = envelope.deliveryId == null ? null : decimal(envelope.deliveryId)
      if (this.unprocessedDeliveryId !== null && id !== this.unprocessedDeliveryId) return malformed()
      this.unprocessedDeliveryId = id
      if (typeof envelope.record !== 'string') return malformed()
      const record = parse(envelope.record)
      if (record.kind === 'empty') {
        if (id !== null) return malformed()
        return null
      }
      if (record.kind === 'closed' || id === null) return malformed()
      let item = decodeOrderedNativeItem(record, {
        collectionId: this.collectionId,
        connectionGeneration: this.connectionGeneration,
        databaseGeneration: this.databaseGeneration,
        sourceGeneration: this.sourceGeneration,
        members: this.members,
        metadataVersion: this.metadataVersion,
        byteCapacity: this.limits.byteCapacity,
        lastOrdinal: this.lastOrdinal
      })
      if (item.kind === 'terminal') {
        const inventory = await this.status()
        if (inventory.fenceError !== null)
          item = {
            ...item,
            uncertainTail: true,
            fenceError: inventory.fenceError
          }
      }
      this.pendingDelivery = { id, item }
      this.unprocessedDeliveryId = null
    }
    const delivery = this.pendingDelivery
    const acknowledge = this.central.acknowledgeOrderedDelivery
    if (acknowledge === undefined) return malformed()
    try {
      await acknowledge.call(this.central, {
        peerId: this.peerId,
        collectionId: this.collectionId,
        deliveryId: delivery.id
      })
    } catch (error) {
      throw this.central.normalizeError(error, 'ordered.native.delivery-ack')
    }
    this.pendingDelivery = null
    const item = delivery.item
    if (item.kind === 'terminal') this.nativeEnded = true
    if (item.kind === 'value' || item.kind === 'marker') this.lastOrdinal = item.ordinal
    return item
  }
  private async finishReceipt(): Promise<OrderedNotificationCleanup> {
    if (this.finalReceipt === null) return malformed()
    const receipt = this.discardRequested
      ? { ...this.finalReceipt, discarded: this.discardReport() }
      : this.finalReceipt
    try {
      if (!this.groupAcknowledged) {
        await this.central.acknowledgeOrderedGroup({ peerId: this.peerId, collectionId: this.collectionId })
        this.groupAcknowledged = true
      }
      await this.central.forgetOrderedGroup({ peerId: this.peerId, collectionId: this.collectionId })
    } catch (error) {
      const failed: OrderedNotificationCleanup = {
        ...receipt,
        group: {
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'ordered-group-receipt',
              error: this.central.normalizeError(error, 'ordered.receipt').normalized
            }
          ]
        }
      }
      return this.discardRequested
        ? { ...failed, discarded: { ...this.discardReport(false), uncertainTail: true } }
        : failed
    }
    if (this.discardRequested) this.queued.length = 0
    this.release = receipt
    return receipt
  }
  async close(): Promise<OrderedNotificationCleanup> {
    return this.locked(async () => {
      if (this.release !== null) {
        if (this.discardRequested && this.release.discarded === undefined) {
          this.release = { ...this.release, discarded: this.discardReport() }
          this.queued.length = 0
        }
        return this.release
      }
      if (this.finalReceipt !== null) return this.finishReceipt()
      const close = this.central.closeOrderedGroup
      if (close === undefined) return malformed()
      let outcome: 'released' | 'pending-source' | 'pending-drain' = 'pending-source'
      let closeError: BackendContractError | null = null
      try {
        outcome = await close.call(this.central, { peerId: this.peerId, collectionId: this.collectionId })
        if (outcome !== 'released' && outcome !== 'pending-source' && outcome !== 'pending-drain') return malformed()
        if (outcome === 'pending-drain') {
          while (!this.nativeEnded) {
            const item = await this.poll()
            if (item === null) break
            this.queued.push(item)
          }
          outcome = await close.call(this.central, { peerId: this.peerId, collectionId: this.collectionId })
          if (outcome !== 'released' && outcome !== 'pending-source' && outcome !== 'pending-drain') return malformed()
        }
      } catch (error) {
        closeError = this.central.normalizeError(error, 'ordered.native.close')
      }
      const inventory = await this.status()
      const memberCleanup = inventory.members.map(member => ({
        memberId: member.memberId,
        cleanup: member.cleanupReleased
          ? { state: 'released' as const, failures: [] }
          : {
              state: 'release-failed' as const,
              failures: [
                {
                  resourceKind: 'ordered-member',
                  error:
                    member.cleanupError === null
                      ? contractError('lifecycle.invalid-state', 'cleanup', 'ordered.native.member-pending').normalized
                      : member.cleanupError
                }
              ]
            }
      }))
      if (outcome === 'released' && closeError === null) {
        if (!inventory.cleanupComplete || !memberCleanup.every(member => member.cleanup.state === 'released'))
          return malformed()
        this.finalReceipt = { group: { state: 'released', failures: [] }, members: memberCleanup }
        return this.finishReceipt()
      }
      const cleanup: CleanupRecord = {
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'ordered-group',
            error:
              closeError?.normalized ??
              contractError('lifecycle.invalid-state', 'cleanup', `ordered.native.${outcome}`).normalized
          }
        ]
      }
      const result = { group: cleanup, members: memberCleanup }
      return this.discardRequested
        ? { ...result, discarded: { ...this.discardReport(false), uncertainTail: true } }
        : result
    })
  }
  async discard(): Promise<OrderedNotificationCleanup> {
    this.discardRequested = true
    this.ended = true
    let result = await this.close()
    if (this.unprocessedDeliveryId !== null || this.pendingDiscard !== null) {
      try {
        await this.locked(() => this.settleUnprocessedDelivery())
        result = await this.close()
      } catch (error) {
        const discardError = this.central.normalizeError(error, 'ordered.native.discard').normalized
        return {
          ...result,
          group: {
            state: 'release-failed',
            failures: [...result.group.failures, { resourceKind: 'ordered-discard', error: discardError }]
          },
          discarded: { ...this.discardReport(false), uncertainTail: true }
        }
      }
    }
    return result.discarded === undefined
      ? { ...result, discarded: { values: '0', bytes: '0', controls: '0', uncertainTail: true } }
      : result
  }
  async setupObservations(): Promise<{
    readonly observations: readonly (
      | import('./ordered-notification-contract').OrderedNotificationValue
      | OrderedNotificationLoss
      | import('./ordered-notification-contract').OrderedNotificationMarker
    )[]
    readonly error: NormalizedBleError | null
  }> {
    return this.locked(async () => {
      const observations: (
        | import('./ordered-notification-contract').OrderedNotificationValue
        | OrderedNotificationLoss
        | import('./ordered-notification-contract').OrderedNotificationMarker
      )[] = []
      while (this.queued.length > 0) {
        const item = this.queued[0]
        if (
          item === undefined ||
          item.kind === 'terminal' ||
          item.kind === 'failure-observation' ||
          item.kind === 'failure-observations-omitted'
        )
          break
        this.queued.shift()
        observations.push(item)
      }
      try {
        while (this.queued.length === 0 && !this.nativeEnded) {
          const item = await this.poll()
          if (item === null) break
          if (
            item.kind === 'terminal' ||
            item.kind === 'failure-observation' ||
            item.kind === 'failure-observations-omitted'
          ) {
            this.queued.push(item)
            break
          }
          observations.push(item)
        }
      } catch (error) {
        return {
          observations,
          error: this.central.normalizeError(error, 'ordered.native.setup-observations').normalized
        }
      }
      return { observations, error: null }
    })
  }
  [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined> {
    if (this.claimed) throw contractError('lifecycle.invalid-state', 'stream', 'ordered.native.iterator')
    this.claimed = true
    return {
      next: async () => {
        for (;;) {
          const result = await this.locked(async () => {
            if (this.ended) return undefined
            const queued = this.queued.shift()
            if (queued !== undefined) return queued
            if (this.nativeEnded) return undefined
            const item = await this.poll()
            if (this.ended && item !== null) {
              this.queued.push(item)
              return undefined
            }
            return item
          })
          if (result === undefined) return { done: true, value: undefined }
          if (result !== null) return { done: false, value: result }
          await new Promise(resolve => setTimeout(resolve, 10))
        }
      },
      return: async () => {
        await this.discard()
        return { done: true, value: undefined }
      }
    }
  }
}
