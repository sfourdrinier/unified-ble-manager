// src/core/bounded-stream.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import { resourceCount } from '../backend-contract/primitives'
import { utf8ByteLength } from '../backend-contract/serializable'
import type { CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { ResourceCount } from '../backend-contract/primitives'
import type {
  BoundedAsyncStream,
  BoundedAsyncStreamIterator,
  OverflowPolicy,
  StreamItem,
  StreamLimits,
  StreamOverflowNotice,
  StreamSourceLossNotice,
  StreamPeerSourceLossNotice,
  StreamTerminalNotice,
  StreamValue
} from '../backend-contract/streams'
import type { TerminalEvidenceCommit } from '../backend-contract/terminal-evidence'

export type CoreStreamTerminalReason = StreamTerminalNotice['reason']

export interface CoreStreamPushResult {
  readonly accepted: boolean
  readonly terminated: boolean
  readonly retainedBytes: number
}

interface RetainedValue<Value> {
  readonly sequence: number
  readonly item: StreamValue<Value>
  readonly byteLength: number
  readonly payloadBytes: number
  readonly key: string | null
  readonly release: (() => void) | null
  readonly chargedBytes: number
  readonly ownedReference?: () => CoreStreamOwnedValue<unknown> | null
  readonly copyForDelivery: ((value: Value) => Value) | null
  visible: boolean
}

interface PendingConsumer<Value> {
  readonly iterator: StreamIteratorState
  readonly resolve: (result: IteratorResult<StreamItem<Value>>) => void
  readonly reject: (reason?: unknown) => void
}

/** A separately charged value; a successful staging transfers this exact obligation. */
export class CoreStreamDeliveryError<Value> extends Error {
  constructor(
    readonly item: StreamValue<Value>,
    readonly cause: unknown
  ) {
    super(`Stream value delivered with cleanup failure: ${String(cause)}`)
    this.name = 'CoreStreamDeliveryError'
  }
}

/** The terminal was consumed even though its subsequent cleanup failed. */
export class CoreStreamTerminalDeliveryError extends Error {
  constructor(
    readonly item: StreamTerminalNotice,
    readonly cause: unknown
  ) {
    super(`Stream terminal delivered with cleanup failure: ${String(cause)}`)
    this.name = 'CoreStreamTerminalDeliveryError'
  }
}

export interface CoreStreamOwnedValue<Value> {
  readonly value: Value
  release(): void
}

const prepareOwnedPublication = Symbol('prepareOwnedPublication')
const revealOwnedPublication = Symbol('revealOwnedPublication')
const flushOwnedPublication = Symbol('flushOwnedPublication')
const replacementStages = new WeakSet<object>()

interface CoreStreamPublication {
  [prepareOwnedPublication](): void
  [revealOwnedPublication](): void
  [flushOwnedPublication](): void
}

export interface CoreStreamStagedValue<Value> extends CoreStreamPublication {
  attach(reference: CoreStreamOwnedValue<Value>, copyForDelivery: (value: Value) => Value): void
  publish(): void
  cancel(): void
}

/** An uncommitted replacement. Cancellation never changes the queue. */
export interface CoreStreamReplacementStage<Value> {
  cancel(): void
  /** Internal transaction boundary; callers must not settle it independently. */
  inspect(): {
    readonly stream: object
    readonly evictions: readonly CoreStreamEviction[]
    readonly acceptsValue?: boolean
    readonly referenceCount?: number
    readonly terminates?: boolean
    readonly acceptedItemIndexes?: readonly number[]
  }
  install(
    reference: CoreStreamOwnedValue<Value> | null,
    additional?: readonly CoreStreamOwnedValue<Value>[]
  ): () => void
}

export interface CoreStreamEviction {
  readonly chargedBytes: number
  readonly reference: CoreStreamOwnedValue<unknown> | null
}

export interface RetainedStreamView {
  retainedBytes(): number
  onRetainedValuesDrained(callback: () => void): void
}

type SourceLossNotice = StreamSourceLossNotice | StreamPeerSourceLossNotice

class RetainedSourceLossView implements RetainedStreamView {
  private released = false
  private readonly drained = new Set<() => void>()

  constructor(private readonly bytes: number) {}

  retainedBytes(): number {
    return this.released ? 0 : this.bytes
  }

  onRetainedValuesDrained(callback: () => void): void {
    if (this.released) callback()
    else this.drained.add(callback)
  }

  release(): void {
    if (this.released) return
    this.released = true
    const callbacks = [...this.drained]
    this.drained.clear()
    const failures: unknown[] = []
    for (const callback of callbacks) {
      try {
        callback()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Source-loss retention release failed')
  }
}

function snapshotSourceLossNotice(notice: SourceLossNotice): SourceLossNotice {
  if (notice.scope === 'host-global') {
    return Object.freeze({
      kind: 'source-loss' as const,
      scope: 'host-global' as const,
      source: notice.source,
      lostRecords: notice.lostRecords
    })
  }
  return Object.freeze({
    kind: 'source-loss' as const,
    scope: 'peer' as const,
    peerId: notice.peerId,
    generation: notice.generation,
    skippedRecords: notice.skippedRecords
  })
}

/** A separately owned byte representation. Each read returns receiver-owned bytes. */
export interface RetainedErrorHandle {
  readonly byteLength: number
  read(offset: number, maxBytes: number): Uint8Array
  release(): void
}

/**
 * An independent bounded owner for already-encoded error evidence. The caller
 * retains and accounts for its input until retain succeeds. The store charges
 * the whole snapshot before copying; its record limit separately bounds handle
 * and bookkeeping overhead. A refused retain leaves ownership with the caller.
 * This does not serialize an error or provide durability or native recovery.
 */
export class RetainedErrorStore {
  private bytes = 0
  private records = 0

  constructor(
    private readonly byteCapacity: number,
    private readonly recordCapacity: number,
    private readonly chunkCapacity: number
  ) {
    for (const limit of [byteCapacity, recordCapacity, chunkCapacity]) {
      if (!Number.isSafeInteger(limit) || limit < 1) {
        throw contractError('argument.invalid', 'stream', 'retained-error-store.capacity')
      }
    }
  }

  retainedBytes(): number {
    return this.bytes
  }

  retain(input: Uint8Array): RetainedErrorHandle | null {
    if (typeof SharedArrayBuffer !== 'undefined' && input.buffer instanceof SharedArrayBuffer) {
      throw contractError('argument.invalid', 'stream', 'retained-error-store.shared-memory')
    }
    const length = input.byteLength
    if (this.records >= this.recordCapacity || length > this.byteCapacity - this.bytes) return null
    this.bytes += length
    this.records += 1
    let retained: Uint8Array | null = null
    try {
      retained = new Uint8Array(input)
    } catch (error) {
      this.bytes -= length
      this.records -= 1
      throw error
    }
    return {
      byteLength: length,
      read: (offset, maxBytes) => {
        if (retained === null) {
          throw contractError('lifecycle.invalid-state', 'stream', 'retained-error-store.released')
        }
        if (
          !Number.isSafeInteger(offset) ||
          offset < 0 ||
          offset > length ||
          !Number.isSafeInteger(maxBytes) ||
          maxBytes < 1 ||
          maxBytes > this.chunkCapacity
        ) {
          throw contractError('argument.invalid', 'stream', 'retained-error-store.read')
        }
        return retained.slice(offset, offset + Math.min(length - offset, maxBytes))
      },
      release: () => {
        if (retained === null) return
        retained = null
        this.bytes -= length
        this.records -= 1
      }
    }
  }
}

interface StreamIteratorState {
  closed: boolean
  terminalReceived: boolean
  terminalHydration: Promise<IteratorResult<StreamItem<unknown>>> | null
}

/** A value remains stream-owned and must not be mutated until committed. */
export interface CoreStreamClaim<Value> {
  readonly item: StreamValue<Value>
  commit(): void
  rollback(): void
}

export interface CoreStreamTerminalClaim {
  readonly item: StreamTerminalNotice
  commit(): void
  rollback(): void
}

export interface CoreStreamTerminalClaimConsumer<Value> extends Omit<CoreStreamClaimConsumer<Value>, 'next'> {
  next(): Promise<
    CoreStreamClaimRead<Value> | { readonly kind: 'terminal-claim'; readonly claim: CoreStreamTerminalClaim }
  >
}

export interface CoreStreamClaimConsumer<Value> {
  claim(): CoreStreamClaim<Value> | null
  next(): Promise<CoreStreamClaimRead<Value>>
  close(): void
}

export type CoreStreamClaimRead<Value> =
  | { readonly kind: 'claim'; readonly claim: CoreStreamClaim<Value> }
  | {
      readonly kind: 'control'
      readonly item:
        | Exclude<StreamItem<Value>, StreamValue<Value>>
        | StreamSourceLossNotice
        | StreamPeerSourceLossNotice
    }
  | { readonly kind: 'done' }

export interface CoreStreamAdmissionAdjustment {
  readonly admission: (bytes: number) => boolean
  readonly bytes: number
  /** Independently retained backing allocations for the admission and successor. */
  readonly buffers?: readonly ArrayBufferLike[]
  readonly valueBuffers?: readonly ArrayBufferLike[]
}

export type CoreStreamRetainedByteAdmission = (bytes: number, buffers?: readonly ArrayBufferLike[]) => boolean

/**
 * The sole in-core bounded asynchronous producer primitive. Values are copied
 * by the owning resource before they enter this stream; this class accounts
 * only queue and overflow policy, never byte ownership conversion.
 */
export class CoreBoundedStream<Value> implements BoundedAsyncStream<Value> {
  private terminalEvidenceHydrator: ((notice: StreamTerminalNotice) => Promise<StreamTerminalNotice>) | null = null
  private terminalEvidenceHydration: Promise<StreamTerminalNotice> | null = null
  private hydratedTerminalEvidence: StreamTerminalNotice | null = null
  private terminalEvidenceHydrationGeneration = 0

  /** Installed by a host-aware owner; iterator delivery awaits exact evidence hydration. */
  setTerminalEvidenceHydrator(hydrator: (notice: StreamTerminalNotice) => Promise<StreamTerminalNotice>): void {
    this.terminalEvidenceHydrator = hydrator
  }

  /** Hydrates one durable terminal for a downstream retained view. */
  hydrateTerminalEvidence(notice: StreamTerminalNotice): Promise<StreamTerminalNotice> {
    if (this.terminalEvidenceHydrator === null)
      return Promise.reject(contractError('capability.unsupported', 'stream', 'bounded-stream.terminal-evidence.read'))
    return this.terminalEvidenceHydrator(notice)
  }

  /**
   * Produces the exact terminal that a following close/finish would publish,
   * without changing ownership. Hosts use it to durably commit evidence first.
   * Callers must serialize the preview and its matching publish on one source
   * lane; a changed stream state makes the preview stale rather than usable.
   */
  previewTerminal(
    reason: CoreStreamTerminalReason,
    error: NormalizedBleError | null = null
  ): StreamTerminalNotice | null {
    if (this.ownerClosed || this.terminalDelivered || this.terminalNotice !== null) return null
    const droppedItems = this.droppedItems + this.sourceDroppedItems + (reason === 'closed' ? this.values.length : 0)
    const droppedBytes =
      this.droppedBytes + this.sourceDroppedBytes + (reason === 'closed' ? this.retainedValueBytes : 0)
    return Object.freeze({
      kind: 'terminal',
      reason,
      droppedItems: resourceCount(droppedItems),
      droppedBytes: resourceCount(droppedBytes),
      replacedItems: resourceCount(this.replacedItems + this.sourceReplacedItems),
      ...(error === null ? {} : { error })
    })
  }
  private sharedClaimReplacement:
    | (<Successor>(
        claim: CoreStreamClaim<Value>,
        value: Successor,
        ownerBytes: number,
        referenceBytes: number,
        stages: readonly CoreStreamReplacementStage<Successor>[],
        extraReferences: number,
        adjustment?: CoreStreamAdmissionAdjustment,
        onCommitted?: (references: readonly CoreStreamOwnedValue<Successor>[]) => void,
        evictedReferences?: readonly CoreStreamOwnedValue<unknown>[]
      ) => readonly CoreStreamOwnedValue<Successor>[] | null)
    | null = null

  setSharedClaimReplacement(replace: NonNullable<CoreBoundedStream<Value>['sharedClaimReplacement']>): void {
    this.sharedClaimReplacement = replace
  }

  replaceSharedClaim<Successor>(
    claim: CoreStreamClaim<Value>,
    value: Successor,
    ownerBytes: number,
    referenceBytes: number,
    stages: readonly CoreStreamReplacementStage<Successor>[],
    extraReferences: number,
    adjustment?: CoreStreamAdmissionAdjustment,
    onCommitted?: (references: readonly CoreStreamOwnedValue<Successor>[]) => void,
    evictedReferences?: readonly CoreStreamOwnedValue<unknown>[]
  ): readonly CoreStreamOwnedValue<Successor>[] | null {
    if (this.sharedClaimReplacement === null)
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.shared-replacement-unavailable')
    return this.sharedClaimReplacement(
      claim,
      value,
      ownerBytes,
      referenceBytes,
      stages,
      extraReferences,
      adjustment,
      onCommitted,
      evictedReferences
    )
  }

  private sharedClaimTransfer:
    | (<Successor>(
        claim: CoreStreamClaim<Value>,
        value: Successor,
        ownerBytes: number,
        referenceBytes: number,
        count: number,
        adjustment?: CoreStreamAdmissionAdjustment
      ) => readonly CoreStreamOwnedValue<Successor>[] | null)
    | null = null

  setSharedClaimTransfer(
    transfer: <Successor>(
      claim: CoreStreamClaim<Value>,
      value: Successor,
      ownerBytes: number,
      referenceBytes: number,
      count: number,
      adjustment?: CoreStreamAdmissionAdjustment
    ) => readonly CoreStreamOwnedValue<Successor>[] | null
  ): void {
    this.sharedClaimTransfer = transfer
  }

  canShareClaim(): boolean {
    return this.sharedClaimTransfer !== null
  }

  shareClaim<Successor>(
    claim: CoreStreamClaim<Value>,
    value: Successor,
    ownerBytes: number,
    referenceBytes: number,
    count: number,
    adjustment?: CoreStreamAdmissionAdjustment
  ): readonly CoreStreamOwnedValue<Successor>[] | null {
    if (this.sharedClaimTransfer === null)
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.shared-claim-unavailable')
    return this.sharedClaimTransfer(claim, value, ownerBytes, referenceBytes, count, adjustment)
  }
  /** Verify a stage was produced by this implementation. */
  static isReplacementStage(stage: object): boolean {
    return replacementStages.has(stage)
  }

  /** Reveal every staged entry before running any delivery or cleanup callback. */
  static publishOwned(stages: readonly CoreStreamPublication[], onPublished?: () => void): void {
    if (new Set(stages).size !== stages.length)
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.duplicate-stage')
    for (const stage of stages) stage[prepareOwnedPublication]()
    for (const stage of stages) stage[revealOwnedPublication]()
    onPublished?.()
    const failures: unknown[] = []
    for (const stage of stages) {
      try {
        stage[flushOwnedPublication]()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Stream publication failed')
  }
  private retainedByteAdmission: (() => CoreStreamRetainedByteAdmission) | null = null
  private retainedByteMaximum: number | null = null

  /** Installed by the aggregate owner for separately retained successors. */
  setRetainedByteAdmission(factory: () => CoreStreamRetainedByteAdmission, maximum: number): void {
    this.retainedByteAdmission = factory
    this.retainedByteMaximum = maximum
  }

  /** A lower-bound impossibility check; other live owners may reduce available capacity. */
  canEverRetainBytes(bytes: number): boolean {
    return this.retainedByteMaximum === null || bytes <= this.retainedByteMaximum
  }

  createRetainedByteAdmission(): CoreStreamRetainedByteAdmission | null {
    return this.retainedByteAdmission?.() ?? null
  }
  private readonly values: RetainedValue<Value>[] = []
  private activeClaim: {
    readonly claim: CoreStreamClaim<Value>
    readonly entry: RetainedValue<Value>
    readonly owner: object
  } | null = null
  private activeTerminalClaim: { readonly claim: CoreStreamTerminalClaim; readonly owner: object } | null = null
  private readingEntry: RetainedValue<Value> | null = null
  private nextSequence = 0
  private readonly peerLosses: { readonly sequence: number; readonly notice: StreamPeerSourceLossNotice }[] = []
  private readonly consumers: PendingConsumer<Value>[] = []
  private readonly claimWaiters = new Set<() => void>()
  private overflowNotice: StreamOverflowNotice | null = null
  private readonly overflowStages = new WeakMap<
    StreamOverflowNotice,
    {
      readonly upstream: {
        readonly droppedItems: number
        readonly droppedBytes: number
        readonly replacedItems: number
      }
      readonly subscriber: {
        readonly droppedItems: number
        readonly droppedBytes: number
        readonly replacedItems: number
      }
    }
  >()
  /**
   * Claim readers observe host-global loss in-band so they cannot claim a
   * later value ahead of its provenance. Ordinary iterators intentionally do
   * not consume this queue: their stable contract reports the eventual
   * terminal rather than a new control item.
   */
  private readonly claimSourceLoss: { readonly sequence: number; readonly notice: StreamSourceLossNotice }[] = []
  private readonly sourceLossObservers = new Set<
    (notice: StreamSourceLossNotice | StreamPeerSourceLossNotice) => void
  >()
  private readonly sourceLossDetails: SourceLossNotice[] = []
  private readonly sourceLossRetentions: RetainedSourceLossView[] = []
  private terminalNotice: StreamTerminalNotice | null = null
  private terminalDelivered = false
  private peerLossTerminated = false
  private ownerClosed = false
  private retainedValueBytes = 0
  private externallyChargedValueBytes = 0
  private retainedPayloadByteCount = 0
  private droppedItems = 0
  private droppedBytes = 0
  private replacedItems = 0
  private sourceDroppedItems = 0
  private sourceDroppedBytes = 0
  private sourceReplacedItems = 0
  private settledTerminalReason: CoreStreamTerminalReason | null = null
  private readonly retainedValuesDrained = new Set<() => void>()
  private readonly retainedViews = new Set<RetainedStreamView>()
  private readonly cleanupFailures: unknown[] = []

  constructor(
    readonly limits: StreamLimits,
    readonly overflowPolicy: OverflowPolicy,
    private readonly preserveAcceptedOnOverflow = false
  ) {
    if (limits.byteCapacity <= limits.reservedControlCapacity) {
      throw contractError('argument.invalid', 'stream', 'bounded-stream.byte-capacity')
    }
  }

  emit(
    value: Value,
    byteLength: number,
    key: string | null = null,
    payloadBytes: number = byteLength,
    retainedBytes: number = byteLength
  ): CoreStreamPushResult {
    return this.emitInternal(value, byteLength, key, payloadBytes, this.preserveAcceptedOnOverflow, retainedBytes)
  }

  /**
   * Reserve bounded queue capacity without retaining a value. Attachment
   * transfers one separately charged reference; publication exposes it.
   * The mapper creates receiver-owned data only on delivery. Claims borrow
   * the original immutable value until settlement.
   */
  reserveOwned(
    byteLength: number,
    key: string | null = null,
    payloadBytes: number = byteLength
  ): CoreStreamStagedValue<Value> | null {
    this.assertByteLength(byteLength)
    this.assertByteLength(payloadBytes)
    if (payloadBytes > byteLength) throw contractError('bytes.invalid', 'stream', 'bounded-stream.payload-bytes')
    if (this.isTerminal() || !this.fits(byteLength)) return null
    let reference: CoreStreamOwnedValue<Value> | null = null
    let copyForDelivery: ((value: Value) => Value) | null = null
    const entry: RetainedValue<Value> = {
      sequence: this.nextSequence++,
      item: {
        kind: 'value',
        get value() {
          if (reference === null)
            throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.unattached-stage')
          return reference.value
        }
      },
      byteLength,
      payloadBytes,
      key,
      release: () => {
        const owned = reference
        reference = null
        owned?.release()
      },
      chargedBytes: 0,
      ownedReference: () => reference,
      copyForDelivery: value => {
        if (copyForDelivery === null)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.unattached-stage')
        return copyForDelivery(value)
      },
      visible: false
    }
    this.values.push(entry)
    this.retainedValueBytes += byteLength
    this.externallyChargedValueBytes += byteLength
    this.retainedPayloadByteCount += payloadBytes
    let settled = false
    const checkCurrent = () => {
      if (settled || !this.values.includes(entry) || this.ownerClosed) {
        throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-stage')
      }
    }
    const stage: CoreStreamStagedValue<Value> = {
      attach: (owned, copy) => {
        checkCurrent()
        if (reference !== null)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.attached-stage')
        reference = owned
        copyForDelivery = copy
      },
      [prepareOwnedPublication]: () => {
        checkCurrent()
        if (reference === null)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.unattached-stage')
      },
      [revealOwnedPublication]: () => {
        settled = true
        entry.visible = true
      },
      [flushOwnedPublication]: () => this.flushPendingConsumers(),
      publish: () => CoreBoundedStream.publishOwned([stage]),
      cancel: () => {
        if (settled) return
        settled = true
        const index = this.values.indexOf(entry)
        if (index < 0) return
        this.values.splice(index, 1)
        this.subtractEntry(entry)
        this.cleanupAndFlush([entry])
      }
    }
    return stage
  }

  /**
   * Plan a replacement without changing or releasing the current entries.
   * The plan becomes stale on any conflicting queue change. Project and copy
   * execute only when a consumer reads the committed value.
   */
  reserveOwnedReplacement<Successor>(
    byteLength: number,
    project: (value: Successor) => Value,
    copy: (value: Value) => Value,
    key: string | null = null,
    payloadBytes: number = byteLength
  ): CoreStreamReplacementStage<Successor> | null {
    this.assertByteLength(byteLength)
    this.assertByteLength(payloadBytes)
    if (payloadBytes > byteLength) throw contractError('bytes.invalid', 'stream', 'bounded-stream.payload-bytes')
    if (this.isTerminal() || this.readingEntry !== null) return null
    const snapshot = [...this.values]
    const removed: RetainedValue<Value>[] = []
    let count = snapshot.length
    let bytes = this.retainedValueBytes
    const fits = () => count < this.limits.itemCapacity && bytes + byteLength <= this.valueByteCapacity()
    if (!fits()) {
      if (this.overflowPolicy === 'drop-oldest') {
        for (const entry of snapshot) {
          if (fits()) break
          if (!entry.visible || entry === this.activeClaim?.entry || entry === this.readingEntry) return null
          removed.push(entry)
          count--
          bytes -= entry.byteLength
        }
      } else if (this.overflowPolicy === 'latest' && key !== null) {
        const entry = snapshot.find(
          candidate =>
            candidate.key === key &&
            candidate.visible &&
            candidate !== this.activeClaim?.entry &&
            candidate !== this.readingEntry
        )
        if (entry === undefined) return null
        removed.push(entry)
        count--
        bytes -= entry.byteLength
      } else return null
      if (!fits()) return null
    }
    let settled = false
    const inspect = () => {
      if (
        settled ||
        this.isTerminal() ||
        this.readingEntry !== null ||
        snapshot.length !== this.values.length ||
        snapshot.some((entry, index) => this.values[index] !== entry) ||
        removed.some(entry => !entry.visible || entry === this.activeClaim?.entry)
      ) {
        throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-replacement')
      }
      return {
        stream: this,
        evictions: removed.map(entry => ({
          chargedBytes: entry.chargedBytes,
          reference: entry.ownedReference?.() ?? null
        }))
      }
    }
    const stage: CoreStreamReplacementStage<Successor> = {
      cancel: () => {
        settled = true
      },
      inspect,
      install: reference => {
        inspect()
        if (reference === null)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.missing-replacement-reference')
        settled = true
        for (const entry of removed) {
          const index = this.values.indexOf(entry)
          this.values.splice(index, 1)
          this.subtractEntry(entry)
          if (this.overflowPolicy === 'latest') this.replacedItems++
          else this.droppedItems++
          this.droppedBytes += entry.byteLength
        }
        if (removed.length > 0) this.noteOverflow()
        const entry: RetainedValue<Value> = {
          sequence: this.nextSequence++,
          item: {
            kind: 'value',
            get value() {
              return project(reference.value)
            }
          },
          byteLength,
          payloadBytes,
          key,
          release: () => reference.release(),
          ownedReference: () => reference,
          chargedBytes: 0,
          copyForDelivery: copy,
          visible: true
        }
        this.values.push(entry)
        this.retainedValueBytes += byteLength
        this.externallyChargedValueBytes += byteLength
        this.retainedPayloadByteCount += payloadBytes
        return () => this.cleanupAndFlush(removed)
      }
    }
    replacementStages.add(stage)
    return Object.freeze(stage)
  }

  /** Plan ordered values as one queue replacement, applying the policy to each value. */
  reserveOwnedReplacementBatch<Successor>(
    items: readonly { byteLength: number; project: (value: Successor) => Value; copy: (value: Value) => Value }[]
  ): CoreStreamReplacementStage<Successor> | null {
    if (this.isTerminal() || this.readingEntry !== null) return null
    for (const item of items) this.assertByteLength(item.byteLength)
    const snapshot = [...this.values]
    const pending: ({ old: RetainedValue<Value> } | { item: (typeof items)[number]; index: number })[] = snapshot.map(
      old => ({ old })
    )
    const removed: RetainedValue<Value>[] = []
    let bytes = this.retainedValueBytes
    let droppedItems = 0
    let droppedBytes = 0
    let terminates = false
    for (const [index, item] of items.entries()) {
      const fits = () =>
        pending.length < this.limits.itemCapacity && bytes + item.byteLength <= this.valueByteCapacity()
      if (!fits() && this.overflowPolicy === 'error') {
        droppedItems++
        droppedBytes += item.byteLength
        terminates = true
        break
      }
      if (!fits() && this.overflowPolicy === 'drop-oldest' && item.byteLength <= this.valueByteCapacity()) {
        while (!fits() && pending.length > 0) {
          const first = pending[0]
          if (first === undefined) break
          if ('old' in first && (!first.old.visible || first.old === this.activeClaim?.entry)) return null
          pending.shift()
          const length = 'old' in first ? first.old.byteLength : first.item.byteLength
          if ('old' in first) removed.push(first.old)
          bytes -= length
          droppedItems++
          droppedBytes += length
        }
      }
      if (!fits()) {
        droppedItems++
        droppedBytes += item.byteLength
        continue
      }
      pending.push({ item, index })
      bytes += item.byteLength
    }
    const accepted = pending.filter(entry => 'item' in entry)
    let settled = false
    const inspect = () => {
      if (
        settled ||
        this.isTerminal() ||
        this.readingEntry !== null ||
        snapshot.length !== this.values.length ||
        snapshot.some((entry, index) => this.values[index] !== entry) ||
        removed.some(entry => !entry.visible || entry === this.activeClaim?.entry)
      )
        throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-replacement')
      return {
        stream: this,
        evictions: removed.map(entry => ({
          chargedBytes: entry.chargedBytes,
          reference: entry.ownedReference?.() ?? null
        })),
        referenceCount: accepted.length,
        terminates,
        acceptedItemIndexes: accepted.map(entry => entry.index)
      }
    }
    const stage: CoreStreamReplacementStage<Successor> = {
      cancel: () => {
        settled = true
      },
      inspect,
      install: (reference, additional = []) => {
        inspect()
        const references = reference === null ? additional : [reference, ...additional]
        if (references.length !== accepted.length)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.replacement-reference-count')
        settled = true
        for (const entry of removed) {
          this.values.splice(this.values.indexOf(entry), 1)
          this.subtractEntry(entry)
        }
        this.droppedItems += droppedItems
        this.droppedBytes += droppedBytes
        if (droppedItems > 0 && !terminates) this.noteOverflow()
        for (let index = 0; index < accepted.length; index++) {
          const item = accepted[index]?.item
          const owned = references[index]
          if (item === undefined || owned === undefined)
            throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.replacement-reference-count')
          this.values.push({
            sequence: this.nextSequence++,
            item: {
              kind: 'value',
              get value() {
                return item.project(owned.value)
              }
            },
            byteLength: item.byteLength,
            payloadBytes: item.byteLength,
            key: null,
            release: () => owned.release(),
            ownedReference: () => owned,
            chargedBytes: 0,
            copyForDelivery: item.copy,
            visible: true
          })
          this.retainedValueBytes += item.byteLength
          this.externallyChargedValueBytes += item.byteLength
          this.retainedPayloadByteCount += item.byteLength
        }
        return () => this.cleanupAndFlush(removed)
      }
    }
    replacementStages.add(stage)
    return Object.freeze(stage)
  }

  /** Stage loss of an incoming value without changing the accepted prefix. */
  reserveOwnedDrop<Successor>(byteLength: number): CoreStreamReplacementStage<Successor> | null {
    this.assertByteLength(byteLength)
    if (this.isTerminal() || this.readingEntry !== null || this.fits(byteLength) || this.overflowPolicy === 'error')
      return null
    const snapshot = [...this.values]
    let settled = false
    const inspect = () => {
      if (
        settled ||
        this.isTerminal() ||
        this.readingEntry !== null ||
        snapshot.length !== this.values.length ||
        snapshot.some((entry, index) => this.values[index] !== entry)
      )
        throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-replacement')
      return { stream: this, evictions: [], acceptsValue: false }
    }
    const stage: CoreStreamReplacementStage<Successor> = {
      cancel: () => {
        settled = true
      },
      inspect,
      install: reference => {
        inspect()
        if (reference !== null)
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.unexpected-replacement-reference')
        settled = true
        this.droppedItems++
        this.droppedBytes += byteLength
        this.noteOverflow()
        return () => this.flushPendingConsumers()
      }
    }
    replacementStages.add(stage)
    return Object.freeze(stage)
  }

  /** Convenience for a reference already admitted by its aggregate owner. */
  stageOwned(
    reference: CoreStreamOwnedValue<Value>,
    byteLength: number,
    copyForDelivery: (value: Value) => Value,
    key: string | null = null,
    payloadBytes: number = byteLength
  ): CoreStreamStagedValue<Value> | null {
    const stage = this.reserveOwned(byteLength, key, payloadBytes)
    stage?.attach(reference, copyForDelivery)
    return stage
  }

  /** On error overflow, retain the accepted FIFO prefix and count the rejected value. */
  emitPreservingAccepted(
    value: Value,
    byteLength: number,
    key: string | null = null,
    payloadBytes: number = byteLength
  ): CoreStreamPushResult {
    return this.emitInternal(value, byteLength, key, payloadBytes, true, byteLength)
  }

  private emitInternal(
    value: Value,
    byteLength: number,
    key: string | null,
    payloadBytes: number,
    preserveAccepted: boolean,
    retainedBytes: number
  ): CoreStreamPushResult {
    this.assertByteLength(byteLength)
    this.assertByteLength(payloadBytes)
    this.assertByteLength(retainedBytes)
    if (payloadBytes > byteLength) {
      throw contractError('bytes.invalid', 'stream', 'bounded-stream.payload-bytes')
    }
    if (this.isTerminal()) {
      return this.pushResult(false, true)
    }
    if (
      this.consumers.length > 0 &&
      this.overflowNotice === null &&
      this.claimSourceLoss.length === 0 &&
      this.values.length === 0 &&
      this.peerLosses.length === 0
    ) {
      const consumer = this.consumers.shift()
      if (consumer === undefined) {
        throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.consumer')
      }
      consumer.resolve({ done: false, value: { kind: 'value', value } })
      return this.pushResult(true, false)
    }
    if (this.fits(byteLength)) {
      this.append(value, byteLength, key, payloadBytes, retainedBytes)
      return this.pushResult(true, false)
    }
    return this.applyOverflow(value, byteLength, key, payloadBytes, preserveAccepted, retainedBytes)
  }

  /** Coalesces bounded-ingress loss from an upstream stream into this stream's next control notice. */
  observeSourceOverflow(notice: StreamOverflowNotice): void {
    if (this.isTerminal()) {
      return
    }
    this.sourceDroppedItems = Math.max(this.sourceDroppedItems, Number(notice.droppedItems))
    this.sourceDroppedBytes = Math.max(this.sourceDroppedBytes, Number(notice.droppedBytes))
    this.sourceReplacedItems = Math.max(this.sourceReplacedItems, Number(notice.replacedItems))
    this.noteOverflow(notice.policy)
    this.flushPendingConsumers()
  }

  /** Retains host-global loss separately from exact stream counters. */
  observeSourceLoss(notice: StreamSourceLossNotice | StreamPeerSourceLossNotice): void {
    this.observeSourceLosses([notice])
  }

  /**
   * Source loss makes completeness unknowable, but it is itself a typed
   * stream fact. Retain and deliver it before later values or a terminal;
   * ending the stream here would erase the ownership of later source facts.
   */
  observeSourceLosses(notices: readonly (StreamSourceLossNotice | StreamPeerSourceLossNotice)[]): void {
    if (this.isTerminal() || notices.length === 0) return
    for (const notice of notices) {
      const retained = snapshotSourceLossNotice(notice)
      const bytes = utf8ByteLength(JSON.stringify(retained))
      if (this.retainedBytes() + bytes > this.limits.byteCapacity) {
        this.finishWithReason(
          'source-failed',
          contractError('stream.quota', 'stream', 'bounded-stream.source-loss-control').normalized
        )
        return
      }
      const view = new RetainedSourceLossView(bytes)
      try {
        this.trackRetainedView(view)
      } catch (error) {
        if (!(error instanceof BackendContractError) || error.normalized.code !== 'stream.quota') throw error
        this.finishWithReason(
          'source-failed',
          contractError('stream.quota', 'stream', 'bounded-stream.source-loss-control').normalized
        )
        return
      }
      this.sourceLossRetentions.push(view)
      this.sourceLossDetails.push(retained)
      if (retained.scope === 'host-global') {
        this.claimSourceLoss.push({ sequence: this.nextSequence, notice: retained })
      } else {
        // Each native lag report is a distinct source fact. Its count may
        // equal a prior delta, so never conflate it by peer/generation alone.
        this.peerLosses.push({ sequence: this.nextSequence, notice: retained })
      }
      for (const observer of [...this.sourceLossObservers]) observer(retained)
    }
    this.flushPendingConsumers()
  }

  /**
   * Internal provenance channel for consumers that explicitly opt in to
   * loss-aware semantics. The ordinary async iterator never yields these
   * notices because its stable contract ends with the legacy overflow terminal.
   */
  onSourceLoss(observer: (notice: StreamSourceLossNotice | StreamPeerSourceLossNotice) => void): () => void {
    this.sourceLossObservers.add(observer)
    for (const notice of this.sourceLossDetails) observer(notice)
    return () => this.sourceLossObservers.delete(observer)
  }

  /** Internal immutable provenance retained for an explicit loss-aware projection. */
  sourceLossNotices(): readonly (StreamSourceLossNotice | StreamPeerSourceLossNotice)[] {
    return this.sourceLossDetails
  }

  private releaseSourceLossRetentions(): void {
    const retentions = this.sourceLossRetentions.splice(0)
    this.claimSourceLoss.length = 0
    this.peerLosses.length = 0
    this.sourceLossDetails.length = 0
    const failures: unknown[] = []
    for (const retention of retentions) {
      try {
        retention.release()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Source-loss retention cleanup failed')
  }

  /**
   * An upstream `error`-policy overflow ends this stream: the upstream loss
   * joins this stream's counters and the stream ends `overflow` at once,
   * exactly as a local `error` overflow does (no separate overflow notice).
   */
  closeWithSourceOverflow(notice: StreamOverflowNotice, evidence?: TerminalEvidenceCommit): void {
    if (this.isTerminal()) {
      return
    }
    this.sourceDroppedItems = Math.max(this.sourceDroppedItems, Number(notice.droppedItems))
    this.sourceDroppedBytes = Math.max(this.sourceDroppedBytes, Number(notice.droppedBytes))
    this.sourceReplacedItems = Math.max(this.sourceReplacedItems, Number(notice.replacedItems))
    this.closeWithReason('overflow', null, evidence)
  }

  /** Carries cumulative upstream counters into this stream's own terminal. */
  observeSourceTerminal(notice: StreamTerminalNotice): void {
    if (this.terminalDelivered) return
    this.sourceDroppedItems = Math.max(this.sourceDroppedItems, Number(notice.droppedItems))
    this.sourceDroppedBytes = Math.max(this.sourceDroppedBytes, Number(notice.droppedBytes))
    this.sourceReplacedItems = Math.max(this.sourceReplacedItems, Number(notice.replacedItems))
  }

  /** Publish any cumulative source loss first learned from its terminal. */
  observeSourceTerminalOverflow(notice: StreamTerminalNotice, policy: OverflowPolicy): void {
    if (
      Number(notice.droppedItems) <= this.sourceDroppedItems &&
      Number(notice.droppedBytes) <= this.sourceDroppedBytes &&
      Number(notice.replacedItems) <= this.sourceReplacedItems
    )
      return
    this.observeSourceOverflow({
      kind: 'overflow',
      policy,
      droppedItems: notice.droppedItems,
      droppedBytes: notice.droppedBytes,
      replacedItems: notice.replacedItems
    })
  }

  close(): Promise<CleanupRecord> {
    this.closeWithReason('closed')
    return Promise.resolve({ state: 'released', failures: [] })
  }

  /** Discard the remaining controls when an exclusive owner abandons a closed stream. */
  dispose(): void {
    if (!this.ownerClosed && !this.terminalDelivered)
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.dispose-open')
    this.activeTerminalClaim = null
    this.terminalEvidenceHydrationGeneration += 1
    this.terminalEvidenceHydration = null
    this.hydratedTerminalEvidence = null
    this.terminalNotice = null
    this.overflowNotice = null
    this.releaseSourceLossRetentions()
    this.sourceLossObservers.clear()
    this.terminalDelivered = true
    try {
      this.notifyRetainedValuesDrained()
    } finally {
      this.flushPendingConsumers()
    }
  }

  closeWithReason(
    reason: CoreStreamTerminalReason,
    error: NormalizedBleError | null = null,
    evidence?: TerminalEvidenceCommit
  ): void {
    this.closeWithTerminal(reason, false, error, evidence)
  }

  /**
   * Closes with a synthetic terminal that must not attribute earlier source or
   * local overflow to the terminal's owning operation.
   */
  closeWithExactZeroCounters(reason: CoreStreamTerminalReason): void {
    this.closeWithTerminal(reason, true, null)
  }

  private closeWithTerminal(
    reason: CoreStreamTerminalReason,
    zeroOverflowCounters: boolean,
    error: NormalizedBleError | null,
    evidence?: TerminalEvidenceCommit
  ): void {
    if (this.terminalEvidenceHydration !== null) {
      this.terminalEvidenceHydrationGeneration += 1
      this.terminalEvidenceHydration = null
      this.hydratedTerminalEvidence = null
      this.terminalNotice = null
    }
    if (this.ownerClosed || this.terminalDelivered) {
      return
    }
    this.activeClaim = null
    this.activeTerminalClaim = null
    if (reason === 'closed' && !zeroOverflowCounters) {
      this.droppedItems += this.values.length
      this.droppedBytes += this.retainedValueBytes
    }
    const removed = this.values.splice(0)
    this.retainedValueBytes = 0
    this.externallyChargedValueBytes = 0
    this.retainedPayloadByteCount = 0
    this.ownerClosed = true
    if (!this.peerLossTerminated) {
      this.overflowNotice = null
      if (zeroOverflowCounters) this.clearOverflowCounters()
      const sourceLossTerminates = this.sourceLossProjectsLegacyOverflow(reason, error, evidence)
      this.settledTerminalReason = sourceLossTerminates ? 'overflow' : reason
      this.terminalNotice = sourceLossTerminates
        ? this.makeTerminal('overflow', null)
        : this.makeTerminal(reason, error, evidence)
    }
    for (const entry of removed) this.releaseEntry(entry)
    try {
      this.notifyRetainedValuesDrained()
    } catch (failure) {
      this.cleanupFailures.push(failure)
    }
    this.cleanupAndFlush([])
  }

  /** Appends a terminal control record after already accepted values drain. */
  finishWithReason(
    reason: CoreStreamTerminalReason,
    error: NormalizedBleError | null = null,
    evidence?: TerminalEvidenceCommit
  ): void {
    if (this.ownerClosed || this.terminalDelivered) {
      return
    }
    if (this.terminalNotice !== null) return
    const sourceLossTerminates = this.sourceLossProjectsLegacyOverflow(reason, error, evidence)
    this.settledTerminalReason = sourceLossTerminates ? 'overflow' : reason
    this.terminalNotice = sourceLossTerminates
      ? // Graceful finish preserves the loss-aware side channel while legacy
        // delivery receives the canonical overflow terminal without inventing a
        // per-stream failure payload.
        this.makeTerminal('overflow', null)
      : this.makeTerminal(reason, error, evidence)
    this.flushPendingConsumers()
  }

  retainedBytes(): number {
    const terminalBytes =
      this.terminalNotice?.error === undefined ? 0 : utf8ByteLength(JSON.stringify(this.terminalNotice.error))
    let viewBytes = 0
    for (const view of this.retainedViews) viewBytes += view.retainedBytes()
    return (
      this.retainedValueBytes -
      this.externallyChargedValueBytes +
      this.controlReservationBytes() +
      terminalBytes +
      viewBytes
    )
  }

  retainedPayloadBytes(excludeReference?: (reference: CoreStreamOwnedValue<unknown>) => boolean): number {
    if (excludeReference === undefined) return this.retainedPayloadByteCount
    let total = this.retainedPayloadByteCount
    for (const entry of this.values) {
      const reference = entry.ownedReference?.()
      if (reference !== undefined && reference !== null && excludeReference(reference)) total -= entry.payloadBytes
    }
    return total
  }

  /**
   * Native terminal evidence is durable host storage, not an in-memory queue
   * allocation. It remains observable until this stream consumes or retires
   * the terminal that references it.
   */
  retainedTerminalEvidenceBytes(): number {
    return this.terminalNotice?.evidence?.byteLength ?? 0
  }

  /** Keeps an owner's quota charged until its terminal and retained views are released. */
  onRetainedValuesDrained(callback: () => void): void {
    if (this.terminalDelivered && this.retainedViews.size === 0) callback()
    else this.retainedValuesDrained.add(callback)
  }

  /** Charge a projected view to the source owner until that view ends. */
  trackRetainedView(view: RetainedStreamView): void {
    if (this.retainedViews.has(view)) return
    const admission = this.createRetainedByteAdmission()
    if (admission !== null && !admission(view.retainedBytes())) {
      throw contractError('stream.quota', 'stream', 'bounded-stream.track-retained-view')
    }
    this.retainedViews.add(view)
    try {
      view.onRetainedValuesDrained(() => {
        this.retainedViews.delete(view)
        this.notifyRetainedValuesDrained()
      })
    } finally {
      admission?.(0)
    }
  }

  tracksRetainedView(view: object): boolean {
    for (const current of this.retainedViews) if (current === view) return true
    return false
  }

  private notifyRetainedValuesDrained(): void {
    if (!this.terminalDelivered || this.retainedViews.size !== 0) return
    const callbacks = [...this.retainedValuesDrained]
    this.retainedValuesDrained.clear()
    const failures: unknown[] = []
    for (const callback of callbacks) {
      try {
        callback()
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Stream retained-value cleanup failed')
  }

  projectedRetainedBytes(byteLength: number, key: string | null = null, retainedBytes: number = byteLength): number {
    this.assertByteLength(byteLength)
    this.assertByteLength(retainedBytes)
    if (this.isTerminal()) {
      return this.retainedBytes()
    }
    if (this.fits(byteLength)) {
      return this.retainedBytes() + retainedBytes
    }
    if (this.overflowPolicy === 'error') {
      return this.preserveAcceptedOnOverflow || this.activeClaim !== null
        ? this.retainedBytes()
        : this.retainedBytes() - (this.retainedValueBytes - this.externallyChargedValueBytes)
    }
    if (this.overflowPolicy === 'drop-newest') {
      return this.retainedBytes()
    }
    if (this.overflowPolicy === 'latest') {
      return this.projectLatestRetainedBytes(byteLength, key, retainedBytes)
    }
    return this.projectDropOldestRetainedBytes(byteLength, retainedBytes)
  }

  terminateForAggregateQuota(byteLength: number, error: NormalizedBleError | null = null): CoreStreamPushResult {
    if (!this.recordRejectedValue(byteLength)) {
      return this.pushResult(false, true)
    }
    if (this.preserveAcceptedOnOverflow || this.activeClaim !== null) this.finishWithReason('overflow', error)
    else this.closeWithReason('overflow', error)
    return this.pushResult(false, true)
  }

  /** Rejects one record while preserving accepted FIFO values before overflow. */
  finishForRejectedValue(byteLength: number, error: NormalizedBleError | null = null): CoreStreamPushResult {
    if (!this.recordRejectedValue(byteLength)) {
      return this.pushResult(false, true)
    }
    this.finishWithReason('overflow', error)
    return this.pushResult(false, true)
  }

  private recordRejectedValue(byteLength: number): boolean {
    this.assertByteLength(byteLength)
    if (this.isTerminal()) return false
    this.droppedItems += 1
    this.droppedBytes += byteLength
    return true
  }

  overflowCounters(): {
    readonly droppedItems: ResourceCount
    readonly droppedBytes: ResourceCount
    readonly replacedItems: ResourceCount
  } {
    return {
      droppedItems: resourceCount(this.totalDroppedItems()),
      droppedBytes: resourceCount(this.totalDroppedBytes()),
      replacedItems: resourceCount(this.totalReplacedItems())
    }
  }

  /** Attribution captured with the notice, even if production continues before its reader resumes. */
  overflowNoticeStages(notice: StreamOverflowNotice) {
    const stages = this.overflowStages.get(notice)
    if (stages === undefined)
      throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.overflow-stages')
    return stages
  }

  /** The opt-in reader retains a terminal until explicitly committed or rolled back. */
  claimConsumer(options: { readonly retainTerminal: true }): CoreStreamTerminalClaimConsumer<Value>
  claimConsumer(): CoreStreamClaimConsumer<Value>
  claimConsumer(options?: { readonly retainTerminal: true }): CoreStreamTerminalClaimConsumer<Value> {
    const owner = {}
    let closed = false
    let reading = false
    const claimValue = (): CoreStreamClaim<Value> | null => {
      if (closed) throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.claim-closed')
      if (
        this.activeClaim !== null ||
        this.activeTerminalClaim !== null ||
        this.readingEntry !== null ||
        this.consumers.length !== 0 ||
        (this.claimSourceLoss[0] !== undefined &&
          (this.values[0] === undefined || this.values[0].sequence >= this.claimSourceLoss[0].sequence)) ||
        this.overflowNotice !== null
      )
        return null
      const entry = this.values[0]
      const peerLoss = this.peerLosses[0]
      if (entry === undefined || !entry.visible || (peerLoss !== undefined && peerLoss.sequence <= entry.sequence))
        return null
      const claim: CoreStreamClaim<Value> = {
        item: entry.item,
        commit: () => this.commitClaim(claim),
        rollback: () => this.rollbackClaim(claim)
      }
      this.activeClaim = { claim, entry, owner }
      return claim
    }
    return {
      claim: claimValue,
      next: async () => {
        if (reading) throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.claim-read-pending')
        reading = true
        try {
          while (!closed) {
            const claim = claimValue()
            if (claim !== null) return { kind: 'claim', claim }
            if (options?.retainTerminal === true) {
              const terminal = this.claimTerminal(owner)
              if (terminal !== null) return { kind: 'terminal-claim', claim: terminal }
            }
            // Claim readers normally transfer controls without allocating an
            // iterator. A durable terminal is different: its exact failure
            // is restored by the iterator hydration path, so bypassing that
            // path would publish a receipt without its verified payload.
            if (
              options?.retainTerminal !== true &&
              this.terminalNotice?.evidence !== undefined &&
              this.terminalEvidenceHydrator !== null &&
              this.claimSourceLoss.length === 0 &&
              this.overflowNotice === null &&
              this.peerLosses.length === 0 &&
              this.values.length === 0
            ) {
              const iterator = this[Symbol.asyncIterator]()
              const hydrated = await iterator.next()
              if (hydrated.done || hydrated.value === undefined)
                throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.claim-hydrated-terminal')
              if (hydrated.value.kind === 'value')
                throw contractError(
                  'lifecycle.invariant-violation',
                  'stream',
                  'bounded-stream.claim-hydrated-terminal-kind'
                )
              return { kind: 'control', item: hydrated.value }
            }
            const control = this.takeNextClaimControl(options?.retainTerminal === true)
            if (control !== null) return { kind: 'control', item: control }
            // Consuming a peer-loss control can expose the terminal that was
            // already installed by close. Re-check before waiting so the
            // terminal remains claimable rather than being downgraded to a
            // transient control delivery.
            if (options?.retainTerminal === true) {
              const terminal = this.claimTerminal(owner)
              if (terminal !== null) return { kind: 'terminal-claim', claim: terminal }
            }
            if (this.terminalDelivered) return { kind: 'done' }
            await new Promise<void>(resolve => this.claimWaiters.add(resolve))
          }
          return { kind: 'done' }
        } finally {
          reading = false
        }
      },
      close: () => {
        if (closed) return
        closed = true
        if (this.activeClaim?.owner === owner) this.rollbackClaim(this.activeClaim.claim)
        if (this.activeTerminalClaim?.owner === owner) this.rollbackTerminalClaim(this.activeTerminalClaim.claim)
        this.wakeClaimWaiters()
      }
    }
  }

  private takeNextClaimControl(
    retainTerminal: boolean
  ): Exclude<StreamItem<Value>, StreamValue<Value>> | StreamSourceLossNotice | StreamPeerSourceLossNotice | null {
    if (this.activeClaim !== null || this.activeTerminalClaim !== null || this.consumers.length !== 0) return null
    const first = this.values[0]
    const pendingSourceLoss = this.claimSourceLoss[0]
    if (pendingSourceLoss !== undefined && first !== undefined && first.sequence < pendingSourceLoss.sequence)
      return null
    const sourceLoss = this.takeNextClaimSourceLossControl()
    if (sourceLoss !== null) return sourceLoss
    const peerLoss = this.peerLosses[0]
    if (peerLoss !== undefined && (this.values[0] === undefined || peerLoss.sequence <= this.values[0].sequence)) {
      this.peerLosses.shift()
      return peerLoss.notice
    }
    if (
      retainTerminal &&
      this.claimSourceLoss.length === 0 &&
      this.overflowNotice === null &&
      this.peerLosses.length === 0 &&
      this.values.length === 0
    )
      return null
    const nextValue = this.values[0]
    const peerLossAfterSourceLoss = this.peerLosses[0]
    if (
      this.claimSourceLoss.length === 0 &&
      this.overflowNotice === null &&
      nextValue !== undefined &&
      (peerLossAfterSourceLoss === undefined || nextValue.sequence < peerLossAfterSourceLoss.sequence)
    )
      return null
    const item = this.takeNextItem()
    if (item === null) return null
    if (item.kind === 'value')
      throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.claim-control')
    if (item.kind === 'terminal') {
      try {
        this.releaseSourceLossRetentions()
        this.notifyRetainedValuesDrained()
      } catch (error) {
        throw new CoreStreamTerminalDeliveryError(item, error)
      }
    }
    return item
  }

  /** Claim consumers retain ordered source-loss provenance before the accepted prefix. */
  private takeNextClaimSourceLossControl(): StreamSourceLossNotice | null {
    const next = this.claimSourceLoss.shift()
    return next?.notice ?? null
  }

  private claimTerminal(owner: object): CoreStreamTerminalClaim | null {
    if (
      this.activeClaim !== null ||
      this.activeTerminalClaim !== null ||
      this.readingEntry !== null ||
      this.consumers.length !== 0 ||
      (this.ownerClosed && this.claimSourceLoss.length !== 0) ||
      this.overflowNotice !== null ||
      (this.ownerClosed && this.peerLosses.length !== 0) ||
      this.values.length !== 0 ||
      this.terminalNotice === null
    )
      return null
    const claim: CoreStreamTerminalClaim = {
      item: this.terminalNotice,
      commit: () => this.commitTerminalClaim(claim),
      rollback: () => this.rollbackTerminalClaim(claim)
    }
    this.activeTerminalClaim = { claim, owner }
    return claim
  }

  /** Error bytes already charged to this stream, excluding its reserved control capacity. */
  claimedTerminalBytes(claim: CoreStreamTerminalClaim): number {
    if (this.activeTerminalClaim?.claim !== claim || this.terminalNotice === null) {
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-terminal-claim')
    }
    return claim.item.error === undefined ? 0 : utf8ByteLength(JSON.stringify(claim.item.error))
  }

  /** Prepare the exact claimed terminal for a callback-free ownership transition. */
  prepareTerminalTransfer(claim: CoreStreamTerminalClaim): {
    readonly credit: number
    detach(): () => void
  } {
    const credit = this.claimedTerminalBytes(claim) + this.controlReservationBytes()
    return {
      credit,
      detach: () => {
        this.claimedTerminalBytes(claim)
        this.activeTerminalClaim = null
        this.terminalNotice = null
        this.terminalDelivered = true
        return () => {
          try {
            this.releaseSourceLossRetentions()
            this.notifyRetainedValuesDrained()
          } finally {
            this.flushPendingConsumers()
          }
        }
      }
    }
  }

  /** Prepare an inline terminal without making it visible before admission. */
  prepareTerminalInstall(notice: StreamTerminalNotice): {
    readonly charge: number
    install(): () => void
  } {
    if (this.ownerClosed || this.isTerminal())
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.terminal-destination')
    const charge = notice.error == null ? 0 : utf8ByteLength(JSON.stringify(notice.error))
    return {
      charge,
      install: () => {
        if (this.ownerClosed || this.isTerminal())
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.terminal-destination')
        this.observeSourceTerminal(notice)
        this.settledTerminalReason = notice.reason
        this.terminalNotice = this.makeTerminal(notice.reason, notice.error ?? null, notice.evidence)
        return () => this.flushPendingConsumers()
      }
    }
  }

  private commitTerminalClaim(claim: CoreStreamTerminalClaim): void {
    this.claimedTerminalBytes(claim)
    this.activeTerminalClaim = null
    this.terminalNotice = null
    this.terminalDelivered = true
    try {
      this.releaseSourceLossRetentions()
      this.notifyRetainedValuesDrained()
    } finally {
      this.flushPendingConsumers()
    }
  }

  private rollbackTerminalClaim(claim: CoreStreamTerminalClaim): void {
    this.claimedTerminalBytes(claim)
    this.activeTerminalClaim = null
    this.flushPendingConsumers()
  }

  private wakeClaimWaiters(): void {
    const waiters = [...this.claimWaiters]
    this.claimWaiters.clear()
    for (const wake of waiters) wake()
  }

  /** Validate identity and return the still-owned charge, for quota transfer. */
  isClaimCurrent(claim: CoreStreamClaim<Value>): boolean {
    return this.activeClaim?.claim === claim && this.values[0] === this.activeClaim.entry
  }

  claimedBytes(claim: CoreStreamClaim<Value>): number {
    if (!this.isClaimCurrent(claim) || this.activeClaim === null) {
      throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-claim')
    }
    return this.activeClaim.entry.chargedBytes
  }

  /** Inspect and detach a claim before running any cleanup callbacks. */
  prepareClaimTransfer(claim: CoreStreamClaim<Value>): {
    readonly eviction: CoreStreamEviction
    detach(): () => void
  } {
    const chargedBytes = this.claimedBytes(claim)
    const entry = this.activeClaim?.entry
    if (entry === undefined) throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-claim')
    return {
      eviction: { chargedBytes, reference: entry.ownedReference?.() ?? null },
      detach: () => {
        this.claimedBytes(claim)
        this.values.shift()
        this.subtractEntry(entry)
        this.activeClaim = null
        return () => this.cleanupAndFlush([entry])
      }
    }
  }

  /** Commits the exact live claim; used by quota transfer after admission. */
  commitClaim(claim: CoreStreamClaim<Value>): void {
    this.claimedBytes(claim)
    const entry = this.values.shift()
    if (entry === undefined) throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.claim')
    this.subtractEntry(entry)
    this.activeClaim = null
    this.cleanupAndFlush([entry])
  }

  private rollbackClaim(claim: CoreStreamClaim<Value>): void {
    this.claimedBytes(claim)
    this.activeClaim = null
    this.flushPendingConsumers()
  }

  [Symbol.asyncIterator](): BoundedAsyncStreamIterator<Value> {
    const state: StreamIteratorState = { closed: false, terminalReceived: false, terminalHydration: null }
    const iterator: BoundedAsyncStreamIterator<Value> = {
      next: () => this.next(state),
      return: () => this.returnIterator(state),
      [Symbol.asyncIterator]: () => iterator
    }
    return iterator
  }

  private next(iterator: StreamIteratorState): Promise<IteratorResult<StreamItem<Value>>> {
    if (iterator.closed) {
      return Promise.resolve({ done: true, value: undefined })
    }
    if (iterator.terminalHydration !== null)
      return iterator.terminalHydration.then(() => ({ done: true, value: undefined })) as Promise<
        IteratorResult<StreamItem<Value>>
      >
    if (this.hydratedTerminalEvidence !== null) {
      if (iterator.terminalReceived) return Promise.resolve({ done: true, value: undefined })
      iterator.terminalReceived = true
      return Promise.resolve({ done: false, value: this.hydratedTerminalEvidence })
    }
    if (this.terminalEvidenceHydration !== null)
      return this.coalesceTerminalHydration(iterator, this.terminalEvidenceHydration)
    let nextItem: StreamItem<Value> | null = null
    try {
      nextItem = this.takeNextItem()
      this.throwCleanupFailures()
    } catch (error) {
      if (nextItem?.kind === 'value') return Promise.reject(new CoreStreamDeliveryError(nextItem, error))
      if (nextItem?.kind === 'terminal') return Promise.reject(new CoreStreamTerminalDeliveryError(nextItem, error))
      return Promise.reject(error)
    }
    if (nextItem !== null) {
      if (nextItem.kind === 'terminal' && nextItem.evidence !== undefined && this.terminalEvidenceHydrator !== null) {
        const hydration = new Promise<StreamTerminalNotice>((resolve, reject) => {
          try {
            resolve(this.terminalEvidenceHydrator!(nextItem))
          } catch (error) {
            reject(error)
          }
        })
        const generation = this.terminalEvidenceHydrationGeneration
        this.terminalEvidenceHydration = hydration
        return this.coalesceTerminalHydration(
          iterator,
          hydration.then(
            value => {
              if (generation !== this.terminalEvidenceHydrationGeneration || this.ownerClosed) return value
              this.terminalEvidenceHydration = null
              try {
                this.releaseSourceLossRetentions()
                this.notifyRetainedValuesDrained()
              } catch (error) {
                throw new CoreStreamTerminalDeliveryError(value, error)
              }
              this.hydratedTerminalEvidence = value
              return value
            },
            error => {
              this.terminalEvidenceHydration = null
              if (generation !== this.terminalEvidenceHydrationGeneration || this.ownerClosed) return nextItem
              // A failed durable read is retryable: restore the exact terminal
              // rather than consuming its reference or inventing a replacement.
              this.terminalNotice = nextItem
              this.terminalDelivered = false
              throw error
            }
          )
        )
      }
      if (nextItem.kind === 'terminal') {
        try {
          this.releaseSourceLossRetentions()
          this.notifyRetainedValuesDrained()
        } catch (error) {
          return Promise.reject(new CoreStreamTerminalDeliveryError(nextItem, error))
        }
      }
      return Promise.resolve({ done: false, value: nextItem })
    }
    if (this.terminalDelivered) {
      return Promise.resolve({ done: true, value: undefined })
    }
    return new Promise((resolve, reject) => {
      if (iterator.closed) {
        resolve({ done: true, value: undefined })
        return
      }
      this.consumers.push({ iterator, resolve, reject })
    })
  }

  private coalesceTerminalHydration(
    iterator: StreamIteratorState,
    hydration: Promise<StreamTerminalNotice>
  ): Promise<IteratorResult<StreamItem<Value>>> {
    const generation = this.terminalEvidenceHydrationGeneration
    const result = hydration
      .then<IteratorResult<StreamItem<Value>>>(value => {
        if (iterator.closed || generation !== this.terminalEvidenceHydrationGeneration)
          return { done: true, value: undefined }
        iterator.terminalReceived = true
        return { done: false, value }
      })
      .finally(() => {
        iterator.terminalHydration = null
      })
    iterator.terminalHydration = result as Promise<IteratorResult<StreamItem<unknown>>>
    return result
  }

  private returnIterator(iterator: StreamIteratorState): Promise<IteratorResult<StreamItem<Value>>> {
    if (iterator.closed) {
      return Promise.resolve({ done: true, value: undefined })
    }
    iterator.closed = true
    const pendingConsumers = this.removePendingConsumers(iterator)
    for (const consumer of pendingConsumers) {
      consumer.resolve({ done: true, value: undefined })
    }
    this.wakeClaimWaiters()
    return Promise.resolve({ done: true, value: undefined })
  }

  private removePendingConsumers(iterator: StreamIteratorState): PendingConsumer<Value>[] {
    const pendingConsumers: PendingConsumer<Value>[] = []
    const remainingConsumers: PendingConsumer<Value>[] = []
    for (const consumer of this.consumers) {
      if (consumer.iterator === iterator) {
        pendingConsumers.push(consumer)
        continue
      }
      remainingConsumers.push(consumer)
    }
    this.consumers.length = 0
    for (const consumer of remainingConsumers) {
      this.consumers.push(consumer)
    }
    return pendingConsumers
  }

  private applyOverflow(
    value: Value,
    byteLength: number,
    key: string | null,
    payloadBytes: number,
    preserveAccepted: boolean,
    retainedBytes: number
  ): CoreStreamPushResult {
    if (this.overflowPolicy === 'error') {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      if (preserveAccepted || this.activeClaim !== null) this.finishWithReason('overflow')
      else this.closeWithReason('overflow')
      return this.pushResult(false, true)
    }
    if (this.overflowPolicy === 'drop-newest') {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.flushPendingConsumers()
      return this.pushResult(false, false)
    }
    if (this.overflowPolicy === 'latest') {
      return this.replaceLatest(value, byteLength, key, payloadBytes, retainedBytes)
    }
    return this.dropOldestThenAppend(value, byteLength, key, payloadBytes, retainedBytes)
  }

  private replaceLatest(
    value: Value,
    byteLength: number,
    key: string | null,
    payloadBytes: number,
    retainedBytes: number
  ): CoreStreamPushResult {
    if (key === null) {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.flushPendingConsumers()
      return this.pushResult(false, false)
    }
    const index = this.values.findIndex(
      entry => entry.key === key && entry.visible && entry !== this.activeClaim?.entry
    )
    if (index < 0) {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.flushPendingConsumers()
      return this.pushResult(false, false)
    }
    const previous = this.values[index]
    if (previous === undefined) {
      throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.latest')
    }
    this.values.splice(index, 1)
    this.subtractEntry(previous)
    if (!this.fits(byteLength)) {
      this.values.splice(index, 0, previous)
      this.retainedValueBytes += previous.byteLength
      this.externallyChargedValueBytes += previous.byteLength - previous.chargedBytes
      this.retainedPayloadByteCount += previous.payloadBytes
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.flushPendingConsumers()
      return this.pushResult(false, false)
    }
    this.replacedItems += 1
    this.droppedBytes += previous.byteLength
    this.noteOverflow()
    this.append(value, byteLength, key, payloadBytes, retainedBytes, false)
    this.cleanupAndFlush([previous])
    return this.pushResult(true, false)
  }

  private dropOldestThenAppend(
    value: Value,
    byteLength: number,
    key: string | null,
    payloadBytes: number,
    retainedBytes: number
  ): CoreStreamPushResult {
    const removedEntries: RetainedValue<Value>[] = []
    // Do not discard an accepted prefix unless removing it can make room.
    // An oversized value, or a claim/stage behind that prefix, cannot fit
    // even after eviction; in that case only the incoming value is lost.
    let remainingBytes = this.retainedValueBytes
    let remainingCount = this.values.length
    for (const entry of this.values) {
      if (remainingCount < this.limits.itemCapacity && remainingBytes + byteLength <= this.valueByteCapacity()) break
      if (entry === this.activeClaim?.entry || !entry.visible) break
      remainingCount -= 1
      remainingBytes -= entry.byteLength
    }
    if (remainingCount >= this.limits.itemCapacity || remainingBytes + byteLength > this.valueByteCapacity()) {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.flushPendingConsumers()
      return this.pushResult(false, false)
    }
    while (
      this.values.length > 0 &&
      !this.fits(byteLength) &&
      this.values[0] !== this.activeClaim?.entry &&
      this.values[0]?.visible === true
    ) {
      const removed = this.values.shift()
      if (removed === undefined) {
        throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.drop-oldest')
      }
      this.subtractEntry(removed)
      removedEntries.push(removed)
      this.droppedItems += 1
      this.droppedBytes += removed.byteLength
    }
    if (!this.fits(byteLength)) {
      this.droppedItems += 1
      this.droppedBytes += byteLength
      this.noteOverflow()
      this.cleanupAndFlush(removedEntries)
      return this.pushResult(false, false)
    }
    this.noteOverflow()
    this.append(value, byteLength, key, payloadBytes, retainedBytes, false)
    this.cleanupAndFlush(removedEntries)
    return this.pushResult(true, false)
  }

  private append(
    value: Value,
    byteLength: number,
    key: string | null,
    payloadBytes: number,
    retainedBytes: number,
    flush = true
  ): void {
    this.values.push({
      sequence: this.nextSequence++,
      item: { kind: 'value', value },
      byteLength,
      payloadBytes,
      key,
      release: null,
      chargedBytes: retainedBytes,
      copyForDelivery: null,
      visible: true
    })
    this.retainedValueBytes += byteLength
    this.externallyChargedValueBytes += byteLength - retainedBytes
    this.retainedPayloadByteCount += payloadBytes
    if (flush) this.flushPendingConsumers()
  }

  private subtractEntry(entry: RetainedValue<Value>): void {
    this.retainedValueBytes -= entry.byteLength
    this.externallyChargedValueBytes -= entry.byteLength - entry.chargedBytes
    this.retainedPayloadByteCount -= entry.payloadBytes
  }

  private releaseEntry(entry: RetainedValue<Value>): void {
    // A reentrant queue operation must not observe another entry's failure.
    const earlierFailures = this.cleanupFailures.splice(0)
    try {
      entry.release?.()
    } catch (error) {
      this.cleanupFailures.push(error)
    } finally {
      this.cleanupFailures.unshift(...earlierFailures)
    }
  }

  private throwCleanupFailures(): void {
    const failures = this.cleanupFailures.splice(0)
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Stream value cleanup failed')
  }

  private cleanupAndFlush(entries: readonly RetainedValue<Value>[]): void {
    for (const entry of entries) this.releaseEntry(entry)
    try {
      this.flushPendingConsumers()
    } catch (error) {
      this.cleanupFailures.push(error)
    }
    this.throwCleanupFailures()
  }

  private noteOverflow(policy: OverflowPolicy = this.overflowPolicy): void {
    this.overflowNotice = {
      kind: 'overflow',
      policy,
      droppedItems: resourceCount(this.totalDroppedItems()),
      droppedBytes: resourceCount(this.totalDroppedBytes()),
      replacedItems: resourceCount(this.totalReplacedItems())
    }
    this.overflowStages.set(this.overflowNotice, {
      upstream: {
        droppedItems: this.sourceDroppedItems,
        droppedBytes: this.sourceDroppedBytes,
        replacedItems: this.sourceReplacedItems
      },
      subscriber: {
        droppedItems: this.droppedItems,
        droppedBytes: this.droppedBytes,
        replacedItems: this.replacedItems
      }
    })
  }

  private takeNextItem(): StreamItem<Value> | null {
    if (this.activeClaim !== null || this.activeTerminalClaim !== null) return null
    if (this.overflowNotice !== null) {
      const notice = this.overflowNotice
      this.overflowNotice = null
      return notice
    }
    const peerLoss = this.peerLosses[0]
    const firstValue = this.values[0]
    if (peerLoss !== undefined && (firstValue === undefined || peerLoss.sequence <= firstValue.sequence)) {
      this.peerLosses.shift()
      // A peer loss is ordered between source records. Later records cannot
      // be presented as a complete continuation, so discard that suffix and
      // expose the legacy overflow terminal immediately after the prefix.
      this.closeWithReason('overflow')
      return this.takeNextItem()
    }
    const retained = this.values[0]
    if (retained !== undefined) {
      if (!retained.visible || this.readingEntry === retained) return null
      this.readingEntry = retained
      let item: StreamValue<Value>
      try {
        const value = retained.item.value
        item = { kind: 'value', value: retained.copyForDelivery === null ? value : retained.copyForDelivery(value) }
        if (this.values[0] !== retained) {
          throw contractError('lifecycle.invalid-state', 'stream', 'bounded-stream.stale-delivery')
        }
      } finally {
        this.readingEntry = null
      }
      this.values.shift()
      this.subtractEntry(retained)
      this.releaseEntry(retained)
      try {
        this.notifyRetainedValuesDrained()
      } catch (error) {
        this.cleanupFailures.push(error)
      }
      return item
    }
    if (this.terminalNotice !== null) {
      const terminal = this.terminalNotice
      this.terminalNotice = null
      this.terminalDelivered = true
      return terminal
    }
    return null
  }

  private flushPendingConsumers(): void {
    try {
      this.wakeClaimWaiters()
      while (this.consumers.length > 0) {
        const consumer = this.consumers.shift()
        if (consumer === undefined)
          throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.flush')
        if (this.terminalEvidenceHydration !== null) {
          this.coalesceTerminalHydration(consumer.iterator, this.terminalEvidenceHydration).then(
            consumer.resolve,
            consumer.reject
          )
          continue
        }
        let item: StreamItem<Value> | null
        try {
          item = this.takeNextItem()
        } catch (error) {
          this.consumers.unshift(consumer)
          if (this.terminalDelivered) this.finishPendingConsumers()
          throw error
        }
        if (item === null) {
          this.consumers.unshift(consumer)
          if (this.terminalDelivered) {
            this.finishPendingConsumers()
          }
          return
        }
        if (item.kind === 'terminal' && item.evidence !== undefined && this.terminalEvidenceHydrator !== null) {
          this.terminalNotice = item
          this.terminalDelivered = false
          this.next(consumer.iterator).then(consumer.resolve, consumer.reject)
          continue
        }
        consumer.resolve({ done: false, value: item })
        if (item.kind === 'terminal') {
          try {
            this.releaseSourceLossRetentions()
            this.notifyRetainedValuesDrained()
          } catch (error) {
            this.cleanupFailures.push(error)
          } finally {
            this.finishPendingConsumers()
          }
        }
      }
    } catch (error) {
      this.cleanupFailures.push(error)
    } finally {
      this.throwCleanupFailures()
    }
  }

  private finishPendingConsumers(): void {
    while (this.consumers.length > 0) {
      const consumer = this.consumers.shift()
      if (consumer === undefined) {
        throw contractError('lifecycle.invariant-violation', 'stream', 'bounded-stream.complete')
      }
      consumer.resolve({ done: true, value: undefined })
    }
  }

  private makeTerminal(
    reason: CoreStreamTerminalReason,
    error: NormalizedBleError | null = null,
    evidence?: TerminalEvidenceCommit
  ): StreamTerminalNotice {
    const terminal: StreamTerminalNotice = {
      kind: 'terminal',
      reason,
      droppedItems: resourceCount(this.totalDroppedItems()),
      droppedBytes: resourceCount(this.totalDroppedBytes()),
      replacedItems: resourceCount(this.totalReplacedItems()),
      ...(error === null ? {} : { error }),
      ...(evidence === undefined ? {} : { evidence })
    }
    return terminal
  }

  /** Source loss is uncertainty; it cannot replace a later authoritative terminal. */
  private sourceLossProjectsLegacyOverflow(
    reason: CoreStreamTerminalReason,
    error: NormalizedBleError | null,
    evidence: TerminalEvidenceCommit | undefined
  ): boolean {
    return (
      this.sourceLossDetails.length !== 0 &&
      error === null &&
      evidence === undefined &&
      (reason === 'closed' || reason === 'owner-released' || reason === 'overflow')
    )
  }

  private clearOverflowCounters(): void {
    this.droppedItems = 0
    this.droppedBytes = 0
    this.replacedItems = 0
    this.sourceDroppedItems = 0
    this.sourceDroppedBytes = 0
    this.sourceReplacedItems = 0
  }

  private fits(byteLength: number): boolean {
    return (
      this.values.length < this.limits.itemCapacity && this.retainedValueBytes + byteLength <= this.valueByteCapacity()
    )
  }

  private valueByteCapacity(): number {
    return this.limits.byteCapacity - this.controlReservationBytes()
  }

  private controlReservationBytes(): number {
    return this.terminalDelivered ? 0 : this.limits.reservedControlCapacity
  }

  isTerminal(): boolean {
    return this.terminalNotice !== null || this.terminalDelivered
  }

  terminalReason(): CoreStreamTerminalReason | null {
    return this.settledTerminalReason
  }

  private assertByteLength(byteLength: number): void {
    if (!Number.isSafeInteger(byteLength) || byteLength < 0) {
      throw contractError('bytes.invalid', 'stream', 'bounded-stream.byte-length')
    }
  }

  private pushResult(accepted: boolean, terminated: boolean): CoreStreamPushResult {
    return { accepted, terminated, retainedBytes: this.retainedBytes() }
  }

  private totalDroppedItems(): number {
    return this.droppedItems + this.sourceDroppedItems
  }

  private totalDroppedBytes(): number {
    return this.droppedBytes + this.sourceDroppedBytes
  }

  private totalReplacedItems(): number {
    return this.replacedItems + this.sourceReplacedItems
  }

  private projectLatestRetainedBytes(byteLength: number, key: string | null, retainedBytes: number): number {
    if (key === null) {
      return this.retainedBytes()
    }
    const current = this.values.find(entry => entry.key === key && entry.visible && entry !== this.activeClaim?.entry)
    if (current === undefined) {
      return this.retainedBytes()
    }
    const projectedValueBytes = this.retainedValueBytes - current.byteLength + byteLength
    if (projectedValueBytes > this.valueByteCapacity()) {
      return this.retainedBytes()
    }
    return this.retainedBytes() - current.chargedBytes + retainedBytes
  }

  private projectDropOldestRetainedBytes(byteLength: number, retainedBytes: number): number {
    let projectedValueBytes = this.retainedValueBytes
    let projectedChargedBytes = this.retainedValueBytes - this.externallyChargedValueBytes
    let projectedCount = this.values.length
    for (const entry of this.values) {
      if (projectedCount < this.limits.itemCapacity && projectedValueBytes + byteLength <= this.valueByteCapacity()) {
        return (
          this.retainedBytes() -
          (this.retainedValueBytes - this.externallyChargedValueBytes) +
          projectedChargedBytes +
          retainedBytes
        )
      }
      if (entry === this.activeClaim?.entry || !entry.visible) return this.retainedBytes()
      projectedValueBytes -= entry.byteLength
      projectedChargedBytes -= entry.chargedBytes
      projectedCount -= 1
    }
    if (projectedCount < this.limits.itemCapacity && projectedValueBytes + byteLength <= this.valueByteCapacity()) {
      return (
        this.retainedBytes() -
        (this.retainedValueBytes - this.externallyChargedValueBytes) +
        projectedChargedBytes +
        retainedBytes
      )
    }
    return this.retainedBytes()
  }
}
