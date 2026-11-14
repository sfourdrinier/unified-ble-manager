// src/core/aggregate-stream-quota.ts

import { contractError } from '../backend-contract/errors'
import {
  CoreBoundedStream,
  type CoreStreamAdmissionAdjustment,
  type CoreStreamRetainedByteAdmission,
  type CoreStreamClaim,
  type CoreStreamTerminalClaim,
  type CoreStreamPushResult,
  type CoreStreamReplacementStage,
  type CoreStreamEviction,
  type CoreStreamOwnedValue
} from './bounded-stream'

/** A successor owner must retain this charge until its represented data is released. */
export interface AggregateStreamReservation {
  readonly byteLength: number
  release(): void
}

interface MutableReservation extends AggregateStreamReservation {
  reduce(bytes: number): void
}

interface SharedOccurrence<Value> {
  current: { readonly value: Value } | null
  remaining: number
  reservation: MutableReservation | null
  releaseBuffers?: () => void
  buffers?: readonly ArrayBufferLike[]
}

/** One obligation to a shared source occurrence. Release it on delivery or discard. */
export interface AggregateStreamReference<Value> {
  readonly value: Value
  release(): void
}

/** The source committed despite a cleanup failure; the references still own it. */
export class AggregateStreamSharedTransferError<Value> extends Error {
  constructor(
    readonly references: readonly AggregateStreamReference<Value>[],
    readonly cause: unknown
  ) {
    super('Shared stream ownership transferred, but delivering a pending item failed')
    this.name = 'AggregateStreamSharedTransferError'
  }
}

function makeSharedReferences<Value>(
  owner: SharedOccurrence<Value>,
  referenceBytes: number,
  referenceCount: number,
  register: (
    reference: AggregateStreamReference<Value>,
    owner: SharedOccurrence<Value>,
    referenceBytes: number,
    released: () => boolean
  ) => void
): readonly AggregateStreamReference<Value>[] {
  return Array.from({ length: referenceCount }, () => {
    let released = false
    const reference = Object.freeze({
      get value(): Value {
        if (released || owner.current === null) {
          throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.reference-released')
        }
        return owner.current.value
      },
      release: () => {
        if (released) return
        released = true
        owner.remaining -= 1
        const reservation = owner.reservation
        if (reservation === null) {
          throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.reference-owner')
        }
        if (owner.remaining === 0) {
          owner.current = null
          reservation.release()
          owner.releaseBuffers?.()
        } else reservation.reduce(referenceBytes)
      }
    })
    register(reference, owner, referenceBytes, () => released)
    return reference
  })
}

/** The source was committed; the caller owns the reservation despite cleanup failure. */
export class AggregateStreamTransferError extends Error {
  constructor(
    readonly reservation: AggregateStreamReservation,
    readonly cause: unknown
  ) {
    super('Stream ownership transferred, but delivering a pending item failed')
    this.name = 'AggregateStreamTransferError'
  }
}

/** The terminal transfer committed, but at least one cleanup or wakeup failed. */
export class AggregateStreamTerminalTransferError extends Error {
  constructor(readonly cause: unknown) {
    super('Stream terminal transferred, but cleanup or delivery failed')
    this.name = 'AggregateStreamTerminalTransferError'
  }
}

interface QuotaTrackedStream {
  retainedBytes(): number
  retainedPayloadBytes(excludeReference?: (reference: CoreStreamOwnedValue<unknown>) => boolean): number
  /** Durable terminal evidence remains host-owned and intentionally sits
   * outside the bounded in-memory queue ceiling. */
  retainedTerminalEvidenceBytes(): number
  projectedRetainedBytes(byteLength: number, key: string | null, retainedBytes: number): number
  terminateForAggregateQuota(byteLength: number): CoreStreamPushResult
  tracksRetainedView(view: object): boolean
}

/**
 * Applies one aggregate retained-byte ceiling across registered core streams.
 * A quota breach terminates the producing stream with its one overflow terminal
 * rather than allowing a producer to allocate past its declared budget.
 */
export class AggregateStreamQuota {
  private readonly referenceMetadata = new WeakMap<
    AggregateStreamReference<unknown>,
    {
      readonly owner: SharedOccurrence<unknown>
      readonly referenceBytes: number
      readonly released: () => boolean
    }
  >()
  private readonly streams = new Set<QuotaTrackedStream>()
  private readonly reservations = new Set<AggregateStreamReservation>()
  private readonly admissions = new WeakMap<
    CoreStreamRetainedByteAdmission,
    {
      size(): number
      buffers(): readonly ArrayBufferLike[]
      set(bytes: number, buffers?: readonly ArrayBufferLike[]): void
    }
  >()
  private readonly sharedBuffers = new Map<ArrayBufferLike, number>()

  private bufferDelta(changes: readonly { buffers: readonly ArrayBufferLike[]; count: number }[]): number {
    const counts = new Map<ArrayBufferLike, number>()
    for (const change of changes)
      for (const buffer of new Set(change.buffers)) {
        counts.set(buffer, (counts.get(buffer) ?? this.sharedBuffers.get(buffer) ?? 0) + change.count)
      }
    let delta = 0
    for (const [buffer, count] of counts) {
      const previous = this.sharedBuffers.get(buffer) ?? 0
      if (count < 0)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.buffer-owner')
      if (previous === 0 && count > 0) delta += buffer.byteLength
      if (previous > 0 && count === 0) delta -= buffer.byteLength
    }
    return delta
  }

  private changeBuffers(buffers: readonly ArrayBufferLike[], change: number): void {
    for (const buffer of new Set(buffers)) {
      const count = (this.sharedBuffers.get(buffer) ?? 0) + change
      if (count < 0)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.buffer-owner')
      if (count === 0) this.sharedBuffers.delete(buffer)
      else this.sharedBuffers.set(buffer, count)
    }
  }

  constructor(private readonly maximumRetainedBytes: number) {
    if (!Number.isSafeInteger(maximumRetainedBytes) || maximumRetainedBytes < 1) {
      throw contractError('argument.invalid', 'stream', 'aggregate-stream-quota.maximum-retained-bytes')
    }
  }

  register<Value>(stream: CoreBoundedStream<Value>): void {
    if (this.streams.has(stream)) return
    if (this.retainedBytes() + stream.retainedBytes() > this.maximumRetainedBytes) {
      throw contractError('stream.quota', 'stream', 'aggregate-stream-quota.register')
    }
    this.streams.add(stream)
    stream.setRetainedByteAdmission(() => this.createRetainedByteAdmission(), this.maximumRetainedBytes)
    stream.setSharedClaimTransfer((claim, value, ownerBytes, referenceBytes, count, adjustment) =>
      this.shareClaimValue(stream, claim, value, ownerBytes, referenceBytes, count, adjustment)
    )
    stream.setSharedClaimReplacement(
      (claim, value, ownerBytes, referenceBytes, stages, count, adjustment, onCommitted, evicted) =>
        this.replaceSharedClaim(
          stream,
          claim,
          value,
          ownerBytes,
          referenceBytes,
          stages,
          count,
          adjustment,
          onCommitted,
          evicted
        )
    )
  }

  /** Atomically transfer an inline terminal to registered destination streams. */
  transferTerminal<Source, Destination>(
    source: CoreBoundedStream<Source>,
    claim: CoreStreamTerminalClaim,
    destinations: readonly CoreBoundedStream<Destination>[]
  ): boolean {
    if (!this.streams.has(source) || destinations.length === 0)
      throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.terminal-source')
    const distinct = new Set<object>([source])
    const sourcePlan = source.prepareTerminalTransfer(claim)
    const plans = destinations.map(destination => {
      if (!this.streams.has(destination) || distinct.has(destination))
        throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.terminal-destination')
      distinct.add(destination)
      return destination.prepareTerminalInstall(claim.item)
    })
    const multiplicity = (view: object): number => {
      let count = 0
      for (const stream of this.streams) {
        if (stream === view || stream.tracksRetainedView(view)) count++
      }
      return count
    }
    const charge = plans.reduce((total, plan, index) => {
      const destination = destinations[index]
      if (destination === undefined)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.terminal-destination')
      return total + plan.charge * multiplicity(destination)
    }, 0)
    const credit = sourcePlan.credit * multiplicity(source)
    if (!Number.isSafeInteger(charge) || !Number.isSafeInteger(credit))
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.terminal-bytes')
    if (this.retainedBytes() - credit + charge > this.maximumRetainedBytes) return false
    const cleanup = [sourcePlan.detach(), ...plans.map(plan => plan.install())]
    const failures: unknown[] = []
    for (const finish of cleanup) {
      try {
        finish()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length > 0) {
      const cause =
        failures.length === 1 ? failures[0] : new AggregateError(failures, 'Terminal transfer cleanup failed')
      throw new AggregateStreamTerminalTransferError(cause)
    }
    return true
  }

  /** A resizable, separately owned charge. Refusal leaves its old charge intact. */
  private createRetainedByteAdmission(): CoreStreamRetainedByteAdmission {
    let retained = 0
    let buffers: readonly ArrayBufferLike[] = []
    const reservation: AggregateStreamReservation = {
      get byteLength() {
        return retained
      },
      release: () => {
        retained = 0
        this.changeBuffers(buffers, -1)
        buffers = []
        this.reservations.delete(reservation)
      }
    }
    const set = (bytes: number, nextBuffers: readonly ArrayBufferLike[] = buffers) => {
      this.changeBuffers(buffers, -1)
      buffers = [...new Set(nextBuffers)]
      this.changeBuffers(buffers, 1)
      retained = bytes
      if (bytes === 0) this.reservations.delete(reservation)
      else this.reservations.add(reservation)
    }
    const admission: CoreStreamRetainedByteAdmission = (bytes, nextBuffers = buffers) => {
      if (!Number.isSafeInteger(bytes) || bytes < 0)
        throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.reservation-bytes')
      const delta = this.bufferDelta([
        { buffers, count: -1 },
        { buffers: nextBuffers, count: 1 }
      ])
      if (bytes - retained + delta > 0 && this.retainedBytes() - retained + bytes + delta > this.maximumRetainedBytes)
        return false
      set(bytes, nextBuffers)
      return true
    }
    this.admissions.set(admission, { size: () => retained, buffers: () => buffers, set })
    return admission
  }

  /**
   * Replace a stream-owned claim with a separately charged successor. The
   * positive caller-supplied charge must include its data and owner overhead.
   * Admission does not allocate or copy the successor's representation.
   */
  transfer<Value>(
    source: CoreBoundedStream<Value>,
    claim: CoreStreamClaim<Value>,
    successorBytes: number
  ): AggregateStreamReservation | null {
    return this.transferInternal(source, claim, successorBytes)
  }

  private transferInternal<Value>(
    source: CoreBoundedStream<Value>,
    claim: CoreStreamClaim<Value>,
    successorBytes: number,
    adjustment?: CoreStreamAdmissionAdjustment
  ): MutableReservation | null {
    if (!this.streams.has(source)) {
      throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.transfer-unregistered')
    }
    if (!Number.isSafeInteger(successorBytes) || successorBytes < 1) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.transfer-bytes')
    }
    const sourceBytes = source.claimedBytes(claim)
    const adjustable = adjustment === undefined ? undefined : this.admissions.get(adjustment.admission)
    if (
      adjustment !== undefined &&
      (adjustable === undefined || !Number.isSafeInteger(adjustment.bytes) || adjustment.bytes < 0)
    ) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.adjustment')
    }
    const delta = adjustment === undefined || adjustable === undefined ? 0 : adjustment.bytes - adjustable.size()
    if (this.retainedBytes() - sourceBytes + successorBytes + delta > this.maximumRetainedBytes) return null
    const reservation = this.createReservation(successorBytes)
    this.reservations.add(reservation)
    if (adjustment !== undefined) adjustable?.set(adjustment.bytes)
    try {
      source.commitClaim(claim)
    } catch (error) {
      throw new AggregateStreamTransferError(reservation, error)
    }
    return reservation
  }

  /**
   * Atomically replace a claim by one shared occurrence and a fixed set of
   * references. The owner charge must cover the retained value and its
   * bookkeeping; each reference charge covers its queue/bookkeeping overhead.
   * Values remain immutable while referenced; a recipient must copy mutable
   * data before releasing its reference.
   */
  shareClaim<Value>(
    source: CoreBoundedStream<Value>,
    claim: CoreStreamClaim<Value>,
    ownerBytes: number,
    referenceBytes: number,
    referenceCount: number,
    adjustment?: CoreStreamAdmissionAdjustment
  ): readonly AggregateStreamReference<Value>[] | null {
    return this.shareClaimValue(source, claim, claim.item.value, ownerBytes, referenceBytes, referenceCount, adjustment)
  }

  private shareClaimValue<Source, Value>(
    source: CoreBoundedStream<Source>,
    claim: CoreStreamClaim<Source>,
    value: Value,
    ownerBytes: number,
    referenceBytes: number,
    referenceCount: number,
    adjustment?: CoreStreamAdmissionAdjustment
  ): readonly AggregateStreamReference<Value>[] | null {
    if (adjustment?.buffers !== undefined || adjustment?.valueBuffers !== undefined)
      throw contractError(
        'lifecycle.invalid-state',
        'stream',
        'aggregate-stream-quota.shared-buffers-require-replacement'
      )
    if (![ownerBytes, referenceBytes, referenceCount].every(bytes => Number.isSafeInteger(bytes) && bytes > 0)) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.shared-bytes')
    }
    const total = ownerBytes + referenceBytes * referenceCount
    if (!Number.isSafeInteger(total)) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.shared-total')
    }
    // Check before allocating the bounded reference list. transferInternal
    // validates again immediately before committing the claim.
    if (!this.streams.has(source)) {
      throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.transfer-unregistered')
    }
    const adjustable = adjustment === undefined ? undefined : this.admissions.get(adjustment.admission)
    if (
      adjustment !== undefined &&
      (adjustable === undefined || !Number.isSafeInteger(adjustment.bytes) || adjustment.bytes < 0)
    ) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.adjustment')
    }
    const delta = adjustment === undefined || adjustable === undefined ? 0 : adjustment.bytes - adjustable.size()
    if (this.retainedBytes() - source.claimedBytes(claim) + total + delta > this.maximumRetainedBytes) return null
    const owner: SharedOccurrence<Value> = {
      current: { value },
      remaining: referenceCount,
      reservation: null
    }
    const references = makeSharedReferences(
      owner,
      referenceBytes,
      referenceCount,
      (reference, occurrence, bytes, released) => {
        this.referenceMetadata.set(reference, { owner: occurrence, referenceBytes: bytes, released })
      }
    )
    try {
      owner.reservation = this.transferInternal(source, claim, total, adjustment)
    } catch (error) {
      if (error instanceof AggregateStreamTransferError) {
        // transferInternal always creates this reservation type.
        owner.reservation = this.reservationForSharedTransfer(error.reservation)
        throw new AggregateStreamSharedTransferError(references, error.cause)
      }
      throw error
    }
    return owner.reservation === null ? null : references
  }

  /**
   * Settle a source claim and validated destination replacements in one
   * callback-free transition. Returned references beyond the stages are
   * caller-owned. A null result leaves every owner and stage unchanged. The
   * optional commit hook receives those references before cleanup or delivery;
   * hook failures do not interrupt cleanup. On a hook or cleanup failure,
   * AggregateStreamSharedTransferError.references contains only extra
   * references; the staged references already belong to their queues.
   */
  replaceSharedClaim<Source, Value>(
    source: CoreBoundedStream<Source>,
    claim: CoreStreamClaim<Source>,
    value: Value,
    ownerBytes: number,
    referenceBytes: number,
    stages: readonly CoreStreamReplacementStage<Value>[],
    extraReferences = 0,
    adjustment?: CoreStreamAdmissionAdjustment,
    onCommitted?: (extraReferences: readonly AggregateStreamReference<Value>[]) => void,
    evictedReferences: readonly AggregateStreamReference<unknown>[] = []
  ): readonly AggregateStreamReference<Value>[] | null {
    if (!this.streams.has(source))
      throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.transfer-unregistered')
    if (
      ![ownerBytes, referenceBytes].every(n => Number.isSafeInteger(n) && n > 0) ||
      !Number.isSafeInteger(extraReferences) ||
      extraReferences < 0
    )
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.shared-bytes')
    const adjustable = adjustment === undefined ? undefined : this.admissions.get(adjustment.admission)
    if (
      adjustment !== undefined &&
      (adjustable === undefined || !Number.isSafeInteger(adjustment.bytes) || adjustment.bytes < 0)
    )
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.adjustment')
    const sourcePlan = source.prepareClaimTransfer(claim)
    const plans = stages.map(stage => {
      if (!CoreBoundedStream.isReplacementStage(stage))
        throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.invalid-stage')
      return stage.inspect()
    })
    const count =
      plans.reduce((sum, plan) => sum + (plan.referenceCount ?? (plan.acceptsValue === false ? 0 : 1)), 0) +
      extraReferences
    const total = count === 0 ? 0 : ownerBytes + referenceBytes * count
    if (!Number.isSafeInteger(total))
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.shared-total')
    const streams = new Set<object>([source])
    const evictions: CoreStreamEviction[] = [
      sourcePlan.eviction,
      ...evictedReferences.map(reference => ({ chargedBytes: 0, reference }))
    ]
    let streamCredit = sourcePlan.eviction.chargedBytes
    for (const plan of plans) {
      if (streams.has(plan.stream))
        throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.duplicate-stream')
      streams.add(plan.stream)
      for (const eviction of plan.evictions) {
        evictions.push(eviction)
        for (const stream of this.streams) {
          if (stream === plan.stream || stream.tracksRetainedView(plan.stream)) streamCredit += eviction.chargedBytes
        }
      }
    }
    const referencesToRelease: AggregateStreamReference<unknown>[] = []
    const byOwner = new Map<SharedOccurrence<unknown>, { count: number; referenceBytes: number }>()
    const seen = new Set<AggregateStreamReference<unknown>>()
    for (const eviction of evictions) {
      const reference = eviction.reference
      if (reference === null) continue
      const metadata = this.referenceMetadata.get(reference)
      if (metadata === undefined) {
        if (evictedReferences.includes(reference))
          throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.foreign-reference')
        continue
      }
      if (metadata.released() || seen.has(reference))
        throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.stale-reference')
      seen.add(reference)
      referencesToRelease.push(reference)
      const group = byOwner.get(metadata.owner)
      if (group === undefined) byOwner.set(metadata.owner, { count: 1, referenceBytes: metadata.referenceBytes })
      else group.count++
    }
    let referenceCredit = 0
    for (const [occurrence, group] of byOwner) {
      if (occurrence.reservation === null || group.count > occurrence.remaining)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.reference-owner')
      referenceCredit +=
        group.count === occurrence.remaining ? occurrence.reservation.byteLength : group.count * group.referenceBytes
    }
    const delta = adjustment === undefined || adjustable === undefined ? 0 : adjustment.bytes - adjustable.size()
    const valueBuffers = count === 0 ? [] : (adjustment?.valueBuffers ?? [])
    const bufferChanges: { buffers: readonly ArrayBufferLike[]; count: number }[] = []
    if (adjustment?.buffers !== undefined && adjustable !== undefined) {
      bufferChanges.push({ buffers: adjustable.buffers(), count: -1 }, { buffers: adjustment.buffers, count: 1 })
    }
    for (const [owner, group] of byOwner) {
      if (group.count === owner.remaining && owner.buffers !== undefined)
        bufferChanges.push({ buffers: owner.buffers, count: -1 })
    }
    bufferChanges.push({ buffers: valueBuffers, count: 1 })
    if (
      this.retainedBytes() - streamCredit - referenceCredit + total + delta + this.bufferDelta(bufferChanges) >
      this.maximumRetainedBytes
    )
      return null
    const occurrence: SharedOccurrence<Value> = {
      current: { value },
      remaining: count,
      reservation: null,
      buffers: adjustment?.valueBuffers === undefined ? undefined : valueBuffers,
      releaseBuffers: () => this.changeBuffers(valueBuffers, -1)
    }
    const references = makeSharedReferences(occurrence, referenceBytes, count, (reference, owner, bytes, released) => {
      this.referenceMetadata.set(reference, { owner, referenceBytes: bytes, released })
    })
    if (count > 0) {
      const reservation = this.createReservation(total)
      occurrence.reservation = reservation
      this.reservations.add(reservation)
      this.changeBuffers(valueBuffers, 1)
    }
    if (adjustment !== undefined) adjustable?.set(adjustment.bytes, adjustment.buffers)
    const cleanup: (() => void)[] = [sourcePlan.detach()]
    let referenceIndex = 0
    for (let i = 0; i < stages.length; i++) {
      const stage = stages[i]
      const plan = plans[i]
      if (stage === undefined || plan === undefined)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.stage-reference')
      const stageCount = plan.referenceCount ?? (plan.acceptsValue === false ? 0 : 1)
      const stageReferences = references.slice(referenceIndex, referenceIndex + stageCount)
      referenceIndex += stageCount
      if (stageReferences.length !== stageCount)
        throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.stage-reference')
      cleanup.push(stage.install(stageReferences[0] ?? null, stageReferences.slice(1)))
    }
    for (const reference of referencesToRelease) reference.release()
    const failures: unknown[] = []
    const extras = references.slice(referenceIndex)
    try {
      onCommitted?.(extras)
    } catch (error) {
      failures.push(error)
    }
    for (const finish of cleanup) {
      try {
        finish()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length > 0) {
      const cause =
        failures.length === 1 ? failures[0] : new AggregateError(failures, 'Stream replacement cleanup failed')
      throw new AggregateStreamSharedTransferError(extras, cause)
    }
    return extras
  }

  private createReservation(bytes: number): MutableReservation {
    let retained = bytes
    const reservation: MutableReservation = Object.freeze({
      get byteLength() {
        return retained
      },
      reduce: (amount: number) => {
        if (!Number.isSafeInteger(amount) || amount < 1 || amount > retained || !this.reservations.has(reservation)) {
          throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.reservation-reduce')
        }
        retained -= amount
        if (retained === 0) this.reservations.delete(reservation)
      },
      release: () => {
        retained = 0
        this.reservations.delete(reservation)
      }
    })
    return reservation
  }

  private reservationForSharedTransfer(reservation: AggregateStreamReservation): MutableReservation {
    if (!('reduce' in reservation) || typeof reservation.reduce !== 'function') {
      throw contractError('lifecycle.invariant-violation', 'stream', 'aggregate-stream-quota.shared-reservation')
    }
    const reduce = reservation.reduce
    return {
      get byteLength() {
        return reservation.byteLength
      },
      release: () => reservation.release(),
      reduce: bytes => reduce(bytes)
    }
  }

  unregister<Value>(stream: CoreBoundedStream<Value>): void {
    this.streams.delete(stream)
  }

  emit<Value>(
    stream: CoreBoundedStream<Value>,
    value: Value,
    byteLength: number,
    key: string | null = null,
    payloadBytes: number = byteLength,
    retainedBytes: number = byteLength
  ): CoreStreamPushResult {
    if (!this.streams.has(stream)) {
      throw contractError('lifecycle.invalid-state', 'stream', 'aggregate-stream-quota.emit-unregistered')
    }
    if (!Number.isSafeInteger(payloadBytes) || payloadBytes < 0 || payloadBytes > byteLength) {
      throw contractError('bytes.invalid', 'stream', 'aggregate-stream-quota.payload-bytes')
    }
    const projected = stream.projectedRetainedBytes(byteLength, key, retainedBytes)
    if (this.retainedBytes() - stream.retainedBytes() + projected > this.maximumRetainedBytes) {
      return stream.terminateForAggregateQuota(byteLength)
    }
    return stream.emit(value, byteLength, key, payloadBytes, retainedBytes)
  }

  retainedBytes(): number {
    let total = 0
    for (const stream of this.streams) {
      total += stream.retainedBytes()
    }
    for (const reservation of this.reservations) total += reservation.byteLength
    for (const buffer of this.sharedBuffers.keys()) total += buffer.byteLength
    return total
  }

  retainedPayloadBytes(): number {
    let total = 0
    for (const stream of this.streams) {
      total += stream.retainedPayloadBytes(
        reference => this.referenceMetadata.get(reference)?.owner.buffers !== undefined
      )
    }
    for (const buffer of this.sharedBuffers.keys()) total += buffer.byteLength
    return total
  }

  /**
   * Reports bounded queue bytes separately from durable terminal evidence.
   * The latter is not charged against `maximumRetainedBytes`: it belongs to
   * the selected host evidence store and remains readable until the terminal
   * is consumed or the stream is closed. Keeping the values separate makes
   * that non-silent ownership boundary observable without weakening queue
   * admission.
   */
  retainedByteUsage(): Readonly<{
    readonly queueBytes: number
    readonly terminalEvidenceBytes: number
    readonly totalBytes: number
  }> {
    const queueBytes = this.retainedBytes()
    let terminalEvidenceBytes = 0
    for (const stream of this.streams) terminalEvidenceBytes += stream.retainedTerminalEvidenceBytes()
    return Object.freeze({
      queueBytes,
      terminalEvidenceBytes,
      totalBytes: queueBytes + terminalEvidenceBytes
    })
  }
}
