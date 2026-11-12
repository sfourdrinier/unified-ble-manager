import { trustedWebRecordingError, webIndexedDbQueueAbortError, webStorageError } from './web-recording-errors'
import type { BackendContractError } from '../backend-contract/errors'

const readinessOrigins = new WeakMap<object, { failure: BackendContractError; retryable: boolean }>()

/** The Web host supplies browser objects; the neutral build does not require DOM types. */
export class WebRecordingReadinessError extends Error {
  constructor(readonly original: unknown) {
    super('Web recording storage readiness failed', { cause: original })
    const trusted = trustedWebRecordingError(original)
    readinessOrigins.set(this, {
      failure: trusted ?? webStorageError(original, 'indexeddb-transaction'),
      retryable: trusted?.normalized.code !== 'protocol.incompatible'
    })
  }
}

export function webRecordingReadinessFailure(value: unknown): BackendContractError | undefined {
  if ((typeof value !== 'object' || value === null) && typeof value !== 'function') return undefined
  return trustedWebRecordingError(readinessOrigins.get(value)?.failure)
}

export function webRecordingReadinessRetryable(value: unknown): boolean | undefined {
  if ((typeof value !== 'object' || value === null) && typeof value !== 'function') return undefined
  return readinessOrigins.get(value)?.retryable
}

export { WebIndexedDbQueueAbortError } from './web-recording-errors'

export interface WebIndexedDbTransaction {
  oncomplete: ((...args: never[]) => unknown) | null
  onabort: ((...args: never[]) => unknown) | null
  readonly error: Error | null
  abort(): void
}

export interface WebIndexedDbDatabase<TTransaction extends WebIndexedDbTransaction> {
  /** Wait for a usable connection before dispatching a new transaction. */
  ready?(): Promise<void>
  transaction(
    stores: string[],
    mode: 'readonly' | 'readwrite',
    options?: { readonly durability: 'strict' }
  ): TTransaction
}

export type WebIndexedDbSettlement =
  | { readonly state: 'complete' }
  | { readonly state: 'aborted'; readonly error: Error | null }
  | { readonly state: 'not-started' }

/** The result may reject before the browser has ended the transaction. */
export function ownWebIndexedDbTransaction<T, TTransaction extends WebIndexedDbTransaction>(
  database: WebIndexedDbDatabase<TTransaction>,
  stores: string[],
  mode: 'readonly' | 'readwrite',
  queue: (transaction: TTransaction) => T
): {
  readonly result: Promise<{ readonly value: T; readonly durability: 'strict' | 'default' }>
  readonly settled: Promise<WebIndexedDbSettlement>
} {
  let settle!: (outcome: WebIndexedDbSettlement) => void
  const settled = new Promise<WebIndexedDbSettlement>(resolve => {
    settle = resolve
  })
  let result: Promise<{ readonly value: T; readonly durability: 'strict' | 'default' }>
  try {
    result = runWebIndexedDbTransaction(database, stores, mode, queue, undefined, undefined, settle)
  } catch (error) {
    settle({ state: 'not-started' })
    result = Promise.reject(error)
  }
  return { result, settled }
}

/** Queue all requests synchronously. The returned value is not observable until
 * transaction completion; individual request success is not a commit. */
export function runWebIndexedDbTransaction<T, TTransaction extends WebIndexedDbTransaction>(
  database: WebIndexedDbDatabase<TTransaction>,
  stores: string[],
  mode: 'readonly' | 'readwrite',
  queue: (transaction: TTransaction) => T,
  onAbort?: () => void,
  onNotStarted?: () => void,
  onSettled?: (outcome: WebIndexedDbSettlement) => void,
  onDispatched?: () => void
): Promise<{ readonly value: T; readonly durability: 'strict' | 'default' }> {
  if (database.ready !== undefined) {
    let ready: Promise<void>
    try {
      ready = database.ready()
    } catch (error) {
      onSettled?.({ state: 'not-started' })
      onNotStarted?.()
      throw error
    }
    return ready.then(
      () =>
        dispatchWebIndexedDbTransaction(database, stores, mode, queue, onAbort, onNotStarted, onSettled, onDispatched),
      error => {
        onSettled?.({ state: 'not-started' })
        onNotStarted?.()
        throw error
      }
    )
  }
  return dispatchWebIndexedDbTransaction(database, stores, mode, queue, onAbort, onNotStarted, onSettled, onDispatched)
}

function dispatchWebIndexedDbTransaction<T, TTransaction extends WebIndexedDbTransaction>(
  database: WebIndexedDbDatabase<TTransaction>,
  stores: string[],
  mode: 'readonly' | 'readwrite',
  queue: (transaction: TTransaction) => T,
  onAbort?: () => void,
  onNotStarted?: () => void,
  onSettled?: (outcome: WebIndexedDbSettlement) => void,
  onDispatched?: () => void
): Promise<{ readonly value: T; readonly durability: 'strict' | 'default' }> {
  let transaction: TTransaction
  let durability: 'strict' | 'default' = 'strict'
  try {
    transaction = database.transaction(stores, mode, { durability: 'strict' })
  } catch (error) {
    let unsupportedOptions = false
    try {
      unsupportedOptions = error instanceof TypeError
    } catch {
      /* hostile proxy */
    }
    if (!unsupportedOptions) {
      onSettled?.({ state: 'not-started' })
      onNotStarted?.()
      throw error
    }
    durability = 'default'
    try {
      transaction = database.transaction(stores, mode)
    } catch (fallbackError) {
      onSettled?.({ state: 'not-started' })
      onNotStarted?.()
      throw fallbackError
    }
  }
  onDispatched?.()
  return new Promise((resolve, reject) => {
    transaction.onabort = () => {
      const error = transaction.error
      onSettled?.({ state: 'aborted', error })
      try {
        onAbort?.()
      } catch (callbackError) {
        reject(webIndexedDbQueueAbortError(error, callbackError))
        return
      }
      reject(error ?? new Error('IndexedDB transaction aborted'))
    }
    let queued: { value: T } | undefined
    transaction.oncomplete = () => {
      onSettled?.({ state: 'complete' })
      if (queued !== undefined) resolve({ value: queued.value, durability })
    }
    try {
      queued = { value: queue(transaction) }
    } catch (error) {
      try {
        transaction.abort()
      } catch (abortError) {
        reject(webIndexedDbQueueAbortError(error, abortError))
        return
      }
      reject(error)
      return
    }
  })
}

const noncommittedOrigins = new WeakMap<object, unknown>()

/** A transaction reported abort, or failed before a transaction was created. */
export class WebRecordingNoncommittedError extends Error {
  constructor(original: unknown) {
    super('Web recording transaction did not commit', { cause: original })
    noncommittedOrigins.set(this, original)
  }
}

export function webRecordingNoncommittedOrigin(error: unknown): { readonly original: unknown } | undefined {
  if ((typeof error !== 'object' || error === null) && typeof error !== 'function') return undefined
  return noncommittedOrigins.has(error) ? { original: noncommittedOrigins.get(error) } : undefined
}

const boundedPending = new WeakMap<object, Promise<unknown>>()

/** The exact attempt still owned after the bounded wait expires. */
export function webRecordingBoundedPending(error: unknown): Promise<unknown> | undefined {
  if ((typeof error !== 'object' || error === null) && typeof error !== 'function') return undefined
  return boundedPending.get(error)
}

/** Retry an idempotent operation without accumulating unobserved transactions.
 * Timed-out attempts retain their slots until their callbacks actually settle. */
export function boundedWebRecordingOperation<T>(
  operation: () => Promise<T>,
  maxPending = 2
): (() => Promise<T>) & { pendingSnapshot(): () => void } {
  const pending = new Set<Promise<T>>()
  const run = async (): Promise<T> => {
    if (pending.size < maxPending) {
      const attempt = Promise.resolve().then(operation)
      pending.add(attempt)
      attempt.then(
        () => pending.delete(attempt),
        () => pending.delete(attempt)
      )
    }
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      return await Promise.race([
        ...pending,
        new Promise<never>((_resolve, reject) => {
          timer = setTimeout(() => {
            const error = new WebRecordingReadinessError(new Error('Web recording operation completion unknown'))
            const attempts = [...pending]
            const settlement = attempts.length === 1 ? (attempts[0] ?? Promise.race(attempts)) : Promise.race(attempts)
            // Some bounded operations have no late-settlement consumer. Keep
            // the rejection observable to a consumer without leaving an
            // unhandled promise when only the timeout is used.
            settlement.catch(() => undefined)
            boundedPending.set(error, settlement)
            reject(error)
          }, 5000)
        })
      ])
    } finally {
      if (timer !== undefined) clearTimeout(timer)
    }
  }
  // Only use after an ordered transaction has independently proven that the
  // earlier effects are absent. Their eventual settlements remain observed.
  run.pendingSnapshot = (): (() => void) => {
    const attempts = [...pending]
    return () => {
      for (const attempt of attempts) pending.delete(attempt)
    }
  }
  return run
}
