import { webRecordingRandomBytes } from './web-recording-errors'
import {
  webRecordingContractError as contractError,
  trustedWebRecordingError,
  webStorageErrorName,
  webStorageError as storageError,
  webIndexedDbQueueAbortError
} from './web-recording-errors'
import {
  isRecordingId,
  MAX_RECORDING_BATCH_BYTES,
  MAX_RECORDING_BATCH_ITEMS,
  MAX_RECORDING_TOKEN_BYTES,
  MAX_RECORDING_BYTES,
  MAX_RECORDING_RECORDS,
  MIN_RECORDING_BYTES,
  MIN_RECORDING_RECORDS
} from '../backend-contract/continuation-recording-bounds'
import { utf8ByteLength } from '../backend-contract/serializable'
import type {
  ContinuationRecordingBatch,
  ContinuationRecordingPrepareOptions,
  ContinuationRecordingStatus,
  OrderedRecordingHostStopResult
} from '../core/continuation-recording'
import {
  parseOrderedRecordingHostCleanup as parseHostCleanup,
  parseOrderedRecordingStopReceipt as parseStopReceipt
} from '../core/continuation-recording'
import {
  parseOrderedRecordingBatch as parseBatch,
  parseOrderedRecordingContext as parseContext,
  parseOrderedRecordingFailure as parseFailure
} from '../core/ordered-recording'
import type { OrderedRecordingBatch, OrderedRecordingContext } from '../core/ordered-recording'
import { rehydratePublicError, rehydratePublicPromise } from '../public/error-bridge'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import { acquireWebRecordingClearLock } from './web-recording-clear-lock'
import type { WebRecordingLockManager } from './web-recording-lock'
import {
  webRecordingAcknowledgedBinding,
  webRecordingCanonical,
  webRecordingMarkerLedgerBytes,
  webRecordingMarkerDebt,
  webRecordingReaderIntegrity,
  webRecordingTerminalOutcome
} from './web-ordered-recording-journal'
import type {
  WebOrderedJournalDatabase,
  WebOrderedJournalTransaction,
  WebRecordingReaderIntegrity
} from './web-ordered-recording-journal'
import { openWebRecordingDatabase } from './web-recording-database'
import type { WebRecordingDatabase, WebRecordingDatabaseFactory } from './web-recording-database'
import type { WebRecordingLiveIdentity, WebRecordingLiveOwners } from './web-recording-live-owners'
import { parseWebRecordingPeerRow, webRecordingOwnerFenceKey, webRecordingPeerKey } from './web-recording-peer-identity'
import { createWebAuthorityReservations } from './web-authority-reservation'
import { checkedWebClearAttempt, webClearAttemptKey } from './web-clear-attempt'
import { boundedWebClearAdmission } from './web-clear-admission-bound'
import type { WebClearAttemptV1, WebContinuationRecordingController } from './web-clear-attempt'
import {
  checkedWebProofAdmission,
  commitWebClearProofInTransaction,
  createWebClearProofLedger
} from './web-clear-proof-ledger'
import type { WebProofAdmission } from './web-clear-proof-ledger'
import { createWebRecordingRecovery, webRetiredClearKey, webRetiredClearMatches } from './web-recording-recovery'
import type { WebClearReceipt } from './web-recording-recovery'
import {
  parseWebRetainedMarkerAnchor,
  WEB_RETAINED_MARKER_BYTES,
  WEB_RETAINED_MARKER_LIMIT,
  WEB_RETAINED_MARKER_PREFIX,
  webRetainedMarkerKey
} from './web-recording-recovery'

// Shared parsers run on storage or host data. Preserve their malformed-data
// contract without treating an arbitrary exception from that data as trusted.
function parseRecordingValue<T>(parse: () => T, operation: string): T {
  try {
    return parse()
  } catch {
    throw contractError('protocol.malformed', 'restoration', operation)
  }
}
const parseOrderedRecordingContext = (...args: Parameters<typeof parseContext>) =>
  parseRecordingValue(() => parseContext(...args), 'continuation.recording.ordered')
const parseOrderedRecordingFailure = (...args: Parameters<typeof parseFailure>) =>
  parseRecordingValue(() => parseFailure(...args), 'continuation.recording.ordered')
const parseOrderedRecordingBatch = (...args: Parameters<typeof parseBatch>) =>
  parseRecordingValue(() => parseBatch(...args), 'continuation.recording.ordered')
const parseOrderedRecordingStopReceipt = (...args: Parameters<typeof parseStopReceipt>) =>
  parseRecordingValue(() => parseStopReceipt(...args), 'continuation.recording')
const parseOrderedRecordingHostCleanup = (...args: Parameters<typeof parseHostCleanup>) =>
  parseRecordingValue(() => parseHostCleanup(...args), 'continuation.recording')

declare const IDBKeyRange: { bound(lower: string, upper: string): unknown }

type Row = Record<string, unknown>
type Database = WebRecordingDatabase & WebOrderedJournalDatabase

declare const indexedDB: WebRecordingDatabaseFactory<Database> | undefined
declare const crypto: { getRandomValues(value: Uint8Array): Uint8Array } | undefined
declare const navigator: { readonly locks?: WebRecordingLockManager } | undefined

/** The optional boundary is intended for host integration and deterministic tests. */
export interface WebContinuationRecordingControllerOptions {
  readonly database?: WebOrderedJournalDatabase
  readonly randomToken?: () => string
  /** Shared by the host and controllers for this storage partition. */
  readonly liveOwners?: WebRecordingLiveOwners
  readonly locks?: WebRecordingLockManager
  /** Stable database name for host-supplied adapters that do not expose one. */
  readonly journalIdentity?: string
  /** Maximum simultaneous unresolved V2 admission transactions in this partition. */
  readonly clearAdmissionLimit?: number
}

const bad = (detail: string): Error => contractError('protocol.malformed', 'restoration', `web-recording.${detail}`)
const argument = (detail: string): Error => contractError('argument.invalid', 'restoration', `web-recording.${detail}`)
const unavailable = (detail: string): Error =>
  contractError('capability.unavailable', 'restoration', `web-recording.${detail}`)
const clearReleaseErrors = new WeakSet<object>()
function clearReleaseError(errors: readonly unknown[], message: string): AggregateError {
  const result = new AggregateError(
    errors.map(error =>
      rehydratePublicError(trustedWebRecordingError(error) ?? storageError(error, 'indexeddb-transaction'))
    ),
    message
  )
  clearReleaseErrors.add(result)
  return result
}
const ownerFenceBlocked = (missing: boolean): Error =>
  contractError(missing ? 'protocol.malformed' : 'capability.unavailable', 'restoration', 'web-recording.owner-fence', {
    domain: 'web-recording-owner',
    code: missing ? 'fence-absent' : 'fence-held',
    safeMessage:
      'Return to the recording owner and settle and forget its marker receipt, then retry clear. If the owner is gone, receipt retirement cannot be established automatically.',
    metadata: {}
  })
async function ownerCall<T>(operation: string, run: () => Promise<T>, acknowledged = false): Promise<T> {
  try {
    return await run()
  } catch (error) {
    const trusted = trustedWebRecordingError(error)
    if (trusted !== undefined && !acknowledged) throw error
    const name = webStorageErrorName(error)
    throw contractError('capability.unavailable', 'restoration', `web-recording.${operation}`, {
      domain: 'web-recording-owner',
      code: trusted?.normalized.code ?? name,
      safeMessage: 'Live recording owner operation failed',
      metadata: acknowledged ? { acknowledgement: 'committed' } : {}
    })
  }
}
function object(value: unknown): Row {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw bad('row')
  return Object.fromEntries(Object.entries(value))
}
function integer(value: unknown, min: number, max = Number.MAX_SAFE_INTEGER): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= min && value <= max
}
function opaque(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= 256
}
function decimal(value: unknown): value is string {
  return (
    typeof value === 'string' &&
    /^(0|[1-9][0-9]*)$/.test(value) &&
    value.length <= 20 &&
    BigInt(value) <= 18446744073709551615n
  )
}
function idCheck(id: string): void {
  if (!isRecordingId(id)) throw argument('id')
}
function bounds(options: ContinuationRecordingPrepareOptions): void {
  if (
    !options ||
    !integer(options.maxItems, 1, MAX_RECORDING_BATCH_ITEMS) ||
    !integer(options.maxBytes, 1, MAX_RECORDING_BATCH_BYTES)
  )
    throw argument('bounds')
}
function tokenCheck(token: unknown): token is string {
  return typeof token === 'string' && token.length > 0 && utf8ByteLength(token) <= MAX_RECORDING_TOKEN_BYTES
}
function defaultToken(): string {
  if (typeof crypto === 'undefined' || typeof crypto.getRandomValues !== 'function') throw unavailable('entropy')
  return Array.from(webRecordingRandomBytes(crypto, new Uint8Array(32)), value =>
    value.toString(16).padStart(2, '0')
  ).join('')
}
function recording(input: unknown, id: string): Row {
  if (input === undefined) throw unavailable('not-found')
  const row = object(input)
  if (
    row.id !== id ||
    row.version !== 1 ||
    row.kind !== 'ordered' ||
    !integer(row.maxBytes, MIN_RECORDING_BYTES, MAX_RECORDING_BYTES) ||
    !integer(row.maxRecords, MIN_RECORDING_RECORDS, MAX_RECORDING_RECORDS) ||
    !opaque(row.producerIncarnation) ||
    !decimal(row.collectionId) ||
    !opaque(row.sourceGeneration) ||
    !decimal(row.ownerEpoch) ||
    row.ownerEpoch === '0' ||
    !['prepared', 'live', 'sealed', 'interrupted'].includes(String(row.ownerState)) ||
    (row.nextOrdinal !== null && !integer(row.nextOrdinal, 1)) ||
    !integer(row.readerCursor, 0) ||
    !integer(row.recordCount, 0, row.maxRecords) ||
    !integer(row.byteCount, 0, row.maxBytes) ||
    (row.nextOrdinal === null
      ? Number.MAX_SAFE_INTEGER - row.readerCursor
      : Number(row.nextOrdinal) - row.readerCursor - 1) !== row.recordCount ||
    (row.lastDeliveryId !== null && !decimal(row.lastDeliveryId)) ||
    (row.lastSourceOrdinal !== null && !decimal(row.lastSourceOrdinal)) ||
    typeof row.terminal !== 'boolean' ||
    !integer(row.pendingCount, 0, row.maxRecords) ||
    !integer(row.pendingBytes, 0, row.maxBytes) ||
    (row.pendingCount === 0) !== (row.pendingBytes === 0) ||
    (row.lastRetiredId !== null && !decimal(row.lastRetiredId)) ||
    (row.ownerState === 'sealed' && (row.terminal !== true || row.pendingCount !== 0)) ||
    (row.recordCount === 0) !== (row.byteCount === 0) ||
    row.pendingCount > (row.nextOrdinal === null ? Number.MAX_SAFE_INTEGER : Number(row.nextOrdinal) - 1)
  )
    throw bad('recording')
  const context = parseOrderedRecordingContext(row.context)
  if (row.failure !== null) parseOrderedRecordingFailure(row.failure, context.version)
  webRecordingTerminalOutcome(row)
  if (!Array.isArray(row.memberEpochs) || row.memberEpochs.length !== context.members.length) throw bad('member-epochs')
  const members = new Set<string>()
  for (const item of row.memberEpochs) {
    const member = object(item)
    if (
      !decimal(member.member) ||
      !decimal(member.epoch) ||
      member.epoch === '0' ||
      members.has(member.member) ||
      !context.members.some(entry => entry.memberId === member.member)
    )
      throw bad('member-epochs')
    members.add(member.member)
  }
  webRecordingReaderIntegrity(row)
  return row
}
interface Receipt {
  readonly recordingId: string
  readonly version: 1
  pending: { token: string; first: number; last: number; bytes: number; more: boolean; payload: string } | null
  lastAck: { token: string; records: number } | null
}
function checkedReceipt(input: unknown, id: string, cursor: number, integrity: WebRecordingReaderIntegrity): Receipt {
  if ((input === undefined) !== (integrity.receipt === null)) throw bad('receipt-missing')
  const result = receipt(input, id, cursor)
  if (input !== undefined && JSON.stringify(result) !== integrity.receipt) throw bad('receipt-integrity')
  return result
}
function receipt(input: unknown, id: string, cursor: number): Receipt {
  if (input === undefined) return { recordingId: id, version: 1, pending: null, lastAck: null }
  const r = object(input)
  if (r.recordingId !== id || r.version !== 1 || !Object.hasOwn(r, 'pending') || !Object.hasOwn(r, 'lastAck'))
    throw bad('receipt')
  let pending: Receipt['pending'] = null
  let lastAck: Receipt['lastAck'] = null
  if (r.pending !== null) {
    const p = object(r.pending)
    if (
      !tokenCheck(p.token) ||
      !integer(p.first, 1) ||
      p.first !== cursor + 1 ||
      !integer(p.last, p.first) ||
      !integer(p.bytes, 1, MAX_RECORDING_BATCH_BYTES) ||
      typeof p.more !== 'boolean' ||
      typeof p.payload !== 'string' ||
      utf8ByteLength(p.payload) > MAX_RECORDING_BATCH_BYTES + MAX_RECORDING_BATCH_ITEMS + 1
    )
      throw bad('receipt')
    pending = { token: p.token, first: p.first, last: p.last, bytes: p.bytes, more: p.more, payload: p.payload }
  }
  if (r.lastAck !== null) {
    const a = object(r.lastAck)
    if (!tokenCheck(a.token) || !integer(a.records, 1, MAX_RECORDING_BATCH_ITEMS) || a.token === pending?.token)
      throw bad('receipt')
    lastAck = { token: a.token, records: a.records }
  }
  return { recordingId: id, version: 1, pending, lastAck }
}
function transact<T>(
  db: WebOrderedJournalDatabase,
  action: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
  includeAuthority = false,
  validate?: () => void
): Promise<T> {
  let result: { value: T } | undefined
  let problem: { error: unknown } | undefined
  return Promise.resolve()
    .then(() =>
      runWebIndexedDbTransaction(
        db,
        [
          'recordings',
          'records',
          'readerReceipts',
          'pendingIdentities',
          ...(includeAuthority ? ['authorityReservations'] : [])
        ],
        'readwrite',
        tx => {
          const guard = (fn: () => void): void => {
            try {
              validate?.()
              fn()
            } catch (error) {
              if (problem !== undefined) return
              problem = { error }
              try {
                tx.abort()
              } catch (abortError) {
                problem = { error: webIndexedDbQueueAbortError(error, abortError) }
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
        }
      )
    )
    .then(
      () => {
        if (problem !== undefined) throw problem.error
        if (result === undefined) throw bad('incomplete-transaction')
        return result.value
      },
      error => {
        throw problem === undefined ? error : problem.error
      }
    )
}
// Integrity is checked for every selected row against the independent source
// identity, including after source retirement. An acknowledgement only scans
// its bounded prepared prefix: corruption confined to an unread row is found
// when that row is read, not before an earlier, valid prefix is acknowledged.
// The duplicated aggregate counter checks its own consistency; it is not a
// proof that unread rows still match their recorded sizes. This detects isolated
// storage corruption, not coordinated modification of both copies.
function readRows(
  tx: WebOrderedJournalTransaction,
  row: Row,
  id: string,
  first: number,
  last: number,
  integrity: WebRecordingReaderIntegrity,
  guard: (fn: () => void) => void,
  done: (items: Row[], bytes: number, boundary: { deliveryId: string | null; sourceOrdinal: string | null }) => void,
  maxBytes = Number.MAX_SAFE_INTEGER,
  retained = false
): void {
  const items: Row[] = []
  let size = 0
  let batchVersion: 1 | 2 | undefined
  let lastDelivery = integrity.deliveryId === null ? undefined : BigInt(integrity.deliveryId)
  let lastSource = integrity.sourceOrdinal === null ? undefined : BigInt(integrity.sourceOrdinal)
  const finish = (): void => {
    const boundary = {
      deliveryId: lastDelivery === undefined ? null : String(lastDelivery),
      sourceOrdinal: lastSource === undefined ? null : String(lastSource)
    }
    if (
      first + items.length - 1 === Number(row.readerCursor) + Number(row.recordCount) &&
      (boundary.deliveryId !== row.lastDeliveryId || boundary.sourceOrdinal !== row.lastSourceOrdinal)
    )
      throw bad('recording-high-water')
    done(items, size, boundary)
  }
  const next = (ordinal: number): void => {
    if (ordinal > last) {
      finish()
      return
    }
    const request = tx.objectStore('records').get([id, ordinal])
    request.onsuccess = () =>
      guard(() => {
        const stored = object(request.result)
        if (
          stored.recordingId !== id ||
          stored.version !== 1 ||
          stored.ordinal !== ordinal ||
          !integer(stored.byteLength, 1) ||
          typeof stored.sourceRetired !== 'boolean'
        )
          throw bad('record')
        const metadata = object(stored.metadata)
        if (metadata.version !== 1 && metadata.version !== 2) throw bad('record-version')
        if (batchVersion !== undefined && batchVersion !== metadata.version) {
          if (retained) throw bad('receipt-range')
          finish()
          return
        }
        if (stored.byteLength > maxBytes - size) {
          if (retained) throw bad('receipt-range')
          if (items.length === 0) throw argument('oldest-record-size')
          finish()
          return
        }
        if (stored.byteLength > MAX_RECORDING_BATCH_BYTES) throw bad('record-bytes')
        const value = { ordinal, metadata: stored.metadata, record: stored.record }
        let encoded: string
        try {
          encoded = JSON.stringify(value)
        } catch {
          throw bad('record-payload')
        }
        const actual = utf8ByteLength(encoded)
        if (stored.byteLength !== actual || !Number.isSafeInteger(size + actual)) throw bad('record-bytes')
        const generation = object(metadata.sourceGeneration)
        const context = parseOrderedRecordingContext(row.context)
        if (
          metadata.peerId !== context.peerId ||
          metadata.collectionId !== row.collectionId ||
          generation.kind !== 'opaque' ||
          generation.value !== row.sourceGeneration
        )
          throw bad('record-owner')
        const serializedValue: unknown = JSON.parse(encoded)
        const decoded = parseOrderedRecordingBatch(
          { token: 'validation', records: [serializedValue], bytes: actual, more: false },
          id,
          {
            maxItems: 1,
            maxBytes: actual
          }
        ).records[0]
        if (!decoded || decoded.metadata.deliveryId === null || decoded.metadata.deliveryId === '0')
          throw bad('delivery-id')
        const delivery = BigInt(decoded.metadata.deliveryId)
        if (lastDelivery !== undefined && delivery <= lastDelivery) throw bad('delivery-order')
        if (typeof row.lastDeliveryId !== 'string' || delivery > BigInt(row.lastDeliveryId)) throw bad('delivery-order')
        lastDelivery = delivery
        const record = decoded.record
        if (record.kind === 'value') {
          if (
            record.member !== record.membership.member ||
            !Array.isArray(row.memberEpochs) ||
            !row.memberEpochs.some(item => {
              const member = object(item)
              return member.member === record.member && member.epoch === record.membership.epoch
            })
          )
            throw bad('record-membership')
        }
        if (record.kind === 'value' || record.kind === 'marker') {
          const source = BigInt(record.ordinal)
          if (lastSource !== undefined && source <= lastSource) throw bad('source-order')
          if (typeof row.lastSourceOrdinal !== 'string' || source > BigInt(row.lastSourceOrdinal))
            throw bad('source-order')
          lastSource = source
        }
        const key = [
          id,
          String(row.producerIncarnation),
          String(row.collectionId),
          String(row.sourceGeneration),
          decoded.metadata.deliveryId
        ]
        const identityRequest = tx.objectStore('pendingIdentities').get(key)
        identityRequest.onsuccess = () =>
          guard(() => {
            const source = object(identityRequest.result)
            const expectedMetadata = {
              kind: 'ordered',
              version: decoded.metadata.version,
              recordingId: id,
              peerId: context.peerId,
              collectionId: row.collectionId,
              sourceGeneration: { kind: 'opaque', value: row.sourceGeneration },
              deliveryId: key[4]
            }
            if (
              source.recordingId !== key[0] ||
              source.producerIncarnation !== key[1] ||
              source.collectionId !== key[2] ||
              source.sourceGeneration !== key[3] ||
              source.deliveryId !== key[4] ||
              source.ordinal !== ordinal ||
              source.byteLength !== actual ||
              source.payload !== webRecordingCanonical(stored.record) ||
              source.readerAcknowledged !== null ||
              source.retired !== stored.sourceRetired ||
              webRecordingCanonical(stored.metadata) !== webRecordingCanonical(expectedMetadata)
            )
              throw bad('identity-conflict')
            items.push(object(serializedValue))
            batchVersion = decoded.metadata.version
            size += actual
            next(ordinal + 1)
          })
      })
  }
  next(first)
}

/** Opens the partition journal without a manager, permission, or radio session. */
export function createWebContinuationRecordingController(
  options: WebContinuationRecordingControllerOptions = {}
): Promise<WebContinuationRecordingController> {
  return rehydratePublicPromise(
    (async () => {
      if (typeof options !== 'object' || options === null || Array.isArray(options)) throw argument('options')
      let db = options.database
      try {
        if (!db) {
          if (typeof indexedDB === 'undefined') throw unavailable('indexeddb')
          db = await openWebRecordingDatabase(indexedDB)
        }
      } catch (error) {
        const trusted = trustedWebRecordingError(error)
        if (trusted !== undefined) {
          let mapped: unknown
          try {
            mapped = rehydratePublicError(trusted)
          } catch {
            throw storageError(error, 'indexeddb-open')
          }
          throw mapped
        }
        throw storageError(error, 'indexeddb-open')
      }
      const database = db
      const randomToken = options.randomToken ?? defaultToken
      const call = <T>(operation: () => Promise<T>): Promise<T> =>
        rehydratePublicPromise(
          Promise.resolve()
            .then(operation)
            .catch(error => {
              const trusted = trustedWebRecordingError(error)
              if (trusted !== undefined) {
                let mapped: unknown
                try {
                  mapped = rehydratePublicError(trusted)
                } catch {
                  throw storageError(error, 'indexeddb-transaction')
                }
                throw mapped
              }
              if (typeof error === 'object' && error !== null && clearReleaseErrors.has(error)) throw error
              throw storageError(error, 'indexeddb-transaction')
            })
        )
      const withRow = <T>(
        id: string,
        fn: (
          row: Row,
          tx: WebOrderedJournalTransaction,
          done: (value: T) => void,
          guard: (fn: () => void) => void
        ) => void,
        includeAuthority = false,
        selectedDatabase: WebOrderedJournalDatabase = database,
        validate?: () => void
      ): Promise<T> => {
        idCheck(id)
        return transact(
          selectedDatabase,
          (tx, done, guard) => {
            const request = tx.objectStore('recordings').get(id)
            request.onsuccess = () => guard(() => fn(recording(request.result, id), tx, done, guard))
          },
          includeAuthority,
          validate
        )
      }
      const identity = (row: Row, id: string): WebRecordingLiveIdentity => ({
        recordingId: id,
        producerIncarnation: String(row.producerIncarnation),
        ownerEpoch: String(row.ownerEpoch)
      })
      const prepareOrdered = (
        id: string,
        limits: ContinuationRecordingPrepareOptions
      ): Promise<OrderedRecordingBatch> =>
        call(() => {
          bounds(limits)
          return withRow(id, (row, tx, done, guard) => {
            const cursor = Number(row.readerCursor),
              count = Number(row.recordCount)
            const integrity = webRecordingReaderIntegrity(row)
            const request = tx.objectStore('readerReceipts').get(id)
            request.onsuccess = () =>
              guard(() => {
                const state = checkedReceipt(request.result, id, cursor, integrity)
                if (
                  state.pending &&
                  (state.pending.last > cursor + count ||
                    state.pending.last - state.pending.first + 1 > MAX_RECORDING_BATCH_ITEMS)
                )
                  throw bad('receipt-range')
                if (!state.pending && count === 0) {
                  done(parseOrderedRecordingBatch({ token: null, records: [], bytes: 0, more: false }, id, limits))
                  return
                }
                const end = state.pending?.last ?? Math.min(cursor + count, cursor + limits.maxItems)
                readRows(
                  tx,
                  row,
                  id,
                  cursor + 1,
                  end,
                  integrity,
                  guard,
                  (items, total) => {
                    if (state.pending) {
                      if (total !== state.pending.bytes || JSON.stringify(items) !== state.pending.payload)
                        throw bad('receipt-payload')
                      if (items.length > limits.maxItems || total > limits.maxBytes) throw argument('prepared-bounds')
                    } else {
                      let selected = 0,
                        bytes = 0
                      for (const item of items) {
                        const n = utf8ByteLength(JSON.stringify(item))
                        if (bytes + n > limits.maxBytes) break
                        bytes += n
                        selected++
                      }
                      if (selected === 0) throw argument('oldest-record-size')
                      items.splice(selected)
                      total = bytes
                      const token = randomToken()
                      if (!tokenCheck(token) || token === state.lastAck?.token) throw unavailable('entropy')
                      state.pending = {
                        token,
                        first: cursor + 1,
                        last: cursor + selected,
                        bytes,
                        more: count > selected,
                        payload: JSON.stringify(items)
                      }
                      integrity.receipt = JSON.stringify(state)
                      row.readerIntegrity = integrity
                      tx.objectStore('recordings').put(row)
                      tx.objectStore('readerReceipts').put(state)
                    }
                    const pending = state.pending
                    if (!pending) throw bad('receipt')
                    done(
                      parseOrderedRecordingBatch(
                        { token: pending.token, records: items, bytes: total, more: pending.more },
                        id,
                        limits
                      )
                    )
                  },
                  state.pending ? state.pending.bytes : limits.maxBytes,
                  state.pending !== null
                )
              })
          })
        })
      const clear = (
        id: string,
        attempt?: WebClearAttemptV1,
        signal?: AbortSignal,
        attemptV2?: WebProofAdmission
      ): Promise<{ readonly cleared: true; readonly records: number }> =>
        call(async () => {
          const checkCancellation = (): void => {
            if (signal?.aborted) throw contractError('operation.aborted', 'restoration', 'web-recording.clear-attempt')
          }
          checkCancellation()
          let lockLost = false
          const databaseName: unknown = Reflect.get(database, 'name')
          if (
            options.journalIdentity !== undefined &&
            (typeof options.journalIdentity !== 'string' ||
              (typeof databaseName === 'string' && options.journalIdentity !== databaseName))
          )
            throw argument('journal-identity')
          const releaseLock = await acquireWebRecordingClearLock(
            options.locks ?? (typeof navigator === 'undefined' ? undefined : navigator.locks),
            () => {
              lockLost = true
            },
            options.journalIdentity ?? (typeof databaseName === 'string' ? databaseName : 'ubm-ordered-recordings'),
            signal
          )
          const checkLock = (): void => {
            if (lockLost) throw unavailable('clear-lock-lost')
          }
          const clearDatabase: WebOrderedJournalDatabase = {
            ready: async () => {
              checkLock()
              await database.ready?.()
              checkLock()
            },
            transaction: (stores, mode, transactionOptions) => {
              checkLock()
              return database.transaction(stores, mode, transactionOptions)
            }
          }
          // Track dispatched work until it settles, even after a caller timeout.
          // Allocation precedes deletion; at most deletion and its barrier overlap.
          const pending = new Set<Promise<unknown>>()
          let finished = false
          let released = false
          const release = (): void => {
            if (finished && pending.size === 0 && !released) {
              released = true
              releaseLock()
            }
          }
          const track = <T>(work: Promise<T>): Promise<T> => {
            pending.add(work)
            work.then(
              () => {
                pending.delete(work)
                release()
              },
              () => {
                pending.delete(work)
                release()
              }
            )
            return work
          }
          let primaryFailure: { readonly error: unknown } | undefined
          const observeRelease = async (): Promise<void> => {
            if (!released) return
            let releaseTimer: ReturnType<typeof setTimeout> | undefined
            try {
              const outcome = await Promise.race([
                releaseLock.ended,
                new Promise<never>((_resolve, reject) => {
                  releaseTimer = setTimeout(() => reject(unavailable('clear-lock-release-uncertain')), 5000)
                })
              ])
              if (
                outcome.state === 'lost' &&
                !(
                  trustedWebRecordingError(primaryFailure?.error)?.normalized.operation ===
                  'web-recording.clear-lock-lost'
                )
              )
                throw unavailable('clear-lock-release-uncertain')
            } catch (error) {
              if (primaryFailure !== undefined)
                throw clearReleaseError([primaryFailure.error, error], 'Clear and lock release both failed')
              throw error
            } finally {
              clearTimeout(releaseTimer)
            }
          }
          try {
            checkCancellation()
            let abandoned = false
            const checkAdmission = (): void => {
              checkLock()
              checkCancellation()
              if (abandoned) throw unavailable('clear-allocation-uncertain')
            }
            const allocation = track(
              (async () => {
                if (attemptV2 !== undefined) {
                  const prior = await createWebClearProofLedger(clearDatabase, randomToken).inspect(attemptV2, true)
                  checkAdmission()
                  if (prior.state === 'committed') return { prior: prior.result }
                  if (prior.state !== 'pending') throw unavailable('clear-attempt-retired-or-cancelled')
                }
                if (attempt !== undefined) {
                  const prior = await createWebRecordingRecovery(clearDatabase).getClearAttemptV1(attempt)
                  checkAdmission()
                  if (prior.outcome === 'committed') return { prior: prior.receipt }
                  if (prior.outcome === 'superseded') throw unavailable('clear-attempt-superseded')
                  // Serialize with every clear transaction. The terminal row is
                  // also checked by deletion, fencing a delayed old dispatch.
                  const admission = await transact<'proceed' | 'existing' | 'superseded'>(
                    clearDatabase,
                    (tx, done, guard) => {
                      const proofs = tx.objectStore('readerReceipts')
                      const existing = proofs.get(webClearAttemptKey(attempt.attemptId))
                      existing.onsuccess = () =>
                        guard(() => {
                          if (existing.result !== undefined) return done('existing')
                          const current = tx.objectStore('recordings').get(id)
                          current.onsuccess = () =>
                            guard(() => {
                              if (
                                current.result !== undefined &&
                                identity(recording(current.result, id), id).producerIncarnation ===
                                  attempt.producerIncarnation
                              )
                                return done('proceed')
                              proofs.add({
                                recordingId: webClearAttemptKey(attempt.attemptId),
                                version: 1,
                                targetRecordingId: id,
                                producerIncarnation: attempt.producerIncarnation,
                                attemptId: attempt.attemptId,
                                disposition: 'superseded'
                              })
                              done('superseded')
                            })
                        })
                    },
                    true,
                    checkAdmission
                  )
                  checkAdmission()
                  if (admission === 'superseded') throw unavailable('clear-attempt-superseded')
                  if (admission === 'existing') {
                    const settled = await createWebRecordingRecovery(clearDatabase).getClearAttemptV1(attempt)
                    checkAdmission()
                    if (settled.outcome === 'committed') return { prior: settled.receipt }
                    throw unavailable(
                      settled.outcome === 'superseded' ? 'clear-attempt-superseded' : 'clear-attempt-uncertain'
                    )
                  }
                }
                const key = await withRow<WebRecordingLiveIdentity>(
                  id,
                  (row, _tx, done) => done(identity(row, id)),
                  false,
                  clearDatabase
                )
                checkAdmission()
                if (
                  (attempt !== undefined && key.producerIncarnation !== attempt.producerIncarnation) ||
                  (attemptV2 !== undefined && key.producerIncarnation !== attemptV2.producerIncarnation)
                )
                  throw unavailable('clear-incarnation')
                if (options.liveOwners?.has(key)) throw unavailable('cleanup-evidence')
                const observed = await createWebAuthorityReservations(clearDatabase).status(id)
                checkAdmission()
                if (observed.phase !== 'released' || observed.ownerState !== 'sealed')
                  throw unavailable('cleanup-evidence')
                const sequenceKey = ':ubm-clear-global-sequence-v1'
                const operationId = await transact<string>(
                  clearDatabase,
                  (tx, done, guard) => {
                    checkAdmission()
                    const store = tx.objectStore('readerReceipts')
                    const request = store.get(sequenceKey)
                    request.onsuccess = () =>
                      guard(() => {
                        checkAdmission()
                        let sequence = 0n
                        if (request.result !== undefined) {
                          const prior = object(request.result)
                          if (
                            Object.keys(prior).length !== 3 ||
                            prior.recordingId !== sequenceKey ||
                            prior.version !== 1 ||
                            !decimal(prior.sequence) ||
                            prior.sequence === '0'
                          )
                            throw bad('clear-sequence')
                          sequence = BigInt(prior.sequence)
                        }
                        if (sequence === 18446744073709551615n) throw unavailable('clear-sequence-exhausted')
                        const next = String(sequence + 1n)
                        const recordings = tx.objectStore('recordings')
                        const selected = recordings.get(id)
                        selected.onsuccess = () =>
                          guard(() => {
                            checkAdmission()
                            const row = recording(selected.result, id)
                            const current = identity(row, id)
                            if (
                              current.producerIncarnation !== key.producerIncarnation ||
                              current.ownerEpoch !== key.ownerEpoch ||
                              row.ownerState !== 'sealed'
                            )
                              throw unavailable('clear-pending')
                            store.put({ recordingId: sequenceKey, version: 1, sequence: next })
                            recordings.put({ ...row, clearOperationId: `g${next}` })
                            done(next)
                          })
                      })
                  },
                  true
                )
                checkAdmission()
                return { key, observed, operationId }
              })()
            )
            let allocationTimer: ReturnType<typeof setTimeout> | undefined
            let allocationResult: Awaited<typeof allocation>
            try {
              allocationResult = await Promise.race([
                allocation,
                new Promise<never>((_resolve, reject) => {
                  allocationTimer = setTimeout(() => {
                    abandoned = true
                    reject(unavailable('clear-allocation-uncertain'))
                  }, 5000)
                })
              ])
            } finally {
              clearTimeout(allocationTimer)
            }
            if ('prior' in allocationResult && allocationResult.prior !== undefined)
              return { cleared: true, records: allocationResult.prior.records }
            const { key, observed, operationId } = allocationResult
            const receiptKey = `:ubm-clear-v4:${operationId.padStart(20, '0')}`
            const clearReceipt: WebClearReceipt = {
              recordingId: receiptKey,
              version: 3,
              targetRecordingId: id,
              operationId: `g${operationId}`,
              records: 0,
              producerIncarnation: key.producerIncarnation,
              ownerEpoch: key.ownerEpoch,
              generation: observed.generation
            }
            const work = track(
              withRow<{ readonly cleared: true; readonly records: number }>(
                id,
                (row, tx, done, guard) => {
                  const current = identity(row, id)
                  if (
                    current.producerIncarnation !== key.producerIncarnation ||
                    current.ownerEpoch !== key.ownerEpoch ||
                    row.clearOperationId !== clearReceipt.operationId ||
                    row.ownerState !== 'sealed' ||
                    row.recordCount !== 0 ||
                    row.pendingCount !== 0
                  )
                    throw unavailable('clear-pending')
                  const markerBytes = webRecordingMarkerLedgerBytes(row)
                  if (webRecordingMarkerDebt(row) !== 0) throw unavailable('clear-pending')
                  const terminal = webRecordingTerminalOutcome(row)
                  if (terminal?.outcome == null || terminal.hostCleanup?.state !== 'released')
                    throw unavailable('cleanup-evidence')
                  const authority = tx.objectStore('authorityReservations').get(id)
                  authority.onsuccess = () =>
                    guard(() => {
                      const reservation = object(authority.result)
                      const releaseReceipt = object(reservation.receipt)
                      const terminalReceipt = object(row.terminalOutcome)
                      const source =
                        reservation.recoveryReceipt === undefined
                          ? reservation
                          : object(object(reservation.recoveryReceipt).source)
                      if (
                        reservation.id !== id ||
                        reservation.phase !== 'released' ||
                        reservation.generation !== observed.generation ||
                        reservation.ownerEpoch !== key.ownerEpoch ||
                        releaseReceipt.phase !== 'released' ||
                        releaseReceipt.transition !== 'release' ||
                        source.generation !== terminalReceipt.generation ||
                        source.ownerToken !== terminalReceipt.ownerToken
                      )
                        throw unavailable('cleanup-evidence')
                      const request = tx.objectStore('readerReceipts').get(id)
                      request.onsuccess = () =>
                        guard(() => {
                          const state = checkedReceipt(
                            request.result,
                            id,
                            Number(row.readerCursor),
                            webRecordingReaderIntegrity(row)
                          )
                          if (state.pending !== null) throw unavailable('clear-pending')
                          const peers = tx.objectStore('readerReceipts')
                          const binding = peers.get(webRecordingPeerKey(id))
                          binding.onsuccess = () =>
                            guard(() => {
                              const peer = parseWebRecordingPeerRow(binding.result, webRecordingPeerKey(id))
                              if (
                                peer === null ||
                                peer.version !== 2 ||
                                peer.producerIncarnation !== key.producerIncarnation ||
                                peer.ownerEpoch !== key.ownerEpoch
                              )
                                throw unavailable('peer-cleanup-evidence')
                              const fenceKey = webRecordingOwnerFenceKey(id)
                              const fence = peers.get(fenceKey)
                              fence.onsuccess = () =>
                                guard(() => {
                                  if (fence.result === undefined) throw ownerFenceBlocked(true)
                                  const value = object(fence.result)
                                  if (
                                    Object.keys(value).length !== 5 ||
                                    value.recordingId !== fenceKey ||
                                    value.version !== 1 ||
                                    value.producerIncarnation !== key.producerIncarnation ||
                                    value.ownerEpoch !== key.ownerEpoch ||
                                    value.state !== 'released'
                                  )
                                    throw ownerFenceBlocked(false)
                                  const slot = peers.get(receiptKey)
                                  slot.onsuccess = () =>
                                    guard(() => {
                                      if (slot.result !== undefined) throw unavailable('clear-receipt-capacity')
                                      const continueClear = (): void => {
                                        const anchorKey = webRetainedMarkerKey(id, key.producerIncarnation)
                                        const anchor = peers.get(anchorKey)
                                        anchor.onsuccess = () =>
                                          guard(() => {
                                            if (anchor.result !== undefined)
                                              throw unavailable('marker-retention-conflict')
                                            const finishClearWrites = (): void => {
                                              if (markerBytes > 0)
                                                peers.add({
                                                  recordingId: anchorKey,
                                                  version: 1,
                                                  disposition: 'cleared-marker-evidence-retained',
                                                  markerLedgerBytes: markerBytes
                                                })
                                              peers.put(clearReceipt)
                                              if (attempt !== undefined)
                                                peers.add({
                                                  recordingId: webClearAttemptKey(attempt.attemptId),
                                                  version: 1,
                                                  targetRecordingId: id,
                                                  producerIncarnation: attempt.producerIncarnation,
                                                  attemptId: attempt.attemptId,
                                                  cleared: true,
                                                  records: 0
                                                })
                                              peers.delete(fenceKey)
                                              peers.delete(webRecordingPeerKey(id))
                                              peers.delete(id)
                                              tx.objectStore('recordings').delete(id)
                                              tx.objectStore('authorityReservations').delete(id)
                                              done({ cleared: true, records: 0 })
                                            }
                                            const finishClear = (): void => {
                                              if (attemptV2 === undefined) return finishClearWrites()
                                              commitWebClearProofInTransaction(tx, attemptV2, guard, finishClearWrites)
                                            }
                                            if (markerBytes === 0) return finishClear()
                                            // The scan includes legacy anchors, so an upgrade never
                                            // forgets existing evidence or requires a counter migration.
                                            const getAll: unknown = Reflect.get(peers, 'getAll')
                                            if (typeof getAll !== 'function') throw unavailable('marker-retention-scan')
                                            const scan: unknown = Reflect.apply(getAll, peers, [
                                              IDBKeyRange.bound(
                                                WEB_RETAINED_MARKER_PREFIX,
                                                `${WEB_RETAINED_MARKER_PREFIX}\uffff`
                                              ),
                                              WEB_RETAINED_MARKER_LIMIT + 1
                                            ])
                                            if (typeof scan !== 'object' || scan === null)
                                              throw bad('marker-retention-scan')
                                            Reflect.set(scan, 'onsuccess', () =>
                                              guard(() => {
                                                const values: unknown = Reflect.get(scan, 'result')
                                                if (
                                                  !Array.isArray(values) ||
                                                  values.length > WEB_RETAINED_MARKER_LIMIT + 1
                                                )
                                                  throw bad('marker-retention-scan')
                                                const retainedBytes = values.reduce(
                                                  (total: number, anchorValue: unknown) =>
                                                    total + parseWebRetainedMarkerAnchor(anchorValue).markerLedgerBytes,
                                                  0
                                                )
                                                if (
                                                  values.length >= WEB_RETAINED_MARKER_LIMIT ||
                                                  retainedBytes + markerBytes > WEB_RETAINED_MARKER_BYTES
                                                )
                                                  throw contractError(
                                                    'stream.quota',
                                                    'restoration',
                                                    'web-recording.marker-retention-capacity'
                                                  )
                                                finishClear()
                                              })
                                            )
                                          })
                                      }
                                      if (attempt === undefined) return continueClear()
                                      const attemptReceipt = peers.get(webClearAttemptKey(attempt.attemptId))
                                      attemptReceipt.onsuccess = () =>
                                        guard(() => {
                                          if (attemptReceipt.result !== undefined)
                                            throw unavailable('clear-attempt-conflict')
                                          continueClear()
                                        })
                                    })
                                })
                            })
                        })
                    })
                },
                true,
                clearDatabase,
                () => {
                  checkLock()
                  checkCancellation()
                }
              )
            )
            const bounded = async <T>(operation: Promise<T>): Promise<T> => {
              const trackedOperation = track(operation)
              let deadline: ReturnType<typeof setTimeout> | undefined
              try {
                return await Promise.race([
                  trackedOperation,
                  new Promise<never>((_resolve, reject) => {
                    deadline = setTimeout(() => reject(unavailable('clear-receipt-uncertain')), 5000)
                  })
                ])
              } finally {
                clearTimeout(deadline)
              }
            }
            let timer: ReturnType<typeof setTimeout> | undefined
            let result: { readonly cleared: true; readonly records: number }
            try {
              result = await Promise.race([
                work,
                new Promise<never>((_resolve, reject) => {
                  timer = setTimeout(() => reject(unavailable('clear-uncertain')), 5000)
                })
              ])
            } catch (error) {
              // This overlapping transaction is a barrier for the dispatched
              // clear, including when its completion notification was lost.
              const committed = await bounded(
                transact<boolean>(
                  clearDatabase,
                  (tx, done, guard) => {
                    const request = tx.objectStore('readerReceipts').get(receiptKey)
                    request.onsuccess = () =>
                      guard(() => {
                        const value = request.result
                        if (value === undefined) {
                          const retired = tx.objectStore('readerReceipts').get(webRetiredClearKey(clearReceipt))
                          retired.onsuccess = () =>
                            guard(() => {
                              if (retired.result === undefined) return done(false)
                              done(webRetiredClearMatches(retired.result, clearReceipt))
                            })
                          return
                        }
                        done(webRecordingCanonical(value) === webRecordingCanonical(clearReceipt))
                      })
                  },
                  true
                )
              )
              if (!committed) throw error
              result = { cleared: true, records: 0 }
            } finally {
              clearTimeout(timer)
            }
            checkLock()
            return result
          } catch (error) {
            primaryFailure = { error }
            throw error
          } finally {
            finished = true
            release()
            await observeRelease()
          }
        })
      return Object.freeze<WebContinuationRecordingController>({
        prepareOrdered,
        contextOrdered: (id: string) =>
          call(() =>
            withRow<OrderedRecordingContext>(id, (row, _tx, done) => done(parseOrderedRecordingContext(row.context)))
          ),
        prepare: (id: string, limits: ContinuationRecordingPrepareOptions) =>
          call(() => {
            bounds(limits)
            return withRow<ContinuationRecordingBatch>(id, () => {
              throw bad('wrong-kind')
            })
          }),
        acknowledge: (id: string, token: string) =>
          call(() => {
            if (!tokenCheck(token)) throw argument('token')
            return withRow<{
              readonly receipt: { readonly token: string; readonly acknowledged: true; readonly records: number }
              readonly identity: WebRecordingLiveIdentity
            }>(id, (row, tx, done, guard) => {
              const cursor = Number(row.readerCursor),
                count = Number(row.recordCount),
                byteCount = Number(row.byteCount)
              const integrity = webRecordingReaderIntegrity(row)
              const request = tx.objectStore('readerReceipts').get(id)
              request.onsuccess = () =>
                guard(() => {
                  const state = checkedReceipt(request.result, id, cursor, integrity)
                  if (state.lastAck?.token === token) {
                    done({
                      receipt: { token, acknowledged: true, records: state.lastAck.records },
                      identity: identity(row, id)
                    })
                    return
                  }
                  const pending = state.pending
                  if (!pending || pending.token !== token) throw argument('token')
                  const n = pending.last - pending.first + 1
                  if (n > count || n > MAX_RECORDING_BATCH_ITEMS || pending.bytes > byteCount)
                    throw bad('receipt-range')
                  readRows(
                    tx,
                    row,
                    id,
                    pending.first,
                    pending.last,
                    integrity,
                    guard,
                    (items, total, boundary) => {
                      if (
                        total !== pending.bytes ||
                        items.length !== n ||
                        JSON.stringify(items) !== pending.payload ||
                        (count === n) !== (byteCount === total)
                      )
                        throw bad('receipt-payload')
                      // The record is still present and has been validated. Bind any
                      // still-pending source identity to this exact ordinal before deletion.
                      const bind = (index: number): void => {
                        if (index < items.length) {
                          const item = items[index]
                          if (!item) throw bad('record')
                          const metadata = object(item.metadata)
                          if (typeof metadata.deliveryId !== 'string' || !integer(item.ordinal, 1))
                            throw bad('identity')
                          const ordinal = item.ordinal
                          const key = [
                            id,
                            String(row.producerIncarnation),
                            String(row.collectionId),
                            String(row.sourceGeneration),
                            metadata.deliveryId
                          ]
                          const identityRequest = tx.objectStore('pendingIdentities').get(key)
                          identityRequest.onsuccess = () =>
                            guard(() => {
                              const source =
                                identityRequest.result === undefined ? undefined : object(identityRequest.result)
                              const payload = webRecordingCanonical(item.record)
                              if (
                                !source ||
                                source.recordingId !== key[0] ||
                                source.producerIncarnation !== key[1] ||
                                source.collectionId !== key[2] ||
                                source.sourceGeneration !== key[3] ||
                                source.deliveryId !== key[4] ||
                                source.ordinal !== ordinal ||
                                source.payload !== payload ||
                                source.readerAcknowledged !== null
                              )
                                throw bad('identity-conflict')
                              const recordRequest = tx.objectStore('records').get([id, ordinal])
                              recordRequest.onsuccess = () =>
                                guard(() => {
                                  if (object(recordRequest.result).sourceRetired !== source.retired)
                                    throw bad('identity-missing')
                                  if (source.retired === true) {
                                    tx.objectStore('pendingIdentities').delete(key)
                                  } else if (source.retired === false) {
                                    source.readerAcknowledged = webRecordingAcknowledgedBinding(key, ordinal, payload)
                                    tx.objectStore('pendingIdentities').put(source)
                                  } else throw bad('identity-conflict')
                                  bind(index + 1)
                                })
                            })
                          return
                        }
                        for (let ordinal = pending.first; ordinal <= pending.last; ordinal++)
                          tx.objectStore('records').delete([id, ordinal])
                        row.readerCursor = pending.last
                        row.recordCount = count - n
                        row.byteCount = byteCount - total
                        state.pending = null
                        state.lastAck = { token, records: n }
                        integrity.accountedBytes -= total
                        integrity.deliveryId = boundary.deliveryId
                        integrity.sourceOrdinal = boundary.sourceOrdinal
                        integrity.receipt = JSON.stringify(state)
                        row.readerIntegrity = integrity
                        tx.objectStore('recordings').put(row)
                        tx.objectStore('readerReceipts').put(state)
                        done({ receipt: { token, acknowledged: true, records: n }, identity: identity(row, id) })
                      }
                      bind(0)
                    },
                    pending.bytes,
                    true
                  )
                })
            }).then(async result => {
              const owners = options.liveOwners
              if (owners?.has(result.identity))
                await ownerCall('space-available', () => owners.spaceAvailable(result.identity), true)
              return result.receipt
            })
          }),
        status: (id: string) =>
          call<ContinuationRecordingStatus>(async () => {
            const snapshot = await withRow<{
              identity: WebRecordingLiveIdentity
              state: unknown
              status: ContinuationRecordingStatus | null
              records: number
              bytes: number
            }>(id, (row, _tx, done) => {
              const retained = webRecordingTerminalOutcome(row)
              done({
                identity: identity(row, id),
                state: row.ownerState,
                status: retained?.status ?? null,
                records: Number(row.recordCount),
                bytes: Number(row.byteCount)
              })
            })
            if (options.liveOwners?.has(snapshot.identity))
              return options.liveOwners.use(snapshot.identity, owner =>
                ownerCall('status-owner', async () => owner.status())
              )
            if (snapshot.state !== 'sealed' || snapshot.status === null || snapshot.status.orderedOwner === undefined)
              throw unavailable('status-owner')
            return {
              ...snapshot.status,
              phase: 'stopped',
              accepting: false,
              records: snapshot.records,
              bytes: snapshot.bytes,
              orderedOwner: { ...snapshot.status.orderedOwner, state: 'sealed', uncertainTail: null }
            }
          }),
        runtimeDiagnostic: (id: string) =>
          call(async () => {
            idCheck(id)
            const owners = options.liveOwners
            if (owners === undefined) throw unavailable('runtime-diagnostic')
            return owners.useByRecordingId(id, async owner => {
              if (owner.runtimeDiagnostic === undefined) throw unavailable('runtime-diagnostic')
              return owner.runtimeDiagnostic()
            })
          }),
        stop: (id: string) =>
          call(() =>
            withRow<ContinuationRecordingStatus & { readonly radioRelease: 'not-requested' }>(id, () => {
              throw unavailable('stop-owner')
            })
          ),
        stopOrdered: (id: string) =>
          call<OrderedRecordingHostStopResult>(async () => {
            const snapshot = await withRow<{
              readonly identity: WebRecordingLiveIdentity
              readonly state: unknown
              readonly failure: unknown
              readonly version: 1 | 2
              readonly outcome: ReturnType<typeof webRecordingTerminalOutcome>
            }>(id, (row, _tx, done) =>
              done({
                identity: identity(row, id),
                state: row.ownerState,
                failure: row.failure,
                version: parseOrderedRecordingContext(row.context).version,
                outcome: webRecordingTerminalOutcome(row)
              })
            )
            if (options.liveOwners?.has(snapshot.identity)) {
              const owners = options.liveOwners
              const observed = await ownerCall('stop-owner', () =>
                owners.stopOrdered(snapshot.identity, snapshot.version)
              )
              return {
                recording: parseOrderedRecordingStopReceipt(observed.recording, snapshot.version),
                hostCleanup: parseOrderedRecordingHostCleanup(observed.hostCleanup)
              }
            }
            if (snapshot.state === 'sealed' && snapshot.outcome?.outcome != null)
              return {
                recording: snapshot.outcome.outcome,
                hostCleanup: snapshot.outcome.hostCleanup ?? { state: 'uncertain' }
              }
            // The persisted failure does not identify whether it was a source
            // or storage failure, as required by the stop receipt. Do not turn
            // it into a successful outcome or guess its provenance.
            if (snapshot.failure !== null) throw unavailable('persisted-failure')
            // Sealing proves a terminal was committed, not why the source
            // ended. The terminal may already have been acknowledged and
            // deleted, and the row does not retain its outcome. Only a live
            // owner or a durable exact outcome receipt can report success.
            if (snapshot.state === 'sealed') throw unavailable('terminal-outcome')
            if (snapshot.state === 'interrupted')
              return {
                recording: { version: snapshot.version, state: 'interrupted', error: null },
                hostCleanup: { state: 'uncertain' }
              }
            throw unavailable('stop-owner')
          }),
        clear: id => clear(id),
        admitClearAttemptV2: input => {
          const snapshot = { ...input }
          return call(() => {
            const name: unknown = Reflect.get(database, 'name')
            if (
              options.journalIdentity !== undefined &&
              (typeof options.journalIdentity !== 'string' ||
                (typeof name === 'string' && name !== options.journalIdentity))
            )
              throw argument('journal-identity')
            const partition = options.journalIdentity ?? (typeof name === 'string' ? name : 'ubm-ordered-recordings')
            return boundedWebClearAdmission(
              partition,
              options.locks ?? (typeof navigator === 'undefined' ? undefined : navigator.locks),
              database,
              options.clearAdmissionLimit ?? 2,
              (own, validate, guardedDatabase) =>
                createWebClearProofLedger(guardedDatabase, randomToken, validate).admitRetryable(snapshot, own)
            )
          })
        },
        inspectClearAttemptV2: attempt => {
          const snapshot = { ...attempt }
          return call(() => createWebClearProofLedger(database, randomToken).inspect(snapshot, true))
        },
        cancelClearAttemptV2: attempt => {
          const snapshot = { ...attempt }
          return call(() => createWebClearProofLedger(database, randomToken).cancel(snapshot, true))
        },
        clearProofUsageV2: () => call(() => createWebClearProofLedger(database, randomToken).usage()),
        configureClearProofV2: policy => {
          const snapshot = { ...policy }
          return call(() => createWebClearProofLedger(database, randomToken).configure(snapshot))
        },
        commitClearAttemptV2: (attempt, clearOptions) => {
          const snapshot = { ...attempt }
          const signal = clearOptions?.signal
          return rehydratePublicPromise(
            Promise.resolve().then(() => {
              const checked = checkedWebProofAdmission(snapshot)
              return clear(checked.recordingId, undefined, signal, checked)
            })
          )
        },
        retireClearAttemptV2: retireOptions => {
          const supplied = retireOptions.outcome
          const snapshot = {
            ...retireOptions,
            outcome:
              supplied.state === 'committed'
                ? { ...supplied, attempt: { ...supplied.attempt }, result: { ...supplied.result } }
                : { ...supplied, attempt: { ...supplied.attempt } }
          }
          return call(async () => {
            let lost = false
            const databaseName: unknown = Reflect.get(database, 'name')
            if (
              options.journalIdentity !== undefined &&
              (typeof options.journalIdentity !== 'string' ||
                (typeof databaseName === 'string' && options.journalIdentity !== databaseName))
            )
              throw argument('journal-identity')
            const lease = await acquireWebRecordingClearLock(
              options.locks ?? (typeof navigator === 'undefined' ? undefined : navigator.locks),
              () => {
                lost = true
              },
              options.journalIdentity ?? (typeof databaseName === 'string' ? databaseName : 'ubm-ordered-recordings')
            )
            const check = (): void => {
              if (lost) throw unavailable('clear-lock-lost')
            }
            let failure: { readonly error: unknown } | undefined
            let result: { readonly state: 'retired' | 'retired-or-unknown' } | undefined
            let finished = false
            let pending = true
            let released = false
            const release = (): void => {
              if (finished && !pending && !released) {
                released = true
                lease()
              }
            }
            const work = Promise.resolve().then(() =>
              createWebClearProofLedger(database, randomToken, check).retire(snapshot, true)
            )
            work.then(
              () => {
                pending = false
                release()
              },
              () => {
                pending = false
                release()
              }
            )
            let workTimer: ReturnType<typeof setTimeout> | undefined
            try {
              result = await Promise.race([
                work,
                new Promise<never>((_resolve, reject) => {
                  workTimer = setTimeout(() => reject(unavailable('clear-retirement-uncertain')), 5000)
                })
              ])
              check()
            } catch (error) {
              failure = { error }
            } finally {
              clearTimeout(workTimer)
              finished = true
              release()
            }
            if (!released) {
              if (failure !== undefined) throw failure.error
              throw bad('incomplete-retirement')
            }
            let timer: ReturnType<typeof setTimeout> | undefined
            try {
              const ended = await Promise.race([
                lease.ended,
                new Promise<never>((_resolve, reject) => {
                  timer = setTimeout(() => reject(unavailable('clear-lock-release-uncertain')), 5000)
                })
              ])
              if (ended.state === 'lost') throw unavailable('clear-lock-release-uncertain')
            } catch (error) {
              if (failure !== undefined)
                throw clearReleaseError([failure.error, error], 'Retirement and lock release both failed')
              throw error
            } finally {
              clearTimeout(timer)
            }
            if (failure !== undefined) throw failure.error
            if (result === undefined) throw bad('incomplete-retirement')
            return result
          })
        },
        prepareClearAttemptV1: id =>
          call(() =>
            withRow<WebClearAttemptV1>(id, (row, _tx, done) => {
              const attemptId = randomToken()
              done(
                checkedWebClearAttempt({
                  version: 1,
                  recordingId: id,
                  producerIncarnation: String(row.producerIncarnation),
                  attemptId
                })
              )
            })
          ),
        commitClearAttemptV1: (attempt, clearOptions) =>
          rehydratePublicPromise(
            Promise.resolve().then(() => {
              const checked = checkedWebClearAttempt(attempt)
              return clear(checked.recordingId, checked, clearOptions?.signal)
            })
          ),
        clearAttemptV1: (attempt, clearOptions) =>
          rehydratePublicPromise(
            Promise.resolve().then(() => {
              const checked = checkedWebClearAttempt(attempt)
              return clear(checked.recordingId, checked, clearOptions?.signal)
            })
          )
      })
    })()
  )
}
