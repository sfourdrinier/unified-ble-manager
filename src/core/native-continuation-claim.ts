// A disclosed native prefix remains owned until the application explicitly ACKs it.
import { BackendContractError, contractError } from '../backend-contract/errors'
import { rehydratePublicPromise } from '../public/error-bridge'
import { assertTextWithinBound } from './recording-wire'
import {
  aggregateContinuationClaim,
  optionalContinuationClaimToken,
  parseContinuationClaimAcknowledgement,
  type ContinuationBacklog,
  type ContinuationRetainedPrefixV1,
  type ContinuationClaimAcknowledgement
} from '../backends/reactnative/react-native-continuation-claim'

export interface NativeContinuationClaimAccess {
  prepareClaim(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeClaim(token: string): Promise<unknown>
}

/** Distinct entrypoints establish retained-prefix semantics before native preparation. */
export interface NativeContinuationRetainedAccessV1 {
  prepareRetainedClaimV1?(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeRetainedClaimV1?(token: string): Promise<unknown>
  retireRetainedClaimReceiptV1?(token: string): Promise<unknown>
}

export interface NativeContinuationClaimOptions {
  readonly maxItems?: number
  readonly maxBytes?: number
}

interface ClaimState {
  pending?: ContinuationRetainedPrefixV1
  preparing?: Promise<ContinuationRetainedPrefixV1>
  acknowledging?: Promise<ContinuationClaimAcknowledgement>
  lastOrdinal: number | null
  controlLost: number
  lastToken?: string
  identity?: string
}
const states = new WeakMap<NativeContinuationRetainedAccessV1, ClaimState>()

function payload(value: unknown, scope: string, bounded = true): unknown {
  if (typeof value !== 'string') return value
  if (bounded) assertTextWithinBound(value, `${scope}.continuation.claim-json`, 'restoration')
  try {
    return JSON.parse(value)
  } catch {
    throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-json`)
  }
}

export function claimNativeContinuationPrefixV1(
  access: NativeContinuationRetainedAccessV1,
  request: NativeContinuationClaimOptions | undefined,
  scope: string
): Promise<ContinuationRetainedPrefixV1> {
  if (
    typeof access.prepareRetainedClaimV1 !== 'function' ||
    typeof access.acknowledgeRetainedClaimV1 !== 'function' ||
    typeof access.retireRetainedClaimReceiptV1 !== 'function'
  )
    return Promise.reject(contractError('capability.unsupported', 'restoration', `${scope}.continuation.retained-v1`))
  const prepare = access.prepareRetainedClaimV1
  const acknowledgeNative = access.acknowledgeRetainedClaimV1
  const retireNative = access.retireRetainedClaimReceiptV1
  const maxItems = request?.maxItems ?? 256
  const maxBytes = request?.maxBytes ?? 65536
  if (![maxItems, maxBytes].every(value => Number.isSafeInteger(value) && value >= 1 && value <= 0x7fffffff)) {
    return Promise.reject(contractError('argument.invalid', 'restoration', `${scope}.continuation.claim-bounds`))
  }
  let state = states.get(access)
  if (state === undefined) {
    state = { lastOrdinal: null, controlLost: 0 }
    states.set(access, state)
  }
  const owner = state
  if (owner.acknowledging !== undefined) {
    return owner.acknowledging.then(() => claimNativeContinuationPrefixV1(access, request, scope))
  }
  if (owner.pending !== undefined) return Promise.resolve(owner.pending)
  if (owner.preparing !== undefined) return owner.preparing
  const preparing = (async () => {
    const prepared = payload(await prepare.call(access, maxItems, maxBytes), scope)
    const backlog = aggregateContinuationClaim(prepared, owner.lastOrdinal, owner.controlLost)
    const token = optionalContinuationClaimToken(prepared)
    if (token === null) {
      if (
        owner.identity !== undefined ||
        backlog.selectors.length !== 0 ||
        backlog.records.length !== 0 ||
        backlog.recording !== undefined ||
        backlog.controlLost !== 0 ||
        backlog.afterCutoffLoss.items !== 0 ||
        backlog.afterCutoffLoss.bytes !== 0 ||
        backlog.disposed ||
        backlog.disposeFailure !== null
      )
        throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-token`)
      return backlog
    }
    const identity = JSON.stringify([backlog.selectors, backlog.recording ?? null])
    if (owner.identity !== undefined && identity !== owner.identity)
      throw contractError('protocol.violation', 'restoration', `${scope}.continuation.identity`)
    if (owner.lastToken === token && backlog.records.length !== 0)
      throw contractError('protocol.violation', 'restoration', `${scope}.continuation.replayed-token`)
    let receipt: ContinuationClaimAcknowledgement | undefined
    let retired = false
    let retiring: Promise<void> | undefined
    const acknowledge = (): Promise<ContinuationClaimAcknowledgement> => {
      if (receipt !== undefined) return Promise.resolve(receipt)
      if (owner.acknowledging !== undefined) return owner.acknowledging
      const attempt = rehydratePublicPromise(
        (async () => {
          const result = parseContinuationClaimAcknowledgement(
            payload(await acknowledgeNative.call(access, token), scope)
          )
          receipt = result
          owner.pending = undefined
          owner.lastToken = token
          owner.lastOrdinal = backlog.records.at(-1)?.ordinal ?? owner.lastOrdinal
          owner.controlLost = backlog.controlLost
          owner.identity = identity
          if (result.disposed) {
            owner.lastOrdinal = null
            owner.controlLost = 0
            owner.identity = undefined
            owner.lastToken = undefined
          }
          return result
        })()
      )
      owner.acknowledging = attempt
      attempt.then(
        () => {
          owner.acknowledging = undefined
        },
        () => {
          owner.acknowledging = undefined
        }
      )
      return attempt
    }
    const retireReceipt = (): Promise<void> => {
      if (receipt === undefined)
        return Promise.reject(
          contractError('lifecycle.invalid-state', 'restoration', `${scope}.continuation.retire-before-ack`)
        )
      if (retired) return Promise.resolve()
      if (retiring !== undefined) return retiring
      const attempt = rehydratePublicPromise(
        (async () => {
          const result = payload(await retireNative.call(access, token), scope)
          if (
            typeof result !== 'object' ||
            result === null ||
            Array.isArray(result) ||
            Object.keys(result).length !== 0
          )
            throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.retire-receipt`)
          retired = true
        })()
      )
      retiring = attempt
      attempt.then(
        () => {
          retiring = undefined
        },
        () => {
          retiring = undefined
        }
      )
      return attempt
    }
    const handoff = Object.freeze({ ...backlog, claimToken: token, acknowledge, retireReceipt })
    owner.pending = handoff
    return handoff
  })()
  owner.preparing = preparing
  preparing.then(
    () => {
      owner.preparing = undefined
    },
    () => {
      owner.preparing = undefined
    }
  )
  return preparing
}

/** 5.2.1 handoff: decode the complete native claim, then automatically ACK. */
export async function claimNativeContinuationBacklog(
  access: NativeContinuationClaimAccess,
  request: NativeContinuationClaimOptions | undefined,
  scope: string
): Promise<ContinuationBacklog> {
  const maxItems = request?.maxItems ?? 256
  const maxBytes = request?.maxBytes ?? 65536
  if (![maxItems, maxBytes].every(value => Number.isSafeInteger(value) && value >= 1 && value <= 0xffffffff))
    throw contractError('argument.invalid', 'restoration', `${scope}.continuation.claim-bounds`)
  const raw = await access.prepareClaim(maxItems, maxBytes)
  let prepared: unknown = raw
  if (typeof raw === 'string') {
    try {
      prepared = JSON.parse(raw)
    } catch {
      throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-json`)
    }
  }
  const decoded = aggregateContinuationClaim(prepared)
  // The legacy API keeps its 5.2.1 result shape; ordered records belong to V1.
  const backlog: ContinuationBacklog = Object.freeze({
    selectors: decoded.selectors,
    ...(decoded.recording === undefined ? {} : { recording: decoded.recording }),
    values: decoded.values,
    streamEnds: decoded.streamEnds,
    control: decoded.control,
    controlLost: decoded.controlLost,
    afterCutoffLoss: decoded.afterCutoffLoss,
    disposed: decoded.disposed,
    disposeFailure: decoded.disposeFailure
  })
  const token = optionalContinuationClaimToken(prepared)
  if (
    token === null &&
    backlog.selectors.length === 0 &&
    backlog.values.length === 0 &&
    backlog.streamEnds.length === 0 &&
    backlog.control.length === 0
  )
    return backlog
  if (token === null) throw contractError('protocol.malformed', 'restoration', `${scope}.continuation.claim-token`)
  try {
    const result = await access.acknowledgeClaim(token)
    return Object.freeze({ ...backlog, ...parseContinuationClaimAcknowledgement(payload(result, scope, false)) })
  } catch (error) {
    const detail =
      error instanceof BackendContractError
        ? error.normalized.code
        : error instanceof Error && error.message.length > 0
          ? error.message.slice(0, 256)
          : 'native acknowledgement did not return a valid receipt'
    return Object.freeze({
      ...backlog,
      disposed: false,
      disposeFailure: `continuation acknowledgement uncertain: ${detail}`
    })
  }
}
