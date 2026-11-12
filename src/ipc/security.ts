import { BackendContractError, contractError } from '../backend-contract/errors'
import type {
  SecurityBackend,
  PeerSecurityState,
  PeerSecurityEvent,
  SecurityPairResult,
  SecurityCancelPairingResult,
  SecurityUnpairResult,
  SecurityPairingChallenge,
  SecurityPairingResponse
} from '../backend-contract/security'
import type { Limitation } from '../backend-contract/capabilities'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import type { NormalizedBleError } from '../backend-contract/errors'
import { CoreBoundedStream } from '../core/bounded-stream'
import { capacity } from '../backend-contract/primitives'
import type { SerializableRecord } from '../backend-contract/primitives'
import type { IpcBleManager } from './manager'

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
function member<const T extends readonly string[]>(value: unknown, values: T): value is T[number] {
  return typeof value === 'string' && values.some(entry => entry === value)
}
function limitation(value: unknown): value is Limitation {
  return (
    record(value) &&
    typeof value.code === 'string' &&
    typeof value.explanation === 'string' &&
    typeof value.affectedGuarantee === 'string'
  )
}
export function isSecurityState(value: unknown): value is PeerSecurityState {
  return (
    record(value) &&
    member(value.bond, ['bonded', 'not-bonded', 'bonding', 'unknown', 'unsupported']) &&
    member(value.encryption, ['encrypted', 'not-encrypted', 'unknown', 'unsupported']) &&
    member(value.authentication, ['authenticated', 'unauthenticated', 'unknown', 'unsupported']) &&
    member(value.secureConnections, ['yes', 'no', 'unknown', 'unsupported']) &&
    (value.pairingPossible === null || typeof value.pairingPossible === 'boolean') &&
    typeof value.measuredAtMonotonicMs === 'number' &&
    Number.isFinite(value.measuredAtMonotonicMs) &&
    value.measuredAtMonotonicMs >= 0 &&
    Array.isArray(value.limitations) &&
    value.limitations.every(limitation)
  )
}
export function isSecurityEvent(value: unknown): value is PeerSecurityEvent {
  return (
    record(value) &&
    value.kind === 'state' &&
    typeof value.peerId === 'string' &&
    typeof value.sequence === 'number' &&
    Number.isSafeInteger(value.sequence) &&
    value.sequence >= 0 &&
    isSecurityState(value.state)
  )
}
function pairResult(value: unknown): SecurityPairResult {
  if (record(value)) {
    if (member(value.outcome, ['paired', 'already-paired', 'repaired']) && isSecurityState(value.state))
      return { outcome: value.outcome, state: value.state }
    if (value.outcome === 'cancelled') return { outcome: 'cancelled' }
    if (value.outcome === 'rejected' && (value.reason === null || typeof value.reason === 'string'))
      return { outcome: 'rejected', reason: value.reason }
  }
  throw contractError('protocol.malformed', 'ipc', 'ipc.security.pair-result')
}
export function isPairingChallenge(value: unknown): value is SecurityPairingChallenge {
  return (
    record(value) &&
    typeof value.peerId === 'string' &&
    typeof value.challengeId === 'string' &&
    typeof value.deadlineMonotonicMs === 'number' &&
    Number.isFinite(value.deadlineMonotonicMs) &&
    (member(value.kind, ['confirm', 'provide-pin', 'provide-passkey']) ||
      (member(value.kind, ['display-passkey', 'confirm-passkey']) &&
        typeof value.passkey === 'number' &&
        Number.isInteger(value.passkey) &&
        value.passkey >= 0 &&
        value.passkey <= 999999))
  )
}
function isWireChallenge(value: unknown): value is SerializableRecord {
  return (
    record(value) &&
    typeof value.budgetMs === 'number' &&
    Number.isFinite(value.budgetMs) &&
    value.budgetMs >= 0 &&
    isPairingChallenge({ ...value, deadlineMonotonicMs: 0 })
  )
}
function challengeFromWire(value: SerializableRecord): SecurityPairingChallenge {
  const budgetMs = value.budgetMs
  if (typeof budgetMs !== 'number') throw contractError('protocol.malformed', 'ipc', 'ipc.security.challenge-budget')
  const challenge = { ...value, deadlineMonotonicMs: globalThis.performance.now() + budgetMs }
  if (!isPairingChallenge(challenge)) throw contractError('protocol.malformed', 'ipc', 'ipc.security.challenge')
  return challenge
}
export function pairingResponse(value: unknown): SecurityPairingResponse {
  if (record(value)) {
    if (member(value.kind, ['confirm', 'confirm-passkey']) && typeof value.confirmed === 'boolean')
      return { kind: value.kind, confirmed: value.confirmed }
    if (value.kind === 'display-passkey' && typeof value.acknowledged === 'boolean')
      return { kind: 'display-passkey', acknowledged: value.acknowledged }
    if (value.kind === 'provide-pin' && typeof value.pin === 'string' && value.pin.length > 0 && value.pin.length <= 64)
      return { kind: 'provide-pin', pin: value.pin }
    if (value.kind === 'provide-passkey' && typeof value.passkey === 'string' && /^[0-9]{6}$/.test(value.passkey))
      return { kind: 'provide-passkey', passkey: value.passkey }
  }
  throw contractError('protocol.malformed', 'ipc', 'ipc.security.challenge-response')
}
let nextCeremony = 0
export function createIpcSecurityBackend(ipc: IpcBleManager): SecurityBackend {
  return {
    state: async (peerId, options) => {
      const result = await ipc.route(
        'security.state',
        { peerId, deadline: options.deadline },
        null,
        options.signal ?? undefined
      )
      if (!isSecurityState(result.state)) throw contractError('protocol.malformed', 'ipc', 'ipc.security.state')
      return result.state
    },
    watch: peerId => {
      const admission = new AbortController()
      let retiring = false
      const fallback = new CoreBoundedStream<PeerSecurityEvent>(
        { itemCapacity: capacity(128), byteCapacity: capacity(65536), reservedControlCapacity: capacity(1) },
        'error'
      )
      const acquired = ipc.route('security.watch.subscribe', { peerId }, null, admission.signal).then(result => {
        if (typeof result.handle !== 'string')
          throw contractError('protocol.malformed', 'ipc', 'ipc.security.watch-handle')
        return {
          handle: result.handle,
          stream: ipc.registerStream(
            result.handle,
            (value): value is PeerSecurityEvent => isSecurityEvent(value) && value.peerId === peerId,
            undefined,
            'error',
            () => releaseOwned(),
            'lease-owned'
          )
        }
      })
      // Route rejection is delivered by next(), but always observed even when never iterated.
      acquired.catch(() => undefined)
      const owned = acquired.then(
        value => value,
        () => null
      )
      let release: Promise<import('../backend-contract/errors').CleanupRecord> | null = null
      const releaseOwned = (): Promise<import('../backend-contract/errors').CleanupRecord> => {
        if (release !== null) return release
        const tracked = owned
          .then(async value => {
            if (value === null) return { state: 'released' as const, failures: [] }
            const result = await ipc.route('security.watch.unsubscribe', { handle: value.handle })
            if (result.state !== 'released')
              throw contractError('lifecycle.invalid-state', 'cleanup', 'ipc.security.watch-release')
            ipc.closeStream(value.handle)
            return { state: 'released' as const, failures: [] }
          })
          .catch(error => {
            if (release === tracked) release = null
            throw error
          })
        release = tracked
        return tracked
      }
      const close = (): Promise<import('../backend-contract/errors').CleanupRecord> => {
        retiring = true
        admission.abort()
        return releaseOwned()
      }
      const watch: BoundedAsyncStream<PeerSecurityEvent> = {
        limits: fallback.limits,
        overflowPolicy: fallback.overflowPolicy,
        close,
        [Symbol.asyncIterator]() {
          const iterator = acquired.then(value => value.stream[Symbol.asyncIterator]())
          return {
            [Symbol.asyncIterator]() {
              return this
            },
            next: async () => {
              try {
                const current = await iterator
                if (retiring) return { done: true, value: undefined }
                const item = await current.next()
                return retiring ? { done: true, value: undefined } : item
              } catch (error) {
                if (
                  retiring &&
                  error instanceof BackendContractError &&
                  error.normalized.code === 'operation.aborted'
                ) {
                  return { done: true, value: undefined }
                }
                throw error
              }
            },
            return: async () => {
              await close()
              if ((await owned) === null) return { done: true, value: undefined }
              return (await iterator).return()
            }
          }
        }
      }
      return watch
    },
    pair: async (peerId, options) => {
      const controller = new AbortController()
      const abort = (): void => controller.abort()
      options.signal?.addEventListener('abort', abort, { once: true })
      if (options.signal?.aborted) controller.abort()
      let retiring = false
      let nativeAnswered = false
      let terminalFailure: BackendContractError | null = null
      const failTerminal = (reason: StreamTerminalNotice['reason'], cause?: NormalizedBleError | null): void => {
        if (retiring) return
        terminalFailure ??=
          cause == null
            ? contractError(
                reason === 'overflow'
                  ? 'stream.overflow'
                  : reason === 'operation-timed-out'
                    ? 'operation.timed-out'
                    : reason === 'operation-aborted' || reason === 'owner-released'
                      ? 'operation.aborted'
                      : 'platform.transport',
                'ipc',
                'ipc.security.ceremony-terminal'
              )
            : new BackendContractError(cause)
        controller.abort()
      }
      const ceremonyHandle = options.ceremony === 'system' ? null : `security-ceremony-${++nextCeremony}`
      const stream =
        ceremonyHandle === null
          ? null
          : ipc.registerStream(ceremonyHandle, isWireChallenge, undefined, 'error', failTerminal)
      const agent = options.ceremony === 'system' ? null : options.ceremony.agent
      const pump =
        stream === null || agent === null
          ? Promise.resolve()
          : (async () => {
              for await (const item of stream) {
                if (item.kind === 'terminal') {
                  if (retiring) break
                  failTerminal(item.reason, item.error)
                  throw terminalFailure
                }
                if (item.kind === 'overflow') {
                  failTerminal('overflow')
                  throw terminalFailure
                }
                const challenge = challengeFromWire(item.value)
                if (challenge.peerId !== peerId)
                  throw contractError('ownership.denied', 'ipc', 'ipc.security.challenge-peer')
                const response = pairingResponse(
                  await abortableChallenge(() => agent.onChallenge(challenge), controller.signal)
                )
                await ipc.route(
                  'security.custom-ceremony',
                  { ceremonyHandle, challengeId: challenge.challengeId, response: { ...response } },
                  null,
                  controller.signal
                )
              }
            })().catch(error => {
              controller.abort()
              throw terminalFailure ?? error
            })
      // Always observe failed user callbacks; propagate them against the routed operation.
      try {
        const pending = ipc
          .route(
            'security.pair',
            {
              peerId,
              deadline: options.deadline,
              transport: options.transport,
              protection: options.protection,
              secureConnections: options.secureConnections ?? 'prefer',
              ceremony: ceremonyHandle === null ? 'system' : 'agent',
              ...(ceremonyHandle === null ? {} : { ceremonyHandle })
            },
            null,
            controller.signal
          )
          .catch(error => {
            throw terminalFailure ?? error
          })
        // A normal pump finish must not win against the native operation.
        const failedPump = new Promise<never>((_resolve, reject) => {
          pump.catch(reject)
        })
        const result = await Promise.race([pending, failedPump])
        nativeAnswered = true
        return pairResult(result.result)
      } finally {
        retiring = true
        controller.abort()
        options.signal?.removeEventListener('abort', abort)
        if (ceremonyHandle !== null) ipc.closeStream(ceremonyHandle)
        // Local abort retires our wait even when an application callback is held.
        // The race above already observes any failure; do not replace a confirmed
        // native pairing result with this deliberate local cleanup cancellation.
        await pump.catch(error => {
          if (
            nativeAnswered &&
            !(error instanceof BackendContractError && error.normalized.code === 'operation.aborted')
          ) {
            console.error('[IpcSecurityBackend] Pairing agent failed during retirement:', error)
          }
        })
      }
    },
    cancelPairing: async (peerId, options): Promise<SecurityCancelPairingResult> => {
      const { result } = await ipc.route(
        'security.cancel-pairing',
        { peerId, deadline: options.deadline },
        null,
        options.signal ?? undefined
      )
      if (record(result)) {
        if (member(result.outcome, ['cancelled', 'not-pairing', 'paired'])) return { outcome: result.outcome }
        if (result.outcome === 'rejected' && (result.reason === null || typeof result.reason === 'string'))
          return { outcome: 'rejected', reason: result.reason }
      }
      throw contractError('protocol.malformed', 'ipc', 'ipc.security.cancel-result')
    },
    unpair: async (peerId, options): Promise<SecurityUnpairResult> => {
      const { result } = await ipc.route(
        'security.unpair',
        { peerId, deadline: options.deadline },
        null,
        options.signal ?? undefined
      )
      if (record(result) && member(result.outcome, ['unpaired', 'already-unpaired', 'unsupported']))
        return { outcome: result.outcome }
      throw contractError('protocol.malformed', 'ipc', 'ipc.security.unpair-result')
    }
  }
}

function abortableChallenge<Value>(action: () => Promise<Value>, signal: AbortSignal): Promise<Value> {
  return new Promise((resolve, reject) => {
    const aborted = (): void => {
      signal.removeEventListener('abort', aborted)
      reject(contractError('operation.aborted', 'platform', 'ipc.security.challenge'))
    }
    if (signal.aborted) {
      aborted()
      return
    }
    signal.addEventListener('abort', aborted, { once: true })
    Promise.resolve()
      .then(action)
      .then(resolve, reject)
      .finally(() => signal.removeEventListener('abort', aborted))
  })
}
