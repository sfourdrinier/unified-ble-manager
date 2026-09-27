import { contractError } from '../backend-contract/errors'
import {
  normalizeContinuationSelector,
  type BackgroundContinuationResubscribeSelector
} from '../backend-contract/continuation-selector'
import { parseDrainText, type WireDrainRecord } from '../backends/reactnative/rust-core-wire'
import { rehydratePublicPromise } from '../public/error-bridge'
import { utf8ByteLength } from '../backend-contract/serializable'
import { decodeNativeRecordingEnvelope, nativeContinuationTransportError } from './native-continuation-envelope'
import {
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
  readonly runtimeFailure: (ContinuationRecordingFailure & { readonly persisted: false }) | null
  readonly collectionFailure:
    | (ContinuationRecordingFailure & {
        readonly persisted: boolean
        readonly persistenceFailure?: ContinuationRecordingFailure
      })
    | null
}

export interface ContinuationRecordingAccess {
  status(id: string): Promise<unknown>
  prepare(id: string, maxItems: number, maxBytes: number): Promise<unknown>
  acknowledge(id: string, token: string): Promise<unknown>
  stop(id: string): Promise<unknown>
  clear(id: string): Promise<unknown>
}

/** Shared mobile/desktop transport adapter. The supplied access is already
 * identity-checked by its host; no radio session or implicit acknowledgement. */
export function createNativeContinuationRecordingController(
  access: ContinuationRecordingAccess,
  hostDomain: string
): ContinuationRecordingController {
  const call = async (operation: RecordingControlOperation, invoke: () => Promise<unknown>) => {
    try {
      return decodeNativeRecordingEnvelope(await invoke(), hostDomain, operation)
    } catch (error) {
      throw nativeContinuationTransportError(error, hostDomain, `continuation.recording.${operation}`)
    }
  }
  return createContinuationRecordingController({
    status: id => call('status', () => access.status(id)),
    prepare: (id, maxItems, maxBytes) => call('prepare', () => access.prepare(id, maxItems, maxBytes)),
    acknowledge: (id, token) => call('acknowledge', () => access.acknowledge(id, token)),
    stop: id => call('stop', () => access.stop(id)),
    clear: id => call('clear', () => access.clear(id))
  })
}

export interface ContinuationRecordingController {
  status(id: string): Promise<ContinuationRecordingStatus>
  prepare(id: string, options: ContinuationRecordingPrepareOptions): Promise<ContinuationRecordingBatch>
  /** Call only after the prepared records are safely handled by the consumer. */
  acknowledge(
    id: string,
    token: string
  ): Promise<{ readonly token: string; readonly acknowledged: true; readonly records: number }>
  /** Stops collection admission, not radio ownership. Retains all unacknowledged records. */
  stop(id: string): Promise<ContinuationRecordingStatus & { readonly radioRelease: 'not-requested' }>
  /** Explicitly deletes retained records from a stopped recording. */
  clear(id: string): Promise<{ readonly cleared: true; readonly records: number }>
}

export interface ContinuationRecordingMetadata {
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

function failure(value: Record<string, unknown>): ContinuationRecordingFailure {
  return Object.freeze({
    kind: text(value.kind),
    detail: text(value.detail),
    operation: text(value.operation),
    sqliteExtendedCode:
      value.sqliteExtendedCode === null ? null : integer(value.sqliteExtendedCode, -2147483648, 2147483647),
    sqliteCode: value.sqliteCode === null ? null : text(value.sqliteCode)
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
    'collectionFailure'
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
    exact(diagnostic, [...keys, 'persisted'])
    if (diagnostic.persisted !== false) return malformed()
    runtimeFailure = Object.freeze({ ...failure(diagnostic), persisted: false })
  }
  let collectionFailure: ContinuationRecordingStatus['collectionFailure'] = null
  if (value.collectionFailure !== null) {
    const diagnostic = object(value.collectionFailure)
    exact(diagnostic, [
      ...keys,
      'persisted',
      ...(Object.hasOwn(diagnostic, 'persistenceFailure') ? ['persistenceFailure'] : [])
    ])
    if (typeof diagnostic.persisted !== 'boolean') return malformed()
    let persistenceFailure: ContinuationRecordingFailure | undefined
    if (diagnostic.persistenceFailure !== undefined) {
      const nested = object(diagnostic.persistenceFailure)
      exact(nested, keys)
      if (diagnostic.persisted) return malformed()
      persistenceFailure = failure(nested)
    }
    collectionFailure = Object.freeze({
      ...failure(diagnostic),
      persisted: diagnostic.persisted,
      ...(persistenceFailure === undefined ? {} : { persistenceFailure })
    })
  }
  if (value.accepting !== (value.phase === 'recording' && collectionFailure === null)) return malformed()
  return Object.freeze({
    recordingId: id,
    phase: value.phase,
    accepting: value.accepting,
    records: integer(value.records, 0),
    bytes: integer(value.bytes, 0),
    lostRecords: integer(value.lostRecords, 0),
    maxBytes: integer(value.maxBytes, 1048576, 1073741824),
    maxRecords: integer(value.maxRecords, 1, 1000000),
    encrypted: false,
    runtimeFailure,
    collectionFailure
  })
}

function recordingId(value: string): string {
  if (typeof value !== 'string' || !/^[A-Za-z0-9_-]{1,64}$/.test(value)) {
    throw contractError('argument.invalid', 'restoration', 'continuation.recording.id')
  }
  return value
}

export function createContinuationRecordingController(
  access: ContinuationRecordingAccess
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
          return parseContinuationRecordingBatch(await access.prepare(id, limits.maxItems, limits.maxBytes), limits)
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
  limits: ContinuationRecordingPrepareOptions
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
  let previous: number | undefined
  let encodedBytes = 0
  const records = value.records.map(inputRecord => {
    const entry = object(inputRecord)
    exact(entry, ['ordinal', 'metadata', 'record'])
    encodedBytes += utf8ByteLength(JSON.stringify(entry))
    if (!Number.isSafeInteger(encodedBytes) || encodedBytes > limits.maxBytes) return malformed()
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
  if (encodedBytes !== bytes) return malformed()
  return Object.freeze({ token, records: Object.freeze(records), bytes, more: value.more })
}
