// src/web/web-ordered-marker.ts

import { contractError } from '../backend-contract/errors'
import type { OrderedNotificationMarker, OrderedSourceGeneration } from '../core/ordered-notification-contract'
import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'
import type { WebIngressGroup } from './web-notification-ingress'

interface MarkerCredit {
  reserveMarker(collectionId: string, markerId: string): void
  admitReservedMarker(marker: OrderedNotificationMarker): boolean
  releaseMarkerNotAdmitted(collectionId: string, markerId: string): void
  markerAdmissionStatus(
    collectionId: string,
    markerId: string
  ):
    | { readonly state: 'reserved' | 'rejected' | 'not-admitted' | 'unknown' }
    | { readonly state: 'admitted'; readonly ordinal: string }
}

type MarkerAdmission =
  | { readonly state: 'not-admitted'; readonly error: unknown }
  | { readonly state: 'admitted'; readonly ordinal: string }
  | { readonly state: 'rejected'; readonly error: unknown }
type MarkerWrite =
  | { readonly state: 'not-started'; readonly error?: unknown }
  | { readonly state: 'started' }
  | {
      readonly state: 'settled'
      readonly result: { readonly ok: true } | { readonly ok: false; readonly error: unknown }
    }
export interface WebMarkerReceipt {
  readonly marker: MarkerAdmission
  readonly write: MarkerWrite
}
interface Entry {
  readonly id: bigint
  readonly target: WebBluetoothCharacteristicBoundary
  readonly mode: 'with-response' | 'without-response'
  readonly bytes: Uint8Array
  marker: MarkerAdmission
  write: MarkerWrite
  evidenceAcknowledged: boolean
  creditUnresolved: boolean
  creditFailure: unknown
}
const MAX_ID = 18446744073709551615n

/** One retained attempt per group; exact retries never invoke the browser twice. */
export class WebOrderedMarkerTransactions {
  private entry: Entry | null = null
  private lastId: bigint | null = null
  private retiredId: bigint | null = null

  constructor(
    private readonly ingress: Pick<WebIngressGroup, 'terminal' | 'admitMarker'>,
    private readonly credit: MarkerCredit,
    private readonly collectionId: string,
    private readonly connectionGeneration: string,
    private readonly databaseGeneration: string,
    private readonly sourceGeneration: OrderedSourceGeneration | null = null,
    private readonly rejectionEvidence: 'required' | 'no-rejection-delivery' = 'required'
  ) {}

  write(
    id: string,
    target: WebBluetoothCharacteristicBoundary,
    value: Readonly<Uint8Array>,
    mode: Entry['mode'],
    signal: AbortSignal | null,
    deadlineExpired = false
  ): WebMarkerReceipt {
    if (!/^(0|[1-9][0-9]*)$/.test(id)) throw contractError('argument.invalid', 'gatt', 'web-gatt.marker-id')
    const number = BigInt(id)
    if (number > MAX_ID) throw contractError('argument.invalid', 'gatt', 'web-gatt.marker-id')
    const existing = this.entry
    if (existing !== null) {
      if (
        existing.id !== number ||
        existing.target !== target ||
        existing.mode !== mode ||
        existing.bytes.length !== value.length ||
        existing.bytes.some((byte, index) => byte !== value[index])
      )
        throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-retry')
      this.reconcile(existing)
      return this.snapshot(existing)
    }
    const expected = this.lastId === null ? 0n : this.lastId + 1n
    if (number !== expected || number > MAX_ID)
      throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-sequence')
    const entry: Entry = {
      id: number,
      target,
      mode,
      bytes: new Uint8Array(value),
      marker: { state: 'not-admitted', error: null },
      write: { state: 'not-started' },
      evidenceAcknowledged: false,
      creditUnresolved: false,
      creditFailure: null
    }
    this.entry = entry
    this.lastId = number
    if (signal?.aborted || deadlineExpired || this.ingress.terminal) {
      entry.marker = {
        state: 'not-admitted',
        error: contractError(
          signal?.aborted ? 'operation.aborted' : deadlineExpired ? 'operation.timed-out' : 'stream.closed',
          'gatt',
          'web-gatt.marker'
        )
      }
      return this.snapshot(entry)
    }
    try {
      this.credit.reserveMarker(this.collectionId, id)
    } catch (error) {
      entry.marker = { state: 'not-admitted', error }
      return this.snapshot(entry)
    }
    let attempted = false
    let ordinal: string | null
    try {
      ordinal = this.ingress.admitMarker(position => {
        attempted = true
        return this.credit.admitReservedMarker({
          kind: 'marker',
          collectionId: this.collectionId,
          connectionGeneration: this.connectionGeneration,
          databaseGeneration: this.databaseGeneration,
          sourceGeneration: this.sourceGeneration,
          sourceMembership: null,
          markerId: id,
          ordinal: position
        })
      })
    } catch (error) {
      entry.creditUnresolved = true
      entry.creditFailure = error
      this.reconcile(entry)
      return this.snapshot(entry)
    }
    if (ordinal === null) {
      if (!attempted) {
        try {
          this.credit.releaseMarkerNotAdmitted(this.collectionId, id)
        } catch (error) {
          entry.creditUnresolved = true
          entry.creditFailure = error
          this.reconcile(entry)
          return this.snapshot(entry)
        }
      }
      entry.marker = {
        state: attempted ? 'rejected' : 'not-admitted',
        error: contractError('stream.closed', 'stream', 'web-gatt.marker')
      }
      return this.snapshot(entry)
    }
    entry.marker = { state: 'admitted', ordinal }
    if (signal?.aborted || this.ingress.terminal) {
      entry.write = {
        state: 'not-started',
        error: contractError(signal?.aborted ? 'operation.aborted' : 'stream.closed', 'gatt', 'web-gatt.marker-write')
      }
      return this.snapshot(entry)
    }
    entry.write = { state: 'started' }
    try {
      const promise =
        mode === 'with-response'
          ? target.writeValueWithResponse(new Uint8Array(entry.bytes))
          : target.writeValueWithoutResponse(new Uint8Array(entry.bytes))
      promise.then(
        () => {
          entry.write = { state: 'settled', result: { ok: true } }
        },
        error => {
          entry.write = { state: 'settled', result: { ok: false, error } }
        }
      )
    } catch (error) {
      entry.write = { state: 'settled', result: { ok: false, error } }
    }
    return this.snapshot(entry)
  }

  hasReceipt(id: string): boolean {
    return this.entry?.id.toString() === id
  }

  acknowledge(id: string): void {
    if (this.entry?.id.toString() !== id) throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-ack')
    this.reconcile(this.entry)
    this.entry.evidenceAcknowledged = true
  }

  forget(id: string): boolean {
    if (!/^(0|[1-9][0-9]*)$/.test(id)) return false
    const number = BigInt(id)
    const entry = this.entry
    if (entry === null || entry.id !== number) return this.retiredId !== null && number <= this.retiredId
    try {
      this.reconcile(entry)
    } catch {
      return false
    }
    if (
      entry.creditUnresolved ||
      entry.write.state === 'started' ||
      (entry.marker.state !== 'not-admitted' &&
        !(entry.marker.state === 'rejected' && this.rejectionEvidence === 'no-rejection-delivery') &&
        !entry.evidenceAcknowledged)
    )
      return false
    this.entry = null
    this.retiredId = number
    return true
  }

  private reconcile(entry: Entry): void {
    if (!entry.creditUnresolved) return
    const id = entry.id.toString()
    let status = this.credit.markerAdmissionStatus(this.collectionId, id)
    if (status.state === 'reserved') {
      this.credit.releaseMarkerNotAdmitted(this.collectionId, id)
      status = this.credit.markerAdmissionStatus(this.collectionId, id)
    }
    if (status.state === 'unknown' || status.state === 'reserved') throw entry.creditFailure
    if (status.state === 'admitted') entry.marker = { state: 'admitted', ordinal: status.ordinal }
    else if (status.state === 'rejected') entry.marker = { state: 'rejected', error: entry.creditFailure }
    else entry.marker = { state: 'not-admitted', error: entry.creditFailure }
    entry.write = { state: 'not-started', error: entry.creditFailure }
    entry.creditUnresolved = false
  }

  private snapshot(entry: Entry): WebMarkerReceipt {
    return { marker: entry.marker, write: entry.write }
  }
}
