import type { SerializableRecord } from './primitives'

/** Versioned, source-native durable terminal evidence contract. */
export type TerminalEvidenceProducer = 'scan' | 'subscription'
export type TerminalEvidenceBytes = Readonly<Uint8Array>
export interface TerminalEvidenceReservationRequest {
  readonly producer: TerminalEvidenceProducer
  readonly maximumBytes: number
  readonly formatVersion: 1
  readonly scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
}
export interface TerminalEvidenceRecord {
  readonly canonicalBytes: TerminalEvidenceBytes
  readonly digest: string
}
export interface TerminalEvidenceCommit extends SerializableRecord {
  readonly producer: TerminalEvidenceProducer
  readonly ref: string
  readonly byteLength: number
  readonly digest: string
  readonly formatVersion: 1
  readonly durability: 'committed'
  /** Exact reservation scope; core rejects a receipt for another producer incarnation. */
  readonly scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
}
export interface TerminalEvidenceRefusal {
  readonly kind: 'refused'
  readonly reason: 'quota-exhausted' | 'storage-unavailable' | 'capability-unsupported'
}
export type TerminalEvidenceAppendResult = TerminalEvidenceCommit | TerminalEvidenceRefusal
export interface TerminalEvidenceReservation {
  readonly maximumBytes: number
  commit(record: TerminalEvidenceRecord): Promise<TerminalEvidenceAppendResult>
  /**
   * Ends only the transient runtime lease. It may reclaim an unused
   * reservation, but it never retires staged or committed evidence.
   */
  release(): Promise<void>
}
/** Implemented only by a source-native backend that reserves before activation. */
export interface TerminalEvidenceSource {
  reserveTerminalEvidence(request: TerminalEvidenceReservationRequest): Promise<TerminalEvidenceReservation>
  /** Bounded exact recovery; `missing` and `unresolved` are never presented as committed evidence. */
  readTerminalEvidence?(
    ref: string,
    maximumBytes: number
  ): Promise<
    | { readonly kind: 'committed'; readonly canonicalBytes: TerminalEvidenceBytes; readonly digest: string }
    | { readonly kind: 'missing' | 'unresolved' }
  >
  /**
   * Explicit authenticated recovery acknowledgement: the sole durable
   * retirement path. Hosts must retain a failed acknowledgement for retry.
   */
  releaseTerminalEvidence?(ref: string): Promise<void>
}
