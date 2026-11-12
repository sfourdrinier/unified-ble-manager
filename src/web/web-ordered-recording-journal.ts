import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { webRecordingContractError as contractError } from './web-recording-errors'
import {
  isRecordingId,
  MAX_RECORDING_BATCH_BYTES,
  MAX_RECORDING_BYTES,
  MAX_RECORDING_RECORDS,
  MIN_RECORDING_BYTES,
  MIN_RECORDING_RECORDS
} from '../backend-contract/continuation-recording-bounds'
import { utf8ByteLength } from '../backend-contract/serializable'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import {
  parseOrderedRecordingBatch,
  parseOrderedRecordingContext,
  parseOrderedRecordingFailure
} from '../core/ordered-recording'
import type {
  OrderedRecordingContext,
  OrderedRecordingFailure,
  OrderedRecordingRecord
} from '../core/ordered-recording'
import { parseOrderedRecordingHostCleanup, parseOrderedRecordingStopReceipt } from '../core/continuation-recording'
import type {
  ContinuationRecordingFailure,
  ContinuationRecordingStatus,
  OrderedRecordingStopReceipt
} from '../core/continuation-recording'
import type { OrderedRecordingHostStopResult } from '../core/continuation-recording'
import { runWebIndexedDbTransaction, WebRecordingNoncommittedError } from './web-indexeddb-transaction'
import type { WebIndexedDbDatabase, WebIndexedDbTransaction } from './web-indexeddb-transaction'
import type { WebAuthorityOwner } from './web-authority-reservation'

/** The host must hold partition-wide exclusive authority for this epoch. */
export interface WebOrderedRecordingDeclaration {
  readonly id: string
  readonly maxBytes: number
  readonly maxRecords: number
  readonly context: OrderedRecordingContext
  readonly producerIncarnation: string
  readonly collectionId: string
  readonly sourceGeneration: string
  readonly ownerEpoch: string
  readonly memberEpochs: readonly { readonly member: string; readonly epoch: string }[]
}

export interface WebOrderedRecordingProducer {
  declare(): Promise<void>
  open(operationId?: string): Promise<void>
  /** Call only after dispatching this exact operation. A fresh overlapping
   * transaction checks its retained arm receipt; absent while still provisional
   * proves this dispatched attempt did not commit. A blocked read stays pending. */
  reconcileOpen(operationId: string): Promise<'committed' | 'aborted' | 'unknown'>
  commit(delivery: WebOrderedRecordingDelivery): Promise<number>
  /** Requested durability of this completed transaction, including on a retry;
   * it does not establish the mode of an earlier commit whose result was lost. */
  commitWithDurability(delivery: WebOrderedRecordingDelivery): Promise<WebOrderedRecordingCommitResult>
  /** Read after the uncertain write transaction was created, on the same database.
   * IndexedDB orders overlapping scopes; a later-issued write is not fenced.
   * Only committed authorizes acknowledgement; unknown needs further recovery.
   * The reported durability is for this read, not the uncertain write. */
  reconcile(delivery: WebOrderedRecordingDelivery): Promise<WebOrderedRecordingReconciliation>
  observeTerminal(observation: WebRecordingTerminalObservation, onDispatched?: () => void): Promise<void>
  reconcileTerminal(
    observation: Omit<WebRecordingTerminalObservation, 'status'>
  ): Promise<'committed' | 'absent' | 'unknown'>
  terminalRetained(observation: Omit<WebRecordingTerminalObservation, 'status'>): Promise<boolean>
  retire(identity: string): Promise<void>
  seal(): Promise<void>
  fail(failure: OrderedRecordingFailure): Promise<void>
}

export interface WebRecordingTerminalObservation {
  readonly operationId: string
  readonly deliveryId: string
  readonly result: OrderedRecordingHostStopResult
  readonly status?: ContinuationRecordingStatus
}

export type WebOrderedRecordingDelivery = {
  readonly id: string
  readonly record: OrderedRecordingRecord
  /** Supplied observations, never inferred from a terminal or a sealed row. */
  readonly outcome?: OrderedRecordingStopReceipt
  readonly status?: ContinuationRecordingStatus
}

/** The mode requested for this transaction, not proof that the browser honored it. */
export type WebOrderedRecordingDurability = 'strict' | 'default'
export type WebOrderedRecordingCommitResult = {
  readonly ordinal: number
  readonly durability: WebOrderedRecordingDurability
}
export type WebOrderedRecordingReconciliation =
  | { readonly outcome: 'committed'; readonly ordinal: number; readonly durability: WebOrderedRecordingDurability }
  | { readonly outcome: 'absent' | 'unknown'; readonly durability: WebOrderedRecordingDurability }

interface Request {
  readonly result: unknown
  onsuccess: ((...args: never[]) => unknown) | null
}
interface Store {
  get(key: string | (string | number)[]): Request
  getAll(range: unknown, count: number): Request
  count(range: unknown): Request
  add(value: object): Request
  put(value: object): Request
  delete(key: string | (string | number)[]): Request
}
export interface WebOrderedJournalTransaction extends WebIndexedDbTransaction {
  objectStore(name: string): Store
}
export type WebOrderedJournalDatabase = WebIndexedDbDatabase<WebOrderedJournalTransaction>

type Row = Record<string, unknown>
const stores = ['recordings', 'records', 'pendingIdentities', 'readerReceipts', 'authorityReservations']
const invalid = (detail: string): Error => contractError('protocol.malformed', 'restoration', `web-recording.${detail}`)
const invariant = (detail: string): Error =>
  contractError('lifecycle.invariant-violation', 'restoration', `web-recording.${detail}`)
function object(value: unknown): Row {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw invalid('row')
  return Object.fromEntries(Object.entries(value))
}
function safe(value: unknown, min: number, max = Number.MAX_SAFE_INTEGER): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= min && value <= max
}
function opaque(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= 256
}
function decimal(value: unknown): value is string {
  return isCanonicalIngressOrdinal(value)
}
export function webRecordingCanonical(value: unknown): string {
  if (value instanceof Uint8Array) return webRecordingCanonical(Array.from(value))
  if (Array.isArray(value)) return `[${value.map(webRecordingCanonical).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const row = object(value)
    return `{${Object.keys(row)
      .sort()
      .map(key => `${JSON.stringify(key)}:${webRecordingCanonical(row[key])}`)
      .join(',')}}`
  }
  const result = JSON.stringify(value)
  if (result === undefined) throw invalid('payload')
  return result
}

/** Version 3 also independently accounts for unretired source evidence.
 * Older versions cannot safely synthesize that accounting from untrusted rows. */
export interface WebRecordingReaderIntegrity {
  version: 3
  receipt: string | null
  accountedBytes: number
  pendingCount: number
  pendingBytes: number
  deliveryId: string | null
  sourceOrdinal: string | null
}
export function webRecordingReaderIntegrity(row: Row): WebRecordingReaderIntegrity {
  const state = object(row.readerIntegrity)
  if (
    state.version !== 3 ||
    (state.receipt !== null && typeof state.receipt !== 'string') ||
    !safe(state.accountedBytes, 0, MAX_RECORDING_BYTES) ||
    state.accountedBytes !== row.byteCount ||
    !safe(state.pendingCount, 0, MAX_RECORDING_RECORDS) ||
    !safe(state.pendingBytes, 0, MAX_RECORDING_BYTES) ||
    state.pendingCount !== row.pendingCount ||
    state.pendingBytes !== row.pendingBytes ||
    (state.deliveryId !== null && !decimal(state.deliveryId)) ||
    (state.sourceOrdinal !== null && !decimal(state.sourceOrdinal)) ||
    (row.readerCursor === 0 && (state.deliveryId !== null || state.sourceOrdinal !== null)) ||
    (row.readerCursor !== 0 && state.deliveryId === null) ||
    (typeof state.deliveryId === 'string' &&
      (typeof row.lastDeliveryId !== 'string' || greater(state.deliveryId, row.lastDeliveryId))) ||
    (typeof state.sourceOrdinal === 'string' &&
      (typeof row.lastSourceOrdinal !== 'string' || greater(state.sourceOrdinal, row.lastSourceOrdinal)))
  )
    throw invalid('reader-integrity')
  return {
    version: 3,
    receipt: state.receipt,
    accountedBytes: state.accountedBytes,
    pendingCount: state.pendingCount,
    pendingBytes: state.pendingBytes,
    deliveryId: state.deliveryId,
    sourceOrdinal: state.sourceOrdinal
  }
}
/** Reader acknowledgement writes this binding from the verified record before
 * deleting it. The pending identity retains it until source retirement. */
export function webRecordingAcknowledgedBinding(key: readonly string[], ordinal: number, payload: string): string {
  return webRecordingCanonical([key, ordinal, payload])
}

/** All dependent IndexedDB requests are queued in request callbacks, without awaiting. */
async function transactWithDurability<T>(
  db: WebOrderedJournalDatabase,
  mode: 'readonly' | 'readwrite',
  action: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
  proveNoncommit = false,
  onDispatched?: () => void
): Promise<{ value: T; durability: WebOrderedRecordingDurability }> {
  let result: { value: T } | undefined
  let problem: unknown
  let settlement: import('./web-indexeddb-transaction').WebIndexedDbSettlement | undefined
  let run: ReturnType<typeof runWebIndexedDbTransaction>
  try {
    run = runWebIndexedDbTransaction(
      db,
      stores,
      mode,
      tx => {
        const done = (value: T): void => {
          result = { value }
        }
        const guarded = (fn: () => void): void => {
          try {
            fn()
          } catch (error) {
            problem = error
            try {
              tx.abort()
            } catch (abortError) {
              problem = webIndexedDbQueueAbortError(error, abortError)
            }
          }
        }
        guarded(() => action(tx, done, guarded))
        return guarded
      },
      undefined,
      undefined,
      outcome => {
        settlement = outcome
      },
      onDispatched
    )
  } catch (error) {
    if (proveNoncommit && settlement?.state === 'not-started') throw new WebRecordingNoncommittedError(error)
    throw error
  }
  return run.then(
    receipt => {
      if (problem !== undefined) throw problem
      if (result === undefined) throw invalid('incomplete-transaction')
      return { value: result.value, durability: receipt.durability }
    },
    error => {
      const original = problem ?? error
      if (proveNoncommit && (settlement?.state === 'aborted' || settlement?.state === 'not-started'))
        throw new WebRecordingNoncommittedError(original)
      throw original
    }
  )
}

async function transact<T>(
  db: WebOrderedJournalDatabase,
  action: (tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
  proveNoncommit = false,
  onDispatched?: () => void
): Promise<T> {
  return (await transactWithDurability(db, 'readwrite', action, proveNoncommit, onDispatched)).value
}

/** Ledger charges share the recording's byte quota with each journal backlog. */
export function webRecordingMarkerLedgerBytes(row: Row): number {
  const value = row.markerLedgerBytes === undefined ? 0 : row.markerLedgerBytes
  if (
    !safe(row.maxBytes, 1, MAX_RECORDING_BYTES) ||
    !safe(value, 0, row.maxBytes) ||
    !safe(row.byteCount, 0, row.maxBytes) ||
    !safe(row.pendingBytes, 0, row.maxBytes) ||
    value > row.maxBytes - row.byteCount ||
    value > row.maxBytes - row.pendingBytes
  )
    throw invalid('marker-ledger-accounting')
  return value
}

/** Capacity held for a live marker envelope and the Web source terminal. */
export function webRecordingControlReservation(row: Row): { total: number; marker: number; unit: number } {
  const total = row.markerControlBytes === undefined ? 0 : row.markerControlBytes
  const marker = row.markerControlMarkerBytes === undefined ? 0 : row.markerControlMarkerBytes
  const unit = row.markerControlUnitBytes === undefined ? 0 : row.markerControlUnitBytes
  if (
    !safe(total, 0, Number(row.maxBytes)) ||
    !safe(marker, 0, total) ||
    !safe(unit, 0, Number(row.maxBytes)) ||
    (marker !== 0 && (unit === 0 || marker % unit !== 0)) ||
    total > Number(row.maxBytes) - webRecordingMarkerLedgerBytes(row) - Number(row.byteCount) ||
    total > Number(row.maxBytes) - webRecordingMarkerLedgerBytes(row) - Number(row.pendingBytes)
  )
    throw invalid('marker-control-capacity')
  return { total, marker, unit }
}

/** An absent counter is safe only before any marker evidence has been retained. */
export function webRecordingMarkerDebt(row: Row): number {
  const value = row.markerDebt
  if (value === undefined && webRecordingMarkerLedgerBytes(row) === 0) return 0
  if (!safe(value, 0, Number.MAX_SAFE_INTEGER)) throw invalid('marker-debt-accounting')
  return value
}

function readRow(input: unknown, declaration: WebOrderedRecordingDeclaration): Row {
  const row = object(input)
  if (
    row.version !== 1 ||
    row.kind !== 'ordered' ||
    row.id !== declaration.id ||
    row.producerIncarnation !== declaration.producerIncarnation ||
    row.collectionId !== declaration.collectionId ||
    row.sourceGeneration !== declaration.sourceGeneration ||
    row.ownerEpoch !== declaration.ownerEpoch
  )
    throw invariant('owner-epoch')
  if (
    !['prepared', 'live', 'sealed', 'interrupted'].includes(String(row.ownerState)) ||
    (row.nextOrdinal !== null && !safe(row.nextOrdinal, 1)) ||
    !safe(row.recordCount, 0, declaration.maxRecords) ||
    !safe(row.pendingCount, 0, declaration.maxRecords) ||
    !safe(row.pendingBytes, 0, declaration.maxBytes) ||
    (row.pendingCount === 0) !== (row.pendingBytes === 0) ||
    !safe(row.byteCount, 0, declaration.maxBytes) ||
    !safe(row.readerCursor, 0) ||
    (row.lastDeliveryId !== null && !decimal(row.lastDeliveryId)) ||
    (row.lastRetiredId !== null && !decimal(row.lastRetiredId)) ||
    (row.lastSourceOrdinal !== null && !decimal(row.lastSourceOrdinal)) ||
    typeof row.terminal !== 'boolean' ||
    webRecordingCanonical(row.context) !== webRecordingCanonical(declaration.context) ||
    webRecordingCanonical(row.memberEpochs) !== webRecordingCanonical(declaration.memberEpochs) ||
    row.maxBytes !== declaration.maxBytes ||
    row.maxRecords !== declaration.maxRecords
  )
    throw invalid('recording')
  const lastOrdinal = row.nextOrdinal === null ? Number.MAX_SAFE_INTEGER : row.nextOrdinal - 1
  if (
    row.readerCursor > lastOrdinal ||
    row.recordCount !== lastOrdinal - row.readerCursor ||
    row.pendingCount > lastOrdinal ||
    (row.recordCount === 0) !== (row.byteCount === 0)
  )
    throw invalid('accounting')
  webRecordingReaderIntegrity(row)
  webRecordingControlReservation(row)
  if (row.failure !== null) parseOrderedRecordingFailure(row.failure, declaration.context.version)
  webRecordingTerminalOutcome(row)
  return row
}
/** Optional on legacy v1 rows. The complete terminal payload survives record and identity deletion. */
export function webRecordingTerminalOutcome(row: Row): {
  readonly deliveryId: string
  readonly ordinal: number
  readonly record: OrderedRecordingRecord
  readonly outcome: OrderedRecordingStopReceipt | null
  readonly status: ContinuationRecordingStatus | null
  readonly hostCleanup: OrderedRecordingHostStopResult['hostCleanup'] | null
} | null {
  const version = parseOrderedRecordingContext(row.context).version
  if (row.terminalOutcome === undefined) {
    if (row.terminalObservation !== undefined) throw invalid('terminal-observation')
    return null
  }
  const receipt = object(row.terminalOutcome)
  if (
    receipt.version !== 1 ||
    receipt.recordingId !== row.id ||
    receipt.producerIncarnation !== row.producerIncarnation ||
    receipt.collectionId !== row.collectionId ||
    receipt.sourceGeneration !== row.sourceGeneration ||
    receipt.ownerEpoch !== row.ownerEpoch ||
    !decimal(receipt.generation) ||
    receipt.generation === '0' ||
    typeof receipt.ownerToken !== 'string' ||
    !/^[0-9a-f]{64}$/.test(receipt.ownerToken) ||
    !decimal(receipt.deliveryId) ||
    receipt.deliveryId === '0' ||
    receipt.deliveryId !== row.lastDeliveryId ||
    !safe(receipt.ordinal, 1) ||
    row.terminal !== true ||
    receipt.ordinal !== (row.nextOrdinal === null ? Number.MAX_SAFE_INTEGER : Number(row.nextOrdinal) - 1)
  )
    throw invalid('terminal-outcome')
  const envelope = {
    ordinal: receipt.ordinal,
    metadata: {
      kind: 'ordered',
      version,
      recordingId: row.id,
      peerId: object(row.context).peerId,
      collectionId: row.collectionId,
      sourceGeneration: { kind: 'opaque', value: row.sourceGeneration },
      deliveryId: receipt.deliveryId
    },
    record: receipt.record
  }
  const bytes = utf8ByteLength(JSON.stringify(envelope))
  const parsed = parseOrderedRecordingBatch(
    { token: 'terminal', records: [envelope], bytes, more: false },
    String(row.id),
    { maxItems: 1, maxBytes: bytes }
  )
  const terminal = parsed.records[0]?.record
  if (terminal?.kind !== 'terminal') throw invalid('terminal-outcome')
  if (
    Object.keys(receipt).sort().join(',') !==
    [
      'collectionId',
      'deliveryId',
      'failure',
      'generation',
      'ordinal',
      'outcome',
      'ownerEpoch',
      'ownerToken',
      'producerIncarnation',
      'record',
      'recordingId',
      'sourceGeneration',
      'status',
      'version'
    ]
      .sort()
      .join(',')
  )
    throw invalid('terminal-outcome')
  if (webRecordingCanonical(receipt.failure) !== webRecordingCanonical(row.failure)) throw invalid('terminal-failure')
  if (row.ownerState === 'sealed') {
    const seal = object(row.sealReceipt)
    if (
      seal.version !== 1 ||
      seal.generation !== receipt.generation ||
      seal.ownerToken !== receipt.ownerToken ||
      seal.ownerEpoch !== receipt.ownerEpoch ||
      seal.deliveryId !== receipt.deliveryId
    )
      throw invalid('terminal-seal')
  }
  let outcome = receipt.outcome === null ? null : parseOrderedRecordingStopReceipt(receipt.outcome, version)
  if (outcome !== null && outcome.state !== 'finished' && outcome.state !== 'failed' && outcome.state !== 'interrupted')
    throw invalid('terminal-outcome')
  let status = receipt.status === null ? null : terminalStatus(receipt.status, row)
  let hostCleanup: OrderedRecordingHostStopResult['hostCleanup'] | null = null
  if (row.terminalObservation !== undefined) {
    const observation = object(row.terminalObservation)
    const result = object(observation.result)
    if (
      Object.keys(observation).sort().join(',') !==
        ['version', 'operationId', 'deliveryId', 'generation', 'ownerToken', 'ownerEpoch', 'result', 'status']
          .sort()
          .join(',') ||
      observation.version !== 1 ||
      typeof observation.operationId !== 'string' ||
      !/^[0-9a-f]{64}$/.test(observation.operationId) ||
      observation.deliveryId !== receipt.deliveryId ||
      observation.generation !== receipt.generation ||
      observation.ownerToken !== receipt.ownerToken ||
      observation.ownerEpoch !== receipt.ownerEpoch ||
      row.ownerState !== 'sealed' ||
      Object.keys(result).sort().join(',') !== 'hostCleanup,recording'
    )
      throw invalid('terminal-observation')
    const observed = parseOrderedRecordingStopReceipt(result.recording, version)
    if (observed.state !== 'finished' && observed.state !== 'failed' && observed.state !== 'interrupted')
      throw invalid('terminal-observation')
    if (outcome !== null && webRecordingCanonical(outcome) !== webRecordingCanonical(observed))
      throw invariant('terminal-outcome-conflict')
    const observedStatus = observation.status === null ? null : terminalStatus(observation.status, row)
    if (
      status !== null &&
      observedStatus !== null &&
      webRecordingCanonical(status) !== webRecordingCanonical(observedStatus)
    )
      throw invariant('terminal-status-conflict')
    outcome = observed
    status = observedStatus ?? status
    hostCleanup = parseOrderedRecordingHostCleanup(result.hostCleanup)
  }
  return { deliveryId: receipt.deliveryId, ordinal: receipt.ordinal, record: terminal, outcome, status, hostCleanup }
}

function diagnostic(input: unknown): ContinuationRecordingFailure {
  const value = object(input)
  if (
    typeof value.kind !== 'string' ||
    typeof value.detail !== 'string' ||
    typeof value.operation !== 'string' ||
    (value.sqliteExtendedCode !== null &&
      (!Number.isInteger(value.sqliteExtendedCode) ||
        typeof value.sqliteExtendedCode !== 'number' ||
        value.sqliteExtendedCode < -2147483648 ||
        value.sqliteExtendedCode > 2147483647)) ||
    (value.sqliteCode !== null && typeof value.sqliteCode !== 'string')
  )
    throw invalid('terminal-status')
  return {
    kind: value.kind,
    detail: value.detail,
    operation: value.operation,
    sqliteExtendedCode: value.sqliteExtendedCode,
    sqliteCode: value.sqliteCode
  }
}

function terminalStatus(input: unknown, row: Row): ContinuationRecordingStatus {
  const value = object(input)
  const owner = object(value.orderedOwner)
  if (
    value.recordingId !== row.id ||
    (value.phase !== 'stopped' && value.phase !== 'recording' && value.phase !== 'capacity-reached') ||
    typeof value.accepting !== 'boolean' ||
    !safe(value.records, 0) ||
    !safe(value.bytes, 0) ||
    !safe(value.lostRecords, 0) ||
    value.maxBytes !== row.maxBytes ||
    value.maxRecords !== row.maxRecords ||
    value.encrypted !== false ||
    owner.version !== 1 ||
    owner.producerIncarnation !== row.producerIncarnation ||
    owner.collectionId !== row.collectionId ||
    webRecordingCanonical(owner.sourceGeneration) !==
      webRecordingCanonical({ kind: 'opaque', value: row.sourceGeneration }) ||
    (owner.state !== 'sealed' && owner.state !== 'live') ||
    owner.uncertainTail !== null
  )
    throw invalid('terminal-status')
  let runtimeFailure: ContinuationRecordingStatus['runtimeFailure'] = null
  if (value.runtimeFailure !== null) {
    const runtime = object(value.runtimeFailure)
    if (runtime.persisted !== false) throw invalid('terminal-status')
    runtimeFailure = { ...diagnostic(runtime), persisted: false }
  }
  let collectionFailure: ContinuationRecordingStatus['collectionFailure'] = null
  if (value.collectionFailure !== null) {
    const failure = object(value.collectionFailure)
    if (typeof failure.persisted !== 'boolean') throw invalid('terminal-status')
    const persistenceFailure =
      failure.persistenceFailure === undefined ? undefined : diagnostic(failure.persistenceFailure)
    if (failure.persisted && persistenceFailure !== undefined) throw invalid('terminal-status')
    let knownRecordedLoss: { items: number; bytes?: number } | undefined
    if (failure.knownRecordedLoss !== undefined) {
      const loss = object(failure.knownRecordedLoss)
      if (!safe(loss.items, 0) || (loss.bytes !== undefined && !safe(loss.bytes, 0))) throw invalid('terminal-status')
      knownRecordedLoss = { items: loss.items, ...(loss.bytes === undefined ? {} : { bytes: loss.bytes }) }
    }
    collectionFailure = {
      ...diagnostic(failure),
      persisted: failure.persisted,
      ...(persistenceFailure === undefined ? {} : { persistenceFailure }),
      ...(knownRecordedLoss === undefined ? {} : { knownRecordedLoss })
    }
  }
  if (value.accepting !== (value.phase === 'recording' && collectionFailure === null)) throw invalid('terminal-status')
  const status: ContinuationRecordingStatus = {
    recordingId: String(row.id),
    phase: value.phase,
    accepting: value.accepting,
    records: value.records,
    bytes: value.bytes,
    lostRecords: value.lostRecords,
    maxBytes: Number(row.maxBytes),
    maxRecords: Number(row.maxRecords),
    encrypted: false,
    orderedOwner: {
      version: 1,
      producerIncarnation: String(row.producerIncarnation),
      collectionId: String(row.collectionId),
      sourceGeneration: { kind: 'opaque', value: String(row.sourceGeneration) },
      state: owner.state,
      uncertainTail: null
    },
    runtimeFailure,
    collectionFailure
  }
  if (webRecordingCanonical(status) !== webRecordingCanonical(input)) throw invalid('terminal-status')
  return status
}

function withReaderReceipt(
  tx: WebOrderedJournalTransaction,
  row: Row,
  done: () => void,
  guard: (fn: () => void) => void
): void {
  const integrity = webRecordingReaderIntegrity(row)
  const request = tx.objectStore('readerReceipts').get(String(row.id))
  request.onsuccess = () =>
    guard(() => {
      let encoded: string | undefined
      try {
        encoded = JSON.stringify(request.result)
      } catch {
        throw invalid('receipt-integrity')
      }
      if (integrity.receipt === null ? request.result !== undefined : encoded !== integrity.receipt)
        throw invalid('receipt-integrity')
      done()
    })
}
function wireRecord(record: OrderedRecordingRecord, version: 1 | 2): unknown {
  // The public terminal contains the failure summary fields; its wire form
  // requires the discriminator that gives those fields their meaning.
  return JSON.parse(
    webRecordingCanonical(
      record.kind === 'terminal' && version === 1 ? { ...record, failureSummaryVersion: 1 } : record
    )
  )
}
function metadata(d: WebOrderedRecordingDeclaration, deliveryId: string): object {
  return {
    kind: 'ordered',
    version: d.context.version,
    recordingId: d.id,
    peerId: d.context.peerId,
    collectionId: d.collectionId,
    sourceGeneration: { kind: 'opaque', value: d.sourceGeneration },
    deliveryId
  }
}
function validateRecord(
  d: WebOrderedRecordingDeclaration,
  id: string,
  record: OrderedRecordingRecord
): { wire: unknown; payload: string } {
  const wire = wireRecord(record, d.context.version)
  const envelope = { ordinal: 1, metadata: metadata(d, id), record: wire }
  const bytes = utf8ByteLength(JSON.stringify(envelope))
  parseOrderedRecordingBatch({ token: 'validation', records: [envelope], bytes, more: false }, d.id, {
    maxItems: 1,
    maxBytes: bytes
  })
  if (
    record.kind === 'value' &&
    (!d.memberEpochs.some(member => member.member === record.member && member.epoch === record.membership.epoch) ||
      record.membership.member !== record.member)
  )
    throw invalid('membership')
  return { wire, payload: webRecordingCanonical(wire) }
}
function identity(d: WebOrderedRecordingDeclaration, id: string): string[] {
  return [d.id, d.producerIncarnation, d.collectionId, d.sourceGeneration, id]
}
function greater(a: string, b: string): boolean {
  return a.length > b.length || (a.length === b.length && a > b)
}

/** Check the complete identity and, unless already reader-acknowledged, its row
 * within the same transaction as the recording cursor and pending identity. */
function verifyPending(
  d: WebOrderedRecordingDeclaration,
  row: Row,
  tx: WebOrderedJournalTransaction,
  id: string,
  wire: unknown,
  payload: string,
  input: unknown,
  done: (ordinal: number) => void,
  guard: (fn: () => void) => void
): void {
  const existing = object(input)
  const lastOrdinal = row.nextOrdinal === null ? Number.MAX_SAFE_INTEGER : Number(row.nextOrdinal) - 1
  if (
    existing.payload !== payload ||
    !safe(existing.ordinal, 1, lastOrdinal) ||
    existing.recordingId !== d.id ||
    existing.producerIncarnation !== d.producerIncarnation ||
    existing.collectionId !== d.collectionId ||
    existing.sourceGeneration !== d.sourceGeneration ||
    existing.deliveryId !== id ||
    existing.retired !== false ||
    !safe(row.pendingCount, 1) ||
    typeof row.lastDeliveryId !== 'string' ||
    greater(id, row.lastDeliveryId)
  )
    throw invariant('identity-conflict')
  const ordinal = existing.ordinal
  const data = { ordinal, metadata: metadata(d, id), record: wire }
  const byteLength = utf8ByteLength(JSON.stringify(data))
  if (existing.byteLength !== byteLength) throw invariant('identity-conflict')
  if (ordinal <= Number(row.readerCursor)) {
    if (existing.readerAcknowledged !== webRecordingAcknowledgedBinding(identity(d, id), ordinal, payload))
      throw invariant('reader-acknowledgement-conflict')
    done(ordinal)
    return
  }
  if (existing.readerAcknowledged !== null) throw invariant('reader-acknowledgement-conflict')
  const request = tx.objectStore('records').get([d.id, ordinal])
  request.onsuccess = () =>
    guard(() => {
      if (
        byteLength > MAX_RECORDING_BATCH_BYTES ||
        byteLength > Number(row.byteCount) ||
        webRecordingCanonical(request.result) !==
          webRecordingCanonical({ recordingId: d.id, ...data, byteLength, version: 1, sourceRetired: false })
      )
        throw invariant('record-conflict')
      done(ordinal)
    })
}

/** A producer mutation requires the exact durable reservation owner. The
 * declaration is created by acquisition, and open atomically arms it. */
export function createWebOrderedRecordingJournal(
  db: WebOrderedJournalDatabase,
  input: WebOrderedRecordingDeclaration,
  reservation?: WebAuthorityOwner
): WebOrderedRecordingProducer {
  const d: WebOrderedRecordingDeclaration = {
    ...input,
    context: parseOrderedRecordingContext(input.context),
    memberEpochs: input.memberEpochs.map(member => ({ member: member.member, epoch: member.epoch }))
  }
  if (
    !isRecordingId(d.id) ||
    !safe(d.maxBytes, MIN_RECORDING_BYTES, MAX_RECORDING_BYTES) ||
    !safe(d.maxRecords, MIN_RECORDING_RECORDS, MAX_RECORDING_RECORDS) ||
    !opaque(d.producerIncarnation) ||
    !opaque(d.sourceGeneration) ||
    !decimal(d.collectionId) ||
    !decimal(d.ownerEpoch) ||
    d.ownerEpoch === '0' ||
    d.memberEpochs.length !== d.context.members.length ||
    new Set(d.memberEpochs.map(member => member.member)).size !== d.memberEpochs.length ||
    d.memberEpochs.some(
      member =>
        !decimal(member.epoch) ||
        member.epoch === '0' ||
        !d.context.members.some(item => item.memberId === member.member)
    )
  )
    throw invalid('declaration')

  const owner = reservation === undefined ? undefined : { ...reservation }
  if (
    owner !== undefined &&
    (owner.id !== d.id ||
      owner.ownerEpoch !== d.ownerEpoch ||
      !decimal(owner.generation) ||
      owner.generation === '0' ||
      !/^[0-9a-f]{64}$/.test(owner.ownerToken))
  )
    throw invalid('reservation')
  const withReservation = (
    tx: WebOrderedJournalTransaction,
    row: Row,
    phases: readonly string[],
    done: (reservationRow: Row) => void,
    guard: (fn: () => void) => void,
    allowReleasedRecovery = false
  ): void => {
    const request = tx.objectStore('authorityReservations').get(d.id)
    request.onsuccess = () =>
      guard(() => {
        if (owner === undefined) throw invariant('reservation-required')
        const value = object(request.result)
        const recoverySource =
          allowReleasedRecovery && value.phase === 'released' && value.recoveryReceipt !== undefined
            ? object(object(value.recoveryReceipt).source)
            : value
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
        const receipt = object(value.receipt)
        const expectedTransition =
          value.phase === 'provisional'
            ? 'acquire'
            : value.phase === 'armed'
              ? 'arm'
              : value.phase === 'cleanup-pending'
                ? 'cleanup'
                : null
        if (
          typeof receipt.operationId !== 'string' ||
          !/^[0-9a-f]{64}$/.test(receipt.operationId) ||
          receipt.phase !== value.phase ||
          (expectedTransition === null
            ? receipt.transition !== 'release' && receipt.transition !== 'revoke'
            : receipt.transition !== expectedTransition)
        )
          throw invalid('reservation-receipt')
        if (
          value.version !== 1 ||
          value.id !== owner.id ||
          recoverySource.generation !== owner.generation ||
          recoverySource.ownerToken !== owner.ownerToken ||
          value.ownerEpoch !== owner.ownerEpoch ||
          value.declaration !== binding ||
          !phases.includes(String(value.phase))
        )
          throw invariant('reservation')
        if (
          (row.ownerState === 'prepared' && value.phase !== 'provisional' && value.phase !== 'released') ||
          (row.ownerState === 'live' && value.phase !== 'armed' && value.phase !== 'cleanup-pending') ||
          ((row.ownerState === 'sealed' || row.ownerState === 'interrupted') &&
            value.phase !== 'cleanup-pending' &&
            value.phase !== 'released')
        )
          throw invariant('reservation-state')
        done(value)
      })
  }

  const mutate = <T>(
    fn: (row: Row, tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
    phases: readonly string[] = ['provisional', 'armed', 'cleanup-pending'],
    proveNoncommit = false,
    onDispatched?: () => void
  ): Promise<T> =>
    transact(
      db,
      (tx, done, guard) => {
        const request = tx.objectStore('recordings').get(d.id)
        request.onsuccess = () =>
          guard(() => {
            const row = readRow(request.result, d)
            withReservation(
              tx,
              row,
              phases,
              () => withReaderReceipt(tx, row, () => fn(row, tx, done, guard), guard),
              guard
            )
          })
      },
      proveNoncommit,
      onDispatched
    )
  const inspect = <T>(
    mode: 'readonly' | 'readwrite',
    fn: (row: Row, tx: WebOrderedJournalTransaction, done: (value: T) => void, guard: (fn: () => void) => void) => void,
    allowReleasedRecovery = false
  ): Promise<{ value: T; durability: WebOrderedRecordingDurability }> =>
    transactWithDurability(db, mode, (tx, done, guard) => {
      const request = tx.objectStore('recordings').get(d.id)
      request.onsuccess = () =>
        guard(() => {
          const row = readRow(request.result, d)
          withReservation(
            tx,
            row,
            mode === 'readonly'
              ? ['provisional', 'armed', 'cleanup-pending', 'released']
              : ['provisional', 'armed', 'cleanup-pending'],
            () => withReaderReceipt(tx, row, () => fn(row, tx, done, guard), guard),
            guard,
            allowReleasedRecovery
          )
        })
    })
  const reconcileTerminal = async (
    observation: Omit<WebRecordingTerminalObservation, 'status'>
  ): Promise<'committed' | 'absent' | 'unknown'> => {
    const recording = parseOrderedRecordingStopReceipt(observation.result.recording, d.context.version)
    const result = { recording, hostCleanup: parseOrderedRecordingHostCleanup(observation.result.hostCleanup) }
    const receipt = await inspect<'committed' | 'absent' | 'unknown'>(
      'readonly',
      (row, _tx, done) => {
        webRecordingTerminalOutcome(row)
        if (row.terminalObservation === undefined) return done('absent')
        const retained = object(row.terminalObservation)
        done(
          retained.operationId === observation.operationId &&
            retained.deliveryId === observation.deliveryId &&
            retained.generation === owner?.generation &&
            retained.ownerToken === owner?.ownerToken &&
            retained.ownerEpoch === owner?.ownerEpoch &&
            webRecordingCanonical(retained.result) === webRecordingCanonical(result)
            ? 'committed'
            : 'unknown'
        )
      },
      true
    )
    return receipt.value
  }
  const commitWithDurability = async (
    delivery: WebOrderedRecordingDelivery
  ): Promise<WebOrderedRecordingCommitResult> => {
    if (!decimal(delivery.id) || delivery.id === '0') throw invalid('delivery-id')
    const { wire, payload } = validateRecord(d, delivery.id, delivery.record)
    const sourceOrdinal =
      delivery.record.kind === 'value' || delivery.record.kind === 'marker' ? delivery.record.ordinal : null
    const terminal = delivery.record.kind === 'terminal'
    if (!terminal && (delivery.outcome !== undefined || delivery.status !== undefined))
      throw invalid('terminal-outcome')
    const outcome =
      delivery.outcome === undefined ? null : parseOrderedRecordingStopReceipt(delivery.outcome, d.context.version)
    const status: unknown = delivery.status === undefined ? null : JSON.parse(webRecordingCanonical(delivery.status))
    if (
      outcome !== null &&
      outcome.state !== 'finished' &&
      outcome.state !== 'failed' &&
      outcome.state !== 'interrupted'
    )
      throw invalid('terminal-outcome')
    const receipt = await inspect<number>('readwrite', (row, tx, done, guard) => {
      if (row.ownerState !== 'live') throw invariant('commit-state')
      if (terminal && webRecordingMarkerDebt(row) !== 0) throw invariant('marker-debt')
      const pending = tx.objectStore('pendingIdentities')
      const request = pending.get(identity(d, delivery.id))
      request.onsuccess = () =>
        guard(() => {
          if (request.result !== undefined) {
            if (terminal) {
              const retained = webRecordingTerminalOutcome(row)
              if (
                retained === null ||
                webRecordingCanonical(retained.outcome) !== webRecordingCanonical(outcome) ||
                webRecordingCanonical(retained.status) !== webRecordingCanonical(status)
              )
                throw invariant('terminal-outcome-conflict')
            }
            if (object(request.result).retired === true) throw invariant('delivery-order')
            verifyPending(d, row, tx, delivery.id, wire, payload, request.result, done, guard)
            return
          }
          if (row.terminal || (typeof row.lastDeliveryId === 'string' && !greater(delivery.id, row.lastDeliveryId)))
            throw invariant('delivery-order')
          if (
            sourceOrdinal !== null &&
            typeof row.lastSourceOrdinal === 'string' &&
            !greater(sourceOrdinal, row.lastSourceOrdinal)
          )
            throw invariant('source-order')
          if (row.nextOrdinal === null) throw invariant('ordinal-exhausted')
          if (
            !safe(row.nextOrdinal, 1) ||
            !safe(row.recordCount, 0) ||
            !safe(row.pendingCount, 0, Number.MAX_SAFE_INTEGER - 1) ||
            !safe(row.byteCount, 0)
          )
            throw invalid('counter')
          const ordinal = row.nextOrdinal
          const data = { ordinal, metadata: metadata(d, delivery.id), record: wire }
          const byteLength = utf8ByteLength(JSON.stringify(data))
          const control = webRecordingControlReservation(row)
          let reserved = control.total
          if (delivery.record.kind === 'marker' && control.marker !== 0) {
            if (byteLength > control.unit) throw invariant('marker-control-bound')
            reserved -= control.unit
          } else if (terminal && control.total !== 0) {
            if (control.marker !== 0 || byteLength > control.total) throw invariant('terminal-control-bound')
            reserved = 0
          }
          // Source evidence can outlive a reader-deleted record. Bound that
          // backlog separately by the same record and serialized-byte limits;
          // retired evidence is bounded by the still-retained record quota.
          if (
            byteLength > MAX_RECORDING_BATCH_BYTES ||
            row.recordCount >= d.maxRecords ||
            row.pendingCount >= d.maxRecords ||
            byteLength > d.maxBytes - row.byteCount - webRecordingMarkerLedgerBytes(row) - reserved ||
            byteLength > d.maxBytes - Number(row.pendingBytes) - webRecordingMarkerLedgerBytes(row) - reserved
          )
            throw contractError('stream.quota', 'restoration', 'web-recording.capacity')
          tx.objectStore('records').add({ recordingId: d.id, ...data, byteLength, version: 1, sourceRetired: false })
          pending.add({
            recordingId: d.id,
            producerIncarnation: d.producerIncarnation,
            collectionId: d.collectionId,
            sourceGeneration: d.sourceGeneration,
            deliveryId: delivery.id,
            payload,
            ordinal,
            byteLength,
            retired: false,
            readerAcknowledged: null
          })
          const integrity = webRecordingReaderIntegrity(row)
          if (reserved !== control.total) {
            row.markerControlBytes = reserved
            row.markerControlMarkerBytes = delivery.record.kind === 'marker' ? control.marker - control.unit : 0
          }
          row.nextOrdinal = ordinal === Number.MAX_SAFE_INTEGER ? null : ordinal + 1
          row.recordCount += 1
          row.pendingCount += 1
          row.pendingBytes = Number(row.pendingBytes) + byteLength
          row.byteCount += byteLength
          integrity.accountedBytes += byteLength
          integrity.pendingCount += 1
          integrity.pendingBytes += byteLength
          row.readerIntegrity = integrity
          row.lastDeliveryId = delivery.id
          if (sourceOrdinal !== null) row.lastSourceOrdinal = sourceOrdinal
          if (terminal) {
            row.terminal = true
            row.terminalOutcome = {
              version: 1,
              recordingId: d.id,
              producerIncarnation: d.producerIncarnation,
              collectionId: d.collectionId,
              sourceGeneration: d.sourceGeneration,
              ownerEpoch: d.ownerEpoch,
              generation: owner?.generation,
              ownerToken: owner?.ownerToken,
              deliveryId: delivery.id,
              ordinal,
              record: wire,
              outcome,
              status: status === null ? null : terminalStatus(status, row),
              failure: row.failure
            }
          }
          tx.objectStore('recordings').put(row)
          done(ordinal)
        })
    })
    return { ordinal: receipt.value, durability: receipt.durability }
  }
  return {
    declare: () =>
      mutate<void>((row, _tx, done) => {
        if (row.ownerState !== 'prepared') throw invariant('declaration-state')
        done(undefined)
      }),
    open: operationId => {
      if (typeof operationId !== 'string' || !/^[0-9a-f]{64}$/.test(operationId))
        return Promise.reject(invalid('operation-id'))
      return transact<void>(db, (tx, done, guard) => {
        const request = tx.objectStore('recordings').get(d.id)
        request.onsuccess = () =>
          guard(() => {
            const row = readRow(request.result, d)
            withReservation(
              tx,
              row,
              ['provisional', 'armed'],
              reservationRow => {
                withReaderReceipt(
                  tx,
                  row,
                  () => {
                    const prior = row.openReceipt
                    if (prior !== undefined) {
                      const receipt = object(prior)
                      if (
                        receipt.version !== 1 ||
                        receipt.operationId !== operationId ||
                        receipt.generation !== owner?.generation ||
                        receipt.ownerToken !== owner?.ownerToken ||
                        receipt.ownerEpoch !== owner?.ownerEpoch ||
                        row.ownerState !== 'live' ||
                        reservationRow.phase !== 'armed'
                      )
                        throw invariant('open-receipt')
                      done(undefined)
                      return
                    }
                    if (row.ownerState !== 'prepared' || reservationRow.phase !== 'provisional')
                      throw invariant('open-state')
                    row.ownerState = 'live'
                    row.openReceipt = {
                      version: 1,
                      operationId,
                      generation: owner?.generation,
                      ownerToken: owner?.ownerToken,
                      ownerEpoch: owner?.ownerEpoch
                    }
                    reservationRow.phase = 'armed'
                    reservationRow.receipt = { operationId, transition: 'arm', phase: 'armed' }
                    tx.objectStore('recordings').put(row)
                    tx.objectStore('authorityReservations').put(reservationRow)
                    done(undefined)
                  },
                  guard
                )
              },
              guard
            )
          })
      })
    },
    reconcileOpen: async operationId => {
      if (!/^[0-9a-f]{64}$/.test(operationId)) throw invalid('operation-id')
      const result = await transactWithDurability<'committed' | 'aborted' | 'unknown'>(
        db,
        'readonly',
        (tx, done, guard) => {
          const request = tx.objectStore('recordings').get(d.id)
          request.onsuccess = () =>
            guard(() => {
              const row = readRow(request.result, d)
              withReservation(
                tx,
                row,
                ['provisional', 'armed', 'cleanup-pending', 'released'],
                reservationRow => {
                  withReaderReceipt(
                    tx,
                    row,
                    () => {
                      if (row.openReceipt === undefined) {
                        done(
                          row.ownerState === 'prepared' && reservationRow.phase === 'provisional'
                            ? 'aborted'
                            : 'unknown'
                        )
                        return
                      }
                      const receipt = object(row.openReceipt)
                      if (
                        row.ownerState === 'prepared' ||
                        (reservationRow.phase === 'armed' &&
                          object(reservationRow.receipt).operationId !== receipt.operationId) ||
                        receipt.version !== 1 ||
                        typeof receipt.operationId !== 'string' ||
                        !/^[0-9a-f]{64}$/.test(receipt.operationId) ||
                        receipt.generation !== owner?.generation ||
                        receipt.ownerToken !== owner?.ownerToken ||
                        receipt.ownerEpoch !== owner?.ownerEpoch
                      )
                        throw invalid('open-receipt')
                      done(receipt.operationId === operationId ? 'committed' : 'unknown')
                    },
                    guard
                  )
                },
                guard
              )
            })
        }
      )
      return result.value
    },
    commit: async delivery => (await commitWithDurability(delivery)).ordinal,
    commitWithDurability,
    reconcile: async delivery => {
      if (!decimal(delivery.id) || delivery.id === '0') throw invalid('delivery-id')
      const { wire, payload } = validateRecord(d, delivery.id, delivery.record)
      const outcome =
        delivery.outcome === undefined ? null : parseOrderedRecordingStopReceipt(delivery.outcome, d.context.version)
      const status: unknown = delivery.status === undefined ? null : JSON.parse(webRecordingCanonical(delivery.status))
      if (delivery.record.kind !== 'terminal' && (outcome !== null || status !== null))
        throw invalid('terminal-outcome')
      const receipt = await inspect<{ outcome: 'committed'; ordinal: number } | { outcome: 'absent' | 'unknown' }>(
        'readonly',
        (row, tx, done, guard) => {
          if (delivery.record.kind === 'terminal') {
            const retained = webRecordingTerminalOutcome(row)
            if (retained !== null && retained.deliveryId === delivery.id) {
              if (
                webRecordingCanonical(retained.record) !== payload ||
                webRecordingCanonical(object(row.terminalOutcome).outcome) !== webRecordingCanonical(outcome) ||
                webRecordingCanonical(object(row.terminalOutcome).status) !== webRecordingCanonical(status) ||
                object(row.terminalOutcome).generation !== owner?.generation ||
                object(row.terminalOutcome).ownerToken !== owner?.ownerToken
              )
                throw invariant('terminal-outcome-conflict')
              done({ outcome: 'committed', ordinal: retained.ordinal })
              return
            }
          }
          const request = tx.objectStore('pendingIdentities').get(identity(d, delivery.id))
          request.onsuccess = () =>
            guard(() => {
              if (request.result === undefined) {
                done({
                  outcome:
                    row.lastDeliveryId === null ||
                    (typeof row.lastDeliveryId === 'string' && greater(delivery.id, row.lastDeliveryId))
                      ? 'absent'
                      : 'unknown'
                })
                return
              }
              if (object(request.result).retired === true) {
                done({ outcome: 'unknown' })
                return
              }
              verifyPending(
                d,
                row,
                tx,
                delivery.id,
                wire,
                payload,
                request.result,
                ordinal => done({ outcome: 'committed', ordinal }),
                guard
              )
            })
        }
      )
      return { ...receipt.value, durability: receipt.durability }
    },
    reconcileTerminal,
    terminalRetained: async observation => (await reconcileTerminal(observation)) === 'committed',
    observeTerminal: async (observation, onDispatched) => {
      try {
        if (!/^[0-9a-f]{64}$/.test(observation.operationId) || !decimal(observation.deliveryId))
          throw invalid('terminal-observation')
        const recording = parseOrderedRecordingStopReceipt(observation.result.recording, d.context.version)
        if (recording.state !== 'finished' && recording.state !== 'failed' && recording.state !== 'interrupted')
          throw invalid('terminal-observation')
        const result = { recording, hostCleanup: parseOrderedRecordingHostCleanup(observation.result.hostCleanup) }
        const status: unknown =
          observation.status === undefined ? null : JSON.parse(webRecordingCanonical(observation.status))
        const snapshot = {
          version: 1,
          operationId: observation.operationId,
          deliveryId: observation.deliveryId,
          generation: owner?.generation,
          ownerToken: owner?.ownerToken,
          ownerEpoch: owner?.ownerEpoch,
          result,
          status
        }
        return mutate<void>(
          (row, tx, done) => {
            const terminal = webRecordingTerminalOutcome(row)
            if (row.ownerState !== 'sealed' || terminal === null || terminal.deliveryId !== snapshot.deliveryId)
              throw invariant('terminal-observation-state')
            if (row.terminalObservation !== undefined) {
              if (webRecordingCanonical(row.terminalObservation) !== webRecordingCanonical(snapshot))
                throw invariant('terminal-observation-conflict')
              done(undefined)
              return
            }
            row.terminalObservation = snapshot
            webRecordingTerminalOutcome(row)
            tx.objectStore('recordings').put(row)
            done(undefined)
          },
          ['cleanup-pending'],
          true,
          onDispatched
        )
      } catch (error) {
        throw new WebRecordingNoncommittedError(error)
      }
    },
    retire: id => {
      if (!decimal(id) || id === '0') return Promise.reject(invalid('delivery-id'))
      return mutate<void>((row, tx, done, guard) => {
        if (row.ownerState !== 'live') throw invariant('retire-state')
        const store = tx.objectStore('pendingIdentities')
        const request = store.get(identity(d, id))
        request.onsuccess = () =>
          guard(() => {
            if (request.result !== undefined) {
              const pending = object(request.result)
              if (pending.retired === true) {
                if (row.lastRetiredId !== id) throw invariant('retire-identity')
                done(undefined)
                return
              }
              if (
                pending.recordingId !== d.id ||
                pending.producerIncarnation !== d.producerIncarnation ||
                pending.collectionId !== d.collectionId ||
                pending.sourceGeneration !== d.sourceGeneration ||
                pending.deliveryId !== id ||
                !safe(pending.ordinal, 1) ||
                typeof pending.payload !== 'string'
              )
                throw invalid('identity')
              if (!safe(row.pendingCount, 1)) throw invalid('pending-count')
              let wire: unknown
              try {
                wire = JSON.parse(pending.payload)
              } catch {
                throw invalid('identity')
              }
              if (webRecordingCanonical(wire) !== pending.payload) throw invalid('identity')
              verifyPending(
                d,
                row,
                tx,
                id,
                wire,
                pending.payload,
                pending,
                ordinal => {
                  const finish = (): void => {
                    if (!safe(pending.byteLength, 1, Number(row.pendingBytes))) throw invalid('pending-bytes')
                    const integrity = webRecordingReaderIntegrity(row)
                    if (ordinal <= Number(row.readerCursor)) store.delete(identity(d, id))
                    else {
                      pending.retired = true
                      store.put(pending)
                    }
                    row.pendingCount = Number(row.pendingCount) - 1
                    row.pendingBytes = Number(row.pendingBytes) - pending.byteLength
                    if ((row.pendingCount === 0) !== (row.pendingBytes === 0)) throw invalid('pending-bytes')
                    integrity.pendingCount -= 1
                    integrity.pendingBytes -= pending.byteLength
                    row.readerIntegrity = integrity
                    row.lastRetiredId = id
                    tx.objectStore('recordings').put(row)
                    done(undefined)
                  }
                  if (ordinal <= Number(row.readerCursor)) {
                    finish()
                    return
                  }
                  const records = tx.objectStore('records')
                  const record = records.get([d.id, ordinal])
                  record.onsuccess = () =>
                    guard(() => {
                      const stored = object(record.result)
                      if (stored.sourceRetired !== false) throw invariant('record-conflict')
                      stored.sourceRetired = true
                      records.put(stored)
                      finish()
                    })
                },
                guard
              )
            } else {
              if (row.lastRetiredId !== id) throw invariant('retire-identity')
              done(undefined)
            }
          })
      })
    },
    seal: () =>
      mutate<void>(
        (row, tx, done) => {
          if (webRecordingMarkerDebt(row) !== 0) throw invariant('marker-debt')
          if (row.ownerState === 'sealed') {
            const receipt = object(row.sealReceipt)
            if (
              receipt.version !== 1 ||
              receipt.generation !== owner?.generation ||
              receipt.ownerToken !== owner?.ownerToken ||
              receipt.ownerEpoch !== owner?.ownerEpoch ||
              receipt.deliveryId !== row.lastDeliveryId ||
              row.terminal !== true ||
              row.pendingCount !== 0
            )
              throw invariant('seal-receipt')
            done(undefined)
            return
          }
          if (
            row.ownerState !== 'live' ||
            row.terminal !== true ||
            row.pendingCount !== 0 ||
            typeof row.lastDeliveryId !== 'string'
          )
            throw invariant('seal-state')
          row.ownerState = 'sealed'
          row.sealReceipt = {
            version: 1,
            generation: owner?.generation,
            ownerToken: owner?.ownerToken,
            ownerEpoch: owner?.ownerEpoch,
            deliveryId: row.lastDeliveryId
          }
          tx.objectStore('recordings').put(row)
          done(undefined)
        },
        ['cleanup-pending']
      ),
    fail: async failure => {
      // This producer emits protocol v1; reject v2-only fields before a write.
      const snapshot = parseOrderedRecordingFailure(failure, d.context.version)
      return mutate<void>((row, tx, done) => {
        if ((row.ownerState !== 'prepared' && row.ownerState !== 'live') || row.terminal === true)
          throw invariant('failure-state')
        row.failure = snapshot
        tx.objectStore('recordings').put(row)
        done(undefined)
      })
    }
  }
}
