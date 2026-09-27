import {
  normalizeHostBackgroundContinuation,
  serializeBackgroundContinuation,
  type BackgroundContinuationDeclaration
} from '../../backend-contract/background-continuation'
import { BackendContractError, contractError, type NormalizedBleError } from '../../backend-contract/errors'
import { rehydratePublicPromise } from '../../public/error-bridge'
import {
  claimNativeContinuationBacklog,
  type NativeContinuationClaimOptions
} from '../../core/native-continuation-claim'
import {
  parseContinuationRecoveryStatus,
  type ContinuationBacklog,
  type ContinuationRecoveryStatus
} from '../reactnative/react-native-continuation-claim'
import { failureEnvelopeError, parseInvokeEnvelope } from '../reactnative/rust-core-wire'
import type { DesktopRustCoreCentral } from './desktop-rust-core-binding'

export interface NativeContinuationCompleted {
  readonly event: 'continuation.completed'
  readonly strategy: 'native'
  readonly peerAddress: string
  readonly resubscribed: number
}

export type NativeContinuationFailed = Extract<ContinuationRecoveryStatus, { readonly event: 'continuation.failed' }>

export interface NativeContinuationStatus {
  readonly queuedData: number
  readonly lastError: NormalizedBleError | null
  readonly continuationOutcome: ContinuationRecoveryStatus | null
}

export interface NativeContinuationController {
  /** Starts the declared order on this already-open central. Requires a peerId.
   * The host owns process startup/OS wake integration; this does not register
   * an OS relaunch service, open a second radio, or depend on renderer lifetime. */
  execute(declaration: BackgroundContinuationDeclaration): Promise<NativeContinuationCompleted>
  /** Decodes all prepared values before acknowledging their handoff. */
  claim(options?: NativeContinuationClaimOptions): Promise<ContinuationBacklog>
  /** Null means no native session is owned; failures are never hidden as null. */
  status(): Promise<NativeContinuationStatus | null>
}

const SCOPE = 'desktop-native'

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

function decodeEnvelope(text: unknown): unknown {
  // Continuation uses the same non-writing native control envelope. The
  // operation's own failure identity is preserved by the existing codec.
  const parsed = parseInvokeEnvelope(text, 'counters.describe')
  if (!parsed.ok) throw parsed.error
  if (parsed.value.kind === 'value') return parsed.value.value
  const error = failureEnvelopeError(parsed.value)
  if (parsed.value.failure.platform === null && error.normalized.platform !== null) {
    throw new BackendContractError({
      ...error.normalized,
      platform: { ...error.normalized.platform, domain: 'ubm-desktop' }
    })
  }
  throw error
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
            domain: parsed.value.failure.platform === null ? 'ubm-desktop' : failure.platform.domain
          })
  })
}

function completed(value: unknown): NativeContinuationCompleted {
  if (!record(value)) throw malformed('outcome')
  exact(value, ['event', 'strategy', 'peerAddress', 'resubscribed'], 'outcome')
  if (
    value.event !== 'continuation.completed' ||
    value.strategy !== 'native' ||
    typeof value.peerAddress !== 'string' ||
    value.peerAddress.length === 0 ||
    !count(value.resubscribed)
  )
    throw malformed('outcome')
  return Object.freeze({
    event: value.event,
    strategy: value.strategy,
    peerAddress: value.peerAddress,
    resubscribed: value.resubscribed
  })
}

/** Trusted Node/Electron main-process API. The caller retains and eventually
 * closes its existing central; this controller never assumes radio ownership. */
export function createNativeContinuationController(central: DesktopRustCoreCentral): NativeContinuationController {
  const call = async (operation: string, invoke: () => Promise<unknown>) => {
    try {
      return decodeEnvelope(await invoke())
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      throw contractError('platform.failure', 'restoration', `${SCOPE}.continuation.${operation}`, {
        domain: 'ubm-desktop',
        code: 'native-bridge',
        safeMessage: error instanceof Error ? error.message.slice(0, 1024) : String(error).slice(0, 1024),
        metadata: {}
      })
    }
  }
  const missing = () => contractError('capability.unsupported', 'restoration', `${SCOPE}.continuation.native-owner`)
  return Object.freeze({
    execute: (input: BackgroundContinuationDeclaration) =>
      rehydratePublicPromise(
        (async () => {
          const declaration = normalizeHostBackgroundContinuation(input)
          if (declaration.onAppearance !== 'native' || declaration.peerId === undefined)
            throw contractError('argument.invalid', 'restoration', `${SCOPE}.continuation.declaration`)
          // Host normalization preserves the exact radio identity; the native
          // radio owns canonical admission and event reconciliation.
          const peerId = declaration.peerId
          const execute = central.continuationExecute
          if (typeof execute !== 'function') throw missing()
          const outcome = completed(
            await call('execute', () => execute.call(central, peerId, serializeBackgroundContinuation(declaration)))
          )
          if (outcome.peerAddress !== declaration.peerId || outcome.resubscribed !== declaration.resubscribe.length)
            throw malformed('outcome.identity')
          return outcome
        })()
      ),
    claim: (request?: NativeContinuationClaimOptions) =>
      rehydratePublicPromise(
        claimNativeContinuationBacklog(
          {
            prepareClaim: (maxItems, maxBytes) => {
              const prepare = central.continuationPrepareClaim
              if (typeof prepare !== 'function' || typeof central.continuationAcknowledgeClaim !== 'function')
                return Promise.reject(missing())
              return call('claim', () => prepare.call(central, maxItems, maxBytes))
            },
            acknowledgeClaim: token => {
              const acknowledge = central.continuationAcknowledgeClaim
              if (typeof acknowledge !== 'function') return Promise.reject(missing())
              return call('acknowledge', () => acknowledge.call(central, token))
            }
          },
          request,
          SCOPE
        )
      ),
    status: () =>
      rehydratePublicPromise(
        (async () => {
          const status = central.continuationDescribeBacklog
          if (typeof status !== 'function') throw missing()
          const value = await call('status', () => status.call(central))
          if (value === null) return null
          if (!record(value)) throw malformed('status')
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
