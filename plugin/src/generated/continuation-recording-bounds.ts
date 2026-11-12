// Generated from src/backend-contract/continuation-recording-bounds.ts; do not edit.
// src/backend-contract/continuation-recording-bounds.ts

/** Opt-in plaintext journal in the host's private storage. No implicit retention
 * quota or caller-selected path; acknowledgement and deletion are explicit. */
export interface ContinuationRecordingConfiguration {
  readonly id: string
  readonly maxBytes: number
  readonly maxRecords: number
}

export const MIN_RECORDING_BYTES = 1_048_576
export const MAX_RECORDING_BYTES = 1_073_741_824
export const MIN_RECORDING_RECORDS = 1
export const MAX_RECORDING_RECORDS = 1_000_000

/** Recording identities are stable nonsecret names, never paths. */
export function isRecordingId(value: unknown): value is string {
  return typeof value === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(value)
}

/** Durable cursor limits shared by request validation and native response admission. */
export const MAX_RECORDING_BATCH_ITEMS = 2048
export const MAX_RECORDING_BATCH_BYTES = 4_194_304
export const MAX_RECORDING_TOKEN_BYTES = 96

/** The byte budget counts serialized records, not array separators or envelope
 * fields. Reserve one separator per record plus bounded token/header overhead. */
export const MAX_RECORDING_PREPARE_TEXT_BYTES = MAX_RECORDING_BATCH_BYTES + MAX_RECORDING_BATCH_ITEMS + 1024

export type RecordingControlOperation =
  | 'status'
  | 'prepare'
  | 'acknowledge'
  | 'stop'
  | 'stop-ordered'
  | 'context-ordered'
  | 'clear'
