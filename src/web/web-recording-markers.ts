import { contractError } from '../backend-contract/errors'
import { byteLimit, ownBytes } from '../backend-contract/primitives'
import { utf8ByteLength } from '../backend-contract/serializable'
import type { WebGattDatabase } from './web-bluetooth-handles'
import { characteristicKey } from './web-bluetooth-handles'
import type { WebRecordingSourceOwner } from './web-recording-source-owner'
import type { WebMarkerReceipt } from './web-ordered-marker'
import type {
  createWebRecordingMarkerLedger,
  WebMarkerLedgerAdmission,
  WebMarkerLedgerSettlement,
  WebMarkerLedgerSnapshot
} from './web-recording-marker-ledger'
import { isDefinitiveWebMarkerAbort } from './web-recording-marker-ledger'
import { WebRecordingReadinessError } from './web-indexeddb-transaction'
import { webRecordingCanonical } from './web-ordered-recording-journal'
import type { CharacteristicPath } from '../backend-contract/gatt'
import { WebOrderedMarkerReservations } from './web-ordered-marker-stream'
import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'

const registrations = new WeakMap<WebGattDatabase, Map<string, WebOrderedMarkerReservations>>()

export function registerWebRecordingMarkers(
  database: WebGattDatabase,
  recordingId: string,
  markers: WebOrderedMarkerReservations
): void {
  let entries = registrations.get(database)
  if (entries === undefined) {
    entries = new Map()
    registrations.set(database, entries)
  }
  entries.set(recordingId, markers)
}

export function unregisterWebRecordingMarkers(
  database: WebGattDatabase,
  recordingId: string,
  markers: WebOrderedMarkerReservations
): void {
  const entries = registrations.get(database)
  if (entries?.get(recordingId) !== markers) return
  entries.delete(recordingId)
  if (entries.size === 0) registrations.delete(database)
}

export function reserveWebRecordingMarker(database: WebGattDatabase, recordingId: string) {
  database.assertCurrent('web-recording.marker')
  const markers = registrations.get(database)?.get(recordingId)
  if (markers === undefined) throw contractError('lifecycle.invalid-state', 'gatt', 'web-recording.marker')
  return markers.reserveMarkerWrite()
}

type Path = CharacteristicPath<string, string, string, string, string, 'current'>
type Ledger = ReturnType<typeof createWebRecordingMarkerLedger>
type Stage = 'reserve' | 'admission' | 'dispatch' | 'settle' | 'ack' | 'retire'
type Entry = {
  id: string
  key: string
  bytes: Uint8Array
  mode: 'with-response' | 'without-response'
  target: WebBluetoothCharacteristicBoundary
  signal: AbortSignal | null
  expired: boolean
  abandoned: boolean
  deadline: number | null
  ops: { reserve: string; admission: string; dispatch: string; settle: string; ack: string; retire: string }
  reserved: boolean
  definitiveRefusal: boolean
  reservationUncertain: boolean
  admission: WebMarkerLedgerAdmission | null
  admissionStored: boolean
  dispatchStored: boolean
  dispatched: boolean
  settlement: WebMarkerLedgerSettlement | null
  settlementStored: boolean
  writeError: unknown
  backgroundFailure: unknown
  acknowledged: boolean
  retirementStored: boolean
  retiring: Promise<void> | null
  receipt: WebMarkerReceipt
  running: Promise<WebMarkerReceipt> | null
  operations: Partial<Record<Stage, Promise<WebMarkerLedgerSnapshot>>>
  completed: Partial<Record<Stage, boolean>>
  reconciliations: Partial<Record<Stage, ReturnType<Ledger['reconcile']>>>
  results: Partial<Record<Stage, WebMarkerLedgerSnapshot>>
}

/** Durable barriers surround the synchronous source admission and the browser write. */
export function createWebRecordingMarkers(
  database: WebGattDatabase,
  owner: WebRecordingSourceOwner,
  currentAuthority: () => boolean,
  collectionId: string,
  generation: string,
  now: () => number,
  ledgerForOwner: () => Ledger,
  token: () => string,
  onForgotten?: () => Promise<void>
): WebOrderedMarkerReservations {
  let entry: Entry | null = null
  let ledger: Ledger | null = null
  const reportedFailures = new WeakSet<object>()
  const reported = (error: unknown): boolean =>
    typeof error === 'object' && error !== null && reportedFailures.has(error)
  const getLedger = (): Ledger => (ledger ??= ledgerForOwner())
  const rememberRefusal = (e: Entry, failure: unknown): void => {
    if (!isDefinitiveWebMarkerAbort(failure) || e.reservationUncertain) return
    e.definitiveRefusal = true
    e.receipt = { marker: { state: 'not-admitted', error: failure }, write: { state: 'not-started' } }
    owner.wakeMarkerDebt()
  }
  const bounded = async <T>(promise: Promise<T>): Promise<{ pending: true } | { pending: false; value: T }> => {
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      return await Promise.race([
        promise.then(value => ({ pending: false as const, value })),
        new Promise<{ pending: true }>(resolve => {
          timer = setTimeout(() => resolve({ pending: true }), 5000)
        })
      ])
    } finally {
      if (timer !== undefined) clearTimeout(timer)
    }
  }
  const resumeAbandoned = (e: Entry): void => {
    if (!e.abandoned) return
    e.abandoned = false
    const running = e.running
    const resume = async (): Promise<void> => {
      if (running !== null) await running.catch(() => undefined)
      await advance(e)
      e.abandoned = false
      owner.wakeMarkerDebt()
    }
    resume().catch(error => {
      e.backgroundFailure ??= error
      owner.wakeMarkerDebt()
    })
  }
  const barrier = async (
    e: Entry,
    stage: Stage,
    operation: () => Promise<WebMarkerLedgerSnapshot>,
    matches: (snapshot: WebMarkerLedgerSnapshot) => boolean
  ): Promise<void> => {
    const retained = e.results[stage]
    let attempt = e.operations[stage]
    if (attempt === undefined && retained === undefined) {
      e.completed[stage] = false
      attempt = Promise.resolve().then(operation)
      e.operations[stage] = attempt
      const current = attempt
      current.then(
        value => {
          e.completed[stage] = true
          e.results[stage] = value
          if (e.operations[stage] === current) delete e.operations[stage]
          resumeAbandoned(e)
        },
        failure => {
          if (stage === 'reserve') {
            if (isDefinitiveWebMarkerAbort(failure)) rememberRefusal(e, failure)
            else e.reservationUncertain = true
          }
          if (e.operations[stage] === current) delete e.operations[stage]
          resumeAbandoned(e)
        }
      )
    }
    const result = retained === undefined && attempt !== undefined ? await bounded(attempt) : null
    let receipt: WebMarkerLedgerSnapshot
    if (retained !== undefined) receipt = retained
    else if (result !== null && !result.pending) receipt = result.value
    else {
      if (attempt === undefined)
        throw new WebRecordingReadinessError(new Error('Web marker transaction outcome unknown'))
      let pending = e.reconciliations[stage]
      if (pending === undefined) {
        pending = getLedger().reconcile(e.ops[stage])
        e.reconciliations[stage] = pending
        const current = pending
        current.then(
          value => {
            if (e.reconciliations[stage] === current) delete e.reconciliations[stage]
            if (value.outcome === 'committed') {
              e.results[stage] = value.result
              resumeAbandoned(e)
            }
          },
          () => {
            if (e.reconciliations[stage] === current) delete e.reconciliations[stage]
          }
        )
      }
      let reconciled: Awaited<
        ReturnType<typeof bounded<{ outcome: 'committed'; result: WebMarkerLedgerSnapshot } | { outcome: 'unknown' }>>
      >
      try {
        reconciled = await bounded(
          Promise.race([pending, attempt.then(value => ({ outcome: 'committed' as const, result: value }))])
        )
      } catch (error) {
        // A failed reconciliation says nothing about the still-running original
        // transaction. Its later callback must resume the owner as well.
        e.abandoned = true
        if (e.completed[stage] || e.results[stage] !== undefined) resumeAbandoned(e)
        throw error
      }
      if (reconciled.pending || reconciled.value.outcome === 'unknown') {
        e.abandoned = true
        if (e.completed[stage] || e.results[stage] !== undefined) resumeAbandoned(e)
        throw new WebRecordingReadinessError(new Error('Web marker transaction outcome unknown'))
      }
      receipt = reconciled.value.result
    }
    try {
      if (
        typeof receipt !== 'object' ||
        receipt === null ||
        typeof receipt.request !== 'object' ||
        receipt.request === null ||
        !Array.isArray(receipt.request.bytes) ||
        receipt.version !== 1 ||
        receipt.recordingId !== getLedger().identity.recordingId ||
        receipt.producerIncarnation !== getLedger().identity.producerIncarnation ||
        receipt.markerId !== e.id ||
        webRecordingCanonical(receipt.request.target) !== webRecordingCanonical(JSON.parse(e.key)) ||
        receipt.request.mode !== e.mode ||
        receipt.request.bytes.length !== e.bytes.length ||
        receipt.request.bytes.some((byte, index) => byte !== e.bytes[index]) ||
        (e.admission !== null && webRecordingCanonical(receipt.admission) !== webRecordingCanonical(e.admission)) ||
        !matches(receipt)
      ) {
        // A malformed reply cannot authorize the transition, but must not poison
        // an exact retry of the durable operation indefinitely.
        throw contractError('lifecycle.invariant-violation', 'restoration', 'web-recording.marker-receipt')
      }
    } catch (error) {
      if (e.results[stage] === receipt) delete e.results[stage]
      throw error
    }
    delete e.results[stage]
  }
  const debt = (): boolean => entry !== null && !entry.retirementStored && !entry.definitiveRefusal
  const retireIfSettled = async (e: Entry): Promise<void> => {
    if (
      e.retirementStored ||
      !e.admissionStored ||
      (e.admission?.state === 'admitted' && (!e.settlementStored || !e.acknowledged))
    )
      return
    if (e.retiring === null) {
      const retiring = barrier(
        e,
        'retire',
        () => getLedger().retire(e.ops.retire, e.id),
        receipt => receipt.retired
      ).then(() => {
        e.retirementStored = true
        owner.wakeMarkerDebt()
      })
      e.retiring = retiring
      retiring.then(
        () => {
          if (e.retiring === retiring) e.retiring = null
        },
        () => {
          if (e.retiring === retiring) e.retiring = null
        }
      )
    }
    await e.retiring
  }
  owner.markerDebtPending = debt
  const evidence = (error: unknown): string => {
    const value = (error instanceof Error ? `${error.name}: ${error.message}` : String(error)) || 'unknown'
    let result = ''
    for (const character of value) {
      if (utf8ByteLength(JSON.stringify(result + character)) > 2048) break
      result += character
    }
    return result || 'unknown'
  }
  const snapshot = (e: Entry): WebMarkerReceipt => ({ marker: e.receipt.marker, write: e.receipt.write })
  const advance = (e: Entry): Promise<WebMarkerReceipt> => {
    if (e.running !== null) return e.running
    const run = (async (): Promise<WebMarkerReceipt> => {
      const store = getLedger()
      if (e.definitiveRefusal) return snapshot(e)
      if (!e.reserved) {
        try {
          await barrier(
            e,
            'reserve',
            () =>
              store.reserve(e.ops.reserve, { markerId: e.id, target: JSON.parse(e.key), mode: e.mode, bytes: e.bytes }),
            receipt =>
              receipt.admission === null && !receipt.dispatchIntent && receipt.settlement === null && !receipt.retired
          )
        } catch (error) {
          rememberRefusal(e, error)
          throw error
        }
        e.reserved = true
      }
      if (e.admission === null) {
        const source = owner.installedSource()
        if (
          e.signal?.aborted ||
          e.expired ||
          (e.deadline !== null && now() >= e.deadline) ||
          source === null ||
          source.terminal ||
          !owner.isCurrent() ||
          !currentAuthority()
        ) {
          const error = contractError(
            e.signal?.aborted
              ? 'operation.aborted'
              : e.expired || (e.deadline !== null && now() >= e.deadline)
                ? 'operation.timed-out'
                : 'stream.closed',
            'gatt',
            'web-recording.marker'
          )
          e.receipt = { marker: { state: 'not-admitted', error }, write: { state: 'not-started' } }
          e.admission = { state: 'not-admitted', evidence: evidence(error) }
        } else {
          let attempted = false
          try {
            source.reserveMarker(collectionId, e.id)
            const ordinal = source.admitMarker(position => {
              attempted = true
              return source.admitReservedMarker({
                kind: 'marker',
                collectionId,
                connectionGeneration: String(database.path.connectionGeneration),
                databaseGeneration: String(database.path.databaseGeneration),
                sourceGeneration: { kind: 'opaque', value: generation },
                sourceMembership: null,
                markerId: e.id,
                ordinal: position
              })
            })
            if (ordinal === null) {
              if (!attempted) source.releaseMarkerNotAdmitted(collectionId, e.id)
              const error = contractError('stream.closed', 'stream', 'web-recording.marker')
              e.admission = attempted
                ? { state: 'rejected', evidence: evidence(error), delivery: 'none' }
                : { state: 'not-admitted', evidence: evidence(error) }
              e.receipt = {
                marker: { state: attempted ? 'rejected' : 'not-admitted', error },
                write: { state: 'not-started' }
              }
            } else {
              e.admission = { state: 'admitted', ordinal }
              e.receipt = { marker: { state: 'admitted', ordinal }, write: { state: 'not-started' } }
            }
          } catch (error) {
            const state = source.markerAdmissionStatus(collectionId, e.id)
            if (state.state === 'reserved') source.releaseMarkerNotAdmitted(collectionId, e.id)
            if (state.state === 'admitted') {
              e.admission = { state: 'admitted', ordinal: state.ordinal }
              e.receipt = {
                marker: { state: 'admitted', ordinal: state.ordinal },
                write: { state: 'not-started', error }
              }
              e.settlement = { state: 'not-started', evidence: evidence(error) }
            } else {
              e.admission =
                state.state === 'rejected'
                  ? { state: 'rejected', evidence: evidence(error), delivery: 'none' }
                  : { state: 'not-admitted', evidence: evidence(error) }
              e.receipt = {
                marker: { state: state.state === 'rejected' ? 'rejected' : 'not-admitted', error },
                write: { state: 'not-started' }
              }
            }
          }
        }
      }
      if (!e.admissionStored) {
        const admission = e.admission
        await barrier(
          e,
          'admission',
          () => store.admit(e.ops.admission, e.id, admission),
          receipt => webRecordingCanonical(receipt.admission) === webRecordingCanonical(admission)
        )
        e.admissionStored = true
      }
      if (e.admission.state !== 'admitted') {
        await retireIfSettled(e)
        return snapshot(e)
      }
      if (!e.dispatched && e.settlement === null) {
        if (
          e.signal?.aborted ||
          e.expired ||
          (e.deadline !== null && now() >= e.deadline) ||
          !owner.isCurrent() ||
          !currentAuthority() ||
          owner.installedSource()?.terminal !== false
        ) {
          const error = contractError(
            e.signal?.aborted
              ? 'operation.aborted'
              : e.expired || (e.deadline !== null && now() >= e.deadline)
                ? 'operation.timed-out'
                : 'stream.closed',
            'gatt',
            'web-recording.marker-write'
          )
          e.receipt = { marker: e.receipt.marker, write: { state: 'not-started', error } }
          e.settlement = { state: 'not-started', evidence: evidence(error) }
        } else {
          if (!e.dispatchStored) {
            await barrier(
              e,
              'dispatch',
              () => store.dispatchIntent(e.ops.dispatch, e.id),
              receipt => receipt.dispatchIntent
            )
            e.dispatchStored = true
          }
          if (
            e.signal?.aborted ||
            e.expired ||
            (e.deadline !== null && now() >= e.deadline) ||
            !owner.isCurrent() ||
            !currentAuthority() ||
            owner.installedSource()?.terminal !== false
          ) {
            const error = contractError(
              e.signal?.aborted
                ? 'operation.aborted'
                : e.expired || (e.deadline !== null && now() >= e.deadline)
                  ? 'operation.timed-out'
                  : 'stream.closed',
              'gatt',
              'web-recording.marker-write'
            )
            e.receipt = { marker: e.receipt.marker, write: { state: 'not-started', error } }
            e.settlement = { state: 'not-started', evidence: evidence(error) }
          } else {
            // Once intent is durable, never invoke the Bluetooth operation twice, even if its result is lost.
            e.dispatched = true
            e.receipt = { marker: e.receipt.marker, write: { state: 'started' } }
            let write: Promise<void>
            try {
              write =
                e.mode === 'with-response'
                  ? e.target.writeValueWithResponse(new Uint8Array(e.bytes))
                  : e.target.writeValueWithoutResponse(new Uint8Array(e.bytes))
            } catch (error) {
              write = Promise.reject(error)
            }
            write
              .then(
                () => {
                  e.settlement = { state: 'settled', outcome: 'success' }
                  e.receipt = { marker: e.receipt.marker, write: { state: 'settled', result: { ok: true } } }
                },
                error => {
                  e.settlement = { state: 'settled', outcome: 'failure', evidence: evidence(error) }
                  e.writeError = error
                  e.receipt = { marker: e.receipt.marker, write: { state: 'settled', result: { ok: false, error } } }
                }
              )
              .then(async () => {
                if (e.running !== null)
                  await e.running.catch(error => {
                    if (!reported(error)) e.backgroundFailure ??= error
                  })
                await advance(e)
              })
              .catch(error => {
                if (!reported(error)) e.backgroundFailure ??= error
                owner.wakeMarkerDebt()
              })
          }
        }
      }
      if (e.settlement !== null && !e.settlementStored) {
        const settlement = e.settlement
        await barrier(
          e,
          'settle',
          () => store.settle(e.ops.settle, e.id, settlement),
          receipt => webRecordingCanonical(receipt.settlement) === webRecordingCanonical(settlement)
        )
        e.settlementStored = true
        if (e.settlement.state === 'settled')
          e.receipt = {
            marker: e.receipt.marker,
            write: {
              state: 'settled',
              result: e.settlement.outcome === 'success' ? { ok: true } : { ok: false, error: e.writeError }
            }
          }
        owner.wakeMarkerDebt()
      }
      await retireIfSettled(e)
      return snapshot(e)
    })()
    e.running = run
    run.then(
      () => {
        if (e.running === run) e.running = null
      },
      () => {
        if (e.running === run) e.running = null
      }
    )
    return run
  }
  const publicAdvance = (e: Entry): Promise<WebMarkerReceipt> => {
    const work = advance(e)
    const boundedWait = new Promise<WebMarkerReceipt>((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined
      let abortTimer: ReturnType<typeof setTimeout> | undefined
      const cleanup = (): void => {
        if (timer !== undefined) clearTimeout(timer)
        if (abortTimer !== undefined) clearTimeout(abortTimer)
        e.signal?.removeEventListener('abort', onAbort)
      }
      const fail = (code: 'operation.aborted' | 'operation.timed-out'): void => {
        e.abandoned = true
        if (code === 'operation.timed-out') e.expired = true
        cleanup()
        reject(contractError(code, 'gatt', 'web-recording.marker'))
      }
      const onAbort = (): void => {
        abortTimer = setTimeout(() => fail('operation.aborted'), 0)
      }
      e.signal?.addEventListener('abort', onAbort, { once: true })
      if (e.signal?.aborted) onAbort()
      if (e.deadline !== null) timer = setTimeout(() => fail('operation.timed-out'), Math.max(0, e.deadline - now()))
      work.then(
        value => {
          cleanup()
          resolve(value)
        },
        error => {
          cleanup()
          reject(error)
        }
      )
    })
    return boundedWait.then(
      receipt => {
        e.abandoned = false
        owner.wakeMarkerDebt()
        return receipt
      },
      error => {
        if (error instanceof WebRecordingReadinessError) e.abandoned = true
        if (typeof error === 'object' && error !== null) reportedFailures.add(error)
        if (e.backgroundFailure === error) e.backgroundFailure = null
        throw error
      }
    )
  }
  const markers = new WebOrderedMarkerReservations(
    {
      markerWrite: (id, path: Path, value, mode, signal, expired = false, deadline = null) => {
        const key = JSON.stringify(path)
        if (entry !== null) {
          if (
            entry.id !== id ||
            entry.key !== key ||
            entry.mode !== mode ||
            entry.bytes.length !== value.length ||
            entry.bytes.some((byte, index) => byte !== value[index])
          )
            throw contractError('lifecycle.invalid-state', 'gatt', 'web-recording.marker-retry')
          if (entry.backgroundFailure !== null) {
            const failure = entry.backgroundFailure
            entry.backgroundFailure = null
            throw failure
          }
          return publicAdvance(entry)
        }
        database.assertPath(path, 'web-recording.marker-write')
        if (!owner.isCurrent() || !currentAuthority())
          throw contractError('lifecycle.invalid-state', 'gatt', 'web-recording.marker-owner')
        if (!database.record.grantedServices.has(String(path.serviceUuid)))
          throw contractError('chooser.optional-service-not-granted', 'chooser', 'web-recording.marker-write')
        const target = database.characteristicBoundaries.get(characteristicKey(path))
        if (target === undefined) throw contractError('gatt.stale-handle', 'gatt', 'web-recording.marker-write')
        if (
          (mode === 'with-response' && !target.properties.write) ||
          (mode === 'without-response' && !target.properties.writeWithoutResponse)
        )
          throw contractError('gatt.property-not-supported', 'gatt', 'web-recording.marker-write')
        entry = {
          id,
          key,
          bytes: ownBytes(value, byteLimit(512 * 1024)),
          mode,
          target,
          signal,
          expired,
          abandoned: false,
          deadline,
          ops: {
            reserve: token(),
            admission: token(),
            dispatch: token(),
            settle: token(),
            ack: token(),
            retire: token()
          },
          reserved: false,
          definitiveRefusal: false,
          reservationUncertain: false,
          admission: null,
          admissionStored: false,
          dispatchStored: false,
          dispatched: false,
          settlement: null,
          settlementStored: false,
          writeError: null,
          backgroundFailure: null,
          acknowledged: false,
          retirementStored: false,
          retiring: null,
          receipt: { marker: { state: 'not-admitted', error: null }, write: { state: 'not-started' } },
          running: null,
          operations: {},
          completed: {},
          reconciliations: {},
          results: {}
        }
        return publicAdvance(entry)
      },
      hasMarkerReceipt: id => entry?.id === id,
      acknowledgeMarker: async id => {
        const e = entry
        if (e === null || e.id !== id)
          throw contractError('lifecycle.invalid-state', 'gatt', 'web-recording.marker-ack')
        // The delivery callback supplies the exact journal identity below.
      },
      forgetMarker: async id => {
        const e = entry
        if (e === null || e.id !== id) return false
        if (!e.definitiveRefusal) await advance(e)
        if (debt()) return false
        getLedger().releaseSignatures(Object.values(e.ops))
        entry = null
        owner.wakeMarkerDebt()
        return true
      }
    },
    now,
    onForgotten
  )
  owner.onAcknowledged = async delivery => {
    if (delivery.record.kind !== 'marker') return
    const e = entry
    if (e === null || e.id !== delivery.record.markerId || delivery.journalOrdinal === undefined)
      throw contractError('lifecycle.invariant-violation', 'restoration', 'web-recording.marker-ack')
    try {
      await advance(e)
    } catch (error) {
      // ACK is independent of settlement, but admission must be durable first.
      if (!e.admissionStored) throw error
      if (!reported(error)) e.backgroundFailure ??= error
    }
    if (!e.acknowledged) {
      const ack = {
        deliveryId: delivery.id,
        journalOrdinal: delivery.journalOrdinal
      }
      await barrier(
        e,
        'ack',
        () => getLedger().acknowledge(e.ops.ack, e.id, ack),
        receipt => webRecordingCanonical(receipt.sourceAck) === webRecordingCanonical(ack)
      )
      e.acknowledged = true
      await retireIfSettled(e)
      owner.wakeMarkerDebt()
    }
  }
  return markers
}
