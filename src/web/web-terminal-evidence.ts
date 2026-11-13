import { contractError } from '../backend-contract/errors'
import type {
  TerminalEvidenceAppendResult,
  TerminalEvidenceReservation,
  TerminalEvidenceReservationRequest,
  TerminalEvidenceSource
} from '../backend-contract/terminal-evidence'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import type { WebOrderedJournalDatabase, WebOrderedJournalTransaction } from './web-ordered-recording-journal'
import { acquireWebRecordingLock } from './web-recording-lock'
import { encodeTerminalEvidence } from '../core/terminal-evidence-codec'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import type { WebRecordingLockManager } from './web-recording-lock'

export interface WebTerminalEvidenceOptions {
  readonly database: WebOrderedJournalDatabase
  readonly locks?: WebRecordingLockManager
  readonly digest: (value: string) => Promise<string>
  /** SHA-256 of the exact persisted bytes, encoded as lowercase hexadecimal. */
  readonly digestBytes: (value: Readonly<Uint8Array>) => Promise<string>
  /** Explicit host-wide durable capacity, independent of each reservation. */
  readonly maximumRetainedBytes: number
}
export interface WebTerminalEvidenceSource extends TerminalEvidenceSource {
  reservationForScope(scope: TerminalEvidenceReservationRequest['scope']): TerminalEvidenceReservation | undefined
  commitTerminal(
    scope: TerminalEvidenceReservationRequest['scope'],
    terminal: StreamTerminalNotice
  ): Promise<TerminalEvidenceAppendResult>
  listTerminalEvidence(): Promise<readonly WebTerminalEvidenceRecoveryEntry[]>
  retryPendingTerminalEvidence(): Promise<void>
}

export interface WebTerminalEvidenceRecoveryEntry {
  readonly ref: string
  readonly producer: TerminalEvidenceReservationRequest['producer']
  readonly scope: TerminalEvidenceReservationRequest['scope']
  readonly maximumBytes: number
  readonly state: 'reserved' | 'staged' | 'committed'
  readonly byteLength: number | null
  readonly digest: string | null
}

type Row = {
  readonly ref: string
  readonly version: 1
  readonly kind: 'terminal-evidence'
  readonly maximumBytes: number
  readonly producer: 'scan' | 'subscription'
  readonly scope: TerminalEvidenceReservationRequest['scope']
  readonly formatVersion: 1
  readonly state: 'reserved' | 'staged' | 'committed'
  readonly bytes?: readonly number[]
  readonly digest?: string
}
type StateRow = {
  readonly key: 'quota'
  readonly version: 1
  readonly maximumBytes: number
  readonly reservedBytes: number
}

function isPersistedStateRow(row: unknown): row is StateRow {
  return (
    typeof row === 'object' &&
    row !== null &&
    'key' in row &&
    row.key === 'quota' &&
    'version' in row &&
    row.version === 1 &&
    'maximumBytes' in row &&
    typeof row.maximumBytes === 'number' &&
    Number.isSafeInteger(row.maximumBytes) &&
    row.maximumBytes > 0 &&
    'reservedBytes' in row &&
    typeof row.reservedBytes === 'number' &&
    Number.isSafeInteger(row.reservedBytes) &&
    row.reservedBytes >= 0 &&
    row.reservedBytes <= row.maximumBytes
  )
}

function assertRequest(request: TerminalEvidenceReservationRequest): void {
  if (request.producer !== 'scan' && request.producer !== 'subscription')
    throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.producer')
  if (request.formatVersion !== 1)
    throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.format-version')
  if (!Number.isSafeInteger(request.maximumBytes) || request.maximumBytes < 1)
    throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.maximum-bytes')
  if (
    request.scope.managerId.length === 0 ||
    request.scope.operationId.length === 0 ||
    request.scope.incarnation.length === 0
  )
    throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.scope')
}

function assertDigest(digest: string): void {
  if (!/^[0-9a-f]{64}$/.test(digest))
    throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.digest')
}

function transact<T>(
  database: WebOrderedJournalDatabase,
  queue: (tx: WebOrderedJournalTransaction) => void,
  value: () => T
): Promise<T> {
  return runWebIndexedDbTransaction(database, ['terminalEvidence', 'terminalEvidenceState'], 'readwrite', tx => {
    queue(tx)
    return value()
  }).then(receipt => receipt.value)
}

function sameRecord(row: Row, bytes: readonly number[], digest: string): boolean {
  return row.digest === digest && JSON.stringify(row.bytes) === JSON.stringify(bytes)
}

/** Every durable row is untrusted after an IndexedDB restart. */
function assertPersistedRow(row: Row): void {
  if (row.producer !== 'scan' && row.producer !== 'subscription')
    throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.producer')
  if (
    typeof row.ref !== 'string' ||
    !/^:ubm-terminal-evidence-v1:[0-9a-f]{64}$/.test(row.ref) ||
    row.version !== 1 ||
    row.kind !== 'terminal-evidence' ||
    !Number.isSafeInteger(row.maximumBytes) ||
    row.maximumBytes < 1 ||
    row.formatVersion !== 1 ||
    (row.state !== 'reserved' && row.state !== 'staged' && row.state !== 'committed') ||
    typeof row.scope?.managerId !== 'string' ||
    row.scope.managerId.length === 0 ||
    typeof row.scope.operationId !== 'string' ||
    row.scope.operationId.length === 0 ||
    typeof row.scope.incarnation !== 'string' ||
    row.scope.incarnation.length === 0
  ) {
    throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.row')
  }
  const hasBytes = row.bytes !== undefined
  const hasDigest = row.digest !== undefined
  if (row.state === 'reserved' ? hasBytes || hasDigest : !hasBytes || !hasDigest) {
    throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.row')
  }
  if (!hasBytes) return
  if (
    !Array.isArray(row.bytes) ||
    row.bytes.length > row.maximumBytes ||
    row.bytes.some(byte => !Number.isSafeInteger(byte) || byte < 0 || byte > 255)
  ) {
    throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.row')
  }
  assertDigest(row.digest ?? '')
}

function recoveryEntry(row: Row): WebTerminalEvidenceRecoveryEntry {
  assertPersistedRow(row)
  return Object.freeze({
    ref: row.ref,
    producer: row.producer,
    scope: Object.freeze({ ...row.scope }),
    maximumBytes: row.maximumBytes,
    state: row.state,
    byteLength: row.bytes?.length ?? null,
    digest: row.digest ?? null
  })
}

function referenceDigest(ref: string, operation: 'release' | 'retry'): string {
  const digest = /^:ubm-terminal-evidence-v1:([0-9a-f]{64})$/.exec(ref)?.[1]
  if (digest === undefined) throw contractError('argument.invalid', 'stream', `web-terminal-evidence.${operation}-ref`)
  return digest
}

/** Web Locks and IndexedDB jointly own one durable preactivation reservation. */
export function createWebTerminalEvidenceSource(options: WebTerminalEvidenceOptions): WebTerminalEvidenceSource {
  const reservations = new Map<string, TerminalEvidenceReservation>()
  const retirements = new Map<
    string,
    {
      readonly lock: Extract<Awaited<ReturnType<typeof acquireWebRecordingLock>>, { state: 'acquired' }>
      databaseReleased: boolean
    }
  >()
  const scopeKey = (
    producer: TerminalEvidenceReservationRequest['producer'],
    scope: TerminalEvidenceReservationRequest['scope']
  ): string => JSON.stringify([producer, scope.managerId, scope.operationId, scope.incarnation])
  return {
    reservationForScope: scope => reservations.get(scopeKey('subscription', scope)),
    async commitTerminal(scope, terminal) {
      const reservation = reservations.get(scopeKey('subscription', scope))
      if (reservation === undefined)
        throw contractError('lifecycle.invalid-state', 'stream', 'web-terminal-evidence.scope')
      const canonicalBytes = encodeTerminalEvidence(terminal, scope)
      const digest = await options.digestBytes(canonicalBytes)
      return reservation.commit({ canonicalBytes, digest })
    },
    async listTerminalEvidence(): Promise<readonly WebTerminalEvidenceRecoveryEntry[]> {
      let rows: readonly Row[] = []
      await runWebIndexedDbTransaction(options.database, ['terminalEvidence'], 'readonly', tx => {
        const request = tx.objectStore('terminalEvidence').getAll(undefined, 0xffffffff)
        request.onsuccess = () => {
          rows = request.result as readonly Row[]
        }
        return undefined
      })
      return rows.map(recoveryEntry)
    },
    async retryPendingTerminalEvidence(): Promise<void> {
      let rows: readonly Row[] = []
      await runWebIndexedDbTransaction(options.database, ['terminalEvidence'], 'readonly', tx => {
        const request = tx.objectStore('terminalEvidence').getAll(undefined, 0xffffffff)
        request.onsuccess = () => {
          rows = request.result as readonly Row[]
        }
        return undefined
      })
      for (const row of rows) {
        assertPersistedRow(row)
        if (row.kind !== 'terminal-evidence' || row.state !== 'staged') continue
        if (row.bytes === undefined || row.digest === undefined)
          throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.row')
        const canonicalBytes = Uint8Array.from(row.bytes)
        if (canonicalBytes.byteLength > row.maximumBytes || (await options.digestBytes(canonicalBytes)) !== row.digest)
          throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.staged')
        if (options.locks === undefined)
          throw contractError('capability.unsupported', 'stream', 'web-terminal-evidence.retry-lock')
        const digest = referenceDigest(row.ref, 'retry')
        const lock = await acquireWebRecordingLock(options.locks, async () => digest, `terminal-${digest.slice(0, 32)}`)
        if (lock.state === 'busy')
          throw contractError('capability.unavailable', 'stream', 'web-terminal-evidence.retry-busy')
        try {
          await transact(
            options.database,
            tx => {
              const read = tx.objectStore('terminalEvidence').get(row.ref)
              read.onsuccess = () => {
                const current = read.result as Row | undefined
                if (current?.state === 'committed' && sameRecord(current, row.bytes ?? [], row.digest ?? '')) return
                if (current?.state !== 'staged' || !sameRecord(current, row.bytes ?? [], row.digest ?? '')) {
                  tx.abort()
                  return
                }
                tx.objectStore('terminalEvidence').put({ ...current, state: 'committed' })
              }
            },
            () => undefined
          )
        } finally {
          await lock.release()
        }
      }
    },
    async readTerminalEvidence(ref, maximumBytes) {
      if (!Number.isSafeInteger(maximumBytes) || maximumBytes < 1)
        throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.read')
      let row: Row | undefined
      await runWebIndexedDbTransaction(options.database, ['terminalEvidence'], 'readonly', tx => {
        const request = tx.objectStore('terminalEvidence').get(ref)
        request.onsuccess = () => {
          row = request.result as Row | undefined
        }
        return undefined
      })
      if (row === undefined) return { kind: 'missing' as const }
      assertPersistedRow(row)
      if (row.state !== 'committed' || row.bytes === undefined || row.digest === undefined)
        return { kind: 'unresolved' as const }
      const canonicalBytes = Uint8Array.from(row.bytes)
      if (canonicalBytes.byteLength > maximumBytes)
        throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.read-bound')
      assertDigest(row.digest)
      if ((await options.digestBytes(canonicalBytes)) !== row.digest)
        throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.digest')
      return { kind: 'committed' as const, canonicalBytes, digest: row.digest }
    },
    async releaseTerminalEvidence(ref) {
      let retirement = retirements.get(ref)
      if (retirement === undefined) {
        if (options.locks === undefined)
          throw contractError('capability.unsupported', 'stream', 'web-terminal-evidence.release-lock')
        const digest = referenceDigest(ref, 'release')
        const lock = await acquireWebRecordingLock(options.locks, async () => digest, `terminal-${digest.slice(0, 32)}`)
        if (lock.state === 'busy')
          throw contractError('capability.unavailable', 'stream', 'web-terminal-evidence.release-busy')
        retirement = { lock, databaseReleased: false }
        retirements.set(ref, retirement)
      }
      if (!retirement.databaseReleased) {
        let stateRejected = false
        try {
          await transact(
            options.database,
            tx => {
              const evidence = tx.objectStore('terminalEvidence').get(ref)
              evidence.onsuccess = () => {
                const row = evidence.result as Row | undefined
                if (row === undefined) return
                assertPersistedRow(row)
                // A live source retains this same exclusive lock, so reaching
                // this point for a reserved row proves its owner is gone. A
                // staged row has bytes awaiting durable retry and cannot be
                // retired by acknowledgement.
                if (row.state === 'staged') {
                  stateRejected = true
                  tx.abort()
                  return
                }
                const quota = tx.objectStore('terminalEvidenceState').get('quota')
                quota.onsuccess = () => {
                  const state = quota.result
                  if (!isPersistedStateRow(state) || state.reservedBytes < row.maximumBytes) {
                    tx.abort()
                    return
                  }
                  tx.objectStore('terminalEvidence').delete(ref)
                  tx.objectStore('terminalEvidenceState').put({
                    ...state,
                    reservedBytes: state.reservedBytes - row.maximumBytes
                  })
                }
              }
              return undefined
            },
            () => undefined
          )
        } catch (error) {
          if (!stateRejected) throw error
          await retirement.lock.release()
          retirements.delete(ref)
          throw contractError('lifecycle.invalid-state', 'stream', 'web-terminal-evidence.release-state')
        }
        retirement.databaseReleased = true
      }
      await retirement.lock.release()
      retirements.delete(ref)
    },
    async reserveTerminalEvidence(request): Promise<TerminalEvidenceReservation> {
      assertRequest(request)
      const maximumRetainedBytes = options.maximumRetainedBytes
      if (!Number.isSafeInteger(maximumRetainedBytes) || maximumRetainedBytes < request.maximumBytes)
        throw contractError('capability.unavailable', 'stream', 'web-terminal-evidence.quota')
      if (options.locks === undefined)
        throw contractError('capability.unsupported', 'stream', 'web-terminal-evidence.web-lock')
      const encodedScope = JSON.stringify([
        request.producer,
        request.formatVersion,
        request.scope.managerId,
        request.scope.operationId,
        request.scope.incarnation
      ])
      const digest = await options.digest(encodedScope)
      if (!/^[0-9a-f]{64}$/.test(digest))
        throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.lock-digest')
      const key = `:ubm-terminal-evidence-v1:${digest}`
      const lock = await acquireWebRecordingLock(options.locks, async () => digest, `terminal-${digest.slice(0, 32)}`)
      if (lock.state === 'busy')
        throw contractError('capability.unavailable', 'stream', 'web-terminal-evidence.reservation-busy')
      let released = false
      let databaseReleased = false
      let committed = false
      // A staged or committed row is durable recovery evidence. Runtime
      // teardown may release the reservation's lock, but only recovery ACK
      // may delete that row and return its durable quota.
      let retainedForRecovery = false
      let committing: Promise<TerminalEvidenceAppendResult> | undefined
      let releasing: Promise<void> | undefined
      try {
        await transact(
          options.database,
          tx => {
            const store = tx.objectStore('terminalEvidence')
            const stateStore = tx.objectStore('terminalEvidenceState')
            const state = stateStore.get('quota')
            const read = store.get(key)
            read.onsuccess = () => {
              if (read.result !== undefined) {
                const row = read.result as Row
                assertPersistedRow(row)
                if (
                  row.producer !== request.producer ||
                  JSON.stringify(row.scope) !== JSON.stringify(request.scope) ||
                  row.maximumBytes !== request.maximumBytes
                )
                  tx.abort()
                return
              }
              const prior = state.result
              if (
                prior !== undefined &&
                (!isPersistedStateRow(prior) ||
                  prior.maximumBytes !== maximumRetainedBytes ||
                  prior.reservedBytes > maximumRetainedBytes - request.maximumBytes)
              ) {
                tx.abort()
                return
              }
              stateStore.put({
                key: 'quota',
                version: 1,
                maximumBytes: maximumRetainedBytes,
                reservedBytes: (prior?.reservedBytes ?? 0) + request.maximumBytes
              } satisfies StateRow)
              store.put({
                ref: key,
                version: 1,
                kind: 'terminal-evidence',
                maximumBytes: request.maximumBytes,
                producer: request.producer,
                scope: request.scope,
                formatVersion: 1,
                state: 'reserved'
              } satisfies Row)
            }
          },
          () => undefined
        )
      } catch (error) {
        await lock.release()
        throw error
      }
      const reservation: TerminalEvidenceReservation = {
        maximumBytes: request.maximumBytes,
        commit: (record): Promise<TerminalEvidenceAppendResult> => {
          if (released) throw contractError('lifecycle.invalid-state', 'stream', 'web-terminal-evidence.released')
          if (committing !== undefined) return committing
          if (committed) throw contractError('lifecycle.invalid-state', 'stream', 'web-terminal-evidence.committed')
          committing = (async (): Promise<TerminalEvidenceAppendResult> => {
            if (record.canonicalBytes.byteLength > request.maximumBytes)
              throw contractError('argument.invalid', 'stream', 'web-terminal-evidence.record-too-large')
            const bytes = Array.from(record.canonicalBytes)
            assertDigest(record.digest)
            if ((await options.digestBytes(record.canonicalBytes)) !== record.digest)
              throw contractError('protocol.malformed', 'stream', 'web-terminal-evidence.digest')
            let alreadyCommitted = false
            await transact(
              options.database,
              tx => {
                const store = tx.objectStore('terminalEvidence')
                const read = store.get(key)
                read.onsuccess = () => {
                  const prior = read.result as Row | undefined
                  if (prior === undefined || prior.maximumBytes !== request.maximumBytes) {
                    tx.abort()
                    return
                  }
                  if (prior.state === 'committed') {
                    if (!sameRecord(prior, bytes, record.digest)) tx.abort()
                    else alreadyCommitted = true
                    return
                  }
                  if (prior.state === 'staged') {
                    if (!sameRecord(prior, bytes, record.digest)) tx.abort()
                    return
                  }
                  if (prior.state !== 'reserved') {
                    tx.abort()
                    return
                  }
                  store.put({ ...prior, state: 'staged', bytes, digest: record.digest })
                }
              },
              () => undefined
            )
            retainedForRecovery = true
            if (!alreadyCommitted)
              await transact(
                options.database,
                tx => {
                  const store = tx.objectStore('terminalEvidence')
                  const read = store.get(key)
                  read.onsuccess = () => {
                    const prior = read.result as Row | undefined
                    if (prior === undefined || prior.state === 'committed') {
                      if (prior?.state === 'committed' && sameRecord(prior, bytes, record.digest)) return
                      tx.abort()
                      return
                    }
                    if (prior.state !== 'staged' || !sameRecord(prior, bytes, record.digest)) {
                      tx.abort()
                      return
                    }
                    store.put({ ...prior, state: 'committed' })
                  }
                },
                () => undefined
              )
            committed = true
            return {
              ref: key,
              byteLength: bytes.length,
              digest: record.digest,
              formatVersion: 1,
              durability: 'committed',
              scope: request.scope,
              producer: request.producer
            }
          })().finally(() => {
            committing = undefined
          })
          return committing
        },
        release: (): Promise<void> => {
          if (released) return Promise.resolve()
          if (releasing !== undefined) return releasing
          releasing = (async () => {
            if (!databaseReleased && !retainedForRecovery) {
              await transact(
                options.database,
                tx => {
                  const evidence = tx.objectStore('terminalEvidence').get(key)
                  evidence.onsuccess = () => {
                    const row = evidence.result as Row | undefined
                    if (row === undefined) return
                    assertPersistedRow(row)
                    if (row.maximumBytes !== request.maximumBytes || row.producer !== request.producer) {
                      tx.abort()
                      return
                    }
                    if (row.state !== 'reserved') {
                      retainedForRecovery = true
                      return
                    }
                    const quota = tx.objectStore('terminalEvidenceState').get('quota')
                    quota.onsuccess = () => {
                      const state = quota.result
                      if (!isPersistedStateRow(state) || state.reservedBytes < request.maximumBytes) {
                        tx.abort()
                        return
                      }
                      tx.objectStore('terminalEvidence').delete(key)
                      tx.objectStore('terminalEvidenceState').put({
                        ...state,
                        reservedBytes: state.reservedBytes - request.maximumBytes
                      })
                    }
                  }
                },
                () => undefined
              )
              databaseReleased = true
            }
            await lock.release()
            released = true
            reservations.delete(scopeKey(request.producer, request.scope))
          })().finally(() => {
            releasing = undefined
          })
          return releasing
        }
      }
      reservations.set(scopeKey(request.producer, request.scope), reservation)
      return reservation
    }
  }
}
