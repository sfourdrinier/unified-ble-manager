import { webIndexedDbQueueAbortError } from './web-recording-errors'
import { webRecordingContractError as contractError } from './web-recording-errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { ownWebIndexedDbTransaction } from './web-indexeddb-transaction'
import type { WebIndexedDbSettlement } from './web-indexeddb-transaction'
import type { WebOrderedJournalDatabase, WebOrderedJournalTransaction } from './web-ordered-recording-journal'

/** Durable identity issued before an opt-in V2 clear. */
export interface WebProofAdmission {
  readonly epoch: string
  readonly sequence: string
  readonly recordingId: string
  readonly producerIncarnation: string
  readonly requestId: string
}
export type WebProofOutcome =
  | { readonly state: 'pending'; readonly attempt: WebProofAdmission }
  | { readonly state: 'cancelled'; readonly attempt: WebProofAdmission }
  | { readonly state: 'committed'; readonly attempt: WebProofAdmission; readonly result: WebProofClearResult }
  | { readonly state: 'retired-or-unknown' }
export interface WebProofClearResult {
  readonly cleared: true
  readonly records: 0
}
export interface WebProofUsage {
  readonly limit: number | null
  readonly retained: number
  readonly revision: string
  /** All rows in the clear namespace, including V1, clear receipts and sequence metadata. */
  readonly clearNamespaceRows: number
  /** Permanent V2 request fences; count verification detects missing rows, not coordinated corruption. */
  readonly requestFences: {
    readonly count: number
    readonly limit: number | null
    readonly integrity: 'count-verified' | 'legacy-unverified'
  }
}
interface Metadata {
  readonly limit: number | null
  readonly retained: number
  readonly revision: string
  readonly recordingId: string
  readonly version: 1
  readonly epoch: string
  readonly watermark: string
  readonly requestCount?: number
  readonly fenceLimit?: number | null
}
interface ProofRow {
  readonly recordingId: string
  readonly version: 1
  readonly state: 'pending' | 'cancelled' | 'committed'
  readonly attempt: WebProofAdmission
  readonly result?: WebProofClearResult
}
const metadataKey = ':ubm-staged-proof-ledger-v1'
const markerKey = ':ubm-staged-proof-ledger-marker-v1'
const proofKey = (sequence: string): string => `:ubm-staged-proof-v1:${sequence}`
const requestKey = (requestId: string): string => `:ubm-proof-request-v2:${requestId}`
declare const IDBKeyRange: { bound(lower: string, upper: string): unknown }
const malformed = (): Error => contractError('protocol.malformed', 'restoration', 'web-recording.staged-proof-ledger')
const invalid = (): Error => contractError('argument.invalid', 'restoration', 'web-recording.staged-proof-ledger')
const conflict = (): Error =>
  contractError('lifecycle.invalid-state', 'restoration', 'web-recording.staged-proof-conflict')
const quota = (): Error => contractError('stream.quota', 'restoration', 'web-recording.staged-proof-capacity')
const decimal = (value: unknown): value is string => typeof value === 'string' && /^(0|[1-9][0-9]*)$/.test(value)
const token = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value)
const record = (value: unknown): Record<string, unknown> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) throw malformed()
  return Object.fromEntries(Object.entries(value))
}
function metadata(value: unknown): Metadata {
  const row = record(value)
  if (
    Object.keys(row).some(
      key =>
        ![
          'recordingId',
          'version',
          'epoch',
          'watermark',
          'revision',
          'retained',
          'limit',
          'requestCount',
          'fenceLimit'
        ].includes(key)
    ) ||
    Object.keys(row).length < 7 ||
    row.recordingId !== metadataKey ||
    row.version !== 1 ||
    !token(row.epoch) ||
    !decimal(row.watermark) ||
    !decimal(row.revision) ||
    !Number.isSafeInteger(row.retained) ||
    typeof row.retained !== 'number' ||
    row.retained < 0 ||
    (row.limit !== null && (typeof row.limit !== 'number' || !Number.isSafeInteger(row.limit) || row.limit < 1)) ||
    ('requestCount' in row &&
      (typeof row.requestCount !== 'number' || !Number.isSafeInteger(row.requestCount) || row.requestCount < 0)) ||
    ('fenceLimit' in row &&
      row.fenceLimit !== null &&
      (typeof row.fenceLimit !== 'number' || !Number.isSafeInteger(row.fenceLimit) || row.fenceLimit < 1))
  )
    throw malformed()
  return {
    recordingId: metadataKey,
    version: 1,
    epoch: row.epoch,
    watermark: row.watermark,
    revision: row.revision,
    retained: row.retained,
    limit: row.limit,
    ...(typeof row.requestCount === 'number' ? { requestCount: row.requestCount } : {}),
    ...('fenceLimit' in row && (row.fenceLimit === null || typeof row.fenceLimit === 'number')
      ? { fenceLimit: row.fenceLimit }
      : {})
  }
}
function admission(value: unknown): WebProofAdmission {
  const row = record(value)
  if (
    Object.keys(row).length !== 5 ||
    !token(row.epoch) ||
    !decimal(row.sequence) ||
    row.sequence === '0' ||
    !isRecordingId(row.recordingId) ||
    typeof row.producerIncarnation !== 'string' ||
    row.producerIncarnation.length === 0 ||
    row.producerIncarnation.length > 256 ||
    typeof row.requestId !== 'string' ||
    row.requestId.length === 0
  )
    throw malformed()
  return {
    epoch: row.epoch,
    sequence: row.sequence,
    recordingId: row.recordingId,
    producerIncarnation: row.producerIncarnation,
    requestId: row.requestId
  }
}
function requestFence(value: unknown, requestId: string): WebProofAdmission {
  const row = record(value)
  if (Object.keys(row).length !== 3 || row.recordingId !== requestKey(requestId) || row.version !== 1) throw malformed()
  const attempt = admission(row.attempt)
  if (attempt.requestId !== requestId) throw malformed()
  return attempt
}
function proof(value: unknown, expected: WebProofAdmission): ProofRow {
  const row = record(value)
  if (
    Object.keys(row).length !== (row.state === 'committed' ? 5 : 4) ||
    row.version !== 1 ||
    row.recordingId !== proofKey(expected.sequence) ||
    (row.state !== 'pending' && row.state !== 'cancelled' && row.state !== 'committed')
  )
    throw malformed()
  const attempt = admission(row.attempt)
  if (JSON.stringify(attempt) !== JSON.stringify(expected)) throw conflict()
  if (row.state === 'committed') {
    const result = record(row.result)
    if (Object.keys(result).length !== 2 || result.cleared !== true || result.records !== 0) throw malformed()
    return {
      recordingId: proofKey(expected.sequence),
      version: 1,
      state: row.state,
      attempt,
      result: { cleared: true, records: 0 }
    }
  }
  return { recordingId: proofKey(expected.sequence), version: 1, state: row.state, attempt }
}
export function checkedWebProofAdmission(value: WebProofAdmission): WebProofAdmission {
  try {
    return admission(value)
  } catch {
    throw invalid()
  }
}
const checkedAttempt = checkedWebProofAdmission

function outcome(row: ProofRow): Exclude<WebProofOutcome, { readonly state: 'retired-or-unknown' }> {
  if (row.state === 'committed') {
    if (row.result === undefined) throw malformed()
    return { state: 'committed', attempt: row.attempt, result: row.result }
  }
  return { state: row.state, attempt: row.attempt }
}

/** Called inside the same readwrite transaction that deletes the recording. */
export function commitWebClearProofInTransaction(
  tx: WebOrderedJournalTransaction,
  input: WebProofAdmission,
  guard: (fn: () => void) => void,
  done: () => void
): void {
  const attempt = checkedAttempt(input)
  if (!token(attempt.requestId)) throw invalid()
  const store = tx.objectStore('readerReceipts')
  const request = store.get(requestKey(attempt.requestId))
  request.onsuccess = () =>
    guard(() => {
      if (JSON.stringify(requestFence(request.result, attempt.requestId)) !== JSON.stringify(attempt)) throw conflict()
      const selected = store.get(proofKey(attempt.sequence))
      selected.onsuccess = () =>
        guard(() => {
          if (selected.result === undefined) throw conflict()
          const row = proof(selected.result, attempt)
          if (row.state !== 'pending') throw conflict()
          store.put({ ...row, state: 'committed', result: { cleared: true, records: 0 } })
          done()
        })
    })
}

/** Uses a shared readwrite scope so independent connections serialize allocation,
 * policy changes and retirement. V1 and default clear proofs remain outside
 * this ledger; V2 commits its result through the same deletion transaction. */
export function createWebClearProofLedger(
  database: WebOrderedJournalDatabase,
  newEpoch: () => string,
  validate?: () => void
) {
  function transact<T>(
    action: (
      tx: WebOrderedJournalTransaction,
      meta: Metadata,
      done: (value: T) => void,
      guard: (fn: () => void) => void
    ) => void,
    onOwned?: (settled: Promise<WebIndexedDbSettlement>) => void
  ): Promise<T> {
    let result: { value: T } | undefined
    let problem: { error: unknown } | undefined
    const owned = ownWebIndexedDbTransaction(database, ['readerReceipts'], 'readwrite', tx => {
      validate?.()
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
      const store = tx.objectStore('readerReceipts')
      const request = store.get(metadataKey)
      request.onsuccess = () =>
        guard(() => {
          const proceed = (meta: Metadata): void => {
            action(
              tx,
              meta,
              value => {
                result = { value }
              },
              guard
            )
          }
          if (request.result !== undefined) {
            const meta = metadata(request.result)
            const marker = store.get(markerKey)
            marker.onsuccess = () =>
              guard(() => {
                if (marker.result === undefined) {
                  if (meta.requestCount !== undefined) throw malformed()
                  store.add({ recordingId: markerKey, version: 1, epoch: meta.epoch })
                } else {
                  const value = record(marker.result)
                  if (
                    Object.keys(value).length !== 3 ||
                    value.recordingId !== markerKey ||
                    value.version !== 1 ||
                    value.epoch !== meta.epoch
                  )
                    throw malformed()
                }
                proceed(meta)
              })
            return
          }
          // A deleted metadata row must never create a new epoch over surviving
          // outcomes or permanent request fences. Read at most one row per
          // namespace, in the same transaction as initialization.
          const check = (prefix: string, next: () => void): void => {
            const found = store.getAll(IDBKeyRange.bound(prefix, `${prefix}\uffff`), 1)
            found.onsuccess = () =>
              guard(() => {
                if (!Array.isArray(found.result) || found.result.length !== 0) throw malformed()
                next()
              })
          }
          const marker = store.get(markerKey)
          marker.onsuccess = () =>
            guard(() => {
              if (marker.result !== undefined) throw malformed()
              check(':ubm-staged-proof-v1:', () =>
                check(':ubm-proof-request-v2:', () => {
                  const epoch = newEpoch()
                  if (!token(epoch)) throw invalid()
                  const meta: Metadata = {
                    recordingId: metadataKey,
                    version: 1,
                    epoch,
                    watermark: '0',
                    revision: '0',
                    retained: 0,
                    requestCount: 0,
                    limit: null
                  }
                  store.add(meta)
                  store.add({ recordingId: markerKey, version: 1, epoch })
                  proceed(meta)
                })
              )
            })
        })
    })
    onOwned?.(owned.settled)
    return owned.result.then(
      () => {
        if (problem !== undefined) throw problem.error
        if (result === undefined) throw malformed()
        return result.value
      },
      error => {
        throw problem === undefined ? error : problem.error
      }
    )
  }
  const usage = (
    tx: WebOrderedJournalTransaction,
    m: Metadata,
    done: (value: WebProofUsage) => void,
    guard: (fn: () => void) => void
  ): void => {
    const count = tx
      .objectStore('readerReceipts')
      .count(IDBKeyRange.bound(':ubm-proof-request-v2:', ':ubm-proof-request-v2:\uffff'))
    count.onsuccess = () =>
      guard(() => {
        if (typeof count.result !== 'number' || !Number.isSafeInteger(count.result) || count.result < 0)
          throw malformed()
        if (m.requestCount !== undefined && count.result !== m.requestCount) throw malformed()
        const fenceCount = count.result
        const clearCount = tx.objectStore('readerReceipts').count(IDBKeyRange.bound(':ubm-clear-', ':ubm-clear-\uffff'))
        clearCount.onsuccess = () =>
          guard(() => {
            if (
              typeof clearCount.result !== 'number' ||
              !Number.isSafeInteger(clearCount.result) ||
              clearCount.result < 0
            )
              throw malformed()
            done({
              limit: m.limit,
              retained: m.retained,
              revision: m.revision,
              clearNamespaceRows: clearCount.result,
              requestFences: {
                count: fenceCount,
                limit: m.fenceLimit ?? null,
                integrity: m.requestCount === undefined ? 'legacy-unverified' : 'count-verified'
              }
            })
          })
      })
  }
  function withProof<T>(
    attempt: WebProofAdmission,
    fn: (
      tx: WebOrderedJournalTransaction,
      meta: Metadata,
      row: ProofRow | undefined,
      done: (value: T) => void,
      guard: (fn: () => void) => void
    ) => void,
    requireRequestFence = false
  ): Promise<T> {
    const checked = checkedAttempt(attempt)
    if (requireRequestFence && !token(checked.requestId)) throw invalid()
    return transact((tx, meta, done, guard) => {
      if (checked.epoch !== meta.epoch || BigInt(checked.sequence) > BigInt(meta.watermark)) {
        if (requireRequestFence) {
          const index = tx.objectStore('readerReceipts').get(requestKey(checked.requestId))
          index.onsuccess = () =>
            guard(() => {
              if (index.result !== undefined) {
                const issued = requestFence(index.result, checked.requestId)
                if (issued.epoch !== meta.epoch || BigInt(issued.sequence) > BigInt(meta.watermark)) throw malformed()
                throw conflict()
              }
              if (meta.requestCount === undefined) throw malformed()
              const count = tx
                .objectStore('readerReceipts')
                .count(IDBKeyRange.bound(':ubm-proof-request-v2:', ':ubm-proof-request-v2:\uffff'))
              count.onsuccess = () =>
                guard(() => {
                  if (count.result !== meta.requestCount) throw malformed()
                  fn(tx, meta, undefined, done, guard)
                })
            })
          return
        }
        fn(tx, meta, undefined, done, guard)
        return
      }
      const req = tx.objectStore('readerReceipts').get(proofKey(checked.sequence))
      req.onsuccess = () =>
        guard(() => {
          const row = req.result === undefined ? undefined : proof(req.result, checked)
          if (!requireRequestFence) return fn(tx, meta, row, done, guard)
          const index = tx.objectStore('readerReceipts').get(requestKey(checked.requestId))
          index.onsuccess = () =>
            guard(() => {
              if (index.result === undefined) {
                if (row !== undefined) throw malformed()
                if (meta.requestCount === undefined) throw malformed()
                const count = tx
                  .objectStore('readerReceipts')
                  .count(IDBKeyRange.bound(':ubm-proof-request-v2:', ':ubm-proof-request-v2:\uffff'))
                count.onsuccess = () =>
                  guard(() => {
                    if (count.result !== meta.requestCount) throw malformed()
                    fn(tx, meta, undefined, done, guard)
                  })
                return
              }
              const issued = requestFence(index.result, checked.requestId)
              if (issued.epoch !== meta.epoch || BigInt(issued.sequence) > BigInt(meta.watermark)) throw malformed()
              if (JSON.stringify(issued) !== JSON.stringify(checked)) throw conflict()
              fn(tx, meta, row, done, guard)
            })
        })
    })
  }
  return {
    usage: (): Promise<WebProofUsage> => transact((tx, meta, done, guard) => usage(tx, meta, done, guard)),
    /** The cursor counts issued identities, including retired gaps. */
    list: (options: {
      readonly after: string
      readonly limit: number
    }): Promise<{
      readonly entries: readonly Exclude<WebProofOutcome, { readonly state: 'retired-or-unknown' }>[]
      readonly next: string | null
    }> => {
      if (!decimal(options.after) || !Number.isSafeInteger(options.limit) || options.limit < 1)
        return Promise.reject(invalid())
      const { after, limit } = options
      return transact((tx, meta, done, guard) => {
        const end = BigInt(meta.watermark)
        let cursor = BigInt(after)
        let scanned = 0
        const entries: Exclude<WebProofOutcome, { readonly state: 'retired-or-unknown' }>[] = []
        const scan = (): void => {
          if (cursor >= end || scanned === limit) {
            done({ entries, next: cursor < end ? cursor.toString() : null })
            return
          }
          cursor += 1n
          scanned++
          const request = tx.objectStore('readerReceipts').get(proofKey(cursor.toString()))
          request.onsuccess = () =>
            guard(() => {
              if (request.result !== undefined) {
                const candidate = record(request.result)
                const attempt = admission(candidate.attempt)
                if (attempt.epoch !== meta.epoch || attempt.sequence !== cursor.toString()) throw malformed()
                const row = proof(candidate, attempt)
                entries.push(outcome(row))
              }
              scan()
            })
        }
        scan()
      })
    },
    configure: (options: {
      readonly limit: number | null
      readonly fenceLimit?: number | null
      readonly expectedRevision: string
    }): Promise<WebProofUsage> => {
      if (
        (options.limit !== null && (!Number.isSafeInteger(options.limit) || options.limit < 1)) ||
        (options.fenceLimit !== undefined &&
          options.fenceLimit !== null &&
          (!Number.isSafeInteger(options.fenceLimit) || options.fenceLimit < 1)) ||
        !decimal(options.expectedRevision)
      )
        return Promise.reject(invalid())
      const { expectedRevision, limit, fenceLimit } = options
      return transact((tx, meta, done, guard) => {
        if (meta.revision !== expectedRevision) throw conflict()
        const updated = {
          ...meta,
          limit,
          ...(fenceLimit === undefined ? {} : { fenceLimit }),
          revision: (BigInt(meta.revision) + 1n).toString()
        }
        tx.objectStore('readerReceipts').put(updated)
        usage(tx, updated, done, guard)
      })
    },
    admit: (identity: {
      readonly recordingId: string
      readonly producerIncarnation: string
      readonly requestId: string
    }): Promise<WebProofAdmission> => {
      const requested = { ...identity }
      return transact((tx, meta, done) => {
        if (meta.limit !== null && meta.retained >= meta.limit) throw quota()
        if (meta.retained === Number.MAX_SAFE_INTEGER) throw quota()
        const attempt = checkedAttempt({
          ...requested,
          epoch: meta.epoch,
          sequence: (BigInt(meta.watermark) + 1n).toString()
        })
        const store = tx.objectStore('readerReceipts')
        store.add({ recordingId: proofKey(attempt.sequence), version: 1, state: 'pending', attempt })
        store.put({ ...meta, watermark: attempt.sequence, retained: meta.retained + 1 })
        done(attempt)
      })
    },
    /** A caller-generated request ID makes admission safe to retry after a lost reply. */
    admitRetryable: (
      identity: {
        readonly recordingId: string
        readonly producerIncarnation: string
        readonly requestId: string
      },
      onOwned?: (settled: Promise<WebIndexedDbSettlement>) => void
    ): Promise<WebProofAdmission> => {
      const requested = { ...identity }
      if (!token(requested.requestId)) return Promise.reject(invalid())
      return transact((tx, meta, done, guard) => {
        const store = tx.objectStore('readerReceipts')
        const key = requestKey(requested.requestId)
        const existing = store.get(key)
        existing.onsuccess = () =>
          guard(() => {
            if (existing.result !== undefined) {
              const attempt = requestFence(existing.result, requested.requestId)
              if (attempt.epoch !== meta.epoch || BigInt(attempt.sequence) > BigInt(meta.watermark)) throw malformed()
              if (
                attempt.recordingId !== requested.recordingId ||
                attempt.producerIncarnation !== requested.producerIncarnation
              )
                throw conflict()
              return done(attempt)
            }
            // Old metadata did not record the number of fences. An absent
            // request in an established ledger cannot safely be called new.
            if (meta.requestCount === undefined && meta.watermark !== '0') throw malformed()
            const count = store.count(IDBKeyRange.bound(':ubm-proof-request-v2:', ':ubm-proof-request-v2:\uffff'))
            count.onsuccess = () =>
              guard(() => {
                const expected = meta.requestCount ?? 0
                if (count.result !== expected) throw malformed()
                if (meta.limit !== null && meta.retained >= meta.limit) throw quota()
                if (meta.fenceLimit != null && expected >= meta.fenceLimit) throw quota()
                if (meta.retained === Number.MAX_SAFE_INTEGER || expected === Number.MAX_SAFE_INTEGER) throw quota()
                const attempt = checkedAttempt({
                  ...requested,
                  epoch: meta.epoch,
                  sequence: (BigInt(meta.watermark) + 1n).toString()
                })
                store.add({ recordingId: proofKey(attempt.sequence), version: 1, state: 'pending', attempt })
                store.add({ recordingId: key, version: 1, attempt })
                store.put({
                  ...meta,
                  watermark: attempt.sequence,
                  retained: meta.retained + 1,
                  requestCount: expected + 1
                })
                done(attempt)
              })
          })
      }, onOwned)
    },
    inspect: (attempt: WebProofAdmission, requireRequestFence = false): Promise<WebProofOutcome> =>
      withProof(
        attempt,
        (_tx, _meta, row, done) => done(row === undefined ? { state: 'retired-or-unknown' } : outcome(row)),
        requireRequestFence
      ),
    cancel: (attempt: WebProofAdmission, requireRequestFence = false): Promise<WebProofOutcome> =>
      withProof(
        attempt,
        (tx, _meta, row, done) => {
          if (row === undefined) {
            done({ state: 'retired-or-unknown' })
            return
          }
          if (row.state === 'pending') tx.objectStore('readerReceipts').put({ ...row, state: 'cancelled' })
          done(row.state === 'committed' ? outcome(row) : { state: 'cancelled', attempt: row.attempt })
        },
        requireRequestFence
      ),
    retire: (
      options: {
        readonly outcome: Exclude<WebProofOutcome, { readonly state: 'pending' | 'retired-or-unknown' }>
        readonly acknowledgeDurableCopy: true
      },
      requireRequestFence = false
    ): Promise<{ readonly state: 'retired' | 'retired-or-unknown' }> => {
      if (
        options.acknowledgeDurableCopy !== true ||
        (options.outcome.state !== 'cancelled' && options.outcome.state !== 'committed')
      )
        return Promise.reject(invalid())
      const terminal = options.outcome
      const state = terminal.state
      if (Object.keys(terminal).length !== (state === 'committed' ? 3 : 2)) return Promise.reject(invalid())
      if (state === 'committed') {
        const result = terminal.result
        if (
          result === null ||
          typeof result !== 'object' ||
          Object.keys(result).length !== 2 ||
          result.cleared !== true ||
          result.records !== 0
        )
          return Promise.reject(invalid())
      }
      return withProof(
        terminal.attempt,
        (tx, meta, row, done) => {
          if (row === undefined) {
            done({ state: 'retired-or-unknown' })
            return
          }
          if (row.state !== state || meta.retained < 1) throw conflict()
          const store = tx.objectStore('readerReceipts')
          store.delete(row.recordingId)
          store.put({ ...meta, retained: meta.retained - 1 })
          done({ state: 'retired' })
        },
        requireRequestFence
      )
    }
  }
}
