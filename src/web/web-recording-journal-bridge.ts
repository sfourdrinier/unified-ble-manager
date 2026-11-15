import { webRecordingNoncommittedOrigin } from './web-indexeddb-transaction'
import {
  webHostErrorEvidence,
  webRecordingContractError as contractError,
  webStorageError
} from './web-recording-errors'
import { webRecordingReadinessFailure } from './web-indexeddb-transaction'
import type { OrderedRecordingFailure, OrderedRecordingReason, OrderedRecordingRecord } from '../core/ordered-recording'
import type { OrderedRecordingHostStopResult } from '../core/continuation-recording'
import type { WebIngressRecord, WebIngressTerminalReason } from './web-notification-ingress'
import {
  createWebOrderedRecordingJournal,
  type WebOrderedJournalDatabase,
  type WebOrderedRecordingDeclaration,
  type WebOrderedRecordingDurability,
  type WebOrderedRecordingProducer,
  type WebOrderedRecordingReconciliation,
  type WebRecordingTerminalObservation
} from './web-ordered-recording-journal'

/** The host holds exclusive partition authority and owns the persisted epoch. */
export interface WebRecordingAuthority {
  current(): boolean
  release(): Promise<void>
}

type Declaration = Omit<WebOrderedRecordingDeclaration, 'memberEpochs'>
type DeclarationSource =
  | Declaration
  | ((memberships: WebOrderedRecordingDeclaration['memberEpochs']) => Promise<Declaration>)
type Factory = (
  database: WebOrderedJournalDatabase,
  declaration: WebOrderedRecordingDeclaration
) => WebOrderedRecordingProducer
type UncertainDelivery = {
  readonly delivery: { readonly id: string; readonly record: OrderedRecordingRecord }
  failure: { readonly error: unknown } | null
  reconciliationFailure: { readonly error: unknown } | null
  lateReconciliationFailure: { readonly error: unknown } | null
}

const sourceFailure = (reason: WebIngressTerminalReason): OrderedRecordingFailure => ({
  code:
    reason === 'operation-aborted'
      ? 'operation.aborted'
      : reason === 'operation-timed-out'
        ? 'operation.timed-out'
        : 'stream.failed',
  domain: 'stream',
  operation: 'web-recording.source',
  detail: reason,
  platformDomain: null,
  platformCode: null,
  platformMessage: null,
  platformMetadata: {}
})

function terminalReason(reason: WebIngressTerminalReason): OrderedRecordingReason {
  switch (reason) {
    case 'closed':
    case 'owner-released':
    case 'connection-lost':
    case 'service-changed':
      return { kind: reason }
    case 'overflow':
      return { kind: 'source-overflow' }
    case 'ordinal-exhausted':
      return { kind: 'source-ordinal-exhausted' }
    case 'host-reset':
      return { kind: 'host-reset', failure: sourceFailure(reason) }
    case 'source-failed':
    case 'operation-aborted':
    case 'operation-timed-out':
      return { kind: 'source-failed', failure: sourceFailure(reason) }
  }
}

/** Translate only facts supplied by ingress; controls have no invented ordinal. */
export function webRecordingRecord(record: WebIngressRecord, version: 1 | 2 = 1): OrderedRecordingRecord {
  switch (record.kind) {
    case 'value':
      if (record.sourceMembership === undefined || record.delivery === undefined || record.provenance === undefined)
        throw contractError('protocol.malformed', 'stream', 'web-recording.ingress-membership')
      if (record.sourceMembership.member !== record.memberId)
        throw contractError('protocol.malformed', 'stream', 'web-recording.ingress-member')
      return {
        kind: 'value',
        ordinal: record.ordinal,
        member: record.memberId,
        membership: { ...record.sourceMembership },
        bytes: new Uint8Array(record.value),
        delivery: record.delivery,
        provenance: record.provenance
      }
    case 'marker':
      return { kind: 'marker', markerId: record.markerId, ordinal: record.ordinal }
    case 'loss':
      if (
        !Number.isSafeInteger(record.lostValues) ||
        record.lostValues < 0 ||
        !Number.isSafeInteger(record.lostBytes) ||
        record.lostBytes < 0
      )
        throw contractError('protocol.malformed', 'stream', 'web-recording.ingress-loss')
      return {
        kind: 'loss',
        scope: { kind: 'group' },
        unit: 'values',
        count: String(record.lostValues),
        bytes: String(record.lostBytes)
      }
    case 'failure-observation':
    case 'failure-observations-omitted':
      if (version !== 2) throw contractError('protocol.incompatible', 'stream', 'web-recording.ingress-version')
      return record
    case 'terminal':
      if (version === 2)
        return { kind: 'terminal', reason: terminalReason(record.reason), tailUncertain: record.uncertainTail }
      return {
        kind: 'terminal',
        reason: terminalReason(record.reason),
        tailUncertain: record.uncertainTail,
        rejectedValues: null,
        rejectedLoss: null,
        sourceRejection: null,
        sourceFailureMember: null,
        secondaryFailure: null,
        additionalFailureCount: '0',
        failureSummaryVersion: 1,
        failureCountOverflowed: false,
        failureObservationsOmitted: false
      }
  }
}

const absentOrigins = new WeakMap<object, unknown>()
const reconciledAbsentDeliveries = new WeakMap<object, string>()
class ProvenAbsent extends Error {
  constructor(
    readonly original: unknown,
    reconciledDeliveryId?: string
  ) {
    super('Web recording transaction did not commit')
    absentOrigins.set(this, original)
    if (reconciledDeliveryId !== undefined) reconciledAbsentDeliveries.set(this, reconciledDeliveryId)
  }
}

/** The exact previously uncertain delivery whose reconciliation proved absence. */
export function webRecordingReconciledAbsentDelivery(error: unknown): string | undefined {
  return typeof error === 'object' && error !== null ? reconciledAbsentDeliveries.get(error) : undefined
}

/** Preserve the underlying operation error when a non-commit was proved. */
export function webRecordingOriginalFailure(error: unknown): unknown {
  const noncommitted = webRecordingNoncommittedOrigin(error)
  if (noncommitted !== undefined) return noncommitted.original
  return ((typeof error === 'object' && error !== null) || typeof error === 'function') && absentOrigins.has(error)
    ? absentOrigins.get(error)
    : error
}

class WebRecordingOutcomeTimeout extends Error {
  constructor() {
    super('Web recording transaction outcome remains unknown')
    this.name = 'WebRecordingOutcomeTimeout'
  }
}

class WebRecordingReconciliationError extends AggregateError {
  readonly evidence: ReturnType<typeof webHostErrorEvidence>

  constructor(errors: readonly unknown[], message: string) {
    super(errors, message)
    // Reserve space for each leg so a deeply nested original failure cannot
    // crowd out the timeout or its late rejection in the public receipt.
    this.evidence = {
      errors: errors.slice(0, 4).map(error => {
        const projected = webHostErrorEvidence([error], 2).errors
        return Array.isArray(projected)
          ? (projected[0] ?? { omitted: 'unavailable error evidence' })
          : { omitted: 'unavailable error evidence' }
      }),
      ...(errors.length > 4 ? { errorsOmitted: true } : {})
    }
  }
}

/** Locate the bridge's own reconciliation failure inside the worker wrapper. */
export function webRecordingReconciliationFailure(error: unknown): ReturnType<typeof webHostErrorEvidence> | null {
  const pending: unknown[] = [error]
  const visited = new Set<unknown>()
  while (pending.length > 0 && visited.size < 64) {
    const current = pending.pop()
    if (visited.has(current)) continue
    visited.add(current)
    try {
      if (current instanceof WebRecordingReconciliationError) return current.evidence
      if (current instanceof AggregateError) pending.push(...current.errors.slice(0, 64))
    } catch {
      // Browser-thrown values are not trusted to expose a readable prototype or errors list.
    }
  }
  return null
}

async function waitBounded<T>(work: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([
      work,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new WebRecordingOutcomeTimeout()), 5000)
      })
    ])
  } finally {
    clearTimeout(timer)
  }
}

/** Internal bridge: authority is acquired first, and declaration waits for the
 * source's reserved membership. Neither a reconciliation read nor its durability
 * mode is evidence of the mode used by a lost write transaction. */
export function createWebRecordingJournalBridge(
  database: WebOrderedJournalDatabase,
  declaration: DeclarationSource,
  acquire: (signal?: AbortSignal) => Promise<WebRecordingAuthority>,
  factory: Factory = createWebOrderedRecordingJournal,
  noAuthorityAcquired: () => boolean = () => false
) {
  let authority: WebRecordingAuthority | null = null
  let producer: WebOrderedRecordingProducer | null = null
  let activeDeclaration: Declaration | null = null
  let uncertain: UncertainDelivery | null = null
  let reconciliation: Promise<WebOrderedRecordingReconciliation> | null = null
  let reconciliationTimedOut = false
  const lateReconciliationErrors = new WeakMap<Promise<WebOrderedRecordingReconciliation>, unknown>()
  let retirement: { id: string; first: Promise<void>; retry: Promise<void> | null } | null = null
  let unstartedCommitted = false
  let terminalDeliveryId: string | null = null
  let openPermission: (() => boolean) | null = null
  let last: {
    evidence: 'commit' | 'reconciled'
    durability: WebOrderedRecordingDurability | null
    reconciliationDurability: WebOrderedRecordingDurability | null
  } | null = null
  const current = (): boolean => authority?.current() === true
  const checkDispatch = (): void => {
    if (!current()) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
    if (openPermission !== null && !openPermission())
      throw contractError('operation.aborted', 'restoration', 'web-recording.open')
  }
  const producerDatabase: WebOrderedJournalDatabase = {
    ready: async () => {
      await database.ready?.()
    },
    transaction: (stores, mode, options) => {
      if (mode === 'readwrite' || openPermission !== null) checkDispatch()
      return database.transaction(stores, mode, options)
    }
  }
  const requireProducer = (): WebOrderedRecordingProducer => {
    if (!current() || producer === null)
      throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
    return producer
  }
  const reconcile = (
    journal: WebOrderedRecordingProducer,
    delivery: { readonly id: string; readonly record: OrderedRecordingRecord }
  ): Promise<WebOrderedRecordingReconciliation> => {
    if (reconciliation === null) {
      reconciliationTimedOut = false
      const pending = uncertain
      const work = journal.reconcile(delivery)
      reconciliation = work
      work.then(
        () => {
          if (reconciliation === work) reconciliation = null
        },
        error => {
          if (reconciliationTimedOut && pending !== null && uncertain === pending) {
            const evidence = webHostErrorEvidence([error]).errors
            if (Array.isArray(evidence)) {
              lateReconciliationErrors.set(work, evidence[0])
              pending.lateReconciliationFailure ??= { error: evidence[0] }
            }
          }
          if (reconciliation === work) reconciliation = null
        }
      )
    }
    const work = reconciliation
    return waitBounded(work).catch(error => {
      if (reconciliation === work && error instanceof WebRecordingOutcomeTimeout) reconciliationTimedOut = true
      if (lateReconciliationErrors.has(work)) throw lateReconciliationErrors.get(work)
      throw error
    })
  }
  const bridge = {
    prepare: async (signal?: AbortSignal): Promise<void> => {
      authority = await acquire(signal)
      if (!current()) throw contractError('capability.unavailable', 'restoration', 'web-recording.authority')
    },
    open: async (
      source: {
        memberships(): readonly { readonly member: string; readonly epoch: string }[] | null
      },
      mayOpen: () => boolean = () => true
    ): Promise<void> => {
      let dispatched = false
      try {
        if (!current() || producer !== null)
          throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.open')
        const reserved = source.memberships()
        if (reserved === null) throw contractError('protocol.malformed', 'stream', 'web-recording.membership')
        const memberEpochs = reserved.map(member => ({ ...member }))
        activeDeclaration =
          typeof declaration === 'function'
            ? await declaration(memberEpochs.map(member => ({ ...member })))
            : declaration
        if (!current()) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
        producer = factory(producerDatabase, { ...activeDeclaration, memberEpochs })
        await producer.declare()
        if (!current()) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
        if (!mayOpen()) throw contractError('operation.aborted', 'restoration', 'web-recording.open')
        dispatched = true
        openPermission = mayOpen
        try {
          await producer.open()
        } finally {
          openPermission = null
        }
        // The transaction's completion is its own receipt. The supervisor
        // independently fences enables if authority was lost in the meantime.
      } catch (error) {
        if (!dispatched) throw new ProvenAbsent(error)
        throw error
      }
    },
    commit: async (delivery: { readonly id: string; readonly record: WebIngressRecord }): Promise<number> => {
      const journal = requireProducer()
      const record = delivery.record
      const details = activeDeclaration
      if (details === null) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.declaration')
      if (
        record.kind === 'marker' &&
        (record.collectionId !== details.collectionId ||
          record.connectionGeneration !== details.context.connectionGeneration ||
          record.databaseGeneration !== details.context.databaseGeneration ||
          record.sourceGeneration?.kind !== 'opaque' ||
          record.sourceGeneration.value !== details.sourceGeneration ||
          record.sourceMembership != null)
      )
        throw contractError('protocol.malformed', 'stream', 'web-recording.marker-identity')
      const translated = { id: delivery.id, record: webRecordingRecord(record, details.context.version) }
      if (uncertain !== null) {
        if (JSON.stringify(uncertain.delivery) !== JSON.stringify(translated))
          throw contractError('lifecycle.invariant-violation', 'stream', 'web-recording.uncertain-head')
        // This read is issued only after the prior commit call settled. Its
        // overlapping IndexedDB scope orders it behind that transaction.
        let receipt: WebOrderedRecordingReconciliation
        try {
          receipt = await reconcile(journal, uncertain.delivery)
        } catch (error) {
          uncertain.reconciliationFailure ??= { error }
          const first = uncertain.reconciliationFailure.error
          throw new WebRecordingReconciliationError(
            [
              uncertain.failure?.error,
              first,
              ...(uncertain.lateReconciliationFailure === null ? [] : [uncertain.lateReconciliationFailure.error]),
              ...(first === error || uncertain.lateReconciliationFailure?.error === error ? [] : [error])
            ],
            'Web recording reconciliation failed'
          )
        }
        if (receipt.outcome === 'unknown')
          throw new WebRecordingReconciliationError(
            [
              uncertain.failure?.error,
              ...(uncertain.reconciliationFailure === null ? [] : [uncertain.reconciliationFailure.error]),
              ...(uncertain.lateReconciliationFailure === null ? [] : [uncertain.lateReconciliationFailure.error])
            ],
            'Web recording commit outcome remains unknown'
          )
        const previous = uncertain.failure
        uncertain = null
        if (receipt.outcome === 'committed') {
          last = { evidence: 'reconciled', durability: null, reconciliationDurability: receipt.durability }
          if (record.kind === 'terminal') terminalDeliveryId = delivery.id
          return receipt.ordinal
        }
        if (previous !== null) throw new ProvenAbsent(previous.error, delivery.id)
      }
      const pendingDelivery: UncertainDelivery = {
        delivery: translated,
        failure: null,
        reconciliationFailure: null,
        lateReconciliationFailure: null
      }
      uncertain = pendingDelivery
      let ordinal: number
      try {
        const receipt = await waitBounded(journal.commitWithDurability(translated))
        ordinal = receipt.ordinal
        uncertain = null
        last = { evidence: 'commit', durability: receipt.durability, reconciliationDurability: null }
      } catch (error) {
        pendingDelivery.failure = { error }
        let receipt: WebOrderedRecordingReconciliation
        try {
          receipt = await reconcile(journal, translated)
        } catch (reconciliationError) {
          pendingDelivery.reconciliationFailure = { error: reconciliationError }
          throw new WebRecordingReconciliationError([error, reconciliationError], 'Web recording reconciliation failed')
        }
        if (receipt.outcome === 'absent') {
          uncertain = null
          throw new ProvenAbsent(error)
        }
        if (receipt.outcome !== 'committed') throw error
        ordinal = receipt.ordinal
        uncertain = null
        last = { evidence: 'reconciled', durability: null, reconciliationDurability: receipt.durability }
      }
      if (record.kind === 'terminal') terminalDeliveryId = delivery.id
      return ordinal
    },
    retire: async (id: string): Promise<void> => {
      const journal = requireProducer()
      if (retirement !== null && retirement.id !== id)
        throw contractError('lifecycle.invariant-violation', 'stream', 'web-recording.uncertain-retirement')
      if (retirement === null) retirement = { id, first: journal.retire(id), retry: null }
      const pending = retirement
      try {
        await waitBounded(pending.first)
      } catch {
        // The repeated transaction overlaps the original stores and checks the
        // same durable delivery identity, so it is also its commit barrier.
        if (pending.retry === null) {
          const work = journal.retire(id)
          pending.retry = work
          work.catch(() => {
            if (pending.retry === work) pending.retry = null
          })
        }
        await waitBounded(pending.retry)
      }
      retirement = null
    },
    seal: (): Promise<void> => requireProducer().seal(),
    observeTerminal: async (
      observation: Omit<WebRecordingTerminalObservation, 'deliveryId'>,
      onDispatched?: () => void
    ): Promise<void> => {
      const journal = requireProducer()
      if (terminalDeliveryId === null)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.terminal-identity')
      await journal.observeTerminal({ ...observation, deliveryId: terminalDeliveryId }, onDispatched)
    },
    terminalRetained: async (operationId: string, result: OrderedRecordingHostStopResult): Promise<boolean> => {
      if (producer === null || terminalDeliveryId === null) return false
      return producer.terminalRetained({ operationId, deliveryId: terminalDeliveryId, result })
    },
    reconcileTerminal: async (operationId: string, result: OrderedRecordingHostStopResult) => {
      if (producer === null || terminalDeliveryId === null) return 'unknown' as const
      return producer.reconcileTerminal({ operationId, deliveryId: terminalDeliveryId, result })
    },
    releaseAuthority: async (): Promise<void> => {
      if (authority === null && noAuthorityAcquired()) return
      if (authority === null)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority-release-unproven')
      await authority.release()
    },
    canAcknowledge: current,
    // Commit reconciles its retained identity first; retire and seal are
    // idempotent journal transactions. This does not cover open/prepare.
    canRetryUnknown: () => true,
    lastCommit: () => last,
    isDefinitiveFailure: (error: unknown) =>
      webRecordingNoncommittedOrigin(error) !== undefined ||
      (((typeof error === 'object' && error !== null) || typeof error === 'function') && absentOrigins.has(error)),
    classifyFailure: (error: unknown): 'capacity' | 'failure' => {
      const original = webRecordingOriginalFailure(error)
      return (webRecordingReadinessFailure(original) ?? webStorageError(original, 'indexeddb-transaction')).normalized
        .code === 'stream.quota'
        ? 'capacity'
        : 'failure'
    }
  }
  return {
    ...bridge,
    /** No source was installed or enabled, and the local reservation has
     * already been released. Persist the terminal as the first delivery. */
    finalizeUnstarted: async (reason: WebIngressTerminalReason, onCommitted: () => void): Promise<void> => {
      if (!unstartedCommitted) {
        await bridge.commit({ id: '1', record: { kind: 'terminal', reason, uncertainTail: false } })
        unstartedCommitted = true
      }
      onCommitted()
      await bridge.retire('1')
    }
  }
}
