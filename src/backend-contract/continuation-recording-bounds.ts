/** Durable cursor limits shared by request validation and native response admission. */
export const MAX_RECORDING_BATCH_ITEMS = 2048
export const MAX_RECORDING_BATCH_BYTES = 4_194_304
export const MAX_RECORDING_TOKEN_BYTES = 96

/** The byte budget counts serialized records, not array separators or envelope
 * fields. Reserve one separator per record plus bounded token/header overhead. */
export const MAX_RECORDING_PREPARE_TEXT_BYTES = MAX_RECORDING_BATCH_BYTES + MAX_RECORDING_BATCH_ITEMS + 1024

export type RecordingControlOperation = 'status' | 'prepare' | 'acknowledge' | 'stop' | 'clear'
