import { contractError } from '../backend-contract/errors'
import type { WebRecordingLock } from './web-recording-lock'
import {
  WEB_RECORDING_WORKER_HANDSHAKE_TIMEOUT_MS,
  WEB_RECORDING_WORKER_NAME,
  WEB_RECORDING_WORKER_VERSION
} from './web-recording-authority-worker'

export interface WebRecordingWorkerPort {
  onmessage: ((event: { readonly data: unknown }) => void) | null
  onmessageerror: (() => void) | null
  onclose?: (() => void) | null
  postMessage(message: unknown): void
  start(): void
  close(): void
}
export interface WebRecordingWorker {
  readonly port: WebRecordingWorkerPort
  onerror: ((event: unknown) => void) | null
}
// This package's TypeScript build excludes DOM libraries. The runtime binding
// is the browser's own constructor, not a caller-supplied worker factory.
declare const SharedWorker: new (
  url: string,
  options: { readonly name: string; readonly type: 'module' }
) => WebRecordingWorker

/** Legacy type retained only so an unconverted authority caller fails closed. */
export type WebRecordingWorkerFactory = (() => WebRecordingWorker) & { readonly coordinatorId: string }

/** Transitional compatibility for the unconverted authority caller. The
 * reservation protocol constructs its own worker and must not use this. */
export function createWebRecordingSharedWorkerFactory(
  SharedWorkerConstructor: new (
    url: string,
    options: { readonly name: string; readonly type: 'module' }
  ) => WebRecordingWorker,
  fixedModuleUrl: string
): WebRecordingWorkerFactory {
  const coordinatorId = normalizeWorkerUrl(fixedModuleUrl)
  return Object.assign(
    () => new SharedWorkerConstructor(coordinatorId, { name: WEB_RECORDING_WORKER_NAME, type: 'module' }),
    { coordinatorId }
  )
}

/** Until the caller uses durable reservations, a worker reply cannot authorize it. */
export async function acquireWebRecordingWorkerLock(
  _createWorker: WebRecordingWorkerFactory,
  _digest: (id: string) => Promise<string>,
  _id: string,
  _onLost?: (error: unknown) => void,
  _signal?: AbortSignal
): Promise<WebRecordingLock> {
  throw contractError('capability.unsupported', 'restoration', 'web-recording.worker-reservations-required')
}

export interface WebRecordingWorkerIdentity {
  readonly kind: 'worker'
  readonly url: string
  readonly name: typeof WEB_RECORDING_WORKER_NAME
  readonly type: 'module'
  readonly version: typeof WEB_RECORDING_WORKER_VERSION
}

const MAX_PENDING_PROBES = 16
let pendingProbes = 0
let nextRequestId = 0n

// Browser messages and worker script URLs can contain application data. Retain
// only standard error names, including cross-realm DOMExceptions.
const safeWorkerErrorNames = new Set([
  'Error',
  'TypeError',
  'RangeError',
  'SyntaxError',
  'ReferenceError',
  'EvalError',
  'URIError',
  'AggregateError',
  'SecurityError',
  'NotAllowedError',
  'NotSupportedError',
  'InvalidStateError',
  'NetworkError',
  'AbortError',
  'DataCloneError',
  'QuotaExceededError',
  'OperationError',
  'TimeoutError',
  'UnknownError',
  'IndexSizeError',
  'HierarchyRequestError',
  'WrongDocumentError',
  'InvalidCharacterError',
  'NoModificationAllowedError',
  'NotFoundError',
  'InUseAttributeError',
  'InvalidModificationError',
  'NamespaceError',
  'InvalidAccessError',
  'TypeMismatchError',
  'InvalidNodeTypeError',
  'EncodingError',
  'NotReadableError',
  'ConstraintError',
  'DataError',
  'TransactionInactiveError',
  'ReadOnlyError',
  'VersionError'
])
function workerErrorName(error: unknown): string {
  try {
    if (typeof error !== 'object' || error === null || !('name' in error)) return 'unknown'
    return typeof error.name === 'string' && safeWorkerErrorNames.has(error.name) ? error.name : 'unknown'
  } catch {
    return 'unknown'
  }
}
function workerStartupFailure(error: unknown, operation: 'create' | 'handshake'): Error {
  return contractError('capability.unsupported', 'restoration', `web-recording.shared-worker-${operation}`, {
    domain: 'SharedWorker',
    code: workerErrorName(error),
    safeMessage: operation === 'create' ? 'SharedWorker construction failed' : 'SharedWorker startup failed',
    metadata: {}
  })
}
function workerEventError(event: unknown): unknown {
  try {
    return typeof event === 'object' && event !== null && 'error' in event ? event.error : event
  } catch {
    return undefined
  }
}

function normalizeWorkerUrl(value: string): string {
  try {
    const url = new URL(value)
    if (
      (url.protocol !== 'https:' && url.protocol !== 'http:') ||
      url.username ||
      url.password ||
      url.hash ||
      url.href.length > 2048
    )
      throw new Error('Invalid SharedWorker URL')
    return url.href
  } catch {
    throw contractError('argument.invalid', 'restoration', 'web-recording.shared-worker-url')
  }
}

/** Probe before creating a provisional reservation. Success is not authority,
 * and closing or losing this port says nothing about producer quiescence. */
export async function probeWebRecordingSharedWorker(
  fixedModuleUrl: string,
  signal?: AbortSignal
): Promise<WebRecordingWorkerIdentity> {
  const url = normalizeWorkerUrl(fixedModuleUrl)
  const aborted = (): Error => contractError('operation.aborted', 'restoration', 'web-recording.acquire')
  const unsupported = (): Error =>
    contractError('capability.unsupported', 'restoration', 'web-recording.shared-worker-handshake')
  if (signal?.aborted) throw aborted()
  if (pendingProbes >= MAX_PENDING_PROBES)
    throw contractError('capability.unavailable', 'restoration', 'web-recording.shared-worker-probe-limit')
  pendingProbes++
  try {
    let worker: WebRecordingWorker
    try {
      worker = new SharedWorker(url, { name: WEB_RECORDING_WORKER_NAME, type: 'module' })
    } catch (error) {
      throw workerStartupFailure(error, 'create')
    }
    const port = worker.port
    nextRequestId = nextRequestId >= 99999999999999999999999999999999n ? 1n : nextRequestId + 1n
    const requestId = nextRequestId.toString()
    return await new Promise<WebRecordingWorkerIdentity>((resolve, reject) => {
      let finished = false
      const finish = (error?: Error): void => {
        if (finished) return
        finished = true
        clearTimeout(timer)
        signal?.removeEventListener('abort', abort)
        worker.onerror = null
        port.onmessage = null
        port.onmessageerror = null
        if ('onclose' in port) port.onclose = null
        try {
          port.close()
        } catch (closeError) {
          reject(
            error === undefined ? closeError : new AggregateError([error, closeError], 'Failed to close probe port')
          )
          return
        }
        if (error !== undefined) reject(error)
        else
          resolve({
            kind: 'worker',
            url,
            name: WEB_RECORDING_WORKER_NAME,
            type: 'module',
            version: WEB_RECORDING_WORKER_VERSION
          })
      }
      const abort = (): void => finish(aborted())
      const timer = setTimeout(() => finish(unsupported()), WEB_RECORDING_WORKER_HANDSHAKE_TIMEOUT_MS)
      worker.onerror = event => finish(workerStartupFailure(workerEventError(event), 'handshake'))
      port.onmessageerror = () => finish(unsupported())
      if ('onclose' in port) port.onclose = () => finish(unsupported())
      port.onmessage = event => {
        const data: unknown = event.data
        if (
          typeof data !== 'object' ||
          data === null ||
          !('type' in data) ||
          data.type !== 'hello' ||
          !('version' in data) ||
          data.version !== WEB_RECORDING_WORKER_VERSION ||
          !('requestId' in data) ||
          data.requestId !== requestId
        )
          finish(unsupported())
        else finish()
      }
      signal?.addEventListener('abort', abort, { once: true })
      if (signal?.aborted) {
        abort()
        return
      }
      try {
        port.start()
        port.postMessage({ type: 'hello', version: WEB_RECORDING_WORKER_VERSION, requestId })
      } catch (error) {
        finish(workerStartupFailure(error, 'handshake'))
      }
    })
  } finally {
    pendingProbes--
  }
}
