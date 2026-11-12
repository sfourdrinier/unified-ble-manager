// src/core/continuation-recording.ts

import {
  contractError,
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  optionalPlatformErrorDetail,
  type CleanupRecord
} from '../backend-contract/errors'
import {
  normalizeContinuationSelector,
  type BackgroundContinuationResubscribeSelector
} from '../backend-contract/continuation-selector'
import { parseDrainText, type WireDrainRecord } from './recording-wire'
import { rehydratePublicPromise } from '../public/error-bridge'
import { utf8ByteLength } from '../backend-contract/serializable'
import {
  parseOrderedRecordingBatch,
  parseOrderedRecordingContext,
  type OrderedRecordingContext,
  parseOrderedRecordingFailure,
  type OrderedRecordingBatch,
  type OrderedRecordingFailure
} from './ordered-recording'
import {
  decodeNativePreparedRecordingEnvelope,
  decodeNativeRecordingEnvelope,
  nativeContinuationTransportError
} from './native-continuation-envelope'
import {
  isRecordingId,
  MIN_RECORDING_BYTES,
  MAX_RECORDING_BYTES,
  MIN_RECORDING_RECORDS,
  MAX_RECORDING_RECORDS,
  MAX_RECORDING_BATCH_ITEMS,
  MAX_RECORDING_BATCH_BYTES,
  MAX_RECORDING_TOKEN_BYTES,
  type RecordingControlOperation
} from '../backend-contract/continuation-recording-bounds'

export interface ContinuationRecordingFailure {
  readonly kind: string
  readonly detail: string
  readonly operation: string
  readonly sqliteExtendedCode: number | null
  readonly sqliteCode: string | null
}

/** A retained diagnostic unavailable in the bounded status projection. */
export interface ContinuationRecordingDiagnosticMarker {
  readonly kind: 'storage.diagnostic-oversized' | 'storage.diagnostic-unreadable'
  readonly detail: string
  readonly operation: 'status'
  readonly persisted: true
  readonly payloadBytes: number
  readonly source: 'occurrence' | 'collectionSummary'
}

/** Versioned lifecycle of an ordered producer; a null generation means setup
 * had not yet established a native source. */
export interface OrderedRecordingOwnerStatus {
  readonly version: 1
  readonly producerIncarnation: string
  readonly collectionId: string
  readonly sourceGeneration: { readonly kind: 'counter' | 'opaque'; readonly value: string } | null
  readonly state: 'live' | 'sealed' | 'interrupted'
  readonly uncertainTail: true | null
}

export interface ContinuationRecordingStatus {
  readonly recordingId: string
  readonly phase: 'recording' | 'stopped' | 'capacity-reached'
  readonly accepting: boolean
  readonly records: number
  readonly bytes: number
  readonly lostRecords: number
  readonly maxBytes: number
  readonly maxRecords: number
  readonly encrypted: false
  readonly orderedOwner?: OrderedRecordingOwnerStatus
  readonly runtimeFailure:
    | (ContinuationRecordingFailure & { readonly persisted: boolean })
    | ContinuationRecordingDiagnosticMarker
    | null
  readonly collectionFailure:
    | (ContinuationRecordingFailure & {
        readonly persisted: boolean
        readonly persistenceFailure?: ContinuationRecordingFailure
        readonly knownRecordedLoss?: { readonly items: number; readonly bytes?: number }
        readonly retainedSummary?: ContinuationRecordingDiagnosticMarker
        /** Additional loss kept separately while the historical summary is unreadable. */
        readonly pendingRecordedLoss?: { readonly items: number; readonly bytes?: number; readonly persisted: boolean }
      })
    | ContinuationRecordingDiagnosticMarker
    | null
}

export interface ContinuationRecordingAccess {
  status(id: string): Promise<unknown>
  prepare(id: string, maxItems: number, maxBytes: number): Promise<unknown>
  acknowledge(id: string, token: string): Promise<unknown>
  stop(id: string): Promise<unknown>
  /** Receipt version offered by this control boundary before stop dispatch. */
  orderedReceiptVersion?(id: string): Promise<number>
  stopOrdered?(id: string): Promise<unknown>
  orderedContext?(id: string): Promise<unknown>
  clear(id: string): Promise<unknown>
}

/** Shared mobile/desktop transport adapter. The supplied access is already
 * identity-checked by its host; no radio session or implicit acknowledgement. */
export function createNativeContinuationRecordingController(
  access: ContinuationRecordingAccess,
  hostDomain: string,
  receiptVersion?: (id: string) => Promise<number>
): ContinuationRecordingController {
  const call = async (operation: RecordingControlOperation, invoke: () => Promise<unknown>) => {
    try {
      return decodeNativeRecordingEnvelope(await invoke(), hostDomain, operation)
    } catch (error) {
      throw nativeContinuationTransportError(error, hostDomain, `continuation.recording.${operation}`)
    }
  }
  const stopOrdered = access.stopOrdered?.bind(access)
  const orderedContext = access.orderedContext?.bind(access)
  const orderedReceiptVersion = receiptVersion ?? access.orderedReceiptVersion?.bind(access)
  return createContinuationRecordingController(
    {
      status: id => call('status', () => access.status(id)),
      prepare: async (id, maxItems, maxBytes) => {
        try {
          return decodeNativePreparedRecordingEnvelope(await access.prepare(id, maxItems, maxBytes), hostDomain)
        } catch (error) {
          throw nativeContinuationTransportError(error, hostDomain, 'continuation.recording.prepare')
        }
      },
      acknowledge: (id, token) => call('acknowledge', () => access.acknowledge(id, token)),
      stop: id => call('stop', () => access.stop(id)),
      ...(orderedReceiptVersion ? { orderedReceiptVersion } : {}),
      ...(stopOrdered ? { stopOrdered: (id: string) => call('stop-ordered', () => stopOrdered(id)) } : {}),
      ...(orderedContext ? { orderedContext: (id: string) => call('context-ordered', () => orderedContext(id)) } : {}),
      clear: id => call('clear', () => access.clear(id))
    },
    true
  )
}

export type OrderedRecordingStopFailure =
  | { readonly kind: 'source'; readonly source: OrderedRecordingFailure }
  | { readonly kind: 'storage'; readonly storage: ContinuationRecordingFailure }
  | {
      readonly kind: 'storage-cleanup'
      readonly storage: ContinuationRecordingFailure
      readonly source: OrderedRecordingFailure
    }
  | {
      readonly kind:
        | 'invalid-delivery'
        | 'invalid-identity'
        | 'worker-failed'
        | 'missing-terminal'
        | 'entropy-unavailable'
        | 'receipt-retired'
        | 'recording-not-accepting'
    }

export type OrderedRecordingStopReceipt =
  | {
      readonly version: 1 | 2
      readonly state: 'finished' | 'waiting-capacity' | 'waiting-source' | 'waiting-drain' | 'interrupted'
      readonly error: null
    }
  | { readonly version: 1 | 2; readonly state: 'failed'; readonly error: OrderedRecordingStopFailure }

export interface OrderedRecordingHostStopResult {
  readonly recording: OrderedRecordingStopReceipt
  readonly hostCleanup:
    | CleanupRecord
    | { readonly state: 'pending' }
    | { readonly state: 'uncertain' }
    | { readonly state: 'not-applicable' }
}

export interface ContinuationRecordingController {
  status(id: string): Promise<ContinuationRecordingStatus>
  prepare(id: string, options: ContinuationRecordingPrepareOptions): Promise<ContinuationRecordingBatch>
  /** Uses the same retained cursor as prepare; decodes ordered rows only. */
  prepareOrdered(id: string, options: ContinuationRecordingPrepareOptions): Promise<OrderedRecordingBatch>
  /** Call only after the prepared records are safely handled by the consumer. */
  acknowledge(
    id: string,
    token: string
  ): Promise<{ readonly token: string; readonly acknowledged: true; readonly records: number }>
  /** Stops collection admission, not radio ownership. Retains all unacknowledged records. */
  stop(id: string): Promise<ContinuationRecordingStatus & { readonly radioRelease: 'not-requested' }>
  /** Requests ordered source shutdown; pending states retain ownership and may be retried. */
  stopOrdered(id: string): Promise<OrderedRecordingHostStopResult>
  contextOrdered(id: string): Promise<OrderedRecordingContext>
  /** Explicit logical record deletion, not file deletion, compaction or secure erase.
   * Requires a stopped recording; does not release radio ownership. */
  clear(id: string): Promise<{ readonly cleared: true; readonly records: number }>
}

export interface ContinuationRecordingMetadata {
  /** Session context and durable ordering; individual value receipts are carried on their records when available. */
  readonly session: {
    readonly peerId: string
    readonly sessionId: string
    readonly backendInstanceId: string
    readonly sessionStartedAtUnixNs: string
    readonly sessionEpoch: string
  }
  readonly consumer: {
    readonly consumer: string
    readonly peerId: string
    readonly connectionGeneration: string
    readonly databaseGeneration: string
    readonly selector: BackgroundContinuationResubscribeSelector
  } | null
}

export interface ContinuationRecordingBatch {
  /** A prepared prefix is retained until this exact token is explicitly acknowledged. */
  readonly token: string | null
  readonly records: readonly {
    readonly ordinal: number
    readonly metadata: ContinuationRecordingMetadata
    readonly record:
      | WireDrainRecord
      | { readonly t: 'consumer-registration'; readonly consumer: string; readonly ordinal: number }
  }[]
  /** Serialized journal bytes, not the decoded payload size. */
  readonly bytes: number
  readonly more: boolean
}

export interface ContinuationRecordingPrepareOptions {
  readonly maxItems: number
  readonly maxBytes: number
}

function malformed(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.recording')
}

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return malformed()
  return Object.fromEntries(Object.entries(value))
}

function exact(value: Record<string, unknown>, keys: readonly string[]) {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) malformed()
}

function text(value: unknown, maximum = 1024): string {
  if (typeof value !== 'string' || value.length === 0 || value.length > maximum) return malformed()
  return value
}

function integer(value: unknown, minimum: number, maximum = Number.MAX_SAFE_INTEGER): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < minimum || value > maximum)
    return malformed()
  return value
}

function failure(value: Record<string, unknown>, maxBytes = 1024): ContinuationRecordingFailure {
  const result = {
    kind: text(value.kind, maxBytes),
    detail: text(value.detail, maxBytes),
    operation: text(value.operation, maxBytes),
    sqliteExtendedCode:
      value.sqliteExtendedCode === null ? null : integer(value.sqliteExtendedCode, -2147483648, 2147483647),
    sqliteCode: value.sqliteCode === null ? null : text(value.sqliteCode, maxBytes)
  }
  if (
    [result.kind, result.detail, result.operation, result.sqliteCode ?? ''].reduce((n, s) => n + utf8ByteLength(s), 0) >
    maxBytes
  )
    return malformed()
  return Object.freeze(result)
}

function diagnosticMarker(
  input: Record<string, unknown>,
  source: ContinuationRecordingDiagnosticMarker['source']
): ContinuationRecordingDiagnosticMarker {
  exact(input, ['kind', 'detail', 'operation', 'persisted', 'payloadBytes', 'source'])
  if (
    (input.kind !== 'storage.diagnostic-oversized' && input.kind !== 'storage.diagnostic-unreadable') ||
    input.operation !== 'status' ||
    input.persisted !== true ||
    input.source !== source
  )
    return malformed()
  return Object.freeze({
    kind: input.kind,
    detail: text(input.detail),
    operation: input.operation,
    persisted: true,
    payloadBytes: integer(input.payloadBytes, input.kind === 'storage.diagnostic-oversized' ? 65537 : 0),
    source
  })
}

/** Version 2 must be explicitly selected by a boundary that negotiated it. */
export function parseOrderedRecordingStopReceipt(
  input: unknown,
  expectedVersion: 1 | 2 = 1
): OrderedRecordingStopReceipt {
  const value = object(input)
  exact(value, ['version', 'state', 'error'])
  if ((expectedVersion !== 1 && expectedVersion !== 2) || value.version !== expectedVersion) return malformed()
  if (
    value.state === 'finished' ||
    value.state === 'waiting-capacity' ||
    value.state === 'waiting-source' ||
    value.state === 'waiting-drain' ||
    value.state === 'interrupted'
  ) {
    if (value.error !== null) return malformed()
    return Object.freeze({ version: expectedVersion, state: value.state, error: null })
  }
  if (value.state !== 'failed') return malformed()
  return Object.freeze({
    version: expectedVersion,
    state: 'failed',
    error: parseOrderedRecordingStopFailure(value.error, expectedVersion)
  })
}

export function parseOrderedRecordingStopFailure(input: unknown, version: 1 | 2 = 1): OrderedRecordingStopFailure {
  if (version !== 1 && version !== 2) return malformed()
  const detail = object(input)
  let error: OrderedRecordingStopFailure
  if (detail.kind === 'source') {
    exact(detail, ['kind', 'source'])
    error = { kind: detail.kind, source: parseOrderedRecordingFailure(detail.source, version) }
  } else if (detail.kind === 'storage' || detail.kind === 'storage-cleanup') {
    exact(detail, detail.kind === 'storage' ? ['kind', 'storage'] : ['kind', 'storage', 'source'])
    const storage = object(detail.storage)
    exact(storage, ['kind', 'detail', 'operation', 'sqliteExtendedCode', 'sqliteCode'])
    error =
      detail.kind === 'storage'
        ? { kind: detail.kind, storage: failure(storage) }
        : { kind: detail.kind, storage: failure(storage), source: parseOrderedRecordingFailure(detail.source, version) }
  } else if (
    detail.kind === 'invalid-delivery' ||
    detail.kind === 'invalid-identity' ||
    detail.kind === 'worker-failed' ||
    detail.kind === 'missing-terminal' ||
    detail.kind === 'entropy-unavailable' ||
    detail.kind === 'receipt-retired' ||
    detail.kind === 'recording-not-accepting'
  ) {
    exact(detail, ['kind'])
    error = { kind: detail.kind }
  } else return malformed()
  return Object.freeze(error)
}

export function parseOrderedRecordingHostCleanup(input: unknown): OrderedRecordingHostStopResult['hostCleanup'] {
  const value = object(input)
  if (value.state === 'pending' || value.state === 'uncertain' || value.state === 'not-applicable') {
    exact(value, ['state'])
    return Object.freeze({ state: value.state })
  }
  exact(value, ['state', 'failures'])
  if ((value.state !== 'released' && value.state !== 'release-failed') || !Array.isArray(value.failures))
    return malformed()
  const failures = value.failures.map(item => {
    const entry = object(item)
    exact(entry, ['resourceKind', 'error'])
    const error = object(entry.error)
    const keys = [
      'code',
      'domain',
      'operation',
      'platform',
      'retryability',
      ...(Object.hasOwn(error, 'commit') ? ['commit'] : [])
    ]
    exact(error, keys)
    const code = BLE_ERROR_CODES.find(candidate => candidate === error.code)
    const domain = BLE_ERROR_DOMAINS.find(candidate => candidate === error.domain)
    if (
      !code ||
      !domain ||
      typeof error.operation !== 'string' ||
      !error.operation ||
      (error.retryability !== 'never' && error.retryability !== 'caller-decides') ||
      (Object.hasOwn(error, 'commit') &&
        error.commit !== null &&
        error.commit !== 'not-dispatched' &&
        error.commit !== 'uncertain')
    )
      return malformed()
    const platform = optionalPlatformErrorDetail(error.platform, 'continuation.recording.host-cleanup') ?? null
    const commit: { readonly commit?: import('../backend-contract/errors').BleCommitUncertainty | null } =
      error.commit === 'not-dispatched' || error.commit === 'uncertain' || error.commit === null
        ? { commit: error.commit }
        : {}
    return Object.freeze({
      resourceKind: text(entry.resourceKind),
      error: Object.freeze({
        code,
        domain,
        operation: error.operation,
        platform,
        retryability: error.retryability,
        ...commit
      })
    })
  })
  if ((value.state === 'released') !== (failures.length === 0)) return malformed()
  return Object.freeze({ state: value.state, failures: Object.freeze(failures) })
}

function unsigned64(value: unknown): string {
  const result = text(value, 20)
  if (!/^(0|[1-9][0-9]*)$/.test(result) || BigInt(result) > 18446744073709551615n) malformed()
  return result
}

function orderedOwner(input: unknown): OrderedRecordingOwnerStatus {
  const owner = object(input)
  exact(owner, ['version', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'state', 'uncertainTail'])
  if (owner.version !== 1 || (owner.state !== 'live' && owner.state !== 'sealed' && owner.state !== 'interrupted'))
    return malformed()
  if (owner.uncertainTail !== (owner.state === 'interrupted' ? true : null)) return malformed()
  let sourceGeneration: OrderedRecordingOwnerStatus['sourceGeneration'] = null
  if (owner.sourceGeneration !== null) {
    const generation = object(owner.sourceGeneration)
    exact(generation, ['kind', 'value'])
    if (generation.kind !== 'counter' && generation.kind !== 'opaque') return malformed()
    sourceGeneration = Object.freeze({
      kind: generation.kind,
      value: generation.kind === 'counter' ? unsigned64(generation.value) : text(generation.value, 256)
    })
  }
  return Object.freeze({
    version: 1,
    producerIncarnation: text(owner.producerIncarnation, 256),
    collectionId: unsigned64(owner.collectionId),
    sourceGeneration,
    state: owner.state,
    uncertainTail: owner.state === 'interrupted' ? true : null
  })
}

function status(input: unknown, id: string): ContinuationRecordingStatus {
  const value = object(input)
  exact(value, [
    'recordingId',
    'phase',
    'accepting',
    'records',
    'bytes',
    'lostRecords',
    'maxBytes',
    'maxRecords',
    'encrypted',
    'runtimeFailure',
    'collectionFailure',
    ...(Object.hasOwn(value, 'orderedOwner') ? ['orderedOwner'] : [])
  ])
  if (
    value.recordingId !== id ||
    value.encrypted !== false ||
    typeof value.accepting !== 'boolean' ||
    (value.phase !== 'recording' && value.phase !== 'stopped' && value.phase !== 'capacity-reached')
  )
    return malformed()
  const keys = ['kind', 'detail', 'operation', 'sqliteExtendedCode', 'sqliteCode']
  let runtimeFailure: ContinuationRecordingStatus['runtimeFailure'] = null
  if (value.runtimeFailure !== null) {
    const diagnostic = object(value.runtimeFailure)
    if (diagnostic.kind === 'storage.diagnostic-oversized' || diagnostic.kind === 'storage.diagnostic-unreadable')
      runtimeFailure = diagnosticMarker(diagnostic, 'occurrence')
    else {
      exact(diagnostic, [...keys, 'persisted'])
      if (typeof diagnostic.persisted !== 'boolean') return malformed()
      runtimeFailure = Object.freeze({ ...failure(diagnostic, 65536), persisted: diagnostic.persisted })
    }
  }
  let collectionFailure: ContinuationRecordingStatus['collectionFailure'] = null
  if (value.collectionFailure !== null) {
    const diagnostic = object(value.collectionFailure)
    if (diagnostic.kind === 'storage.diagnostic-oversized' || diagnostic.kind === 'storage.diagnostic-unreadable') {
      collectionFailure = diagnosticMarker(diagnostic, 'collectionSummary')
    } else {
      exact(diagnostic, [
        ...keys,
        'persisted',
        ...(Object.hasOwn(diagnostic, 'persistenceFailure') ? ['persistenceFailure'] : []),
        ...(Object.hasOwn(diagnostic, 'knownRecordedLoss') ? ['knownRecordedLoss'] : []),
        ...(Object.hasOwn(diagnostic, 'retainedSummary') ? ['retainedSummary'] : []),
        ...(Object.hasOwn(diagnostic, 'pendingRecordedLoss') ? ['pendingRecordedLoss'] : [])
      ])
      if (typeof diagnostic.persisted !== 'boolean') return malformed()
      let persistenceFailure: ContinuationRecordingFailure | undefined
      if (diagnostic.persistenceFailure !== undefined) {
        const nested = object(diagnostic.persistenceFailure)
        exact(nested, keys)
        if (diagnostic.persisted) return malformed()
        persistenceFailure = failure(nested, 65536)
      }
      let knownRecordedLoss: { readonly items: number; readonly bytes?: number } | undefined
      if (diagnostic.knownRecordedLoss !== undefined) {
        const loss = object(diagnostic.knownRecordedLoss)
        exact(loss, ['items', ...(Object.hasOwn(loss, 'bytes') ? ['bytes'] : [])])
        knownRecordedLoss = Object.freeze({
          items: integer(loss.items, 0),
          ...(Object.hasOwn(loss, 'bytes') ? { bytes: integer(loss.bytes, 0) } : {})
        })
      }
      let pendingRecordedLoss:
        | { readonly items: number; readonly bytes?: number; readonly persisted: boolean }
        | undefined
      if (Object.hasOwn(diagnostic, 'pendingRecordedLoss')) {
        const loss = object(diagnostic.pendingRecordedLoss)
        exact(loss, ['items', ...(Object.hasOwn(loss, 'bytes') ? ['bytes'] : []), 'persisted'])
        if (
          typeof loss.persisted !== 'boolean' ||
          (diagnostic.persisted && !loss.persisted) ||
          !Object.hasOwn(diagnostic, 'retainedSummary') ||
          knownRecordedLoss !== undefined
        )
          return malformed()
        pendingRecordedLoss = Object.freeze({
          items: integer(loss.items, 0),
          ...(Object.hasOwn(loss, 'bytes') ? { bytes: integer(loss.bytes, 0) } : {}),
          persisted: loss.persisted
        })
      }
      collectionFailure = Object.freeze({
        ...failure(diagnostic, 65536),
        persisted: diagnostic.persisted,
        ...(persistenceFailure === undefined ? {} : { persistenceFailure }),
        ...(knownRecordedLoss === undefined ? {} : { knownRecordedLoss }),
        ...(pendingRecordedLoss === undefined ? {} : { pendingRecordedLoss }),
        ...(Object.hasOwn(diagnostic, 'retainedSummary')
          ? { retainedSummary: diagnosticMarker(object(diagnostic.retainedSummary), 'collectionSummary') }
          : {})
      })
      if (
        Object.hasOwn(diagnostic, 'retainedSummary') &&
        (knownRecordedLoss !== undefined || (diagnostic.persisted && pendingRecordedLoss?.persisted !== true))
      )
        return malformed()
    }
  }
  if (value.accepting !== (value.phase === 'recording' && collectionFailure === null)) return malformed()
  return Object.freeze({
    recordingId: id,
    phase: value.phase,
    accepting: value.accepting,
    records: integer(value.records, 0),
    bytes: integer(value.bytes, 0),
    lostRecords: integer(value.lostRecords, 0),
    maxBytes: integer(value.maxBytes, MIN_RECORDING_BYTES, MAX_RECORDING_BYTES),
    maxRecords: integer(value.maxRecords, MIN_RECORDING_RECORDS, MAX_RECORDING_RECORDS),
    encrypted: false,
    ...(Object.hasOwn(value, 'orderedOwner') ? { orderedOwner: orderedOwner(value.orderedOwner) } : {}),
    runtimeFailure,
    collectionFailure
  })
}

function recordingId(value: string): string {
  if (!isRecordingId(value)) {
    throw contractError('argument.invalid', 'restoration', 'continuation.recording.id')
  }
  return value
}

export function createContinuationRecordingController(
  access: ContinuationRecordingAccess,
  nativePreparedBytes = false
): ContinuationRecordingController {
  return Object.freeze({
    status: (id: string) => rehydratePublicPromise((async () => status(await access.status(recordingId(id)), id))()),
    stop: (id: string) =>
      rehydratePublicPromise(
        (async () => {
          const { radioRelease, ...snapshot } = object(await access.stop(recordingId(id)))
          if (radioRelease !== 'not-requested') return malformed()
          const result = status(snapshot, id)
          if (result.phase !== 'stopped' || result.accepting) return malformed()
          return Object.freeze({ ...result, radioRelease })
        })()
      ),
    contextOrdered: (id: string) =>
      rehydratePublicPromise(
        (async () => {
          recordingId(id)
          if (!access.orderedContext)
            throw contractError('capability.unsupported', 'restoration', 'continuation.recording.context-ordered')
          return parseOrderedRecordingContext(await access.orderedContext(id))
        })()
      ),
    stopOrdered: (id: string) =>
      rehydratePublicPromise(
        (async () => {
          recordingId(id)
          if (!access.stopOrdered)
            throw contractError('capability.unsupported', 'restoration', 'continuation.recording.stop-ordered')
          const version = await access.orderedReceiptVersion?.(id)
          if (version !== 1 && version !== 2)
            throw contractError('protocol.incompatible', 'boundary', 'continuation.recording.stop-ordered.version')
          const value = object(await access.stopOrdered(id))
          exact(value, ['recording', 'hostCleanup'])
          return Object.freeze({
            recording: parseOrderedRecordingStopReceipt(value.recording, version),
            hostCleanup: parseOrderedRecordingHostCleanup(value.hostCleanup)
          })
        })()
      ),
    prepare: (id: string, options: ContinuationRecordingPrepareOptions) =>
      rehydratePublicPromise(
        (async () => {
          recordingId(id)
          if (
            !options ||
            !Number.isSafeInteger(options.maxItems) ||
            options.maxItems < 1 ||
            options.maxItems > MAX_RECORDING_BATCH_ITEMS ||
            !Number.isSafeInteger(options.maxBytes) ||
            options.maxBytes < 1 ||
            options.maxBytes > MAX_RECORDING_BATCH_BYTES
          ) {
            throw contractError('argument.invalid', 'restoration', 'continuation.recording.bounds')
          }
          const limits = { maxItems: options.maxItems, maxBytes: options.maxBytes }
          const prepared = await access.prepare(id, limits.maxItems, limits.maxBytes)
          if (!nativePreparedBytes) return parseContinuationRecordingBatch(prepared, limits)
          const native = object(prepared)
          exact(native, ['value', 'serializedRecordBytes'])
          return parseContinuationRecordingBatch(
            native.value,
            limits,
            integer(native.serializedRecordBytes, 0, limits.maxBytes)
          )
        })()
      ),
    prepareOrdered: (id: string, options: ContinuationRecordingPrepareOptions) =>
      rehydratePublicPromise(
        (async () => {
          recordingId(id)
          if (
            !options ||
            !Number.isSafeInteger(options.maxItems) ||
            options.maxItems < 1 ||
            options.maxItems > MAX_RECORDING_BATCH_ITEMS ||
            !Number.isSafeInteger(options.maxBytes) ||
            options.maxBytes < 1 ||
            options.maxBytes > MAX_RECORDING_BATCH_BYTES
          )
            throw contractError('argument.invalid', 'restoration', 'continuation.recording.bounds')
          const limits = { maxItems: options.maxItems, maxBytes: options.maxBytes }
          const prepared = await access.prepare(id, limits.maxItems, limits.maxBytes)
          if (!nativePreparedBytes) return parseOrderedRecordingBatch(prepared, id, limits)
          const native = object(prepared)
          exact(native, ['value', 'serializedRecordBytes'])
          return parseOrderedRecordingBatch(
            native.value,
            id,
            limits,
            integer(native.serializedRecordBytes, 0, limits.maxBytes)
          )
        })()
      ),
    acknowledge: (id: string, token: string) =>
      rehydratePublicPromise(
        (async () => {
          recordingId(id)
          if (typeof token !== 'string' || token.length === 0 || utf8ByteLength(token) > MAX_RECORDING_TOKEN_BYTES) {
            throw contractError('argument.invalid', 'restoration', 'continuation.recording.token')
          }
          const value = object(await access.acknowledge(id, token))
          exact(value, ['token', 'acknowledged', 'records'])
          if (value.token !== token || value.acknowledged !== true) return malformed()
          return Object.freeze({ token, acknowledged: value.acknowledged, records: integer(value.records, 0) })
        })()
      ),
    clear: (id: string) =>
      rehydratePublicPromise(
        (async () => {
          const value = object(await access.clear(recordingId(id)))
          exact(value, ['cleared', 'records'])
          if (value.cleared !== true) return malformed()
          return Object.freeze({ cleared: value.cleared, records: integer(value.records, 0) })
        })()
      )
  })
}

function metadata(input: unknown): ContinuationRecordingMetadata {
  const value = object(input)
  exact(value, ['session', 'consumer'])
  const source = object(value.session)
  exact(source, ['peerId', 'sessionId', 'backendInstanceId', 'sessionStartedAtUnixNs', 'sessionEpoch'])
  const timestamp = text(source.sessionStartedAtUnixNs, 32)
  if (!/^[0-9]+$/.test(timestamp)) return malformed()
  const session = Object.freeze({
    peerId: text(source.peerId),
    sessionId: text(source.sessionId),
    backendInstanceId: text(source.backendInstanceId),
    sessionStartedAtUnixNs: timestamp,
    sessionEpoch: text(source.sessionEpoch, 4096)
  })
  if (value.consumer === null) return Object.freeze({ session, consumer: null })
  const consumer = object(value.consumer)
  exact(consumer, ['consumer', 'peerId', 'connectionGeneration', 'databaseGeneration', 'selector'])
  if (consumer.peerId !== session.peerId) return malformed()
  let selector: BackgroundContinuationResubscribeSelector
  try {
    selector = normalizeContinuationSelector(consumer.selector)
  } catch {
    return malformed()
  }
  return Object.freeze({
    session,
    consumer: Object.freeze({
      consumer: text(consumer.consumer, 256),
      peerId: session.peerId,
      connectionGeneration: text(consumer.connectionGeneration),
      databaseGeneration: text(consumer.databaseGeneration),
      selector
    })
  })
}

/** Decode the entire prepared prefix before returning it. This never acknowledges
 * storage, stops collection, releases radio ownership, or writes a file. */
export function parseContinuationRecordingBatch(
  input: unknown,
  limits: ContinuationRecordingPrepareOptions,
  serializedRecordBytes?: number
): ContinuationRecordingBatch {
  const value = object(input)
  exact(value, ['token', 'records', 'bytes', 'more'])
  const bytes = integer(value.bytes, 0, limits.maxBytes)
  if (!Array.isArray(value.records) || value.records.length > limits.maxItems || typeof value.more !== 'boolean')
    return malformed()
  const token = value.token === null ? null : text(value.token, MAX_RECORDING_TOKEN_BYTES)
  if (token !== null && utf8ByteLength(token) > MAX_RECORDING_TOKEN_BYTES) return malformed()
  if ((value.records.length === 0) !== (token === null)) return malformed()
  if (token === null && (bytes !== 0 || value.more)) return malformed()
  const decoded = parseContinuationRecordingRecords(value.records, limits, serializedRecordBytes)
  if (decoded.bytes !== bytes) return malformed()
  return Object.freeze({ token, records: decoded.records, bytes, more: value.more })
}

/** Internal canonical decoding for retained raw journal rows. No prepared-token,
 * acknowledgement, storage or radio authority is created by this pure decoder. */
export function parseContinuationRecordingRecords(
  input: unknown,
  limits: ContinuationRecordingPrepareOptions,
  serializedRecordBytes?: number
): Pick<ContinuationRecordingBatch, 'records' | 'bytes'> {
  if (!Array.isArray(input) || input.length > limits.maxItems) return malformed()
  if (serializedRecordBytes !== undefined) integer(serializedRecordBytes, 0, limits.maxBytes)
  let previous: number | undefined
  let encodedBytes = 0
  const records = input.map(inputRecord => {
    const entry = object(inputRecord)
    exact(entry, ['ordinal', 'metadata', 'record'])
    encodedBytes += utf8ByteLength(JSON.stringify(entry))
    if (!Number.isSafeInteger(encodedBytes) || (serializedRecordBytes === undefined && encodedBytes > limits.maxBytes))
      return malformed()
    const ordinal = integer(entry.ordinal, 1)
    if (previous !== undefined && ordinal !== previous + 1) return malformed()
    previous = ordinal
    const context = metadata(entry.metadata)
    const source = object(entry.record)
    if (source.t === 'consumer-registration') {
      exact(source, ['t', 'consumer'])
      const consumer = text(source.consumer, 256)
      if (context.consumer?.consumer !== consumer) return malformed()
      return Object.freeze({ ordinal, metadata: context, record: Object.freeze({ t: source.t, consumer, ordinal }) })
    }
    // Journal ordinals form their own durable sequence; source outbox ordinals
    // are not stored. Reuse the canonical native record decoder, not a second codec.
    if (Object.hasOwn(source, 'ordinal')) return malformed()
    const decoded = parseDrainText(
      JSON.stringify({ more: false, controlLost: 0, records: [{ ...source, ordinal }] }),
      null
    )
    if (!decoded.ok) return malformed()
    const record = decoded.value.records[0]
    if (record === undefined) return malformed()
    if ('consumer' in record && context.consumer?.consumer !== record.consumer) return malformed()
    if ('peerId' in record && record.peerId !== context.session.peerId) return malformed()
    return Object.freeze({ ordinal, metadata: context, record })
  })
  return Object.freeze({ records: Object.freeze(records), bytes: serializedRecordBytes ?? encodedBytes })
}
