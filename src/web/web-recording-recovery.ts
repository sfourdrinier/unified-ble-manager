import { webRecordingContractError as contractError, webIndexedDbQueueAbortError } from './web-recording-errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import { createWebRecordingMarkerLedger, webMarkerLedgerSnapshot } from './web-recording-marker-ledger'
import type { WebMarkerLedgerSnapshot } from './web-recording-marker-ledger'
import type { WebAuthorityOwner } from './web-authority-reservation'
import type { WebOrderedJournalDatabase, WebOrderedJournalTransaction } from './web-ordered-recording-journal'
import { checkedWebClearAttempt, webClearAttemptKey } from './web-clear-attempt'
import type { WebClearAttemptReceiptV1, WebClearAttemptV1 } from './web-clear-attempt'

/** Inspection does not establish that a former owner has stopped, or that a write settled. */
export interface WebRecordingRecoveryInspection {
  readonly recordingId: string
  readonly producerIncarnation: string
  readonly recordingDisposition: 'active' | 'cleared-marker-evidence-retained'
  readonly markerLedgerBytes: number
  /** Older journals did not record whether every marker was inventoried. */
  readonly markerInventory: 'unknown'
  /** Raw, retained ledger rows; callers must not treat these as radio outcomes. */
  readonly markers: readonly Readonly<Record<string, unknown>>[]
  readonly uncertainty: {
    readonly priorMarkerOutcome: 'unknown'
    readonly sourceCleanup: 'unknown'
    readonly oldOwnerActivity: 'possible'
  }
}
export interface WebRecordingRecovery {
  /** Partition-wide proof inventory; pages are independent observations. */
  listClearProofs(options: { readonly after: string | null; readonly limit: number }): Promise<{
    readonly entries: readonly WebClearProof[]
    readonly next: string | null
  }>
  /** An absent receipt is unknown, including while a dispatched commit is unsettled. */
  getClearAttemptV1(
    attempt: WebClearAttemptV1,
    options?: { readonly signal?: AbortSignal }
  ): Promise<
    | { readonly outcome: 'unknown' }
    | { readonly outcome: 'superseded'; readonly attempt: WebClearAttemptV1 }
    | { readonly outcome: 'committed'; readonly receipt: WebClearAttemptReceiptV1 }
  >
  /** Verify an exact receipt without deleting the durable outcome. */
  acknowledgeClearAttemptV1(attempt: WebClearAttemptV1, receipt: WebClearAttemptReceiptV1): Promise<void>
  /** Active identity plus a page of retained identities. Pages are independent observations. */
  listIncarnations(
    recordingId: string,
    options: {
      readonly after: string | null
      readonly limit: number
      readonly includeClearReceipts?: boolean
      /** Cursor for the bounded partition-wide clear outcome page. */
      readonly clearReceiptsAfter?: string | null
      readonly includePartitionRetained?: boolean
      readonly partitionRetainedAfter?: string | null
    }
  ): Promise<{
    readonly active: string | null
    readonly retained: readonly string[]
    readonly next: string | null
    /** Original IndexedDB values that could not be interpreted; never removed by inspection. */
    readonly malformedRetained?: readonly { readonly recordingId: string; readonly value: unknown }[]
    readonly clearReceipts?: readonly WebClearReceipt[]
    readonly clearReceiptsNext?: string | null
    /** Corrupt slots remain untouched; valid receipts can still be retired. */
    readonly malformedClearReceipts?: readonly {
      readonly recordingId: string
      readonly error: 'malformed-clear-receipt'
    }[]
    /** A partition-wide page of proven-cleared marker evidence. */
    readonly partitionRetained?: readonly {
      readonly recordingId: string
      readonly producerIncarnation: string
      readonly markerLedgerBytes: number
    }[]
    readonly partitionRetainedNext?: string | null
    readonly malformedPartitionRetained?: readonly { readonly recordingId: string; readonly value: unknown }[]
    readonly malformedLegacyClearSequence?: { readonly recordingId: string; readonly value: unknown }
    /** An older per-ID allocation fence, retained for older open clients. */
    readonly legacyClearSequence?: string | null
  }>
  listMarkerOperations(
    recordingId: string,
    producerIncarnation: string,
    options: {
      readonly after: string | null
      readonly limit: number
    }
  ): Promise<{ readonly operations: readonly WebRecordingOperationEvidence[]; readonly next: string | null }>
  /** Irreversibly dispose only proven-cleared evidence after the application exports it. */
  disposeRetained(
    recordingId: string,
    producerIncarnation: string,
    options: {
      readonly acknowledgeExport: true
      readonly limit: number
      /** Retire this exact clear receipt instead of disposing marker evidence. */
      readonly clearReceipt?: WebClearReceipt
      /** Verify the archived exact outcome without changing storage. Requires clearReceipt. */
      readonly verifyRetiredClearReceipt?: true
    }
  ): Promise<{
    readonly state: 'more' | 'disposed' | 'absent'
    readonly deleted: number
    readonly retiredReceipt?: WebClearReceipt
  }>
  inspect(recordingId: string, producerIncarnation: string): Promise<WebRecordingRecoveryInspection>
  /** Pages are independent observations, not a revision-fenced snapshot. */
  listMarkers(
    recordingId: string,
    producerIncarnation: string,
    options: { readonly after: string | null; readonly limit: number }
  ): Promise<{ readonly markers: readonly Readonly<Record<string, unknown>>[]; readonly next: string | null }>
  reconcileMarkerOperation(
    recordingId: string,
    producerIncarnation: string,
    operationId: string
  ): Promise<
    | { readonly outcome: 'observed'; readonly signature: 'unverified'; readonly result: WebMarkerLedgerSnapshot }
    | { readonly outcome: 'unknown' }
  >
}

export type WebClearProof =
  | { readonly kind: 'active' | 'retired'; readonly key: string; readonly receipt: WebClearReceipt }
  | { readonly kind: 'attempt'; readonly key: string; readonly receipt: WebClearAttemptReceiptV1 }
  | { readonly kind: 'superseded'; readonly key: string; readonly attempt: WebClearAttemptV1 }
  | { readonly kind: 'malformed'; readonly key: string; readonly value: unknown }

export interface WebClearReceipt {
  readonly recordingId: string
  readonly version: 1 | 2 | 3
  /** Version 3 records the exact cleared count. */
  readonly records?: number
  /** The application recording ID; recordingId is the private receipt key. */
  readonly targetRecordingId: string
  readonly operationId: string
  readonly producerIncarnation: string
  readonly ownerEpoch: string
  readonly generation: string
}

export interface WebRecordingOperationEvidence {
  readonly operationId: string
  readonly signature: 'unverified'
  readonly transition: { readonly kind: string; readonly data: unknown }
  readonly ownerGeneration: string
  readonly ownerEpoch: string
  readonly result: WebMarkerLedgerSnapshot
}

interface RangeFactory {
  bound(id: string, incarnation: string, after?: string): unknown
  retained?(id: string, after?: string): unknown
}
declare const IDBKeyRange: {
  bound(
    lower: string | readonly string[],
    upper: string | readonly (string | readonly never[])[],
    lowerOpen?: boolean
  ): unknown
}
export const WEB_RETAINED_MARKER_PREFIX = ':ubm-marker-retained-v1:'
export const WEB_RETAINED_MARKER_LIMIT = 64
export const WEB_RETAINED_MARKER_BYTES = 1_073_741_824
const retainedPrefix = (id: string): string => `:ubm-marker-retained-v1:${JSON.stringify([id]).slice(0, -1)},`
const browserRanges: RangeFactory = {
  bound: (id, incarnation, after) =>
    IDBKeyRange.bound(
      after === undefined ? [id, incarnation] : [id, incarnation, after],
      [id, incarnation, []],
      after !== undefined
    ),
  retained: (id, after) => {
    const prefix = retainedPrefix(id)
    return IDBKeyRange.bound(after ?? prefix, `${prefix}\uffff`, after !== undefined)
  }
}
const failure = (detail: string): Error =>
  contractError('lifecycle.invariant-violation', 'restoration', `web-recording.recovery-${detail}`)
const quota = (): Error => contractError('stream.quota', 'restoration', 'web-recording.recovery-inspection-capacity')
const validId = (id: string, incarnation: string): boolean =>
  isRecordingId(id) && typeof incarnation === 'string' && incarnation.length > 0 && incarnation.length <= 256
const token = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value)
const clearKey = (id: string, slot: number): string => `:ubm-clear-v2:${id}:${slot}`
const globalClearKey = (slot: number): string => `:ubm-clear-v3:${slot}`
function clearReceipt(value: unknown, id: string, slot: number, global: boolean): WebClearReceipt {
  const receipt = row(value)
  const keys = Object.keys(receipt)
  const allowed = [
    'recordingId',
    'version',
    'targetRecordingId',
    'operationId',
    'producerIncarnation',
    'ownerEpoch',
    'generation'
  ]
  const operation = receipt.operationId
  const number =
    typeof operation === 'string' && /^g?[1-9][0-9]{0,19}$/.test(operation) ? operation.replace(/^g/, '') : ''
  if (
    !isRecordingId(id) ||
    (global ? keys.length !== 7 : ![6, 7].includes(keys.length)) ||
    keys.some(key => !allowed.includes(key)) ||
    (keys.length === 7 && !Object.hasOwn(receipt, 'targetRecordingId')) ||
    receipt.recordingId !== (global ? globalClearKey(slot) : clearKey(id, slot)) ||
    receipt.version !== (global ? 2 : 1) ||
    (global
      ? receipt.targetRecordingId !== id
      : receipt.targetRecordingId !== undefined && receipt.targetRecordingId !== id) ||
    (global && (typeof operation !== 'string' || !operation.startsWith('g'))) ||
    number === '' ||
    BigInt(number) > 18446744073709551615n ||
    Number(BigInt(number) % 64n) !== slot ||
    typeof operation !== 'string' ||
    typeof receipt.producerIncarnation !== 'string' ||
    receipt.producerIncarnation.length === 0 ||
    receipt.producerIncarnation.length > 256 ||
    !isCanonicalIngressOrdinal(receipt.ownerEpoch) ||
    receipt.ownerEpoch === '0' ||
    !isCanonicalIngressOrdinal(receipt.generation) ||
    receipt.generation === '0'
  )
    throw failure('clear-receipt')
  return {
    recordingId: global ? globalClearKey(slot) : clearKey(id, slot),
    version: global ? 2 : 1,
    targetRecordingId: id,
    operationId: operation,
    producerIncarnation: receipt.producerIncarnation,
    ownerEpoch: receipt.ownerEpoch,
    generation: receipt.generation
  }
}
const durableClearPrefix = ':ubm-clear-v4:'
function durableClearReceipt(value: unknown): WebClearReceipt {
  const receipt = row(value)
  const operation = receipt.operationId
  if (
    Object.keys(receipt).length !== 8 ||
    receipt.version !== 3 ||
    typeof receipt.recordingId !== 'string' ||
    typeof operation !== 'string' ||
    !/^g[1-9][0-9]{0,19}$/.test(operation) ||
    BigInt(operation.slice(1)) > 18446744073709551615n ||
    receipt.recordingId !== `${durableClearPrefix}${operation.slice(1).padStart(20, '0')}` ||
    !isRecordingId(receipt.targetRecordingId) ||
    receipt.records !== 0 ||
    typeof receipt.producerIncarnation !== 'string' ||
    receipt.producerIncarnation.length === 0 ||
    receipt.producerIncarnation.length > 256 ||
    !isCanonicalIngressOrdinal(receipt.ownerEpoch) ||
    receipt.ownerEpoch === '0' ||
    !isCanonicalIngressOrdinal(receipt.generation) ||
    receipt.generation === '0'
  )
    throw failure('clear-receipt')
  return {
    recordingId: receipt.recordingId,
    version: 3,
    targetRecordingId: receipt.targetRecordingId,
    operationId: operation,
    records: 0,
    producerIncarnation: receipt.producerIncarnation,
    ownerEpoch: receipt.ownerEpoch,
    generation: receipt.generation
  }
}
export const webRetiredClearKey = (receipt: WebClearReceipt): string =>
  `:ubm-clear-retired-v1:${JSON.stringify([receipt.recordingId, receipt.operationId])}`

function checkedClearReceipt(value: unknown): WebClearReceipt {
  const receipt = row(value)
  if (receipt.version === 3) return durableClearReceipt(receipt)
  const global = receipt.version === 2
  if (
    (!global && receipt.version !== 1) ||
    typeof receipt.targetRecordingId !== 'string' ||
    typeof receipt.recordingId !== 'string'
  )
    throw failure('clear-receipt')
  const target = receipt.targetRecordingId
  const slot = Array.from({ length: 64 }, (_, i) => (global ? globalClearKey(i) : clearKey(target, i))).indexOf(
    receipt.recordingId
  )
  if (slot < 0) throw failure('clear-receipt')
  return clearReceipt(receipt, target, slot, global)
}

export function webRetiredClearMatches(value: unknown, expected: WebClearReceipt): boolean {
  const archive = row(value)
  if (
    Object.keys(archive).length !== 3 ||
    archive.recordingId !== webRetiredClearKey(expected) ||
    archive.version !== 1
  )
    throw failure('retired-clear-receipt')
  const actual = checkedClearReceipt(archive.receipt)
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw failure('clear-receipt-conflict')
  return true
}
export const webRetainedMarkerKey = (id: string, incarnation: string): string =>
  `:ubm-marker-retained-v1:${JSON.stringify([id, incarnation])}`
export function parseWebRetainedMarkerAnchor(value: unknown): {
  recordingId: string
  producerIncarnation: string
  markerLedgerBytes: number
  key: string
} {
  const anchor = row(value)
  if (typeof anchor.recordingId !== 'string' || !anchor.recordingId.startsWith(WEB_RETAINED_MARKER_PREFIX))
    throw failure('anchor')
  let decoded: unknown
  try {
    decoded = JSON.parse(anchor.recordingId.slice(WEB_RETAINED_MARKER_PREFIX.length))
  } catch {
    throw failure('anchor')
  }
  if (
    !Array.isArray(decoded) ||
    decoded.length !== 2 ||
    typeof decoded[0] !== 'string' ||
    typeof decoded[1] !== 'string' ||
    !validId(decoded[0], decoded[1]) ||
    !retainedAnchor(anchor, decoded[0], decoded[1]) ||
    Number(anchor.markerLedgerBytes) > 1_073_741_824
  )
    throw failure('anchor')
  return {
    recordingId: decoded[0],
    producerIncarnation: decoded[1],
    markerLedgerBytes: Number(anchor.markerLedgerBytes),
    key: anchor.recordingId
  }
}
function row(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw failure('row')
  return Object.fromEntries(Object.entries(value))
}
function identity(id: string, incarnation: string): void {
  if (!validId(id, incarnation)) throw failure('identity')
}
function pageLimit(limit: number, max: number): void {
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > max) throw failure('page')
}
function retainedAnchor(value: unknown, id: string, incarnation: string): boolean {
  if (value === undefined) return false
  const anchor = row(value)
  return (
    anchor.recordingId === webRetainedMarkerKey(id, incarnation) &&
    anchor.version === 1 &&
    anchor.disposition === 'cleared-marker-evidence-retained' &&
    Number.isSafeInteger(anchor.markerLedgerBytes) &&
    Number(anchor.markerLedgerBytes) >= 0
  )
}
function getAll(store: object, range: unknown, count = 257): object {
  const method = Reflect.get(store, 'getAll')
  if (typeof method !== 'function') throw failure('cursor-unavailable')
  const request: unknown = Reflect.apply(method, store, [range, count])
  if (typeof request !== 'object' || request === null || !('result' in request) || !('onsuccess' in request))
    throw failure('request')
  // The IndexedDB request property is assigned after its structural check.
  return request
}
function markerEvidence(value: unknown, id: string, incarnation: string): Record<string, unknown> {
  const marker = row(value)
  if (
    marker.recordingId !== id ||
    marker.producerIncarnation !== incarnation ||
    marker.version !== 1 ||
    !isCanonicalIngressOrdinal(marker.markerId)
  )
    throw failure('marker')
  const { owner: _owner, ...publicEvidence } = marker
  return publicEvidence
}
const uncertainReads = new WeakSet<object>()
class RecoveryReadUncertain extends Error {
  constructor(message?: string, options?: ErrorOptions) {
    super(message, options)
    uncertainReads.add(this)
  }
}

// Named connections to the same journal share a gate. A timed-out read keeps
// its slot until the underlying readiness/transaction actually settles.
const recoveryReads = new Map<object | string, number>()
const MAX_RECOVERY_READS = 2
const RECOVERY_READ_WAIT_MS = 5000

async function transact<T>(
  database: WebOrderedJournalDatabase,
  stores: string[],
  queue: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
  mode: 'readonly' | 'readwrite' = 'readonly',
  signal?: AbortSignal
): Promise<T> {
  if (mode === 'readwrite') return executeRecoveryTransaction(database, stores, queue, mode)
  const name: unknown = Reflect.get(database, 'name')
  const readKey = typeof name === 'string' && name.length > 0 ? name : database
  if (signal?.aborted || (recoveryReads.get(readKey) ?? 0) >= MAX_RECOVERY_READS) throw new RecoveryReadUncertain()
  recoveryReads.set(readKey, (recoveryReads.get(readKey) ?? 0) + 1)
  let cancelled = false
  let active: WebOrderedJournalTransaction | undefined
  const boundedDatabase: WebOrderedJournalDatabase = {
    ready: async () => {
      await database.ready?.()
      if (cancelled) throw new RecoveryReadUncertain()
    },
    transaction: (names, access, options) => {
      if (cancelled) throw new RecoveryReadUncertain()
      active = database.transaction(names, access, options)
      return active
    }
  }
  const work = executeRecoveryTransaction(boundedDatabase, stores, queue, mode)
  work.then(
    () => releaseRecoveryRead(readKey),
    () => releaseRecoveryRead(readKey)
  )
  let timer: ReturnType<typeof setTimeout> | undefined
  let cancel: (() => void) | undefined
  try {
    return await Promise.race([
      work,
      new Promise<never>((_resolve, reject) => {
        cancel = () => {
          cancelled = true
          let abortFailure: unknown
          try {
            active?.abort()
          } catch (error) {
            abortFailure = error
          }
          reject(new RecoveryReadUncertain('Recovery read did not settle', { cause: abortFailure }))
        }
        timer = setTimeout(cancel, RECOVERY_READ_WAIT_MS)
        signal?.addEventListener('abort', cancel, { once: true })
        if (signal?.aborted) cancel()
      })
    ])
  } finally {
    if (timer !== undefined) clearTimeout(timer)
    if (cancel !== undefined) signal?.removeEventListener('abort', cancel)
  }
}

function releaseRecoveryRead(database: object | string): void {
  const remaining = (recoveryReads.get(database) ?? 1) - 1
  if (remaining === 0) recoveryReads.delete(database)
  else recoveryReads.set(database, remaining)
}

async function executeRecoveryTransaction<T>(
  database: WebOrderedJournalDatabase,
  stores: string[],
  queue: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
  mode: 'readonly' | 'readwrite'
): Promise<T> {
  let result: { value: T } | undefined
  let problem: unknown
  try {
    await runWebIndexedDbTransaction(database, stores, mode, tx => {
      const guard = (fn: () => void): void => {
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
        queue(
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
  if (result === undefined) throw failure('incomplete')
  return result.value
}

/** Read-only evidence access. This does not authorize ownership or marker dispatch. */
export function createWebRecordingRecovery(
  database: WebOrderedJournalDatabase,
  ranges: RangeFactory = browserRanges
): WebRecordingRecovery {
  const listClearReceipts = (
    id: string
  ): Promise<{
    clearReceipts: readonly WebClearReceipt[]
    malformedClearReceipts: readonly { recordingId: string; error: 'malformed-clear-receipt' }[]
    legacyClearSequence: string | null
    malformedLegacyClearSequence?: { recordingId: string; value: unknown }
  }> => {
    if (!isRecordingId(id)) throw failure('identity')
    return transact(database, ['readerReceipts'], (tx, done, guard) => {
      const store = tx.objectStore('readerReceipts')
      const found: WebClearReceipt[] = []
      const malformed: { recordingId: string; error: 'malformed-clear-receipt' }[] = []
      let remaining = 129
      let legacyClearSequence: string | null = null
      let malformedLegacyClearSequence: { recordingId: string; value: unknown } | undefined
      const finish = (): void => {
        if (--remaining === 0)
          done({
            clearReceipts: found,
            malformedClearReceipts: malformed,
            legacyClearSequence,
            ...(malformedLegacyClearSequence === undefined ? {} : { malformedLegacyClearSequence })
          })
      }
      const legacyKey = `:ubm-clear-sequence-v1:${id}`
      const legacy = store.get(legacyKey)
      legacy.onsuccess = () =>
        guard(() => {
          if (legacy.result !== undefined) {
            try {
              const value = row(legacy.result)
              if (
                Object.keys(value).length !== 3 ||
                value.recordingId !== legacyKey ||
                value.version !== 1 ||
                !isCanonicalIngressOrdinal(value.sequence) ||
                value.sequence === '0'
              )
                throw failure('clear-sequence')
              legacyClearSequence = value.sequence
            } catch {
              malformedLegacyClearSequence = { recordingId: legacyKey, value: legacy.result }
            }
          }
          finish()
        })
      for (let slot = 0; slot < 64; slot++) {
        const request = store.get(clearKey(id, slot))
        request.onsuccess = () =>
          guard(() => {
            if (request.result !== undefined) {
              try {
                found.push(clearReceipt(request.result, id, slot, false))
              } catch {
                malformed.push({ recordingId: clearKey(id, slot), error: 'malformed-clear-receipt' })
              }
            }
            finish()
          })
        const partitionReceipt = store.get(globalClearKey(slot))
        partitionReceipt.onsuccess = () =>
          guard(() => {
            if (partitionReceipt.result !== undefined) {
              try {
                const target = row(partitionReceipt.result).targetRecordingId
                if (typeof target !== 'string') throw failure('clear-receipt')
                found.push(clearReceipt(partitionReceipt.result, target, slot, true))
              } catch {
                malformed.push({ recordingId: globalClearKey(slot), error: 'malformed-clear-receipt' })
              }
            }
            finish()
          })
      }
    })
  }
  const retireClearReceipt = (id: string, expected: WebClearReceipt): Promise<boolean> => {
    if (!isRecordingId(id) || typeof expected !== 'object' || expected === null) throw failure('identity')
    const checked = checkedClearReceipt(expected)
    if (checked.targetRecordingId !== id) throw failure('clear-receipt')
    return transact(
      database,
      ['readerReceipts'],
      (tx, done, guard) => {
        const store = tx.objectStore('readerReceipts')
        const request = store.get(checked.recordingId)
        request.onsuccess = () =>
          guard(() => {
            if (request.result === undefined) return done(false)
            const actual =
              checked.version === 3
                ? durableClearReceipt(request.result)
                : clearReceipt(
                    request.result,
                    id,
                    Number(BigInt(checked.operationId.replace(/^g/, '')) % 64n),
                    checked.version === 2
                  )
            if (JSON.stringify(actual) !== JSON.stringify(checked)) throw failure('clear-receipt-conflict')
            const archiveKey = webRetiredClearKey(checked)
            const archive = store.get(archiveKey)
            archive.onsuccess = () =>
              guard(() => {
                if (archive.result === undefined) store.add({ recordingId: archiveKey, version: 1, receipt: checked })
                else webRetiredClearMatches(archive.result, checked)
                store.delete(checked.recordingId)
                done(true)
              })
          })
      },
      'readwrite'
    )
  }
  const getRetiredClearReceipt = (receipt: WebClearReceipt): Promise<WebClearReceipt | undefined> => {
    const checked = checkedClearReceipt(receipt)
    return transact(database, ['readerReceipts'], (tx, done, guard) => {
      const request = tx.objectStore('readerReceipts').get(webRetiredClearKey(checked))
      request.onsuccess = () =>
        guard(() => {
          if (request.result === undefined) return done(undefined)
          webRetiredClearMatches(request.result, checked)
          done(checked)
        })
    })
  }
  return {
    listClearProofs(options) {
      pageLimit(options.limit, 256)
      const prefix = ':ubm-clear-'
      if (options.after !== null && (typeof options.after !== 'string' || !options.after.startsWith(prefix)))
        throw failure('page')
      return transact(database, ['readerReceipts'], (tx, done, guard) => {
        const request = getAll(
          tx.objectStore('readerReceipts'),
          IDBKeyRange.bound(options.after ?? prefix, `${prefix}\uffff`, options.after !== null),
          options.limit + 1
        )
        Reflect.set(request, 'onsuccess', () =>
          guard(() => {
            const values: unknown = Reflect.get(request, 'result')
            if (!Array.isArray(values) || values.length > options.limit + 1) throw failure('clear-proofs')
            const entries: WebClearProof[] = []
            let last: string | null = null
            for (const value of values.slice(0, options.limit)) {
              const item = row(value)
              if (typeof item.recordingId !== 'string') throw failure('clear-proofs')
              const key = item.recordingId
              last = key
              if (key.startsWith(':ubm-clear-sequence-') || key === ':ubm-clear-global-sequence-v1') continue
              try {
                if (key.startsWith(':ubm-clear-retired-v1:')) {
                  const receipt = checkedClearReceipt(item.receipt)
                  webRetiredClearMatches(value, receipt)
                  entries.push({ kind: 'retired', key, receipt })
                } else if (key.startsWith(':ubm-clear-attempt-v1:')) {
                  const attempt = checkedWebClearAttempt({
                    version: 1,
                    recordingId: String(item.targetRecordingId),
                    producerIncarnation: String(item.producerIncarnation),
                    attemptId: String(item.attemptId)
                  })
                  if (item.recordingId !== webClearAttemptKey(attempt.attemptId) || item.version !== 1)
                    throw failure('clear-attempt')
                  if (Object.keys(item).length === 6 && item.disposition === 'superseded')
                    entries.push({ kind: 'superseded', key, attempt })
                  else if (Object.keys(item).length === 7 && item.cleared === true && item.records === 0)
                    entries.push({ kind: 'attempt', key, receipt: { ...attempt, cleared: true, records: 0 } })
                  else throw failure('clear-attempt')
                } else {
                  const legacy = /^:ubm-clear-v2:([A-Za-z0-9_-]{1,64}):([0-9]+)$/.exec(key)
                  const receipt =
                    legacy === null
                      ? checkedClearReceipt(value)
                      : clearReceipt(value, legacy[1] ?? '', Number(legacy[2]), false)
                  entries.push({ kind: 'active', key, receipt })
                }
              } catch {
                entries.push({ kind: 'malformed', key, value })
              }
            }
            done({ entries, next: values.length > options.limit ? last : null })
          })
        )
      })
    },
    getClearAttemptV1(attempt, options) {
      const checked = checkedWebClearAttempt(attempt)
      return transact<
        | { readonly outcome: 'unknown' }
        | { readonly outcome: 'superseded'; readonly attempt: WebClearAttemptV1 }
        | { readonly outcome: 'committed'; readonly receipt: WebClearAttemptReceiptV1 }
      >(
        database,
        ['readerReceipts'],
        (tx, done, guard) => {
          const request = tx.objectStore('readerReceipts').get(webClearAttemptKey(checked.attemptId))
          request.onsuccess = () =>
            guard(() => {
              if (request.result === undefined) return done({ outcome: 'unknown' })
              const value = row(request.result)
              const expected: WebClearAttemptReceiptV1 = { ...checked, cleared: true, records: 0 }
              if (
                Object.keys(value).length === 6 &&
                value.recordingId === webClearAttemptKey(checked.attemptId) &&
                value.version === 1 &&
                value.targetRecordingId === checked.recordingId &&
                value.producerIncarnation === checked.producerIncarnation &&
                value.attemptId === checked.attemptId &&
                value.disposition === 'superseded'
              )
                return done({ outcome: 'superseded', attempt: checked })
              if (
                Object.keys(value).length !== 7 ||
                value.recordingId !== webClearAttemptKey(checked.attemptId) ||
                value.version !== 1 ||
                value.targetRecordingId !== checked.recordingId ||
                value.producerIncarnation !== checked.producerIncarnation ||
                value.attemptId !== checked.attemptId ||
                value.cleared !== true ||
                value.records !== 0
              )
                throw failure('clear-attempt-conflict')
              done({ outcome: 'committed', receipt: Object.freeze(expected) })
            })
        },
        'readonly',
        options?.signal
      ).catch(error => {
        if (((typeof error === 'object' && error !== null) || typeof error === 'function') && uncertainReads.has(error))
          return { outcome: 'unknown' as const }
        throw error
      })
    },
    async acknowledgeClearAttemptV1(attempt, receipt) {
      const checked = checkedWebClearAttempt(attempt)
      const result = await this.getClearAttemptV1(checked)
      if (
        result.outcome !== 'committed' ||
        typeof receipt !== 'object' ||
        receipt === null ||
        Object.keys(receipt).length !== 6 ||
        receipt.version !== 1 ||
        receipt.recordingId !== checked.recordingId ||
        receipt.producerIncarnation !== checked.producerIncarnation ||
        receipt.attemptId !== checked.attemptId ||
        receipt.cleared !== true ||
        receipt.records !== 0
      )
        throw failure('clear-attempt-receipt')
    },
    listIncarnations(id, options) {
      if (!isRecordingId(id) || typeof options !== 'object' || options === null) throw failure('identity')
      if (
        (options.includeClearReceipts !== undefined && typeof options.includeClearReceipts !== 'boolean') ||
        (options.clearReceiptsAfter != null &&
          (typeof options.clearReceiptsAfter !== 'string' ||
            !options.clearReceiptsAfter.startsWith(durableClearPrefix))) ||
        (options.includePartitionRetained !== undefined && typeof options.includePartitionRetained !== 'boolean') ||
        (options.partitionRetainedAfter != null &&
          (typeof options.partitionRetainedAfter !== 'string' ||
            !options.partitionRetainedAfter.startsWith(WEB_RETAINED_MARKER_PREFIX) ||
            options.partitionRetainedAfter.length > 2048))
      )
        throw failure('page')
      pageLimit(options.limit, 32)
      const prefix = retainedPrefix(id)
      if (
        options.after !== null &&
        (typeof options.after !== 'string' || !options.after.startsWith(prefix) || options.after.length > 2048)
      )
        throw failure('page')
      const retainedRange = ranges.retained
      if (retainedRange === undefined) throw failure('cursor-unavailable')
      return transact<{
        active: string | null
        retained: string[]
        next: string | null
        malformedRetained: { recordingId: string; value: unknown }[]
      }>(database, ['recordings', 'readerReceipts'], (tx, done, guard) => {
        const current = tx.objectStore('recordings').get(id)
        const anchors = getAll(
          tx.objectStore('readerReceipts'),
          retainedRange(id, options.after ?? undefined),
          options.limit + 1
        )
        Reflect.set(anchors, 'onsuccess', () =>
          guard(() => {
            const active = current.result === undefined ? null : row(current.result)
            if (
              active !== null &&
              (active.id !== id ||
                typeof active.producerIncarnation !== 'string' ||
                !validId(id, active.producerIncarnation))
            )
              throw failure('incarnation')
            const values: unknown = Reflect.get(anchors, 'result')
            if (!Array.isArray(values) || values.length > options.limit + 1) throw failure('incarnations')
            const selected = values.slice(0, options.limit)
            const retained: string[] = []
            const malformedRetained: { recordingId: string; value: unknown }[] = []
            let lastKey = ''
            for (const value of selected) {
              const anchor = row(value)
              if (typeof anchor.recordingId !== 'string' || !anchor.recordingId.startsWith(prefix))
                throw failure('anchor-key')
              lastKey = anchor.recordingId
              try {
                const parsed = parseWebRetainedMarkerAnchor(value)
                if (parsed.recordingId !== id) throw failure('anchor')
                retained.push(parsed.producerIncarnation)
              } catch {
                malformedRetained.push({ recordingId: anchor.recordingId, value })
              }
            }
            done({
              active: active === null ? null : String(active.producerIncarnation),
              retained,
              malformedRetained,
              next: values.length > options.limit ? lastKey : null
            })
          })
        )
      }).then(async page => {
        const clear = options.includeClearReceipts ? await listClearReceipts(id) : undefined
        const durable = options.includeClearReceipts
          ? await transact<{
              clearReceipts: WebClearReceipt[]
              malformedClearReceipts: { recordingId: string; error: 'malformed-clear-receipt' }[]
              clearReceiptsNext: string | null
            }>(database, ['readerReceipts'], (tx, done, guard) => {
              const after = options.clearReceiptsAfter ?? undefined
              const request = getAll(
                tx.objectStore('readerReceipts'),
                IDBKeyRange.bound(after ?? durableClearPrefix, `${durableClearPrefix}\uffff`, after !== undefined),
                options.limit + 1
              )
              Reflect.set(request, 'onsuccess', () =>
                guard(() => {
                  const values: unknown = Reflect.get(request, 'result')
                  if (!Array.isArray(values) || values.length > options.limit + 1) throw failure('clear-receipts')
                  const found: WebClearReceipt[] = []
                  const malformed: { recordingId: string; error: 'malformed-clear-receipt' }[] = []
                  let last = ''
                  for (const value of values.slice(0, options.limit)) {
                    const entry = row(value)
                    if (typeof entry.recordingId !== 'string' || !entry.recordingId.startsWith(durableClearPrefix))
                      throw failure('clear-receipt')
                    last = entry.recordingId
                    try {
                      found.push(durableClearReceipt(entry))
                    } catch {
                      malformed.push({ recordingId: last, error: 'malformed-clear-receipt' })
                    }
                  }
                  done({
                    clearReceipts: found,
                    malformedClearReceipts: malformed,
                    clearReceiptsNext: values.length > options.limit ? last : null
                  })
                })
              )
            })
          : undefined
        const partition = options.includePartitionRetained
          ? await transact<{
              partitionRetained: { recordingId: string; producerIncarnation: string; markerLedgerBytes: number }[]
              partitionRetainedNext: string | null
              malformedPartitionRetained: { recordingId: string; value: unknown }[]
            }>(database, ['readerReceipts'], (tx, done, guard) => {
              const after = options.partitionRetainedAfter ?? undefined
              const request = getAll(
                tx.objectStore('readerReceipts'),
                IDBKeyRange.bound(
                  after ?? WEB_RETAINED_MARKER_PREFIX,
                  `${WEB_RETAINED_MARKER_PREFIX}\uffff`,
                  after !== undefined
                ),
                33
              )
              Reflect.set(request, 'onsuccess', () =>
                guard(() => {
                  const values: unknown = Reflect.get(request, 'result')
                  if (!Array.isArray(values) || values.length > 33) throw failure('anchors')
                  const selected = values.slice(0, 32)
                  const partitionRetained: {
                    recordingId: string
                    producerIncarnation: string
                    markerLedgerBytes: number
                  }[] = []
                  const malformedPartitionRetained: { recordingId: string; value: unknown }[] = []
                  let lastKey = ''
                  for (const value of selected) {
                    const anchor = row(value)
                    if (
                      typeof anchor.recordingId !== 'string' ||
                      !anchor.recordingId.startsWith(WEB_RETAINED_MARKER_PREFIX)
                    )
                      throw failure('anchor-key')
                    lastKey = anchor.recordingId
                    try {
                      const { recordingId, producerIncarnation, markerLedgerBytes } =
                        parseWebRetainedMarkerAnchor(value)
                      partitionRetained.push({ recordingId, producerIncarnation, markerLedgerBytes })
                    } catch {
                      malformedPartitionRetained.push({ recordingId: anchor.recordingId, value })
                    }
                  }
                  done({
                    partitionRetained,
                    malformedPartitionRetained,
                    partitionRetainedNext: values.length > 32 ? lastKey : null
                  })
                })
              )
            })
          : {}
        return {
          ...page,
          ...clear,
          ...partition,
          ...(durable === undefined
            ? {}
            : {
                clearReceipts: [...(clear?.clearReceipts ?? []), ...durable.clearReceipts],
                malformedClearReceipts: [...(clear?.malformedClearReceipts ?? []), ...durable.malformedClearReceipts],
                clearReceiptsNext: durable.clearReceiptsNext
              })
        }
      })
    },
    listMarkerOperations(id, incarnation, options) {
      identity(id, incarnation)
      if (typeof options !== 'object' || options === null || (options.after !== null && !token(options.after)))
        throw failure('page')
      pageLimit(options.limit, 8)
      return transact(
        database,
        ['recordings', 'readerReceipts', 'markerLedger', 'markerOperations'],
        (tx, done, guard) => {
          const current = tx.objectStore('recordings').get(id)
          const anchor = tx.objectStore('readerReceipts').get(webRetainedMarkerKey(id, incarnation))
          const requests = getAll(
            tx.objectStore('markerOperations'),
            ranges.bound(id, incarnation, options.after ?? undefined),
            options.limit + 1
          )
          Reflect.set(requests, 'onsuccess', () =>
            guard(() => {
              const active = current.result === undefined ? null : row(current.result)
              if (
                !(active?.id === id && active.producerIncarnation === incarnation) &&
                !retainedAnchor(anchor.result, id, incarnation)
              )
                throw failure('incarnation')
              const values: unknown = Reflect.get(requests, 'result')
              if (!Array.isArray(values) || values.length > options.limit + 1) throw failure('operations')
              const selected = values.slice(0, options.limit)
              const evidence: WebRecordingOperationEvidence[] = []
              let previous = options.after
              for (const value of values) {
                const receipt = row(value)
                if (
                  receipt.recordingId !== id ||
                  receipt.producerIncarnation !== incarnation ||
                  !token(receipt.operationId) ||
                  (previous !== null && receipt.operationId <= previous)
                )
                  throw failure('receipt')
                previous = receipt.operationId
              }
              if (selected.length === 0) return done({ operations: [], next: null })
              for (const value of selected) {
                const receipt = row(value)
                const result = row(receipt.result)
                const bound = row(receipt.owner)
                if (
                  receipt.version !== 1 ||
                  !token(receipt.operationId) ||
                  !isCanonicalIngressOrdinal(result.markerId) ||
                  bound.id !== id ||
                  typeof bound.generation !== 'string' ||
                  typeof bound.ownerEpoch !== 'string' ||
                  !token(bound.ownerToken) ||
                  typeof receipt.signature !== 'string'
                )
                  throw failure('receipt')
                let signed: Record<string, unknown>
                try {
                  signed = row(JSON.parse(receipt.signature))
                } catch {
                  throw failure('receipt')
                }
                if (typeof signed.kind !== 'string' || !('data' in signed)) throw failure('receipt')
                const owner: WebAuthorityOwner = {
                  id,
                  generation: bound.generation,
                  ownerEpoch: bound.ownerEpoch,
                  ownerToken: bound.ownerToken
                }
                const marker = tx.objectStore('markerLedger').get([id, incarnation, result.markerId])
                marker.onsuccess = () =>
                  guard(() => {
                    const currentMarker = row(marker.result)
                    webMarkerLedgerSnapshot(currentMarker, owner, incarnation)
                    evidence.push({
                      operationId: String(receipt.operationId),
                      signature: 'unverified',
                      transition: { kind: String(signed.kind), data: signed.data },
                      ownerGeneration: owner.generation,
                      ownerEpoch: owner.ownerEpoch,
                      result: webMarkerLedgerSnapshot(
                        { ...result, request: currentMarker.request, owner },
                        owner,
                        incarnation
                      )
                    })
                    if (evidence.length === selected.length) {
                      evidence.sort((a, b) => a.operationId.localeCompare(b.operationId))
                      done({
                        operations: evidence,
                        next: values.length > options.limit ? evidence[evidence.length - 1]!.operationId : null
                      })
                    }
                  })
              }
            })
          )
        }
      )
    },
    disposeRetained(id, incarnation, options) {
      identity(id, incarnation)
      if (typeof options !== 'object' || options === null || options.acknowledgeExport !== true)
        throw failure('export-acknowledgement')
      pageLimit(options.limit, 32)
      if (options.verifyRetiredClearReceipt !== undefined && options.verifyRetiredClearReceipt !== true)
        throw failure('clear-receipt')
      if (options.verifyRetiredClearReceipt && options.clearReceipt === undefined) throw failure('clear-receipt')
      if (options.clearReceipt !== undefined) {
        if (typeof options.clearReceipt !== 'object' || options.clearReceipt === null) throw failure('clear-receipt')
        if (options.clearReceipt.producerIncarnation !== incarnation) throw failure('clear-receipt')
        if (options.verifyRetiredClearReceipt) {
          if (options.clearReceipt.targetRecordingId !== id) throw failure('clear-receipt')
          return getRetiredClearReceipt(options.clearReceipt).then(receipt =>
            receipt === undefined
              ? { state: 'absent', deleted: 0 }
              : { state: 'more', deleted: 0, retiredReceipt: receipt }
          )
        }
        return retireClearReceipt(id, options.clearReceipt).then(deleted => ({
          state: deleted ? 'more' : 'absent',
          deleted: deleted ? 1 : 0
        }))
      }
      return transact(
        database,
        ['recordings', 'readerReceipts', 'markerLedger', 'markerOperations'],
        (tx, done, guard) => {
          const current = tx.objectStore('recordings').get(id)
          const anchors = tx.objectStore('readerReceipts')
          const anchor = anchors.get(webRetainedMarkerKey(id, incarnation))
          const operations = tx.objectStore('markerOperations')
          const markers = tx.objectStore('markerLedger')
          const operationRows = getAll(operations, ranges.bound(id, incarnation), options.limit)
          const markerRows = getAll(markers, ranges.bound(id, incarnation), options.limit)
          Reflect.set(markerRows, 'onsuccess', () =>
            guard(() => {
              const active = current.result === undefined ? null : row(current.result)
              if (active?.id === id && active.producerIncarnation === incarnation) throw failure('active')
              if (anchor.result === undefined) return done({ state: 'absent', deleted: 0 })
              if (!retainedAnchor(anchor.result, id, incarnation)) throw failure('anchor')
              const ops: unknown = Reflect.get(operationRows, 'result')
              const marks: unknown = Reflect.get(markerRows, 'result')
              if (
                !Array.isArray(ops) ||
                !Array.isArray(marks) ||
                ops.length > options.limit ||
                marks.length > options.limit
              )
                throw failure('rows')
              if (ops.length > 0) {
                for (const value of ops) {
                  const item = row(value)
                  if (item.recordingId !== id || item.producerIncarnation !== incarnation || !token(item.operationId))
                    throw failure('receipt')
                  operations.delete([id, incarnation, item.operationId])
                }
                return done({ state: 'more', deleted: ops.length })
              }
              if (marks.length > 0) {
                for (const value of marks) {
                  const item = markerEvidence(value, id, incarnation)
                  if (typeof item.markerId !== 'string') throw failure('marker')
                  markers.delete([id, incarnation, item.markerId])
                }
                return done({ state: 'more', deleted: marks.length })
              }
              anchors.delete(webRetainedMarkerKey(id, incarnation))
              done({ state: 'disposed', deleted: 0 })
            })
          )
        },
        'readwrite'
      )
    },
    inspect(id, incarnation) {
      identity(id, incarnation)
      return transact(database, ['recordings', 'markerLedger', 'readerReceipts'], (tx, done, guard) => {
        const recording = tx.objectStore('recordings').get(id)
        const retained = tx.objectStore('readerReceipts').get(webRetainedMarkerKey(id, incarnation))
        const markers = getAll(tx.objectStore('markerLedger'), ranges.bound(id, incarnation))
        Reflect.set(markers, 'onsuccess', () =>
          guard(() => {
            const current = recording.result === undefined ? null : row(recording.result)
            const active = current?.id === id && current.producerIncarnation === incarnation
            const anchor = retained.result === undefined ? null : row(retained.result)
            if (
              !active &&
              (anchor === null ||
                anchor.recordingId !== webRetainedMarkerKey(id, incarnation) ||
                anchor.version !== 1 ||
                anchor.disposition !== 'cleared-marker-evidence-retained')
            )
              throw failure('incarnation')
            const bytes = active ? (current.markerLedgerBytes ?? 0) : anchor?.markerLedgerBytes
            if (!Number.isSafeInteger(bytes) || Number(bytes) < 0) throw failure('accounting')
            const values: unknown = Reflect.get(markers, 'result')
            if (!Array.isArray(values)) throw failure('markers')
            if (values.length > 256) throw quota()
            const evidence = values.map(value => markerEvidence(value, id, incarnation))
            done({
              recordingId: id,
              producerIncarnation: incarnation,
              recordingDisposition: active ? 'active' : 'cleared-marker-evidence-retained',
              markerLedgerBytes: Number(bytes),
              markerInventory: 'unknown',
              markers: evidence,
              uncertainty: { priorMarkerOutcome: 'unknown', sourceCleanup: 'unknown', oldOwnerActivity: 'possible' }
            })
          })
        )
      })
    },
    listMarkers(id, incarnation, options) {
      identity(id, incarnation)
      if (
        typeof options !== 'object' ||
        options === null ||
        (options.after !== null && !isCanonicalIngressOrdinal(options.after)) ||
        !Number.isSafeInteger(options.limit) ||
        options.limit < 1 ||
        options.limit > 32
      )
        throw failure('page')
      const { after, limit } = options
      return transact(database, ['recordings', 'markerLedger', 'readerReceipts'], (tx, done, guard) => {
        const recording = tx.objectStore('recordings').get(id)
        const retained = tx.objectStore('readerReceipts').get(webRetainedMarkerKey(id, incarnation))
        const markers = getAll(
          tx.objectStore('markerLedger'),
          ranges.bound(id, incarnation, after ?? undefined),
          limit + 1
        )
        Reflect.set(markers, 'onsuccess', () =>
          guard(() => {
            const current = recording.result === undefined ? null : row(recording.result)
            const anchor = retained.result === undefined ? null : row(retained.result)
            if (
              !(current?.id === id && current.producerIncarnation === incarnation) &&
              !(
                anchor?.recordingId === webRetainedMarkerKey(id, incarnation) &&
                anchor.version === 1 &&
                anchor.disposition === 'cleared-marker-evidence-retained'
              )
            )
              throw failure('incarnation')
            const values: unknown = Reflect.get(markers, 'result')
            if (!Array.isArray(values) || values.length > limit + 1) throw failure('markers')
            const evidence = values.map(value => markerEvidence(value, id, incarnation))
            let previous = after
            for (const marker of evidence) {
              if (typeof marker.markerId !== 'string' || (previous !== null && marker.markerId <= previous))
                throw failure('marker-order')
              previous = marker.markerId
            }
            const page = evidence.slice(0, limit)
            const last = page[page.length - 1]
            done({
              markers: page,
              next: evidence.length > limit && typeof last?.markerId === 'string' ? last.markerId : null
            })
          })
        )
      })
    },
    async reconcileMarkerOperation(id, incarnation, operationId) {
      identity(id, incarnation)
      if (!token(operationId)) throw failure('operation-id')
      const owner = await transact<WebAuthorityOwner | null>(database, ['markerOperations'], (tx, done, guard) => {
        const request = tx.objectStore('markerOperations').get([id, incarnation, operationId])
        request.onsuccess = () =>
          guard(() => {
            if (request.result === undefined) return done(null)
            const receipt = row(request.result)
            const bound = row(receipt.owner)
            if (
              receipt.version !== 1 ||
              receipt.recordingId !== id ||
              receipt.producerIncarnation !== incarnation ||
              receipt.operationId !== operationId ||
              bound.id !== id ||
              typeof bound.generation !== 'string' ||
              typeof bound.ownerEpoch !== 'string' ||
              !token(bound.ownerToken)
            )
              throw failure('receipt')
            done({ id, generation: bound.generation, ownerEpoch: bound.ownerEpoch, ownerToken: bound.ownerToken })
          })
      })
      if (owner === null) return { outcome: 'unknown' }
      const receipt = await createWebRecordingMarkerLedger(database, owner, incarnation).reconcile(operationId)
      return receipt.outcome === 'unknown'
        ? receipt
        : { outcome: 'observed', signature: 'unverified', result: receipt.result }
    }
  }
}
