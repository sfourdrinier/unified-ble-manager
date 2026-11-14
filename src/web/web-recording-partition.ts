import { webRecordingRandomBytes } from './web-recording-errors'
import {
  webRecordingContractError as contractError,
  trustedWebRecordingError,
  webStorageErrorName
} from './web-recording-errors'
import type { WebContinuationRecordingController } from './web-clear-attempt'
import { createWebContinuationRecordingController } from './web-continuation-recording-controller'
import { webRecordingSha256 } from './web-recording-authority'
import { openWebRecordingDatabase } from './web-recording-database'
import type { WebRecordingDatabase, WebRecordingDatabaseFactory } from './web-recording-database'
import type { WebOrderedJournalDatabase } from './web-ordered-recording-journal'
import { createWebRecordingLiveOwners } from './web-recording-live-owners'
import type { WebRecordingLockManager } from './web-recording-lock'
import type { WebRecordingStartDependencies } from './web-ordered-recording-start'

type Database = WebRecordingDatabase & WebOrderedJournalDatabase
declare const indexedDB: WebRecordingDatabaseFactory<Database> | undefined
declare const navigator: { readonly locks?: WebRecordingLockManager } | undefined
declare const crypto:
  | {
      getRandomValues(bytes: Uint8Array): Uint8Array
      readonly subtle: { digest(algorithm: 'SHA-256', data: Uint8Array): Promise<ArrayBuffer> }
    }
  | undefined

/** One registry and journal per browser storage partition. */
export interface WebRecordingPartition {
  createController(): Promise<WebContinuationRecordingController>
}

export type WebRecordingPartitionOptions = Omit<
  WebRecordingStartDependencies,
  'liveOwners' | 'forRecording' | 'preparePartition'
> & { readonly journalIdentity?: string; readonly clearAdmissionLimit?: number }

function hasSha256(value: WebRecordingStartDependencies['crypto']): value is WebRecordingStartDependencies['crypto'] & {
  readonly subtle: { digest(algorithm: 'SHA-256', data: Uint8Array): Promise<ArrayBuffer> }
} {
  return typeof Reflect.get(value, 'subtle') === 'object' && Reflect.get(value, 'subtle') !== null
}

export function createWebRecordingPartition(options: WebRecordingPartitionOptions): WebRecordingPartition {
  return partitionWithOwners(options, createWebRecordingLiveOwners())
}

function partitionWithOwners(
  options: WebRecordingPartitionOptions,
  liveOwners: WebRecordingStartDependencies['liveOwners']
): WebRecordingPartition {
  const partitionCrypto = options.crypto
  const exactDigest = hasSha256(partitionCrypto)
    ? async (value: Readonly<Uint8Array>): Promise<string> => {
        const hash = await partitionCrypto.subtle.digest('SHA-256', Uint8Array.from(value))
        return Array.from(new Uint8Array(hash), byte => byte.toString(16).padStart(2, '0')).join('')
      }
    : undefined
  const dependencies: WebRecordingStartDependencies = {
    ...options,
    liveOwners,
    ...(options.terminalEvidenceDigest === undefined && exactDigest === undefined
      ? {}
      : { terminalEvidenceDigest: options.terminalEvidenceDigest ?? exactDigest })
  }
  const partition: WebRecordingPartition = {
    createController: () =>
      createWebContinuationRecordingController({
        database: dependencies.database,
        liveOwners: dependencies.liveOwners,
        locks: dependencies.locks,
        journalIdentity: options.journalIdentity,
        clearAdmissionLimit: options.clearAdmissionLimit,
        randomToken: () =>
          Array.from(webRecordingRandomBytes(dependencies.crypto, new Uint8Array(32)), byte =>
            byte.toString(16).padStart(2, '0')
          ).join('')
      })
  }
  partitions.set(partition, dependencies)
  return partition
}

const partitions = new WeakMap<WebRecordingPartition, WebRecordingStartDependencies>()
export function webRecordingDependencies(partition: WebRecordingPartition): WebRecordingStartDependencies {
  const dependencies = partitions.get(partition)
  if (dependencies === undefined) throw contractError('argument.invalid', 'restoration', 'web-recording.partition')
  return dependencies
}

interface DefaultOpen {
  readonly abort: AbortController
  work: Promise<WebRecordingPartition>
  readonly waiters: Set<(outcome: OpenOutcome) => void>
  outcome?: OpenOutcome
  database?: Database
  delivered: boolean
  abandoned: boolean
  invalidated: boolean
}
type OpenOutcome = { readonly partition: WebRecordingPartition } | { readonly error: unknown }
const MAX_OPEN_WAITERS = 64
const OPEN_WAIT_MS = 5000
const defaultLiveOwners = createWebRecordingLiveOwners()
let defaultPartition: DefaultOpen | undefined

/** Resolve at transaction creation; existing transactions retain their own outcomes. */
export function currentDefaultWebRecordingDatabase(): WebOrderedJournalDatabase {
  const database = defaultPartition?.database
  if (database === undefined)
    throw contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-version-change')
  return database
}

function storageOpenError(error: unknown): Error {
  const trusted = trustedWebRecordingError(error)
  if (trusted !== undefined) return trusted
  const name = webStorageErrorName(error)
  const code =
    name === 'VersionError'
      ? 'protocol.incompatible'
      : name === 'QuotaExceededError'
        ? 'stream.quota'
        : 'capability.unavailable'
  return contractError(code, 'restoration', 'web-recording.indexeddb-open', {
    domain: 'indexeddb',
    code: name,
    safeMessage: 'Browser storage operation failed',
    metadata: {}
  })
}

export function defaultWebRecordingPartition(signal?: AbortSignal): Promise<WebRecordingPartition> {
  if (signal?.aborted)
    return Promise.reject(contractError('operation.aborted', 'restoration', 'web-recording.indexeddb-open'))
  try {
    if (defaultPartition === undefined) {
      if (typeof indexedDB === 'undefined' || typeof crypto === 'undefined')
        throw contractError('capability.unsupported', 'restoration', 'web-recording.browser-storage')
      const source = crypto
      const factory = indexedDB
      const locks = typeof navigator === 'undefined' ? undefined : navigator.locks
      const entry: DefaultOpen = {
        abort: new AbortController(),
        get work() {
          return work
        },
        waiters: new Set(),
        delivered: false,
        abandoned: false,
        invalidated: false
      }
      const work = openWebRecordingDatabase(factory, {
        signal: entry.abort.signal,
        onVersionChange: () => {
          entry.invalidated = true
          entry.database = undefined
          if (defaultPartition === entry) defaultPartition = undefined
        }
      })
        .then(database => {
          if (entry.abandoned) {
            if (!entry.invalidated) database.close()
            throw contractError('operation.aborted', 'restoration', 'web-recording.indexeddb-open')
          }
          if (entry.invalidated)
            throw contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-version-change')
          entry.database = database
          return partitionWithOwners(
            { database, locks, crypto: source, digest: webRecordingSha256(source) },
            defaultLiveOwners
          )
        })
        .catch(error => {
          if (defaultPartition === entry) defaultPartition = undefined
          throw storageOpenError(error)
        })
      work.then(
        partition => {
          entry.outcome = { partition }
          for (const waiter of entry.waiters) waiter(entry.outcome)
        },
        error => {
          entry.outcome = { error }
          for (const waiter of entry.waiters) waiter(entry.outcome)
        }
      )
      defaultPartition = entry
    }
    const entry = defaultPartition
    if (entry.waiters.size >= MAX_OPEN_WAITERS)
      return Promise.reject(contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-waiters'))
    return new Promise((resolve, reject) => {
      let finished = false
      const finish = (): boolean => {
        if (finished) return false
        finished = true
        signal?.removeEventListener('abort', abort)
        clearTimeout(timer)
        entry.waiters.delete(settle)
        return true
      }
      const cancel = (code: 'operation.aborted' | 'operation.timed-out'): void => {
        if (!finish()) return
        if (!entry.delivered && entry.waiters.size === 0) {
          entry.abandoned = true
          if (defaultPartition === entry) defaultPartition = undefined
          if (entry.database !== undefined) {
            entry.database.close()
            entry.database = undefined
          } else entry.abort.abort()
        }
        reject(contractError(code, 'restoration', 'web-recording.indexeddb-open'))
      }
      const abort = (): void => cancel('operation.aborted')
      const settle = (outcome: OpenOutcome): void => {
        if (!finish()) return
        if ('error' in outcome) reject(outcome.error)
        else if (entry.invalidated)
          reject(contractError('capability.unavailable', 'restoration', 'web-recording.indexeddb-version-change'))
        else {
          entry.delivered = true
          resolve(outcome.partition)
        }
      }
      entry.waiters.add(settle)
      const timer = setTimeout(() => cancel('operation.timed-out'), OPEN_WAIT_MS)
      signal?.addEventListener('abort', abort, { once: true })
      if (signal?.aborted) abort()
      if (entry.outcome !== undefined) settle(entry.outcome)
    })
  } catch (error) {
    return Promise.reject(storageOpenError(error))
  }
}

export function webRecordingAvailable(): boolean {
  return typeof indexedDB !== 'undefined' && typeof crypto !== 'undefined'
}
