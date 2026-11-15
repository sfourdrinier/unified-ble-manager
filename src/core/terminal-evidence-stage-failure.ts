import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamTerminalNotice } from '../backend-contract/streams'

// Private source authority. A backend SDK cannot manufacture this marker by
// sending an object over its public stream surface. Trusted Electron IPC
// decoders rehydrate it only after validating their versioned main-process
// terminal record.
const marked = new WeakSet<NormalizedBleError>()

export function markTerminalEvidenceStageFailure(error: NormalizedBleError): NormalizedBleError {
  marked.add(error)
  return error
}

export function isTerminalEvidenceStageFailure(error: NormalizedBleError | null | undefined): boolean {
  return error !== null && error !== undefined && marked.has(error)
}

/** Trusted IPC encoders may expose a private discriminator only for this. */
export function isTrustedTerminalEvidenceStageFailure(notice: StreamTerminalNotice): boolean {
  return (
    notice.reason === 'source-failed' && notice.evidence === undefined && isTerminalEvidenceStageFailure(notice.error)
  )
}

/** Rehydrates only a validated, authenticated Electron-main wire terminal. */
export function rehydrateTrustedTerminalEvidenceStageFailure(notice: StreamTerminalNotice): StreamTerminalNotice {
  if (
    notice.reason !== 'source-failed' ||
    notice.evidence !== undefined ||
    notice.error === undefined ||
    notice.error === null ||
    notice.error.operation.length === 0
  ) {
    throw new TypeError('invalid trusted terminal-evidence staging failure')
  }
  marked.add(notice.error)
  return notice
}
