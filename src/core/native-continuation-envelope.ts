import { BackendContractError, contractError } from '../backend-contract/errors'
import {
  failureEnvelopeError,
  parseNativeControlEnvelope,
  parseRecordingControlEnvelope,
  type WireInvokeEnvelope,
  type WireResult
} from '../backends/reactnative/rust-core-wire'
import type { RecordingControlOperation } from '../backend-contract/continuation-recording-bounds'
import { BleError } from '../public/errors'
import { toPublicNormalizedError } from '../public/cleanup'

/** Trusted host-bridge encoding of a decoded native control failure only.
 * Unknown exceptions are rethrown, never accepted by code-shaped duck typing.
 * This is not a general BleError serializer: native control envelopes do not
 * support write commit states, limitations, or nested/binary platform metadata.
 * Their existing strict decoder validates the encoded shape and size.
 */
export function encodeNativeContinuationFailure(error: unknown): string {
  if (!(error instanceof BleError) && !(error instanceof BackendContractError)) throw error
  if (error instanceof BleError && error.limitations.length !== 0) {
    throw contractError('protocol.malformed', 'restoration', 'continuation.failure-encode.limitations')
  }
  const normalized = toPublicNormalizedError(error instanceof BleError ? error : error.normalized)
  const platform = normalized.platform
  const encoded = JSON.stringify({
    ok: false,
    error: {
      code: normalized.code,
      domain: normalized.domain,
      operation: normalized.operation,
      detail: platform?.safeMessage ?? null,
      platform:
        platform === null
          ? null
          : {
              domain: platform.domain,
              code: platform.code,
              message: platform.safeMessage,
              metadata: platform.metadata
            }
    },
    commit: normalized.commit ?? null,
    retryability: normalized.retryability
  })
  const checked = parseNativeControlEnvelope(encoded, 'continuation.failure-encode')
  if (!checked.ok) throw checked.error
  return encoded
}

export function nativeContinuationTransportError(
  error: unknown,
  hostDomain: string,
  operation: string
): BackendContractError {
  if (error instanceof BackendContractError) return error
  return contractError('platform.failure', 'restoration', operation, {
    domain: hostDomain,
    code: 'native-bridge',
    safeMessage: error instanceof Error ? error.message.slice(0, 1024) : String(error).slice(0, 1024),
    metadata: {}
  })
}

export function assertContinuationRecordingConfigured(value: unknown): void {
  if (
    typeof value !== 'object' ||
    value === null ||
    Array.isArray(value) ||
    Object.keys(value).length !== 2 ||
    Reflect.get(value, 'state') !== 'configured' ||
    Reflect.get(value, 'encrypted') !== false
  ) {
    throw contractError('protocol.malformed', 'restoration', 'continuation.recording.configure')
  }
}

/** Shared non-writing native control envelope. Preserve the native operation's
 * own failure; only supply the host domain when native provided no platform. */
export function decodeNativeContinuationEnvelope(
  text: unknown,
  hostDomain: string,
  operation = 'continuation.control'
): unknown {
  return decodeEnvelope(parseNativeControlEnvelope(text, operation), hostDomain)
}

export function decodeNativeRecordingEnvelope(
  text: unknown,
  hostDomain: string,
  operation: RecordingControlOperation
): unknown {
  return decodeEnvelope(parseRecordingControlEnvelope(text, operation), hostDomain)
}

function decodeEnvelope(parsed: WireResult<WireInvokeEnvelope>, hostDomain: string): unknown {
  if (!parsed.ok) throw parsed.error
  if (parsed.value.kind === 'value') return parsed.value.value
  const error = failureEnvelopeError(parsed.value)
  if (parsed.value.failure.platform === null && error.normalized.platform !== null) {
    throw new BackendContractError({
      ...error.normalized,
      platform: { ...error.normalized.platform, domain: hostDomain }
    })
  }
  throw error
}
