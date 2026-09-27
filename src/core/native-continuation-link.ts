import { contractError } from '../backend-contract/errors'
import { parseRemoteFailureText, type WireRemoteFailure } from '../backends/reactnative/rust-core-wire'

export interface ContinuationLinkOutcome {
  readonly mtu:
    | { readonly requested: number; readonly outcome: 'negotiated'; readonly mtu: number }
    | { readonly requested: number; readonly outcome: 'unsupported'; readonly error: WireRemoteFailure }
}

function malformed(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.link')
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exact(value: Record<string, unknown>, keys: readonly string[]) {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) malformed()
}

function mtu(value: unknown): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 23 || value > 517) return malformed()
  return value
}

/** Shared strict projection for desktop completion and mobile recovery status. */
export function parseContinuationLinkOutcome(value: unknown): ContinuationLinkOutcome | undefined {
  if (value === undefined) return undefined
  if (!record(value)) return malformed()
  exact(value, ['mtu'])
  const result = value.mtu
  if (!record(result)) return malformed()
  const requested = mtu(result.requested)
  if (result.outcome === 'negotiated') {
    exact(result, ['requested', 'outcome', 'mtu'])
    return Object.freeze({ mtu: Object.freeze({ requested, outcome: result.outcome, mtu: mtu(result.mtu) }) })
  }
  if (result.outcome === 'unsupported') {
    exact(result, ['requested', 'outcome', 'error'])
    const error = parseRemoteFailureText(JSON.stringify(result.error), 'continuation.link.mtu.error')
    if (!error.ok || error.value.code !== 'capability.unsupported') return malformed()
    return Object.freeze({ mtu: Object.freeze({ requested, outcome: result.outcome, error: error.value }) })
  }
  return malformed()
}
