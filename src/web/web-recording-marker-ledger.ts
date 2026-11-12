import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { BackendContractError, contractError } from '../backend-contract/errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { utf8ByteLength } from '../backend-contract/serializable'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import {
  webRecordingCanonical,
  webRecordingReaderIntegrity,
  webRecordingMarkerLedgerBytes,
  webRecordingControlReservation,
  webRecordingMarkerDebt,
  webRecordingAcknowledgedBinding
} from './web-ordered-recording-journal'
import type { WebOrderedJournalDatabase, WebOrderedJournalTransaction } from './web-ordered-recording-journal'
import type { WebAuthorityOwner } from './web-authority-reservation'
import { webRecordingControlBudget } from './web-recording-control-budget'

type Row = Record<string, unknown>
type Guard = (action: () => void) => void
const STORES = [
  'recordings',
  'authorityReservations',
  'markerLedger',
  'markerOperations',
  'pendingIdentities',
  'records'
]
const error = (detail: string): Error =>
  contractError('lifecycle.invariant-violation', 'restoration', `web-recording.marker-ledger-${detail}`)
const quota = (): Error => contractError('stream.quota', 'restoration', 'web-recording.marker-ledger-capacity')
const aborted = new WeakSet<object>()
export function isDefinitiveWebMarkerAbort(cause: unknown): boolean {
  return typeof cause === 'object' && cause !== null && aborted.has(cause)
}
const token = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value)
const opaque = (value: unknown): value is string => typeof value === 'string' && value.length > 0 && value.length <= 256
const ordinal = (value: unknown): value is string => isCanonicalIngressOrdinal(value)
const object = (value: unknown): Row => {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw error('row')
  return Object.fromEntries(Object.entries(value))
}
const safe = (value: unknown, min: number, max: number): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= min && value <= max

export interface WebMarkerLedgerRequest {
  readonly markerId: string
  readonly target: Readonly<Record<string, unknown>>
  readonly mode: 'with-response' | 'without-response'
  readonly bytes: Readonly<Uint8Array>
}
export type WebMarkerLedgerAdmission =
  | { readonly state: 'admitted'; readonly ordinal: string }
  | { readonly state: 'not-admitted'; readonly evidence: string }
  | { readonly state: 'rejected'; readonly evidence: string; readonly delivery: 'none' | 'unknown' }
export type WebMarkerLedgerSettlement =
  | { readonly state: 'not-started'; readonly evidence: string }
  | { readonly state: 'settled'; readonly outcome: 'success' | 'failure'; readonly evidence?: string }
export interface WebMarkerLedgerSourceAck {
  readonly deliveryId: string
  readonly journalOrdinal: number
}
export interface WebMarkerLedgerSnapshot {
  readonly version: 1
  readonly recordingId: string
  readonly producerIncarnation: string
  readonly markerId: string
  readonly request: {
    readonly target: Row
    readonly mode: 'with-response' | 'without-response'
    readonly bytes: readonly number[]
  }
  readonly admission: WebMarkerLedgerAdmission | null
  readonly dispatchIntent: boolean
  readonly settlement: WebMarkerLedgerSettlement | null
  readonly sourceAck: WebMarkerLedgerSourceAck | null
  readonly retired: boolean
}

async function transaction<T>(
  db: WebOrderedJournalDatabase,
  mode: 'readonly' | 'readwrite',
  action: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: Guard) => void
): Promise<T> {
  let result: { value: T } | undefined
  let problem: unknown
  let abortObserved = false
  try {
    await runWebIndexedDbTransaction(
      db,
      STORES,
      mode,
      tx => {
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
      },
      () => {
        abortObserved = true
      },
      () => {
        abortObserved = true
      }
    )
  } catch (cause) {
    if (abortObserved) {
      const original = problem ?? cause
      const failure =
        original instanceof BackendContractError
          ? new BackendContractError(original.normalized)
          : new Error(original instanceof Error ? original.message : String(original), { cause: original })
      if (original instanceof Error && !(original instanceof BackendContractError)) failure.name = original.name
      aborted.add(failure)
      throw failure
    }
    throw problem ?? cause
  }
  if (problem !== undefined) throw problem
  if (result === undefined) throw error('incomplete')
  return result.value
}

function checkedText(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && utf8ByteLength(JSON.stringify(value)) <= 2048
}
function copyRequest(input: WebMarkerLedgerRequest): WebMarkerLedgerSnapshot['request'] {
  if (
    !ordinal(input.markerId) ||
    (input.mode !== 'with-response' && input.mode !== 'without-response') ||
    !(input.bytes instanceof Uint8Array) ||
    input.bytes.length > 512 * 1024
  )
    throw error('request')
  const target = object(input.target)
  const encoded = webRecordingCanonical(target)
  if (utf8ByteLength(encoded) > 4096) throw error('target')
  return { target: object(JSON.parse(encoded)), mode: input.mode, bytes: Array.from(input.bytes) }
}
function checkedAdmission(value: WebMarkerLedgerAdmission): WebMarkerLedgerAdmission {
  if (value.state === 'admitted' && ordinal(value.ordinal)) return { state: 'admitted', ordinal: value.ordinal }
  if (value.state === 'not-admitted' && checkedText(value.evidence))
    return { state: value.state, evidence: value.evidence }
  if (
    value.state === 'rejected' &&
    checkedText(value.evidence) &&
    (value.delivery === 'none' || value.delivery === 'unknown')
  )
    return { state: value.state, evidence: value.evidence, delivery: value.delivery }
  throw error('admission')
}
function checkedSettlement(value: WebMarkerLedgerSettlement): WebMarkerLedgerSettlement {
  if (value.state === 'not-started' && checkedText(value.evidence))
    return { state: value.state, evidence: value.evidence }
  if (value.state === 'settled' && value.outcome === 'success' && value.evidence === undefined)
    return { state: value.state, outcome: value.outcome }
  if (value.state === 'settled' && value.outcome === 'failure' && checkedText(value.evidence))
    return { state: value.state, outcome: value.outcome, evidence: value.evidence }
  throw error('settlement')
}

export function webMarkerLedgerSnapshot(
  value: unknown,
  owner: WebAuthorityOwner,
  incarnation: string
): WebMarkerLedgerSnapshot {
  const row = object(value)
  const bound = object(row.owner)
  const request = object(row.request)
  const bytes = request.bytes
  if (
    row.version !== 1 ||
    row.recordingId !== owner.id ||
    row.producerIncarnation !== incarnation ||
    !ordinal(row.markerId) ||
    bound.id !== owner.id ||
    bound.generation !== owner.generation ||
    bound.ownerToken !== owner.ownerToken ||
    bound.ownerEpoch !== owner.ownerEpoch ||
    (request.mode !== 'with-response' && request.mode !== 'without-response') ||
    !Array.isArray(bytes) ||
    bytes.length > 512 * 1024 ||
    !Array.from(bytes).every(byte => safe(byte, 0, 255)) ||
    typeof row.dispatchIntent !== 'boolean' ||
    typeof row.retired !== 'boolean'
  )
    throw error('corrupt-marker')
  const target = object(request.target)
  if (utf8ByteLength(webRecordingCanonical(target)) > 4096) throw error('corrupt-marker')
  let admission: WebMarkerLedgerAdmission | null = null
  if (row.admission !== null) {
    const item = object(row.admission)
    if (item.state === 'admitted' && ordinal(item.ordinal)) admission = { state: item.state, ordinal: item.ordinal }
    else if (item.state === 'not-admitted' && checkedText(item.evidence))
      admission = { state: item.state, evidence: item.evidence }
    else if (
      item.state === 'rejected' &&
      checkedText(item.evidence) &&
      (item.delivery === 'none' || item.delivery === 'unknown')
    )
      admission = { state: item.state, evidence: item.evidence, delivery: item.delivery }
    else throw error('corrupt-marker')
  }
  let settlement: WebMarkerLedgerSettlement | null = null
  if (row.settlement !== null) {
    const item = object(row.settlement)
    if (item.state === 'not-started' && checkedText(item.evidence))
      settlement = { state: item.state, evidence: item.evidence }
    else if (item.state === 'settled' && item.outcome === 'success' && item.evidence === undefined)
      settlement = { state: item.state, outcome: item.outcome }
    else if (item.state === 'settled' && item.outcome === 'failure' && checkedText(item.evidence))
      settlement = { state: item.state, outcome: item.outcome, evidence: item.evidence }
    else throw error('corrupt-marker')
  }
  let sourceAck: WebMarkerLedgerSourceAck | null = null
  if (row.sourceAck !== null) {
    const ack = object(row.sourceAck)
    if (!ordinal(ack.deliveryId) || ack.deliveryId === '0' || !safe(ack.journalOrdinal, 1, Number.MAX_SAFE_INTEGER))
      throw error('corrupt-marker')
    sourceAck = { deliveryId: ack.deliveryId, journalOrdinal: ack.journalOrdinal }
  }
  if (
    (row.dispatchIntent && admission?.state !== 'admitted') ||
    (settlement !== null &&
      (admission?.state !== 'admitted' || (settlement.state === 'settled' && !row.dispatchIntent))) ||
    (sourceAck !== null && admission?.state !== 'admitted') ||
    (row.retired &&
      !(
        admission?.state === 'not-admitted' ||
        (admission?.state === 'rejected' && admission.delivery === 'none') ||
        (admission?.state === 'admitted' && settlement !== null && sourceAck !== null)
      ))
  )
    throw error('corrupt-marker')
  return {
    version: 1,
    recordingId: owner.id,
    producerIncarnation: incarnation,
    markerId: row.markerId,
    request: { target, mode: request.mode, bytes: [...bytes] },
    admission,
    dispatchIntent: row.dispatchIntent,
    settlement,
    sourceAck,
    retired: row.retired
  }
}

/** Internal foundation only: no marker dispatch is authorized by this API. */
export function createWebRecordingMarkerLedger(
  db: WebOrderedJournalDatabase,
  inputOwner: WebAuthorityOwner,
  incarnation: string
) {
  const owner = { ...inputOwner }
  const signatures = new Map<string, string>()
  if (
    !isRecordingId(owner.id) ||
    !ordinal(owner.generation) ||
    owner.generation === '0' ||
    !ordinal(owner.ownerEpoch) ||
    owner.ownerEpoch === '0' ||
    !token(owner.ownerToken) ||
    !opaque(incarnation)
  )
    throw error('owner')
  const markerKey = (id: string): string[] => [owner.id, incarnation, id]
  const operationKey = (id: string): string[] => [owner.id, incarnation, id]
  const load = (
    tx: WebOrderedJournalTransaction,
    guard: Guard,
    cleanupAllowed: boolean,
    fn: (row: Row, phase: unknown) => void
  ): void => {
    const recording = tx.objectStore('recordings').get(owner.id)
    recording.onsuccess = () =>
      guard(() => {
        const row = object(recording.result)
        const reservation = tx.objectStore('authorityReservations').get(owner.id)
        reservation.onsuccess = () =>
          guard(() => {
            const authority = object(reservation.result)
            const binding = webRecordingCanonical([
              row.id,
              row.maxBytes,
              row.maxRecords,
              row.context,
              row.producerIncarnation,
              row.collectionId,
              row.sourceGeneration,
              row.memberEpochs
            ])
            const receipt = object(authority.receipt)
            if (
              row.version !== 1 ||
              row.kind !== 'ordered' ||
              row.id !== owner.id ||
              row.producerIncarnation !== incarnation ||
              row.ownerEpoch !== owner.ownerEpoch ||
              row.ownerState !== 'live' ||
              authority.version !== 1 ||
              authority.id !== owner.id ||
              authority.generation !== owner.generation ||
              authority.ownerToken !== owner.ownerToken ||
              authority.ownerEpoch !== owner.ownerEpoch ||
              !(
                (authority.phase === 'armed' && receipt.phase === 'armed' && receipt.transition === 'arm') ||
                (cleanupAllowed &&
                  authority.phase === 'cleanup-pending' &&
                  receipt.phase === 'cleanup-pending' &&
                  receipt.transition === 'cleanup')
              ) ||
              authority.declaration !== binding ||
              !token(receipt.operationId)
            )
              throw error('owner')
            webRecordingReaderIntegrity(row)
            webRecordingMarkerLedgerBytes(row)
            fn(row, authority.phase)
          })
      })
  }
  const apply = (
    operationId: string,
    markerId: string,
    kind: string,
    data: unknown,
    mutate: (
      row: Row,
      marker: Row | undefined,
      tx: WebOrderedJournalTransaction,
      done: (snapshot: Row) => void,
      guard: Guard
    ) => void
  ): Promise<WebMarkerLedgerSnapshot> => {
    if (!token(operationId) || !ordinal(markerId)) return Promise.reject(error('identity'))
    const signature = webRecordingCanonical({ kind, markerId, data, owner, incarnation })
    const previousSignature = signatures.get(operationId)
    if (previousSignature !== undefined && previousSignature !== signature)
      return Promise.reject(error('operation-conflict'))
    signatures.set(operationId, signature)
    return transaction(db, 'readwrite', (tx, done, guard) =>
      load(tx, guard, true, (row, phase) => {
        const operations = tx.objectStore('markerOperations')
        const previous = operations.get(operationKey(operationId))
        previous.onsuccess = () =>
          guard(() => {
            const markers = tx.objectStore('markerLedger')
            const current = markers.get(markerKey(markerId))
            current.onsuccess = () =>
              guard(() => {
                const marker = current.result === undefined ? undefined : object(current.result)
                if (previous.result !== undefined) {
                  const prior = object(previous.result)
                  if (prior.version !== 1 || prior.signature !== signature || marker === undefined)
                    throw error('operation-conflict')
                  webMarkerLedgerSnapshot(marker, owner, incarnation)
                  const retained = object(prior.result)
                  done(webMarkerLedgerSnapshot({ ...retained, request: marker.request, owner }, owner, incarnation))
                  return
                }
                if (phase === 'cleanup-pending' && (kind === 'reserve' || kind === 'dispatch'))
                  throw error('cleanup-state')
                if (row.terminal === true && (kind === 'reserve' || kind === 'dispatch')) throw error('terminal-state')
                if (marker !== undefined) webMarkerLedgerSnapshot(marker, owner, incarnation)
                mutate(
                  row,
                  marker,
                  tx,
                  next => {
                    const result = webMarkerLedgerSnapshot(next, owner, incarnation)
                    const { request: _request, ...compact } = result
                    markers.put(next)
                    operations.add({
                      recordingId: owner.id,
                      producerIncarnation: incarnation,
                      operationId,
                      version: 1,
                      signature,
                      result: compact,
                      owner
                    })
                    done(result)
                  },
                  guard
                )
              })
          })
      })
    )
  }
  return {
    identity: { recordingId: owner.id, producerIncarnation: incarnation },
    releaseSignatures(operationIds: readonly string[]): void {
      for (const operationId of operationIds) signatures.delete(operationId)
    },
    reserve(operationId: string, input: WebMarkerLedgerRequest): Promise<WebMarkerLedgerSnapshot> {
      const request = copyRequest(input)
      const markerId = input.markerId
      return apply(operationId, markerId, 'reserve', request, (row, marker, tx, done) => {
        if (marker !== undefined) throw error('marker-exists')
        // Both the ledger row and the reserve operation preserve exact request bytes.
        // 64 KiB reserves the remaining bounded transition receipts and row metadata.
        const charge = utf8ByteLength(webRecordingCanonical(request)) * 2 + 65536
        const used = webRecordingMarkerLedgerBytes(row)
        const debt = webRecordingMarkerDebt(row)
        const control = webRecordingControlReservation(row)
        const budget = webRecordingControlBudget(row)
        if (control.marker !== 0 && control.unit !== budget.marker) throw error('marker-control-bound')
        const reserved = Math.max(control.total - control.marker, budget.terminal) + control.marker + budget.marker
        if (debt === Number.MAX_SAFE_INTEGER) throw quota()
        if (
          charge + reserved > Number(row.maxBytes) - used - Number(row.byteCount) ||
          charge + reserved > Number(row.maxBytes) - used - Number(row.pendingBytes)
        )
          throw quota()
        row.markerLedgerBytes = used + charge
        row.markerDebt = debt + 1
        row.markerControlBytes = reserved
        row.markerControlMarkerBytes = control.marker + budget.marker
        row.markerControlUnitBytes = budget.marker
        tx.objectStore('recordings').put(row)
        done({
          version: 1,
          recordingId: owner.id,
          producerIncarnation: incarnation,
          markerId,
          owner,
          request,
          admission: null,
          dispatchIntent: false,
          settlement: null,
          sourceAck: null,
          retired: false
        })
      })
    },
    admit(operationId: string, markerId: string, input: WebMarkerLedgerAdmission): Promise<WebMarkerLedgerSnapshot> {
      const admission = checkedAdmission(input)
      return apply(operationId, markerId, 'admit', admission, (row, marker, tx, done) => {
        if (marker === undefined || marker.admission !== null || marker.retired !== false)
          throw error('admission-state')
        marker.admission = admission
        if (admission.state === 'not-admitted' || (admission.state === 'rejected' && admission.delivery === 'none')) {
          const debt = webRecordingMarkerDebt(row)
          if (debt === 0) throw error('debt-state')
          row.markerDebt = debt - 1
          const control = webRecordingControlReservation(row)
          if (control.marker < control.unit || control.unit === 0) throw error('marker-control-state')
          row.markerControlBytes = control.total - control.unit
          row.markerControlMarkerBytes = control.marker - control.unit
          tx.objectStore('recordings').put(row)
        }
        done(marker)
      })
    },
    dispatchIntent(operationId: string, markerId: string): Promise<WebMarkerLedgerSnapshot> {
      return apply(operationId, markerId, 'dispatch', null, (_row, marker, _tx, done) => {
        if (
          marker === undefined ||
          object(marker.admission).state !== 'admitted' ||
          marker.dispatchIntent !== false ||
          marker.settlement !== null ||
          marker.retired !== false
        )
          throw error('dispatch-state')
        marker.dispatchIntent = true
        done(marker)
      })
    },
    settle(operationId: string, markerId: string, input: WebMarkerLedgerSettlement): Promise<WebMarkerLedgerSnapshot> {
      const settlement = checkedSettlement(input)
      return apply(operationId, markerId, 'settle', settlement, (row, marker, tx, done) => {
        if (
          marker === undefined ||
          object(marker.admission).state !== 'admitted' ||
          marker.settlement !== null ||
          marker.retired !== false ||
          (settlement.state === 'settled' && marker.dispatchIntent !== true)
        )
          throw error('settlement-state')
        marker.settlement = settlement
        if (marker.sourceAck !== null) {
          const debt = webRecordingMarkerDebt(row)
          if (debt === 0) throw error('debt-state')
          row.markerDebt = debt - 1
          tx.objectStore('recordings').put(row)
        }
        done(marker)
      })
    },
    /** Call only after the source confirms acknowledgement; the transaction also verifies its exact journal identity. */
    acknowledge(
      operationId: string,
      markerId: string,
      input: WebMarkerLedgerSourceAck
    ): Promise<WebMarkerLedgerSnapshot> {
      const ack = { deliveryId: input.deliveryId, journalOrdinal: input.journalOrdinal }
      if (!ordinal(ack.deliveryId) || ack.deliveryId === '0' || !safe(ack.journalOrdinal, 1, Number.MAX_SAFE_INTEGER))
        return Promise.reject(error('ack'))
      return apply(operationId, markerId, 'ack', ack, (row, marker, tx, done, guard) => {
        if (marker === undefined || marker.sourceAck !== null || marker.retired !== false) throw error('ack-state')
        const admission = object(marker.admission)
        if (admission.state !== 'admitted') throw error('ack-state')
        const pending = tx
          .objectStore('pendingIdentities')
          .get([owner.id, incarnation, String(row.collectionId), String(row.sourceGeneration), ack.deliveryId])
        pending.onsuccess = () =>
          guard(() => {
            const identity = object(pending.result)
            if (
              identity.recordingId !== owner.id ||
              identity.producerIncarnation !== incarnation ||
              identity.collectionId !== row.collectionId ||
              identity.sourceGeneration !== row.sourceGeneration ||
              identity.deliveryId !== ack.deliveryId ||
              identity.ordinal !== ack.journalOrdinal ||
              identity.retired !== false ||
              identity.payload !== webRecordingCanonical({ kind: 'marker', markerId, ordinal: admission.ordinal })
            )
              throw error('ack-identity')
            const context = object(row.context)
            if (
              (context.version !== 1 && context.version !== 2) ||
              typeof context.peerId !== 'string' ||
              typeof row.collectionId !== 'string' ||
              typeof row.sourceGeneration !== 'string' ||
              !safe(row.readerCursor, 0, Number.MAX_SAFE_INTEGER)
            )
              throw error('ack-journal')
            const wire = { kind: 'marker', markerId, ordinal: admission.ordinal }
            const metadata = {
              kind: 'ordered',
              version: context.version,
              recordingId: owner.id,
              peerId: context.peerId,
              collectionId: row.collectionId,
              sourceGeneration: { kind: 'opaque', value: row.sourceGeneration },
              deliveryId: ack.deliveryId
            }
            const data = { ordinal: ack.journalOrdinal, metadata, record: wire }
            const byteLength = utf8ByteLength(JSON.stringify(data))
            if (identity.byteLength !== byteLength) throw error('ack-identity')
            const finish = (): void => {
              marker.sourceAck = ack
              if (marker.settlement !== null) {
                const debt = webRecordingMarkerDebt(row)
                if (debt === 0) throw error('debt-state')
                row.markerDebt = debt - 1
                tx.objectStore('recordings').put(row)
              }
              done(marker)
            }
            if (ack.journalOrdinal <= row.readerCursor) {
              if (
                identity.readerAcknowledged !==
                webRecordingAcknowledgedBinding(
                  [owner.id, incarnation, row.collectionId, row.sourceGeneration, ack.deliveryId],
                  ack.journalOrdinal,
                  String(identity.payload)
                )
              )
                throw error('ack-reader')
              finish()
            } else {
              if (identity.readerAcknowledged !== null) throw error('ack-reader')
              const record = tx.objectStore('records').get([owner.id, ack.journalOrdinal])
              record.onsuccess = () =>
                guard(() => {
                  if (
                    webRecordingCanonical(record.result) !==
                    webRecordingCanonical({
                      recordingId: owner.id,
                      ...data,
                      byteLength,
                      version: 1,
                      sourceRetired: false
                    })
                  )
                    throw error('ack-record')
                  finish()
                })
            }
          })
      })
    },
    retire(operationId: string, markerId: string): Promise<WebMarkerLedgerSnapshot> {
      return apply(operationId, markerId, 'retire', null, (_row, marker, _tx, done) => {
        if (marker === undefined || marker.retired !== false) throw error('retire-state')
        const admission = object(marker.admission)
        if (admission.state === 'admitted' && (marker.sourceAck === null || marker.settlement === null))
          throw error('retire-unresolved')
        if (
          admission.state !== 'admitted' &&
          admission.state !== 'not-admitted' &&
          !(admission.state === 'rejected' && admission.delivery === 'none')
        )
          throw error('retire-unresolved')
        marker.retired = true
        done(marker)
      })
    },
    inspect(markerId: string): Promise<WebMarkerLedgerSnapshot | null> {
      if (!ordinal(markerId)) return Promise.reject(error('identity'))
      return transaction(db, 'readonly', (tx, done, guard) => {
        const request = tx.objectStore('markerLedger').get(markerKey(markerId))
        request.onsuccess = () =>
          guard(() =>
            done(request.result === undefined ? null : webMarkerLedgerSnapshot(request.result, owner, incarnation))
          )
      })
    },
    reconcile(
      operationId: string
    ): Promise<
      { readonly outcome: 'committed'; readonly result: WebMarkerLedgerSnapshot } | { readonly outcome: 'unknown' }
    > {
      if (!token(operationId)) return Promise.reject(error('identity'))
      return transaction(db, 'readonly', (tx, done, guard) => {
        const request = tx.objectStore('markerOperations').get(operationKey(operationId))
        request.onsuccess = () =>
          guard(() => {
            if (request.result === undefined) return done({ outcome: 'unknown' })
            const receipt = object(request.result)
            const bound = object(receipt.owner)
            const result = object(receipt.result)
            if (
              receipt.version !== 1 ||
              receipt.recordingId !== owner.id ||
              receipt.producerIncarnation !== incarnation ||
              receipt.operationId !== operationId ||
              (signatures.has(operationId) && receipt.signature !== signatures.get(operationId)) ||
              bound.id !== owner.id ||
              bound.generation !== owner.generation ||
              bound.ownerToken !== owner.ownerToken ||
              bound.ownerEpoch !== owner.ownerEpoch ||
              !ordinal(result.markerId)
            )
              throw error('receipt')
            const marker = tx.objectStore('markerLedger').get(markerKey(result.markerId))
            marker.onsuccess = () =>
              guard(() => {
                const current = object(marker.result)
                webMarkerLedgerSnapshot(current, owner, incarnation)
                done({
                  outcome: 'committed',
                  result: webMarkerLedgerSnapshot({ ...result, request: current.request, owner }, owner, incarnation)
                })
              })
          })
      })
    }
  }
}
