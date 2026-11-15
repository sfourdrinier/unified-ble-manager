import { contractError } from '../backend-contract/errors'
import type {
  BleErrorCode,
  BleErrorDomain,
  BackendContractError,
  PlatformErrorDetail
} from '../backend-contract/errors'
import { snapshotSerializableRecord } from '../backend-contract/serializable'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'

// The identity is held privately, not on the error or its prototype. Store a
// separate snapshot so even mutation of the original error cannot change it.
const origins = new WeakMap<object, BackendContractError>()

export function webRecordingContractError(
  code: BleErrorCode,
  domain: BleErrorDomain,
  operation: string,
  platform: PlatformErrorDetail | null = null
): BackendContractError {
  const error = contractError(code, domain, operation, platform)
  const detail =
    platform === null
      ? null
      : {
          domain: platform.domain,
          code: platform.code,
          safeMessage: platform.safeMessage,
          metadata: snapshotSerializableRecord(platform.metadata).value,
          ...(platform.metadataEntries === undefined
            ? {}
            : {
                metadataEntries: platform.metadataEntries.map(entry => ({ key: entry.key, value: { ...entry.value } }))
              })
        }
  origins.set(error, contractError(code, domain, operation, detail))
  return error
}

export function trustedWebRecordingError(value: unknown): BackendContractError | undefined {
  if ((typeof value !== 'object' || value === null) && typeof value !== 'function') return undefined
  const snapshot = origins.get(value)
  if (snapshot === undefined) return undefined
  const n = snapshot.normalized
  const platform =
    n.platform === null
      ? null
      : {
          ...n.platform,
          metadata: snapshotSerializableRecord(n.platform.metadata).value,
          ...(n.platform.metadataEntries === undefined
            ? {}
            : {
                metadataEntries: n.platform.metadataEntries.map(entry => ({
                  key: entry.key,
                  value: { ...entry.value }
                }))
              })
        }
  return webRecordingContractError(n.code, n.domain, n.operation, platform)
}

// Names are the only browser-supplied text that may cross this boundary.
const storageNames = new Set([
  'Error',
  'AggregateError',
  'BackendContractError',
  'AbortError',
  'ConstraintError',
  'DataError',
  'InvalidStateError',
  'NotFoundError',
  'NotAllowedError',
  'NetworkError',
  'NotSupportedError',
  'QuotaExceededError',
  'ReadOnlyError',
  'SecurityError',
  'TransactionInactiveError',
  'UnknownError',
  'VersionError',
  'WebRecordingOutcomeTimeout',
  'WebRecordingOutcomeUnknown',
  'WebRecordingAuthorityLost'
])
export function webStorageErrorName(error: unknown): string {
  try {
    if ((typeof error !== 'object' || error === null) && typeof error !== 'function') return 'UnknownError'
    const name: unknown = Reflect.get(error, 'name')
    return typeof name === 'string' && storageNames.has(name) ? name : 'UnknownError'
  } catch {
    return 'UnknownError'
  }
}

// Error text and arbitrary names can contain client data. Only publish a fixed
// vocabulary and the shape of the failure graph, never host-provided text.
function safeProperty(value: unknown, key: string): unknown {
  try {
    return (typeof value === 'object' && value !== null) || typeof value === 'function'
      ? Reflect.get(value, key)
      : undefined
  } catch {
    return undefined
  }
}
/** A single node budget covers both failures. At most 64 nodes and one
 * omission per edge, with fixed ASCII field names and values, keep the full
 * encoded JSON well below 1 MiB, including JSON escaping and envelope fields. */
export function webHostErrorEvidence(errors: readonly unknown[], maximumNodes = 64): SerializableRecord {
  const budget = { remaining: maximumNodes }
  const active = new Set<unknown>()
  const project = (error: unknown, depth: number, reserve: number): SerializableRecord => {
    if (budget.remaining <= reserve) return { omitted: 'error evidence limit' }
    budget.remaining--
    const name = webStorageErrorName(error)
    if ((typeof error !== 'object' || error === null) && typeof error !== 'function') return { name }
    if (active.has(error)) return { name, omitted: 'cyclic error' }
    if (depth >= 16) return { name, omitted: 'error evidence depth limit' }
    active.add(error)
    try {
      const evidence: Record<string, SerializableValue> = { name }
      const children = safeProperty(error, 'errors')
      let array = false
      try {
        array = Array.isArray(children)
      } catch {
        /* revoked proxy */
      }
      if (array) {
        const length = safeProperty(children, 'length')
        if (typeof length === 'number' && Number.isSafeInteger(length) && length >= 0) {
          const items: SerializableRecord[] = []
          for (let i = 0; i < length && budget.remaining > reserve; i++)
            items.push(project(safeProperty(children, String(i)), depth + 1, reserve))
          evidence.errors = items
          if (items.length < length) evidence.errorsOmitted = true
        } else evidence.omitted = 'unavailable error evidence'
      }
      const cause = safeProperty(error, 'cause')
      if (cause !== undefined) evidence.cause = project(cause, depth + 1, reserve)
      return evidence
    } finally {
      active.delete(error)
    }
  }
  const projected = errors.map((error, index) => project(error, 0, errors.length - index - 1))
  return { errors: projected }
}

export function webStorageError(error: unknown, operation: string): BackendContractError {
  const combined = queueAbortEvidence(error)
  const primary = combined === undefined ? error : undefined
  const primaryName = combined?.name ?? webStorageErrorName(primary)
  const typed = combined?.typed ?? trustedWebRecordingError(primary)
  const code =
    typed !== undefined
      ? typed.normalized.code
      : primaryName === 'VersionError'
        ? 'protocol.incompatible'
        : primaryName === 'QuotaExceededError'
          ? 'stream.quota'
          : 'capability.unavailable'
  return webRecordingContractError(
    code,
    typed?.normalized.domain ?? 'restoration',
    typed?.normalized.operation ?? `web-recording.${operation}`,
    {
      domain: combined?.domain ?? 'indexeddb',
      code: combined === undefined ? webStorageErrorName(error) : 'AggregateError',
      safeMessage:
        combined?.domain === 'web-recording-authority'
          ? 'Recording authority setup and release failed'
          : 'Browser storage operation failed',
      metadata: combined === undefined ? {} : combined.evidence
    }
  )
}

// Only the transaction owner can register a combined failure. Capture its
// classification and bounded graph immediately, before host objects mutate.
const combinedOrigins = new WeakMap<
  object,
  {
    name: string
    typed: BackendContractError | undefined
    evidence: SerializableRecord
    domain: 'indexeddb' | 'web-recording-authority'
  }
>()
export class WebIndexedDbQueueAbortError extends AggregateError {
  constructor(
    readonly queueError: unknown,
    abortError: unknown
  ) {
    super([queueError, abortError], 'Failed to queue and abort IndexedDB transaction')
  }
}
export function webIndexedDbQueueAbortError(queueError: unknown, abortError: unknown): WebIndexedDbQueueAbortError {
  const error = new WebIndexedDbQueueAbortError(queueError, abortError)
  combinedOrigins.set(error, {
    name: webStorageErrorName(queueError),
    typed: trustedWebRecordingError(queueError),
    evidence: webHostErrorEvidence([queueError, abortError]),
    domain: 'indexeddb'
  })
  return error
}
export function webRecordingAuthorityCombinedError(primary: unknown, release: unknown): AggregateError {
  const error = new AggregateError([primary, release], 'Failed to release coordinator')
  combinedOrigins.set(error, {
    name: webStorageErrorName(primary),
    typed: trustedWebRecordingError(primary),
    evidence: webHostErrorEvidence([primary, release]),
    domain: 'web-recording-authority'
  })
  return error
}
function queueAbortEvidence(value: unknown) {
  if ((typeof value !== 'object' || value === null) && typeof value !== 'function') return undefined
  return combinedOrigins.get(value)
}

/** Entropy is supplied by the host; never treat its thrown values as library errors. */
export function webRecordingRandomBytes(
  source: { getRandomValues(bytes: Uint8Array): Uint8Array },
  bytes: Uint8Array
): Uint8Array {
  try {
    return source.getRandomValues(bytes)
  } catch (error) {
    throw webRecordingContractError('capability.unavailable', 'restoration', 'web-recording.entropy', {
      domain: 'Web Crypto',
      code: webStorageErrorName(error),
      safeMessage: 'Web recording entropy failed',
      metadata: {}
    })
  }
}
