// src/core/terminal-evidence.ts

import { contractError } from '../backend-contract/errors'
import type {
  TerminalEvidenceAppendResult,
  TerminalEvidenceBytes,
  TerminalEvidenceCommit,
  TerminalEvidenceProducer,
  TerminalEvidenceRecord,
  TerminalEvidenceRefusal,
  TerminalEvidenceReservation,
  TerminalEvidenceReservationRequest
} from '../backend-contract/terminal-evidence'

/**
 * Host-owned durable bytes for one terminal observation. The core never
 * synthesizes, truncates, or substitutes these bytes: a committed record is
 * exactly the record supplied to `append`.
 */
export type {
  TerminalEvidenceAppendResult,
  TerminalEvidenceBytes,
  TerminalEvidenceCommit,
  TerminalEvidenceProducer,
  TerminalEvidenceRecord,
  TerminalEvidenceRefusal,
  TerminalEvidenceReservation,
  TerminalEvidenceReservationRequest
}

/** The two shared-core producer families that can retain a terminal. */
/**
 * Host injection point. It is intentionally path- and runtime-neutral: the
 * selected backend owns encryption, storage location, and retention policy.
 */
export interface TerminalEvidenceSink {
  reserve(request: TerminalEvidenceReservationRequest): Promise<TerminalEvidenceReservation>
}

/** An oversized record was never offered to the host sink. Its original bytes remain caller-owned. */
export class TerminalEvidenceRecordTooLargeError extends Error {
  constructor(
    readonly maximumBytes: number,
    readonly actualBytes: number
  ) {
    super(`terminal-evidence.record-too-large: ${actualBytes} bytes exceeds the ${maximumBytes}-byte reservation`)
    this.name = 'TerminalEvidenceRecordTooLargeError'
  }
}

/**
 * Owns the pre-activation reservation for a single producer. This narrow
 * state machine deliberately does not create a fallback memory store: a host
 * either commits the exact bytes or returns an explicit refusal.
 */
export class TerminalEvidenceAdmission {
  private reservation: TerminalEvidenceReservation | null = null
  private released = false
  private reserveInFlight: Promise<void> | null = null
  private releaseInFlight: Promise<void> | null = null

  constructor(
    private readonly sink: TerminalEvidenceSink,
    private readonly request: TerminalEvidenceReservationRequest
  ) {
    assertRequest(request)
  }

  async reserve(): Promise<void> {
    if (this.released) throw contractError('lifecycle.invalid-state', 'stream', 'terminal-evidence.released')
    if (this.reservation !== null) return
    if (this.reserveInFlight === null) {
      this.reserveInFlight = (async () => {
        const reservation = await this.sink.reserve(this.request)
        this.reservation = reservation
        try {
          assertMaximum(reservation.maximumBytes, 'terminal-evidence.reservation-maximum')
          if (reservation.maximumBytes < this.request.maximumBytes)
            throw contractError('capability.unsupported', 'stream', 'terminal-evidence.reservation-too-small')
        } catch (error) {
          await this.release()
          throw error
        }
      })()
      this.reserveInFlight
        .catch(() => undefined)
        .finally(() => {
          this.reserveInFlight = null
        })
    }
    await this.reserveInFlight
  }

  async commit(record: TerminalEvidenceRecord): Promise<TerminalEvidenceAppendResult> {
    const reservation = this.reservation
    if (reservation === null || this.released)
      throw contractError('lifecycle.invalid-state', 'stream', 'terminal-evidence.reservation-required')
    const actualBytes = record.canonicalBytes.byteLength
    if (actualBytes > reservation.maximumBytes)
      throw new TerminalEvidenceRecordTooLargeError(reservation.maximumBytes, actualBytes)
    return reservation.commit(record)
  }

  /** Transfer the one acquired reservation to the producer lifecycle. */
  takeReservation(): TerminalEvidenceReservation {
    const reservation = this.reservation
    if (reservation === null || this.released)
      throw contractError('lifecycle.invalid-state', 'stream', 'terminal-evidence.reservation-required')
    this.reservation = null
    this.released = true
    return reservation
  }

  async release(): Promise<void> {
    let reservation = this.reservation
    if (reservation === null && this.reserveInFlight !== null) {
      await this.reserveInFlight.catch(() => undefined)
      reservation = this.reservation
    }
    if (reservation === null) {
      this.released = true
      return
    }
    this.released = true
    if (this.releaseInFlight === null) {
      this.releaseInFlight = reservation
        .release()
        .then(() => {
          this.reservation = null
          this.released = true
        })
        .finally(() => {
          this.releaseInFlight = null
        })
    }
    await this.releaseInFlight
  }
}

function assertRequest(request: TerminalEvidenceReservationRequest): void {
  if (request.producer !== 'scan' && request.producer !== 'subscription')
    throw contractError('argument.invalid', 'stream', 'terminal-evidence.producer')
  assertMaximum(request.maximumBytes, 'terminal-evidence.maximum-bytes')
  if (request.formatVersion !== 1) throw contractError('argument.invalid', 'stream', 'terminal-evidence.format-version')
  if (
    request.scope.managerId.length === 0 ||
    request.scope.operationId.length === 0 ||
    request.scope.incarnation.length === 0
  )
    throw contractError('argument.invalid', 'stream', 'terminal-evidence.scope')
}

function assertMaximum(value: number, code: string): void {
  if (!Number.isSafeInteger(value) || value < 1) throw contractError('argument.invalid', 'stream', code)
}
