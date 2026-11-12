import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { webRecordingContractError as contractError } from './web-recording-errors'

const LEGACY_STORES = [
  { name: 'recordings', keyPath: 'id' },
  { name: 'records', keyPath: ['recordingId', 'ordinal'] },
  {
    name: 'pendingIdentities',
    keyPath: ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']
  },
  { name: 'readerReceipts', keyPath: 'recordingId' },
  { name: 'authorityReservations', keyPath: 'id' }
] as const
const NEW_STORES = [
  { name: 'markerLedger', keyPath: ['recordingId', 'producerIncarnation', 'markerId'] },
  { name: 'markerOperations', keyPath: ['recordingId', 'producerIncarnation', 'operationId'] },
  { name: 'recoveryOperations', keyPath: ['recordingId', 'producerIncarnation', 'operationId'] },
  { name: 'abandonments', keyPath: ['recordingId', 'producerIncarnation'] }
] as const
const TERMINAL_EVIDENCE_STORES = [
  { name: 'terminalEvidence', keyPath: 'ref' },
  { name: 'terminalEvidenceState', keyPath: 'key' }
] as const
const STORES = [...LEGACY_STORES, ...NEW_STORES, ...TERMINAL_EVIDENCE_STORES]

// A browser open cannot be cancelled. Keep its slot until the request itself
// settles, even when every caller has already timed out or been rejected.
const outstandingOpens = new WeakMap<object, number>()
const MAX_OUTSTANDING_OPENS = 2

export interface WebRecordingDatabase {
  readonly objectStoreNames: { readonly length: number; contains(name: string): boolean }
  onversionchange: ((...args: never[]) => unknown) | null
  createObjectStore(name: string, options: { keyPath: string | string[] }): { put(value: object): unknown }
  transaction(
    stores: string[],
    mode: 'readonly'
  ): {
    objectStore(name: string): { readonly keyPath: string | readonly string[] | null }
  }
  close(): void
}

export interface WebRecordingOpenRequest<TDatabase extends WebRecordingDatabase> {
  readonly result: TDatabase
  readonly error: Error | null
  readonly transaction: {
    abort(): void
    objectStore(name: string): { readonly keyPath: string | readonly string[] | null }
  } | null
  onupgradeneeded: ((...args: never[]) => unknown) | null
  onsuccess: ((...args: never[]) => unknown) | null
  onerror: ((...args: never[]) => unknown) | null
  onblocked: ((...args: never[]) => unknown) | null
}

export interface WebRecordingDatabaseFactory<TDatabase extends WebRecordingDatabase> {
  open(name: string, version: number): WebRecordingOpenRequest<TDatabase>
}

function schemaMatches(
  database: WebRecordingDatabase,
  upgrade?: NonNullable<WebRecordingOpenRequest<WebRecordingDatabase>['transaction']>,
  stores: readonly { readonly name: string; readonly keyPath: string | readonly string[] }[] = STORES
): boolean {
  if (
    database.objectStoreNames.length !== stores.length ||
    stores.some(store => !database.objectStoreNames.contains(store.name))
  )
    return false
  const transaction =
    upgrade ??
    database.transaction(
      stores.map(store => store.name),
      'readonly'
    )
  return stores.every(store => {
    const actual = transaction.objectStore(store.name).keyPath
    const expected = store.keyPath
    return typeof expected === 'string'
      ? actual === expected
      : Array.isArray(actual) &&
          actual.length === expected.length &&
          expected.every((part, index) => actual[index] === part)
  })
}

/** Fixed partition-scoped journal. A blocked request is abandoned, not allowed
 * to create a database later after its caller was told that opening failed. */
export function openWebRecordingDatabase<TDatabase extends WebRecordingDatabase>(
  factory: WebRecordingDatabaseFactory<TDatabase>,
  options: { readonly signal?: AbortSignal; readonly onVersionChange?: () => void } = {}
): Promise<TDatabase> {
  return new Promise((resolve, reject) => {
    if (options.signal?.aborted) {
      reject(contractError('operation.aborted', 'restoration', 'web-recording.indexeddb-open'))
      return
    }
    const count = outstandingOpens.get(factory) ?? 0
    if (count >= MAX_OUTSTANDING_OPENS) {
      reject(contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-open-pending'))
      return
    }
    // Version 5 adds separately-owned terminal evidence; upgrades preserve all rows.
    const request = factory.open('ubm-ordered-recordings', 5)
    outstandingOpens.set(factory, count + 1)
    let settled = false
    const releaseSlot = (): void => {
      if (settled) return
      settled = true
      const remaining = (outstandingOpens.get(factory) ?? 1) - 1
      if (remaining === 0) outstandingOpens.delete(factory)
      else outstandingOpens.set(factory, remaining)
    }
    let abandoned = false
    let upgradeError: unknown = null
    const detach = (): void => options.signal?.removeEventListener('abort', abort)
    const abort = (): void => {
      abandoned = true
      detach()
      reject(contractError('operation.aborted', 'restoration', 'web-recording.indexeddb-open'))
    }
    options.signal?.addEventListener('abort', abort, { once: true })
    if (options.signal?.aborted) abort()
    request.onblocked = () => {
      abandoned = true
      detach()
      reject(contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-blocked'))
    }
    request.onupgradeneeded = (event: { readonly oldVersion: number }) => {
      if (
        abandoned ||
        (event.oldVersion !== 0 && event.oldVersion !== 2 && event.oldVersion !== 3 && event.oldVersion !== 4)
      ) {
        if (!abandoned)
          upgradeError = contractError('protocol.incompatible', 'restoration', 'web-recording.schema-version')
        try {
          if (request.transaction === null) throw new Error('IndexedDB upgrade transaction missing')
          request.transaction.abort()
        } catch (error) {
          reject(error)
        }
        return
      }
      try {
        if (event.oldVersion !== 0) {
          const existingStores = event.oldVersion === 4 ? [...LEGACY_STORES, ...NEW_STORES] : LEGACY_STORES
          if (request.transaction === null || !schemaMatches(request.result, request.transaction, existingStores))
            throw contractError('protocol.incompatible', 'restoration', 'web-recording.schema-version')
        }
        const additions =
          event.oldVersion === 0
            ? STORES
            : event.oldVersion === 4
              ? TERMINAL_EVIDENCE_STORES
              : [...NEW_STORES, ...TERMINAL_EVIDENCE_STORES]
        for (const store of additions) {
          const created = request.result.createObjectStore(store.name, {
            keyPath: typeof store.keyPath === 'string' ? store.keyPath : [...store.keyPath]
          })
          if (store.name === 'readerReceipts')
            created.put({
              recordingId: ':ubm-authority-policy-v1',
              version: 2,
              protocol: 'ubm-authority-reservations/1'
            })
        }
      } catch (error) {
        upgradeError = error
        try {
          if (request.transaction === null) throw new Error('IndexedDB upgrade transaction missing')
          request.transaction.abort()
        } catch (abortError) {
          reject(webIndexedDbQueueAbortError(error, abortError))
        }
      }
    }
    request.onerror = () => {
      releaseSlot()
      detach()
      reject(upgradeError ?? request.error ?? new Error('IndexedDB open failed'))
    }
    request.onsuccess = () => {
      releaseSlot()
      detach()
      const database = request.result
      if (abandoned) {
        database.close()
        return
      }
      try {
        if (!schemaMatches(database)) throw contractError('protocol.malformed', 'restoration', 'web-recording.schema')
        database.onversionchange = () => {
          database.close()
          options.onVersionChange?.()
        }
        resolve(database)
      } catch (error) {
        database.close()
        reject(error)
      }
    }
  })
}
