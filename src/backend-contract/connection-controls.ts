// src/backend-contract/connection-controls.ts

import type { OperationOptions, OperationTerminalRecord, WriteMode } from './operations'
import type { ConnectionId, GenerationId } from './primitives'
import type { CleanupRecord } from './errors'
import type { BoundedAsyncStream } from './streams'

/** BLE's mandatory lower ATT MTU bound, including opcode and attribute handle bytes. */
export const MINIMUM_ATT_MTU = 23
/** Android's documented maximum request value; a peer may negotiate a lower value. */
export const MAXIMUM_REQUESTED_ATT_MTU = 517

/** Android's supported caller-directed connection link-priority requests. */
export type ConnectionPriority = 'low-power' | 'balanced' | 'high-throughput'

/** Whether a platform boundary can truthfully dispatch a connection-control operation. */
export type ConnectionControlSupport = 'available' | 'unavailable'

/** Per-platform dispatch capabilities for the canonical connection-control surface. */
export interface ConnectionControlCapabilities {
  readonly rssi: ConnectionControlSupport
  readonly requestMtu: ConnectionControlSupport
  readonly priority?: ConnectionControlSupport
}

export interface RssiMeasurement<Attachment extends string, _Operation extends string> {
  readonly rssi: number
  readonly observedAtMonotonicMs: number
  readonly terminal: OperationTerminalRecord<Attachment, string>
}

export interface MtuNegotiation<Attachment extends string, _Operation extends string> {
  readonly requestedMtu: number
  readonly negotiatedMtu: number
  readonly observedAtMonotonicMs: number
  readonly terminal: OperationTerminalRecord<Attachment, string>
}

/** Backend result for a priority request; it does not assert observed link parameters. */
export interface ConnectionPriorityRequest<Attachment extends string, _Operation extends string> {
  readonly requested: ConnectionPriority
  readonly accepted: boolean
  readonly observedAtMonotonicMs: number
  readonly terminal: OperationTerminalRecord<Attachment, string>
}

export interface ConnectionWriteReadinessObservation<Attachment extends string> {
  readonly connectionId: ConnectionId<Attachment, string>
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  readonly ready: boolean
  readonly observedAtMonotonicMs: number
  readonly ordinal: number
}

export interface ConnectionWriteReadinessWatch<Attachment extends string> {
  readonly events: BoundedAsyncStream<ConnectionWriteReadinessObservation<Attachment>>
  close(): Promise<CleanupRecord>
}

/** Backend result for the connection-level write-length boundary. */
export interface ConnectionMaximumWriteLengthMeasurement<Attachment extends string, _Operation extends string> {
  readonly connectionId: ConnectionId<Attachment, string>
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  readonly mode: WriteMode
  readonly maximumWriteLength: number
  readonly observedAtMonotonicMs: number
  readonly terminal: OperationTerminalRecord<Attachment, string>
}

export interface ReadRssiRequest<Attachment extends string, Operation extends string> {
  readonly operation: OperationOptions<Attachment, Operation>
}

export interface RequestMtuRequest<Attachment extends string, Operation extends string> {
  readonly operation: OperationOptions<Attachment, Operation>
  readonly requestedMtu: number
}

export interface RequestPriorityRequest<Attachment extends string, Operation extends string> {
  readonly operation: OperationOptions<Attachment, Operation>
  readonly priority: ConnectionPriority
}

export interface ConnectionMaximumWriteLengthRequest<Attachment extends string, Operation extends string> {
  readonly operation: OperationOptions<Attachment, Operation>
  readonly mode: WriteMode
}
