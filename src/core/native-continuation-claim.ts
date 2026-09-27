// Shared prepare/decode/acknowledge ownership handoff. Both host bridges use
// the same bounded drain codec; no acknowledgement precedes full decoding.
import { BackendContractError, contractError } from '../backend-contract/errors'
import {
  aggregateContinuationClaim,
  optionalContinuationClaimToken,
  parseContinuationClaimAcknowledgement,
  type ContinuationBacklog
} from '../backends/reactnative/react-native-continuation-claim'

export interface NativeContinuationClaimAccess {
  prepareClaim(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeClaim(token: string): Promise<unknown>
}

export interface NativeContinuationClaimOptions {
  readonly maxItems?: number
  readonly maxBytes?: number
}

function payload(value: unknown, scope: string): unknown {
  if (typeof value !== 'string') return value
  try {
    return JSON.parse(value)
  } catch {
    throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-json`)
  }
}

function acknowledgementFailureDetail(error: unknown): string {
  if (error instanceof BackendContractError) return error.normalized.code
  if (error instanceof Error && error.message.length > 0) return error.message.slice(0, 256)
  return 'native acknowledgement did not return a valid receipt'
}

export async function claimNativeContinuationBacklog(
  access: NativeContinuationClaimAccess,
  request: NativeContinuationClaimOptions | undefined,
  scope: string
): Promise<ContinuationBacklog> {
  const maxItems = request?.maxItems ?? 256
  const maxBytes = request?.maxBytes ?? 65536
  if (![maxItems, maxBytes].every(value => Number.isSafeInteger(value) && value >= 1 && value <= 0xffffffff)) {
    throw contractError('argument.invalid', 'restoration', `${scope}.continuation.claim-bounds`)
  }
  const prepared = payload(await access.prepareClaim(maxItems, maxBytes), scope)
  const backlog = aggregateContinuationClaim(prepared)
  const claimToken = optionalContinuationClaimToken(prepared)
  if (
    claimToken === null &&
    backlog.selectors.length === 0 &&
    backlog.values.length === 0 &&
    backlog.streamEnds.length === 0 &&
    backlog.control.length === 0
  )
    return backlog
  if (claimToken === null) throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-token`)
  try {
    const acknowledgement = parseContinuationClaimAcknowledgement(
      payload(await access.acknowledgeClaim(claimToken), scope)
    )
    return Object.freeze({ ...backlog, ...acknowledgement })
  } catch (error) {
    // Successfully decoded data stays observable even when the acknowledgement
    // transport or receipt fails. Native ownership remains explicitly uncertain.
    return Object.freeze({
      ...backlog,
      disposed: false,
      disposeFailure: `continuation acknowledgement uncertain: ${acknowledgementFailureDetail(error)}`
    })
  }
}
