import { webRecordingRandomBytes } from './web-recording-errors'
import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { webRecordingContractError as contractError } from './web-recording-errors'
import {
  isRecordingId,
  MIN_RECORDING_BYTES,
  MAX_RECORDING_BYTES,
  MIN_RECORDING_RECORDS,
  MAX_RECORDING_RECORDS
} from '../backend-contract/continuation-recording-bounds'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { parseOrderedRecordingContext } from '../core/ordered-recording'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import {
  createWebOrderedRecordingJournal,
  webRecordingCanonical,
  webRecordingReaderIntegrity,
  webRecordingMarkerDebt,
  webRecordingTerminalOutcome
} from './web-ordered-recording-journal'
import type {
  WebOrderedJournalDatabase,
  WebOrderedJournalTransaction,
  WebOrderedRecordingDeclaration
} from './web-ordered-recording-journal'

const MAX = '18446744073709551615'
const STORES = ['recordings', 'authorityReservations', 'readerReceipts']
const error = (detail: string): Error =>
  contractError('lifecycle.invariant-violation', 'restoration', `web-recording.reservation-${detail}`)
const token = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value)
const decimal = (value: unknown): value is string => isCanonicalIngressOrdinal(value) && value !== '0'
const record = (value: unknown): Record<string, unknown> => {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw error('malformed')
  return Object.fromEntries(Object.entries(value))
}
const next = (value: string): string => {
  if (value === MAX) throw error('exhausted')
  return (BigInt(value) + 1n).toString()
}

export type WebAuthorityCoordinator =
  | { readonly kind: 'locks'; readonly name: string }
  | { readonly kind: 'worker'; readonly url: string; readonly name: string; readonly type: 'module' }
export interface WebAuthorityOwner {
  readonly id: string
  readonly generation: string
  readonly ownerToken: string
  readonly ownerEpoch: string
}
export type WebAuthorityPhase = 'provisional' | 'armed' | 'cleanup-pending' | 'released'
export type WebAuthorityDeclaration = Omit<WebOrderedRecordingDeclaration, 'ownerEpoch'>
export interface WebAuthorityAcquire {
  readonly declaration: WebAuthorityDeclaration
  readonly ownerToken: string
  readonly operationId: string
  readonly coordinator: WebAuthorityCoordinator
  /** Null for an absent reservation, otherwise the observed released generation. */
  readonly expectedGeneration: string | null
}
export interface WebAuthorityRecovery {
  readonly id: string
  readonly ownerToken: string
  readonly operationId: string
  readonly expectedGeneration: string
  readonly coordinator: WebAuthorityCoordinator
}
/** Generate before starting a transaction and retain the token for reconciliation. */
export function createWebAuthorityToken(crypto: { getRandomValues(bytes: Uint8Array): Uint8Array }): string {
  const bytes = new Uint8Array(32)
  webRecordingRandomBytes(crypto, bytes)
  return Array.from(bytes, value => value.toString(16).padStart(2, '0')).join('')
}
export type WebAuthorityResult =
  | { readonly state: 'acquired'; readonly owner: WebAuthorityOwner }
  | { readonly state: 'busy'; readonly phase: Exclude<WebAuthorityPhase, 'released'> }
export type WebAuthorityTransition = { readonly state: WebAuthorityPhase; readonly owner: WebAuthorityOwner }
export type WebAuthorityReconciliation =
  | { readonly outcome: 'committed'; readonly result: WebAuthorityTransition }
  | { readonly outcome: 'unknown' }

function validateCoordinator(value: WebAuthorityCoordinator): void {
  if (
    typeof value !== 'object' ||
    value === null ||
    typeof value.name !== 'string' ||
    value.name.length < 1 ||
    value.name.length > 512
  )
    throw error('coordinator')
  if (value.kind === 'worker') {
    if (
      value.type !== 'module' ||
      typeof value.url !== 'string' ||
      value.url.length > 2048 ||
      !/^https?:\/\//.test(value.url)
    )
      throw error('coordinator')
  } else if (value.kind !== 'locks') throw error('coordinator')
}
function checkedOwner(owner: WebAuthorityOwner): void {
  if (!isRecordingId(owner.id) || !decimal(owner.generation) || !decimal(owner.ownerEpoch) || !token(owner.ownerToken))
    throw error('owner')
}
function checkedRow(value: unknown, id: string): Record<string, unknown> {
  const row = record(value)
  if (
    row.id !== id ||
    row.version !== 1 ||
    row.kind !== 'ordered' ||
    !decimal(row.ownerEpoch) ||
    !['prepared', 'live', 'sealed', 'interrupted'].includes(String(row.ownerState))
  )
    throw error('journal')
  const safe = (candidate: unknown, minimum: number, maximum: number): candidate is number =>
    typeof candidate === 'number' && Number.isSafeInteger(candidate) && candidate >= minimum && candidate <= maximum
  if (
    !safe(row.maxBytes, MIN_RECORDING_BYTES, MAX_RECORDING_BYTES) ||
    !safe(row.maxRecords, MIN_RECORDING_RECORDS, MAX_RECORDING_RECORDS) ||
    (row.nextOrdinal !== null && !safe(row.nextOrdinal, 1, Number.MAX_SAFE_INTEGER)) ||
    !safe(row.recordCount, 0, row.maxRecords) ||
    !safe(row.pendingCount, 0, row.maxRecords) ||
    !safe(row.pendingBytes, 0, row.maxBytes) ||
    !safe(row.byteCount, 0, row.maxBytes) ||
    !safe(row.readerCursor, 0, Number.MAX_SAFE_INTEGER) ||
    (row.pendingCount === 0) !== (row.pendingBytes === 0) ||
    (row.recordCount === 0) !== (row.byteCount === 0) ||
    (row.lastDeliveryId !== null && !isCanonicalIngressOrdinal(row.lastDeliveryId)) ||
    (row.lastRetiredId !== null && !isCanonicalIngressOrdinal(row.lastRetiredId)) ||
    (row.lastSourceOrdinal !== null && !isCanonicalIngressOrdinal(row.lastSourceOrdinal)) ||
    typeof row.terminal !== 'boolean'
  )
    throw error('journal-accounting')
  const lastOrdinal = row.nextOrdinal === null ? Number.MAX_SAFE_INTEGER : row.nextOrdinal - 1
  if (
    row.readerCursor > lastOrdinal ||
    row.recordCount !== lastOrdinal - row.readerCursor ||
    row.pendingCount > lastOrdinal
  )
    throw error('journal-accounting')
  webRecordingReaderIntegrity(row)
  return row
}
function checkedReservation(value: unknown, id: string): Record<string, unknown> {
  const row = record(value)
  if (
    row.id !== id ||
    row.version !== 1 ||
    !decimal(row.generation) ||
    !decimal(row.ownerEpoch) ||
    !token(row.ownerToken) ||
    typeof row.declaration !== 'string' ||
    row.declaration.length > 65536 ||
    !['provisional', 'armed', 'cleanup-pending', 'released'].includes(String(row.phase))
  )
    throw error('corrupt')
  const coordinator = record(row.coordinator)
  if (coordinator.kind === 'locks' && typeof coordinator.name === 'string')
    validateCoordinator({ kind: 'locks', name: coordinator.name })
  else if (
    coordinator.kind === 'worker' &&
    typeof coordinator.name === 'string' &&
    typeof coordinator.url === 'string' &&
    coordinator.type === 'module'
  )
    validateCoordinator({ kind: 'worker', name: coordinator.name, url: coordinator.url, type: 'module' })
  else throw error('corrupt')
  const storedReceipt = record(row.receipt)
  if (
    !token(storedReceipt.operationId) ||
    typeof storedReceipt.transition !== 'string' ||
    !['acquire', 'arm', 'cleanup', 'release', 'revoke'].includes(storedReceipt.transition) ||
    storedReceipt.phase !== row.phase ||
    (storedReceipt.transition === 'acquire' && row.phase !== 'provisional') ||
    (storedReceipt.transition === 'arm' && row.phase !== 'armed') ||
    (storedReceipt.transition === 'cleanup' && row.phase !== 'cleanup-pending') ||
    ((storedReceipt.transition === 'release' || storedReceipt.transition === 'revoke') && row.phase !== 'released')
  )
    throw error('receipt')
  return row
}
function initialRow(d: WebAuthorityDeclaration, epoch: string): Record<string, unknown> {
  return {
    ...d,
    context: parseOrderedRecordingContext(d.context),
    ownerEpoch: epoch,
    kind: 'ordered',
    version: 1,
    ownerState: 'prepared',
    nextOrdinal: 1,
    recordCount: 0,
    pendingCount: 0,
    pendingBytes: 0,
    byteCount: 0,
    readerCursor: 0,
    readerIntegrity: {
      version: 3,
      receipt: null,
      accountedBytes: 0,
      pendingCount: 0,
      pendingBytes: 0,
      deliveryId: null,
      sourceOrdinal: null
    },
    lastDeliveryId: null,
    lastRetiredId: null,
    lastSourceOrdinal: null,
    terminal: false,
    failure: null
  }
}
function exactDeclaration(row: Record<string, unknown>, d: WebAuthorityDeclaration): void {
  for (const key of [
    'id',
    'maxBytes',
    'maxRecords',
    'context',
    'producerIncarnation',
    'collectionId',
    'sourceGeneration',
    'memberEpochs'
  ] as const) {
    if (webRecordingCanonical(row[key]) !== webRecordingCanonical(d[key])) throw error('declaration')
  }
}
const declarationKeys = [
  'id',
  'maxBytes',
  'maxRecords',
  'context',
  'producerIncarnation',
  'collectionId',
  'sourceGeneration',
  'memberEpochs'
] as const
function declarationBinding(row: Record<string, unknown>): string {
  return webRecordingCanonical(declarationKeys.map(key => row[key]))
}
function checkBinding(reservation: Record<string, unknown>, journal: Record<string, unknown>): void {
  if (reservation.declaration !== declarationBinding(journal)) throw error('declaration')
}

type Guard = (fn: () => void) => void
async function transaction<T>(
  db: WebOrderedJournalDatabase,
  action: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: Guard) => void,
  stores: string[] = STORES
): Promise<T> {
  let result: { value: T } | undefined
  let problem: unknown
  try {
    await runWebIndexedDbTransaction(db, stores, 'readwrite', tx => {
      const guard: Guard = fn => {
        try {
          fn()
        } catch (cause) {
          problem = cause
          try {
            tx.abort()
          } catch (abortError) {
            problem = webIndexedDbQueueAbortError(cause, abortError)
          }
        }
      }
      guard(() =>
        action(
          tx,
          value => {
            result = { value }
          },
          guard
        )
      )
    })
  } catch (cause) {
    throw problem ?? cause
  }
  if (problem !== undefined) throw problem
  if (result === undefined) throw error('incomplete')
  return result.value
}
function load(
  tx: WebOrderedJournalTransaction,
  id: string,
  guard: Guard,
  fn: (reservation: unknown, journal: unknown) => void
): void {
  const legacy = tx.objectStore('readerReceipts').get(':ubm-authority-policy-v1')
  legacy.onsuccess = () =>
    guard(() => {
      const policy = record(legacy.result)
      if (
        policy.recordingId !== ':ubm-authority-policy-v1' ||
        policy.version !== 2 ||
        policy.protocol !== 'ubm-authority-reservations/1'
      )
        throw error('legacy-policy')
      const a = tx.objectStore('authorityReservations').get(id)
      a.onsuccess = () =>
        guard(() => {
          const b = tx.objectStore('recordings').get(id)
          b.onsuccess = () => guard(() => fn(a.result, b.result))
        })
    })
}
function ownerFrom(row: Record<string, unknown>): WebAuthorityOwner {
  if (
    typeof row.id !== 'string' ||
    typeof row.generation !== 'string' ||
    typeof row.ownerToken !== 'string' ||
    typeof row.ownerEpoch !== 'string'
  )
    throw error('corrupt')
  return { id: row.id, generation: row.generation, ownerToken: row.ownerToken, ownerEpoch: row.ownerEpoch }
}
function matches(row: Record<string, unknown>, owner: WebAuthorityOwner): boolean {
  return (
    row.id === owner.id &&
    row.generation === owner.generation &&
    row.ownerToken === owner.ownerToken &&
    row.ownerEpoch === owner.ownerEpoch
  )
}
function receipt(row: Record<string, unknown>, operationId: string, operation: string, phase: WebAuthorityPhase): void {
  row.phase = phase
  row.receipt = { operationId, transition: operation, phase }
}

/** This core never grants a coordinator or establishes physical cleanup. Callers must
 * fence source ingress and satisfy the documented cleanup obligations before release. */
export function createWebAuthorityReservations(db: WebOrderedJournalDatabase) {
  return {
    recover(input: WebAuthorityRecovery): Promise<WebAuthorityOwner> {
      const { id, ownerToken, operationId, expectedGeneration } = input
      const coordinator = { ...input.coordinator }
      if (!isRecordingId(id) || !token(ownerToken) || !token(operationId) || !decimal(expectedGeneration))
        throw error('identity')
      validateCoordinator(coordinator)
      return transaction(db, (tx, done, guard) =>
        load(tx, id, guard, (value, journal) => {
          const row = checkedReservation(value, id),
            recording = checkedRow(journal, id)
          checkBinding(row, recording)
          if (row.ownerEpoch !== recording.ownerEpoch) throw error('epoch')
          const priorRecovery = row.recoveryReceipt === undefined ? undefined : record(row.recoveryReceipt)
          if (priorRecovery?.operationId === operationId) {
            if (
              row.ownerToken !== ownerToken ||
              priorRecovery.expectedGeneration !== expectedGeneration ||
              webRecordingCanonical(row.coordinator) !== webRecordingCanonical(coordinator)
            )
              throw error('operation-conflict')
            if (row.phase !== 'cleanup-pending' && row.phase !== 'released') throw error('recovery-state')
            done(ownerFrom(row))
            return
          }
          if (
            row.phase !== 'released' ||
            row.generation !== expectedGeneration ||
            row.ownerToken === ownerToken ||
            record(row.receipt).transition !== 'release' ||
            (recording.ownerState !== 'sealed' && recording.ownerState !== 'interrupted')
          )
            throw error('recovery-state')
          // The terminal and seal receipts belong to the producer epoch. Never
          // rewrite that epoch when transferring authority over a closed journal.
          webRecordingTerminalOutcome(recording)
          if (recording.ownerState === 'sealed') {
            const seal = record(recording.sealReceipt)
            const source = priorRecovery === undefined ? row : record(priorRecovery.source)
            if (
              recording.terminal !== true ||
              recording.pendingCount !== 0 ||
              seal.version !== 1 ||
              seal.generation !== source.generation ||
              seal.ownerToken !== source.ownerToken ||
              seal.ownerEpoch !== recording.ownerEpoch ||
              seal.deliveryId !== recording.lastDeliveryId
            )
              throw error('recovery-seal')
          }
          const previous = { generation: row.generation, ownerToken: row.ownerToken, receipt: row.receipt }
          const source =
            priorRecovery === undefined
              ? { generation: row.generation, ownerToken: row.ownerToken }
              : record(priorRecovery.source)
          row.generation = next(expectedGeneration)
          row.ownerToken = ownerToken
          row.coordinator = coordinator
          row.recoveryReceipt = { operationId, expectedGeneration, previous, source }
          receipt(row, operationId, 'cleanup', 'cleanup-pending')
          tx.objectStore('authorityReservations').put(row)
          done(ownerFrom(row))
        })
      )
    },
    reconcileRecovery(
      input: WebAuthorityRecovery
    ): Promise<
      { readonly outcome: 'committed'; readonly owner: WebAuthorityOwner } | { readonly outcome: 'absent' | 'unknown' }
    > {
      const { id, ownerToken, operationId, expectedGeneration } = input
      if (!isRecordingId(id) || !token(ownerToken) || !token(operationId) || !decimal(expectedGeneration))
        throw error('identity')
      validateCoordinator(input.coordinator)
      return transaction(db, (tx, done, guard) =>
        load(tx, id, guard, (value, journal) => {
          const row = checkedReservation(value, id),
            recording = checkedRow(journal, id)
          checkBinding(row, recording)
          if (row.ownerEpoch !== recording.ownerEpoch) throw error('epoch')
          const recovery = row.recoveryReceipt === undefined ? undefined : record(row.recoveryReceipt)
          if (
            recovery?.operationId === operationId &&
            row.ownerToken === ownerToken &&
            recovery.expectedGeneration === expectedGeneration &&
            row.generation === next(expectedGeneration) &&
            (row.phase === 'cleanup-pending' || row.phase === 'released') &&
            webRecordingCanonical(row.coordinator) === webRecordingCanonical(input.coordinator)
          ) {
            done({ outcome: 'committed', owner: ownerFrom(row) })
          } else if (
            recovery?.operationId !== operationId &&
            row.phase === 'released' &&
            row.generation === expectedGeneration &&
            row.ownerToken !== ownerToken
          ) {
            done({ outcome: 'absent' })
          } else done({ outcome: 'unknown' })
        })
      )
    },
    validateOwner(owner: WebAuthorityOwner, expectedPhase: 'provisional' | 'armed'): Promise<void> {
      checkedOwner(owner)
      owner = { ...owner }
      return transaction(db, (tx, done, guard) =>
        load(tx, owner.id, guard, (value, journal) => {
          const row = checkedReservation(value, owner.id)
          const recording = checkedRow(journal, owner.id)
          checkBinding(row, recording)
          if (!matches(row, owner) || recording.ownerEpoch !== owner.ownerEpoch) throw error('stale')
          if (
            row.phase !== expectedPhase ||
            !(
              (row.phase === 'provisional' && recording.ownerState === 'prepared') ||
              (row.phase === 'armed' && recording.ownerState === 'live')
            )
          )
            throw error('revalidation-state')
          done(undefined)
        })
      )
    },
    status(
      id: string
    ): Promise<
      | { readonly phase: WebAuthorityPhase; readonly generation: string; readonly ownerState: string }
      | { readonly phase: 'missing' }
    > {
      if (!isRecordingId(id)) throw error('identity')
      return transaction(db, (tx, done, guard) =>
        load(tx, id, guard, (value, journal) => {
          if (value === undefined) {
            if (journal !== undefined) throw error('legacy-owner')
            done({ phase: 'missing' })
            return
          }
          const row = checkedReservation(value, id),
            recording = checkedRow(journal, id)
          checkBinding(row, recording)
          if (
            row.ownerEpoch !== recording.ownerEpoch ||
            typeof row.generation !== 'string' ||
            typeof recording.ownerState !== 'string'
          )
            throw error('epoch')
          const phase = row.phase
          if (phase !== 'provisional' && phase !== 'armed' && phase !== 'cleanup-pending' && phase !== 'released')
            throw error('corrupt')
          done({ phase, generation: row.generation, ownerState: recording.ownerState })
        })
      )
    },
    revokeObservedProvisional(
      id: string,
      generation: string,
      operationId: string
    ): Promise<{ readonly state: 'released'; readonly generation: string }> {
      if (!isRecordingId(id) || !decimal(generation) || !token(operationId)) throw error('identity')
      return transaction(db, (tx, done, guard) =>
        load(tx, id, guard, (value, journal) => {
          const row = checkedReservation(value, id),
            recording = checkedRow(journal, id)
          checkBinding(row, recording)
          if (row.generation !== generation || row.ownerEpoch !== recording.ownerEpoch) throw error('stale')
          const previous = record(row.receipt)
          if (row.phase === 'released' && previous.operationId === operationId && previous.transition === 'revoke') {
            done({ state: 'released', generation })
            return
          }
          if (row.phase !== 'provisional' || recording.ownerState !== 'prepared') throw error('revoke-state')
          receipt(row, operationId, 'revoke', 'released')
          tx.objectStore('authorityReservations').put(row)
          done({ state: 'released', generation })
        })
      )
    },
    acquire(input: WebAuthorityAcquire): Promise<WebAuthorityResult> {
      const { ownerToken, operationId, expectedGeneration } = input
      const d: WebAuthorityDeclaration = {
        ...input.declaration,
        context: parseOrderedRecordingContext(input.declaration.context),
        memberEpochs: input.declaration.memberEpochs.map(member => ({ member: member.member, epoch: member.epoch }))
      }
      const coordinator = { ...input.coordinator }
      if (!token(ownerToken) || !token(operationId)) throw error('identity')
      if (expectedGeneration !== null && !decimal(expectedGeneration)) throw error('generation')
      validateCoordinator(coordinator)
      createWebOrderedRecordingJournal(db, { ...d, ownerEpoch: '1' })
      const binding = declarationBinding(initialRow(d, '1'))
      if (binding.length > 65536) throw error('declaration-bound')
      return transaction(
        db,
        (tx, done, guard) => {
          const abandoned = tx.objectStore('abandonments').get([d.id, d.producerIncarnation])
          abandoned.onsuccess = () =>
            guard(() => {
              if (abandoned.result !== undefined) throw error('abandoned-incarnation')
              load(tx, d.id, guard, (existing, journal) => {
                let generation = '1',
                  epoch = '1'
                if (existing !== undefined) {
                  const prior = checkedReservation(existing, d.id)
                  const row = checkedRow(journal, d.id)
                  checkBinding(prior, row)
                  if (row.ownerEpoch !== prior.ownerEpoch) throw error('epoch')
                  if (prior.phase !== 'released') {
                    const last = record(prior.receipt)
                    if (
                      prior.ownerToken === ownerToken &&
                      last.operationId === operationId &&
                      last.transition === 'acquire'
                    ) {
                      exactDeclaration(row, d)
                      if (webRecordingCanonical(prior.coordinator) !== webRecordingCanonical(coordinator))
                        throw error('operation-conflict')
                      done({ state: 'acquired', owner: ownerFrom(prior) })
                      return
                    }
                    if (prior.phase !== 'provisional' && prior.phase !== 'armed' && prior.phase !== 'cleanup-pending')
                      throw error('corrupt')
                    done({ state: 'busy', phase: prior.phase })
                    return
                  }
                  if (prior.generation !== expectedGeneration || prior.ownerToken === ownerToken)
                    throw error('stale-acquire')
                  if (row.ownerState !== 'prepared' || record(prior.receipt).transition !== 'revoke')
                    throw error('journal-state')
                  // Only an untouched prepared journal may be rebound. Its recording
                  // contract stays fixed; the new source supplies its own incarnation
                  // and membership epochs under the new generation.
                  for (const key of [
                    'id',
                    'maxBytes',
                    'maxRecords',
                    'context',
                    'collectionId',
                    'sourceGeneration'
                  ] as const) {
                    if (webRecordingCanonical(row[key]) !== webRecordingCanonical(d[key])) throw error('declaration')
                  }
                  if (
                    row.nextOrdinal !== 1 ||
                    row.recordCount !== 0 ||
                    row.pendingCount !== 0 ||
                    row.readerCursor !== 0 ||
                    row.lastDeliveryId !== null ||
                    row.lastRetiredId !== null ||
                    row.lastSourceOrdinal !== null ||
                    row.terminal !== false ||
                    row.failure !== null ||
                    row.openReceipt !== undefined ||
                    webRecordingCanonical(row.readerIntegrity) !==
                      webRecordingCanonical(initialRow(d, epoch).readerIntegrity)
                  )
                    throw error('journal-state')
                  generation = next(String(prior.generation))
                  epoch = next(String(prior.ownerEpoch))
                  row.ownerEpoch = epoch
                  row.producerIncarnation = d.producerIncarnation
                  row.memberEpochs = d.memberEpochs
                  tx.objectStore('recordings').put(row)
                } else {
                  if (expectedGeneration !== null) throw error('stale-acquire')
                  if (journal !== undefined) throw error('legacy-owner')
                  tx.objectStore('recordings').add(initialRow(d, epoch))
                }
                const owner = { id: d.id, generation, ownerToken, ownerEpoch: epoch }
                tx.objectStore('authorityReservations').put({
                  ...owner,
                  version: 1,
                  coordinator,
                  declaration: binding,
                  phase: 'provisional',
                  receipt: { operationId, transition: 'acquire', phase: 'provisional' }
                })
                done({ state: 'acquired', owner })
              })
            })
        },
        [...STORES, 'abandonments']
      )
    },
    arm(owner: WebAuthorityOwner, operationId: string): Promise<WebAuthorityTransition> {
      return transition(db, owner, operationId, 'arm')
    },
    revokeProvisional(owner: WebAuthorityOwner, operationId: string): Promise<WebAuthorityTransition> {
      return transition(db, owner, operationId, 'revoke')
    },
    cleanupPending(owner: WebAuthorityOwner, operationId: string): Promise<WebAuthorityTransition> {
      return transition(db, owner, operationId, 'cleanup')
    },
    release(owner: WebAuthorityOwner, operationId: string): Promise<WebAuthorityTransition> {
      return transition(db, owner, operationId, 'release')
    },
    reconcileAcquire(
      id: string,
      ownerToken: string,
      operationId: string,
      expectedGeneration?: string | null
    ): Promise<
      { readonly outcome: 'committed'; readonly owner: WebAuthorityOwner } | { readonly outcome: 'absent' | 'unknown' }
    > {
      if (!isRecordingId(id) || !token(ownerToken) || !token(operationId)) throw error('identity')
      if (expectedGeneration != null && !decimal(expectedGeneration)) throw error('generation')
      return transaction(db, (tx, done, guard) =>
        load(tx, id, guard, (value, journal) => {
          if (value === undefined) {
            if (journal !== undefined) throw error('legacy-owner')
            done({ outcome: expectedGeneration === null ? 'absent' : 'unknown' })
            return
          }
          const row = checkedReservation(value, id),
            recording = checkedRow(journal, id)
          checkBinding(row, recording)
          if (row.ownerEpoch !== recording.ownerEpoch) throw error('epoch')
          const r = record(row.receipt)
          if (
            expectedGeneration !== undefined &&
            expectedGeneration !== null &&
            row.phase === 'released' &&
            row.generation === expectedGeneration &&
            row.ownerToken !== ownerToken
          ) {
            done({ outcome: 'absent' })
            return
          }
          if (row.ownerToken !== ownerToken || r.operationId !== operationId || r.transition !== 'acquire') {
            done({ outcome: 'unknown' })
            return
          }
          done({ outcome: 'committed', owner: ownerFrom(row) })
        })
      )
    },
    reconcile(owner: WebAuthorityOwner, operationId: string): Promise<WebAuthorityReconciliation> {
      checkedOwner(owner)
      if (!token(operationId)) throw error('identity')
      owner = { ...owner }
      return transaction(db, (tx, done, guard) =>
        load(tx, owner.id, guard, (value, journal) => {
          if (value === undefined) {
            done({ outcome: 'unknown' })
            return
          }
          const row = checkedReservation(value, owner.id),
            recording = checkedRow(journal, owner.id)
          checkBinding(row, recording)
          if (row.ownerEpoch !== recording.ownerEpoch) throw error('epoch')
          const r = record(row.receipt)
          if (!matches(row, owner) || r.operationId !== operationId) {
            done({ outcome: 'unknown' })
            return
          }
          const phase = row.phase
          if (phase !== 'provisional' && phase !== 'armed' && phase !== 'cleanup-pending' && phase !== 'released')
            throw error('corrupt')
          done({ outcome: 'committed', result: { state: phase, owner: ownerFrom(row) } })
        })
      )
    }
  }
}
function transition(
  db: WebOrderedJournalDatabase,
  owner: WebAuthorityOwner,
  operationId: string,
  kind: 'arm' | 'revoke' | 'cleanup' | 'release'
): Promise<WebAuthorityTransition> {
  checkedOwner(owner)
  if (!token(operationId)) throw error('identity')
  owner = { ...owner }
  return transaction(db, (tx, done, guard) =>
    load(tx, owner.id, guard, (value, journal) => {
      const row = checkedReservation(value, owner.id),
        recording = checkedRow(journal, owner.id)
      checkBinding(row, recording)
      if (!matches(row, owner) || recording.ownerEpoch !== owner.ownerEpoch) throw error('stale')
      const previous = record(row.receipt)
      if (previous.operationId === operationId) {
        if (previous.transition !== kind) throw error('operation-conflict')
        const phase = row.phase
        if (phase !== 'provisional' && phase !== 'armed' && phase !== 'cleanup-pending' && phase !== 'released')
          throw error('corrupt')
        done({ state: phase, owner })
        return
      }
      let phase: WebAuthorityPhase
      if (kind === 'arm') {
        if (row.phase !== 'provisional' || recording.ownerState !== 'prepared') throw error('arm-state')
        phase = 'armed'
        recording.ownerState = 'live'
        tx.objectStore('recordings').put(recording)
      } else if (kind === 'revoke') {
        if (row.phase !== 'provisional' || recording.ownerState !== 'prepared') throw error('revoke-state')
        phase = 'released'
      } else if (kind === 'cleanup') {
        if (row.phase !== 'armed' || recording.ownerState !== 'live') throw error('cleanup-state')
        phase = 'cleanup-pending'
      } else {
        if (
          row.phase !== 'cleanup-pending' ||
          webRecordingMarkerDebt(recording) !== 0 ||
          (recording.ownerState !== 'sealed' && recording.ownerState !== 'interrupted')
        )
          throw error('release-state')
        phase = 'released'
      }
      receipt(row, operationId, kind, phase)
      tx.objectStore('authorityReservations').put(row)
      done({ state: phase, owner })
    })
  )
}
