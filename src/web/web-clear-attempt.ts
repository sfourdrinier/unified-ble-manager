import { webRecordingContractError as contractError } from './web-recording-errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import type {
  ContinuationRecordingController,
  ContinuationRecordingPrepareOptions,
  OrderedRecordingHostStopResult
} from '../core/continuation-recording'
import type { OrderedRecordingBatch, OrderedRecordingContext } from '../core/ordered-recording'
import type { WebProofAdmission, WebProofOutcome, WebProofUsage } from './web-clear-proof-ledger'
import type { WebRecordingSupervisorState } from './web-ordered-recording-supervisor'

/** Realm-local observation; it is not a durable recording receipt. */
export interface WebRecordingRuntimeDiagnostic {
  readonly state: WebRecordingSupervisorState
  readonly lastFailure: {
    readonly persisted: false
    readonly error: {
      readonly code: string
      readonly domain: string
      readonly operation: string
      readonly platformCode: string | null
    }
  } | null
}

/** Save this identity before requesting deletion so its outcome can be inspected later. */
export interface WebClearAttemptV1 {
  readonly version: 1
  readonly recordingId: string
  readonly producerIncarnation: string
  readonly attemptId: string
}

export interface WebClearAttemptReceiptV1 extends WebClearAttemptV1 {
  readonly cleared: true
  readonly records: 0
}

export interface WebContinuationRecordingController extends ContinuationRecordingController {
  prepareOrdered(id: string, options: ContinuationRecordingPrepareOptions): Promise<OrderedRecordingBatch>
  stopOrdered(id: string): Promise<OrderedRecordingHostStopResult>
  contextOrdered(id: string): Promise<OrderedRecordingContext>
  runtimeDiagnostic(id: string): Promise<WebRecordingRuntimeDiagnostic>
  admitClearAttemptV2(identity: {
    readonly recordingId: string
    readonly producerIncarnation: string
    readonly requestId: string
  }): Promise<WebProofAdmission>
  commitClearAttemptV2(
    attempt: WebProofAdmission,
    options?: { readonly signal?: AbortSignal }
  ): Promise<{ readonly cleared: true; readonly records: number }>
  inspectClearAttemptV2(attempt: WebProofAdmission): Promise<WebProofOutcome>
  cancelClearAttemptV2(attempt: WebProofAdmission): Promise<WebProofOutcome>
  retireClearAttemptV2(options: {
    readonly outcome:
      | { readonly state: 'cancelled'; readonly attempt: WebProofAdmission }
      | {
          readonly state: 'committed'
          readonly attempt: WebProofAdmission
          readonly result: { readonly cleared: true; readonly records: 0 }
        }
    readonly acknowledgeDurableCopy: true
  }): Promise<{ readonly state: 'retired' | 'retired-or-unknown' }>
  clearProofUsageV2(): Promise<WebProofUsage>
  configureClearProofV2(options: {
    readonly limit: number | null
    readonly fenceLimit?: number | null
    readonly expectedRevision: string
  }): Promise<WebProofUsage>
  prepareClearAttemptV1(id: string): Promise<WebClearAttemptV1>
  commitClearAttemptV1(
    attempt: WebClearAttemptV1,
    options?: { readonly signal?: AbortSignal }
  ): Promise<{ readonly cleared: true; readonly records: number }>
  /** Alias for commitClearAttemptV1. */
  clearAttemptV1(
    attempt: WebClearAttemptV1,
    options?: { readonly signal?: AbortSignal }
  ): Promise<{ readonly cleared: true; readonly records: number }>
}

export const webClearAttemptKey = (attemptId: string): string => `:ubm-clear-attempt-v1:${attemptId}`

export function checkedWebClearAttempt(value: WebClearAttemptV1): WebClearAttemptV1 {
  if (
    typeof value !== 'object' ||
    value === null ||
    Array.isArray(value) ||
    Object.keys(value).length !== 4 ||
    value.version !== 1 ||
    !isRecordingId(value.recordingId) ||
    typeof value.producerIncarnation !== 'string' ||
    value.producerIncarnation.length === 0 ||
    value.producerIncarnation.length > 256 ||
    typeof value.attemptId !== 'string' ||
    !/^[0-9a-f]{64}$/.test(value.attemptId)
  )
    throw contractError('argument.invalid', 'restoration', 'web-recording.clear-attempt')
  return Object.freeze({
    version: 1,
    recordingId: value.recordingId,
    producerIncarnation: value.producerIncarnation,
    attemptId: value.attemptId
  })
}
