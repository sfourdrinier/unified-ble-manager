import {
  normalizeHostBackgroundContinuation,
  normalizeBackgroundContinuation,
  serializeBackgroundContinuation,
  type BackgroundContinuationDeclaration
} from '../backend-contract/background-continuation'
import { BackendContractError, contractError, type NormalizedBleError } from '../backend-contract/errors'
import { rehydratePublicPromise } from '../public/error-bridge'
import { claimNativeContinuationBacklog, type NativeContinuationClaimOptions } from './native-continuation-claim'
import {
  parseContinuationRecoveryStatus,
  type ContinuationBacklog,
  type ContinuationRecoveryStatus
} from '../backends/reactnative/react-native-continuation-claim'
import {
  failureEnvelopeError,
  parseInvokeEnvelope,
  parseOpValue,
  type WireCounters
} from '../backends/reactnative/rust-core-wire'
import { parseContinuationLinkOutcome, type ContinuationLinkOutcome } from './native-continuation-link'
import { decodeNativeContinuationEnvelope } from './native-continuation-envelope'

export interface NativeContinuationCompleted {
  readonly event: 'continuation.completed'
  readonly strategy: 'native'
  readonly peerAddress: string
  readonly resubscribed: number
  readonly link?: ContinuationLinkOutcome
}

export type NativeContinuationFailed = Extract<ContinuationRecoveryStatus, { readonly event: 'continuation.failed' }>

export interface NativeContinuationDesktopStatus {
  readonly queuedData: number
  readonly lastError: NormalizedBleError | null
  readonly continuationOutcome: ContinuationRecoveryStatus | null
}

/** Mobile's exact session/process counter report; no desktop error or queue facts are inferred. */
export interface NativeContinuationMobileStatus extends WireCounters {
  readonly continuationOutcome: ContinuationRecoveryStatus | null
}

export type NativeContinuationStatus = NativeContinuationDesktopStatus | NativeContinuationMobileStatus

/** Canonical native-envelope operations supplied by an authenticated host bridge. */
export interface NativeContinuationControlAccess {
  execute(peerId: string, declarationJson: string): Promise<unknown>
  describeBacklog(): Promise<unknown>
  prepareClaim(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeClaim(token: string): Promise<unknown>
}

export interface NativeContinuationControl {
  /** Starts the declared order on this already-open central. Requires a peerId.
   * The host owns process startup/OS wake integration; this does not register
   * an OS relaunch service, open a second radio, or depend on renderer lifetime. */
  execute(declaration: BackgroundContinuationDeclaration): Promise<NativeContinuationCompleted>
  /** Decodes all prepared values before acknowledging their handoff. */
  claim(options?: NativeContinuationClaimOptions): Promise<ContinuationBacklog>
  /** Null means no native session is owned; failures are never hidden as null.
   * Desktop reads queue behind autonomous recovery, while explicit execution
   * or another foreground handoff retains its busy lifecycle refusal. */
  status(): Promise<NativeContinuationStatus | null>
}

/** Trusted transport diagnostics only; does not select or acquire a backend. */
export interface NativeContinuationControlContext {
  readonly hostDomain: string
  readonly scope: string
  /** Selects the native result/peer vocabulary, never a radio or platform fallback. */
  readonly format?: 'desktop' | 'mobile'
}

const DESKTOP_CONTEXT: NativeContinuationControlContext = Object.freeze({
  hostDomain: 'ubm-desktop',
  scope: 'desktop-native'
})

export async function callNativeContinuationControl(
  operation: string,
  invoke: () => Promise<unknown>,
  context: NativeContinuationControlContext = DESKTOP_CONTEXT
): Promise<unknown> {
  const SCOPE = context.scope
  try {
    return decodeNativeContinuationEnvelope(await invoke(), context.hostDomain, `${SCOPE}.continuation.${operation}`)
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('platform.failure', 'restoration', `${SCOPE}.continuation.${operation}`, {
      domain: context.hostDomain,
      code: 'native-bridge',
      safeMessage: error instanceof Error ? error.message.slice(0, 1024) : String(error).slice(0, 1024),
      metadata: {}
    })
  }
}

/** Canonical transport-neutral control; callers sequence execute and claim. No radio or filesystem authority is created. */
export function createNativeContinuationControl(
  access: NativeContinuationControlAccess,
  context: NativeContinuationControlContext = DESKTOP_CONTEXT
): NativeContinuationControl {
  const SCOPE = context.scope
  function record(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
  }

  function malformed(operation: string): BackendContractError {
    return contractError('protocol.malformed', 'restoration', `${SCOPE}.continuation.${operation}`)
  }

  function exact(value: Record<string, unknown>, fields: readonly string[], operation: string): void {
    if (Object.keys(value).length !== fields.length || fields.some(field => !Object.hasOwn(value, field)))
      throw malformed(operation)
  }

  function count(value: unknown): value is number {
    return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
  }

  function decodeError(value: unknown): NormalizedBleError {
    if (!record(value)) throw malformed('status.error')
    const { retryability = 'never', ...error } = value
    const parsed = parseInvokeEnvelope(
      JSON.stringify({ ok: false, error, commit: null, retryability }),
      'counters.describe'
    )
    if (!parsed.ok) throw parsed.error
    if (parsed.value.kind !== 'failure') throw malformed('status.error')
    const failure = failureEnvelopeError(parsed.value).normalized
    return Object.freeze({
      ...failure,
      platform:
        failure.platform === null
          ? null
          : Object.freeze({
              ...failure.platform,
              domain: parsed.value.failure.platform === null ? context.hostDomain : failure.platform.domain
            })
    })
  }

  function completed(value: unknown): NativeContinuationCompleted {
    if (!record(value)) throw malformed('outcome')
    exact(
      value,
      ['event', 'strategy', 'peerAddress', 'resubscribed', ...(Object.hasOwn(value, 'link') ? ['link'] : [])],
      'outcome'
    )
    if (
      value.event !== 'continuation.completed' ||
      value.strategy !== 'native' ||
      typeof value.peerAddress !== 'string' ||
      value.peerAddress.length === 0 ||
      !count(value.resubscribed)
    )
      throw malformed('outcome')
    const link = parseContinuationLinkOutcome(value.link)
    return Object.freeze({
      event: value.event,
      strategy: value.strategy,
      peerAddress: value.peerAddress,
      resubscribed: value.resubscribed,
      ...(link === undefined ? {} : { link })
    })
  }

  const call = (operation: string, invoke: () => Promise<unknown>) =>
    callNativeContinuationControl(operation, invoke, context)
  const missing = () => contractError('capability.unsupported', 'restoration', `${SCOPE}.continuation.native-owner`)

  return Object.freeze({
    execute: (input: BackgroundContinuationDeclaration) =>
      rehydratePublicPromise(
        (async () => {
          const declaration =
            context.format === 'mobile'
              ? normalizeBackgroundContinuation(input)
              : normalizeHostBackgroundContinuation(input)
          if (declaration.onAppearance !== 'native' || declaration.peerId === undefined)
            throw contractError('argument.invalid', 'restoration', `${SCOPE}.continuation.declaration`)
          // Host normalization preserves the exact radio identity; the native
          // radio owns canonical admission and event reconciliation.
          const peerId = declaration.peerId
          const execute = access.execute
          if (typeof execute !== 'function') throw missing()
          const outcome = completed(
            await call('execute', () => execute.call(access, peerId, serializeBackgroundContinuation(declaration)))
          )
          if (outcome.peerAddress !== declaration.peerId || outcome.resubscribed !== declaration.resubscribe.length)
            throw malformed('outcome.identity')
          if (outcome.link?.mtu.requested !== declaration.link?.mtu.requested) throw malformed('outcome.link.identity')
          if (outcome.link?.mtu.outcome === 'unsupported' && declaration.link?.mtu.onUnsupported !== 'continue')
            throw malformed('outcome.link.policy')
          return outcome
        })()
      ),
    claim: (request?: NativeContinuationClaimOptions) =>
      rehydratePublicPromise(
        claimNativeContinuationBacklog(
          {
            prepareClaim: (maxItems, maxBytes) => {
              const prepare = access.prepareClaim
              if (typeof prepare !== 'function' || typeof access.acknowledgeClaim !== 'function')
                return Promise.reject(missing())
              return call('claim', () => prepare.call(access, maxItems, maxBytes))
            },
            acknowledgeClaim: token => {
              const acknowledge = access.acknowledgeClaim
              if (typeof acknowledge !== 'function') return Promise.reject(missing())
              return call('acknowledge', () => acknowledge.call(access, token))
            }
          },
          request,
          SCOPE
        )
      ),
    status: () =>
      rehydratePublicPromise(
        (async () => {
          const status = access.describeBacklog
          if (typeof status !== 'function') throw missing()
          const value = await call('status', () => status.call(access))
          if (value === null) return null
          if (!record(value)) throw malformed('status')
          if (context.format === 'mobile') {
            exact(value, ['counters', 'native', 'process', 'continuationOutcome'], 'status')
            const { continuationOutcome, ...counters } = value
            const parsed = parseOpValue('counters.describe', counters)
            if (!parsed.ok) throw parsed.error
            return Object.freeze({
              ...parsed.value,
              continuationOutcome: parseContinuationRecoveryStatus(continuationOutcome)
            })
          }
          exact(value, ['queuedData', 'lastError', 'continuationOutcome'], 'status')
          if (!count(value.queuedData)) throw malformed('status.queued-data')
          return Object.freeze({
            queuedData: value.queuedData,
            lastError: value.lastError === null ? null : decodeError(value.lastError),
            continuationOutcome: parseContinuationRecoveryStatus(value.continuationOutcome)
          })
        })()
      )
  })
}
