import {
  normalizeHostBackgroundContinuation,
  normalizeBackgroundContinuation,
  serializeBackgroundContinuation,
  type BackgroundContinuationDeclaration
} from '../backend-contract/background-continuation'
import { BackendContractError, contractError, type NormalizedBleError } from '../backend-contract/errors'
import { rehydratePublicPromise } from '../public/error-bridge'
import {
  claimNativeContinuationBacklog,
  claimNativeContinuationPrefixV1,
  type NativeContinuationClaimOptions
} from './native-continuation-claim'
import {
  parseContinuationRecoveryStatus,
  parseContinuationClaimAcknowledgement,
  type ContinuationBacklog,
  type ContinuationClaimAcknowledgement,
  type ContinuationRetainedPrefixV1,
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
import { decodeBase64OrThrow } from './recording-wire'

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
  /** Separate versioned native operations; absent means retained handoff is unsupported. */
  prepareRetainedClaimV1?(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeRetainedClaimV1?(token: string): Promise<unknown>
  retireRetainedClaimReceiptV1?(token: string): Promise<unknown>
  /** Explicit V2 retained-prefix protocol. It is never inferred from V1. */
  prepareRetainedClaimV2?(maxItems: number, maxBytes: number): Promise<unknown>
  acknowledgeRetainedClaimV2?(token: string): Promise<unknown>
  readPeerFailureRecovery?(record: string, offset: string): Promise<unknown>
  acknowledgePeerFailureRecovery?(record: string): Promise<unknown>
}

/** One exact page of retained source-failure evidence. */
export interface NativeContinuationPeerFailureRecoveryPage {
  readonly offset: number
  readonly nextOffset: number
  readonly bytes: Uint8Array
  readonly done: boolean
}

/**
 * A V2 recovery record stays opaque except for the operations needed to read
 * and acknowledge its exact retained evidence. Its route and lease remain in
 * the native record passed back to the host; applications never construct
 * them from descriptor identities.
 */
export interface NativeContinuationPeerFailureRecovery {
  readonly record: Readonly<Record<string, unknown>>
  read(offset: number): Promise<NativeContinuationPeerFailureRecoveryPage>
  acknowledge(): Promise<void>
}

/** Explicit V2 retained prefix. `acknowledge` is unavailable until every recovery has been fully read and acknowledged. */
export interface NativeContinuationRetainedPrefixV2 {
  readonly claimToken?: string
  readonly records: readonly Readonly<Record<string, unknown>>[]
  readonly recoveries: readonly NativeContinuationPeerFailureRecovery[]
  acknowledge(): Promise<ContinuationClaimAcknowledgement>
}

export interface NativeContinuationControl {
  /** Starts the declared order on this already-open central. Requires a peerId.
   * The host owns process startup/OS wake integration; this does not register
   * an OS relaunch service, open a second radio, or depend on renderer lifetime. */
  execute(declaration: BackgroundContinuationDeclaration): Promise<NativeContinuationCompleted>
  /** Decodes and automatically acknowledges the complete native handoff. */
  claim(options?: NativeContinuationClaimOptions): Promise<ContinuationBacklog>
  /** Discloses one retained prefix; acknowledge it before claiming the next. */
  claimRetainedV1?(options?: NativeContinuationClaimOptions): Promise<ContinuationRetainedPrefixV1>
  /**
   * Explicit rich retained-prefix protocol. It is offered only when native
   * claim and peer-failure read/ack operations all exist as one capability.
   */
  claimRetainedV2?(options?: NativeContinuationClaimOptions): Promise<NativeContinuationRetainedPrefixV2>
  /** Null means no native session is owned; failures are never hidden as null.
   * Desktop reads queue behind autonomous recovery, while explicit execution
   * or another foreground handoff retains its busy lifecycle refusal. */
  status(): Promise<NativeContinuationStatus | null>
}

/** The factory supplies the additive V1 method even if native support is unavailable. */
export interface NativeContinuationControlV1 extends NativeContinuationControl {
  claimRetainedV1(options?: NativeContinuationClaimOptions): Promise<ContinuationRetainedPrefixV1>
}

export interface NativeContinuationControlV2 extends NativeContinuationControlV1 {
  claimRetainedV2(options?: NativeContinuationClaimOptions): Promise<NativeContinuationRetainedPrefixV2>
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
): NativeContinuationControlV1 | NativeContinuationControlV2 {
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

  /**
   * This wire capability is intentionally opaque to applications, but the
   * control must validate its paging bounds before it lets a native page alter
   * the retained-prefix state.  JavaScript offsets cannot faithfully carry a
   * Rust u64 above Number.MAX_SAFE_INTEGER, and the common evidence protocol
   * limits one page to 64 KiB.
   */
  function peerFailureRecoveryDescriptor(value: unknown): {
    readonly encodedLength: number
    readonly chunkBytes: number
  } {
    if (!record(value)) throw malformed('retained-v2.recovery.descriptor')
    const identities = ['host', 'peer', 'sourceGenerationIdentity', 'nativeGroup', 'transfer', 'recovery'] as const
    const encodedLength = value.encodedLength
    const chunkBytes = value.chunkBytes
    if (
      value.protocol !== 1 ||
      value.representation !== 'ubm-peer-source-failure/1' ||
      typeof encodedLength !== 'string' ||
      !/^[1-9][0-9]*$/.test(encodedLength) ||
      !Number.isSafeInteger(Number(encodedLength)) ||
      Number(encodedLength) <= 0 ||
      typeof chunkBytes !== 'number' ||
      !Number.isSafeInteger(chunkBytes) ||
      chunkBytes <= 0 ||
      chunkBytes > 64 * 1024 ||
      identities.some(identity => typeof value[identity] !== 'string' || !/^[0-9a-f]{32}$/.test(value[identity]))
    )
      throw malformed('retained-v2.recovery.descriptor')
    return Object.freeze({ encodedLength: Number(encodedLength), chunkBytes })
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

  const claimAccess = {
    prepareClaim: (maxItems: number, maxBytes: number) => {
      const prepare = access.prepareClaim
      if (typeof prepare !== 'function' || typeof access.acknowledgeClaim !== 'function')
        return Promise.reject(missing())
      return call('claim', () => prepare.call(access, maxItems, maxBytes))
    },
    acknowledgeClaim: (token: string) => {
      const acknowledge = access.acknowledgeClaim
      if (typeof acknowledge !== 'function') return Promise.reject(missing())
      return call('acknowledge', () => acknowledge.call(access, token))
    }
  }

  const retainedAccess = {
    ...(access.prepareRetainedClaimV1 && access.acknowledgeRetainedClaimV1 && access.retireRetainedClaimReceiptV1
      ? {
          prepareRetainedClaimV1: (items: number, bytes: number) =>
            call('retained-v1.prepare', () => access.prepareRetainedClaimV1!(items, bytes)),
          acknowledgeRetainedClaimV1: (token: string) =>
            call('retained-v1.acknowledge', () => access.acknowledgeRetainedClaimV1!(token)),
          retireRetainedClaimReceiptV1: (token: string) =>
            call('retained-v1.retire', () => access.retireRetainedClaimReceiptV1!(token))
        }
      : {})
  }

  const retainedV2Access =
    typeof access.prepareRetainedClaimV2 === 'function' &&
    typeof access.acknowledgeRetainedClaimV2 === 'function' &&
    typeof access.readPeerFailureRecovery === 'function' &&
    typeof access.acknowledgePeerFailureRecovery === 'function'
      ? {
          prepare: (items: number, bytes: number) =>
            call('retained-v2.prepare', () => access.prepareRetainedClaimV2!(items, bytes)),
          acknowledge: (token: string) =>
            call('retained-v2.acknowledge', () => access.acknowledgeRetainedClaimV2!(token)),
          read: (recordId: string, offset: string) =>
            call('retained-v2.recovery.read', () => access.readPeerFailureRecovery!(recordId, offset)),
          acknowledgeRecovery: (recordId: string) =>
            call('retained-v2.recovery.acknowledge', () => access.acknowledgePeerFailureRecovery!(recordId))
        }
      : undefined

  const v2States = new WeakMap<
    object,
    { pending?: NativeContinuationRetainedPrefixV2; preparing?: Promise<NativeContinuationRetainedPrefixV2> }
  >()

  const claimRetainedV2 = (request?: NativeContinuationClaimOptions): Promise<NativeContinuationRetainedPrefixV2> => {
    if (retainedV2Access === undefined)
      return Promise.reject(contractError('capability.unsupported', 'restoration', `${SCOPE}.continuation.retained-v2`))
    const maxItems = request?.maxItems ?? 256
    const maxBytes = request?.maxBytes ?? 65536
    if (![maxItems, maxBytes].every(value => Number.isSafeInteger(value) && value >= 1 && value <= 0x7fffffff))
      return Promise.reject(contractError('argument.invalid', 'restoration', `${SCOPE}.continuation.claim-bounds`))
    const stateKey = retainedV2Access
    let state = v2States.get(stateKey)
    if (state === undefined) {
      state = {}
      v2States.set(stateKey, state)
    }
    if (state.pending !== undefined) return Promise.resolve(state.pending)
    if (state.preparing !== undefined) return state.preparing
    const preparing = (async () => {
      const prepared = await retainedV2Access.prepare(maxItems, maxBytes)
      if (!record(prepared)) throw malformed('retained-v2.prepare')
      const token = prepared.claimToken
      const batches = prepared.batches
      if (
        (token !== undefined && (typeof token !== 'string' || token.length === 0 || token.length > 256)) ||
        !Array.isArray(batches) ||
        batches.some(batch => typeof batch !== 'string')
      )
        throw malformed('retained-v2.prepare')
      const records: Readonly<Record<string, unknown>>[] = []
      for (const batchText of batches) {
        let batch: unknown
        try {
          batch = JSON.parse(batchText)
        } catch {
          throw malformed('retained-v2.batch')
        }
        if (
          !record(batch) ||
          !Array.isArray(batch.records) ||
          typeof batch.more !== 'boolean' ||
          !count(batch.controlLost)
        )
          throw malformed('retained-v2.batch')
        for (const entry of batch.records) {
          if (!record(entry)) throw malformed('retained-v2.record')
          records.push(Object.freeze({ ...entry }))
        }
      }
      if (token === undefined) {
        if (records.length !== 0) throw malformed('retained-v2.token')
        const empty = Object.freeze({
          records: Object.freeze([]),
          recoveries: Object.freeze([]),
          acknowledge: () =>
            Promise.reject(
              contractError('lifecycle.invalid-state', 'restoration', `${SCOPE}.continuation.retained-v2.empty`)
            )
        })
        // A no-token result owns no retained source prefix.  Caching it would
        // prevent a later prepare from observing work that arrived afterwards.
        return empty
      }
      const recoveries: NativeContinuationPeerFailureRecovery[] = []
      const recoveryStates: { complete: boolean; acknowledged: boolean }[] = []
      for (const entry of records) {
        if (entry.t !== 'peer-source-failure-recovery') continue
        if (entry.v !== 2) throw malformed('retained-v2.recovery.version')
        const descriptor = peerFailureRecoveryDescriptor(entry.descriptor)
        const serialized = JSON.stringify(entry)
        let nextOffset = 0
        let complete = false
        let acknowledged = false
        let acknowledgingRecovery: Promise<void> | undefined
        const recoveryState = { complete, acknowledged }
        recoveryStates.push(recoveryState)
        const recovery: NativeContinuationPeerFailureRecovery = Object.freeze({
          record: entry,
          read: (offset: number) =>
            rehydratePublicPromise(
              (async () => {
                if (!Number.isSafeInteger(offset) || offset < 0 || offset !== nextOffset || complete)
                  throw contractError(
                    'lifecycle.invalid-state',
                    'restoration',
                    `${SCOPE}.continuation.retained-v2.recovery.offset`
                  )
                const response = await retainedV2Access.read(serialized, String(offset))
                if (!record(response)) throw malformed('retained-v2.recovery.page')
                const responseOffset = response.offset
                const responseNextOffset = response.nextOffset
                if (
                  typeof responseOffset !== 'string' ||
                  typeof responseNextOffset !== 'string' ||
                  !/^(?:0|[1-9][0-9]*)$/.test(responseOffset) ||
                  !/^(?:0|[1-9][0-9]*)$/.test(responseNextOffset) ||
                  Number(responseOffset) !== offset ||
                  !Number.isSafeInteger(Number(responseNextOffset)) ||
                  Number(responseNextOffset) < offset ||
                  typeof response.done !== 'boolean'
                )
                  throw malformed('retained-v2.recovery.page')
                const bytes = decodeBase64OrThrow(response.bytesB64, `${SCOPE}.continuation.retained-v2.recovery.bytes`)
                if (Number(responseNextOffset) !== offset + bytes.byteLength)
                  throw malformed('retained-v2.recovery.page')
                if (
                  bytes.byteLength === 0 ||
                  bytes.byteLength > descriptor.chunkBytes ||
                  Number(responseNextOffset) > descriptor.encodedLength ||
                  response.done !== (Number(responseNextOffset) === descriptor.encodedLength)
                )
                  throw malformed('retained-v2.recovery.page')
                nextOffset = Number(responseNextOffset)
                complete = response.done
                recoveryState.complete = complete
                return Object.freeze({ offset, nextOffset, bytes, done: response.done })
              })()
            ),
          acknowledge: () =>
            rehydratePublicPromise(
              (() => {
                if (!complete)
                  return Promise.reject(
                    contractError(
                      'lifecycle.invalid-state',
                      'restoration',
                      `${SCOPE}.continuation.retained-v2.recovery.incomplete`
                    )
                  )
                if (acknowledged) return Promise.resolve()
                if (acknowledgingRecovery !== undefined) return acknowledgingRecovery
                const attempt = (async () => {
                  const response = await retainedV2Access.acknowledgeRecovery(serialized)
                  if (!record(response) || Object.keys(response).length !== 0)
                    throw malformed('retained-v2.recovery.acknowledge')
                  acknowledged = true
                  recoveryState.acknowledged = true
                })()
                acknowledgingRecovery = attempt
                attempt.then(
                  () => {
                    if (acknowledgingRecovery === attempt) acknowledgingRecovery = undefined
                  },
                  () => {
                    if (acknowledgingRecovery === attempt) acknowledgingRecovery = undefined
                  }
                )
                return attempt
              })()
            )
        })
        recoveries.push(recovery)
      }
      let receipt: ContinuationClaimAcknowledgement | undefined
      let acknowledging: Promise<ContinuationClaimAcknowledgement> | undefined
      const prefix: NativeContinuationRetainedPrefixV2 = Object.freeze({
        claimToken: token,
        records: Object.freeze(records),
        recoveries: Object.freeze(recoveries),
        acknowledge: () =>
          rehydratePublicPromise(
            (() => {
              if (receipt !== undefined) return Promise.resolve(receipt)
              if (acknowledging !== undefined) return acknowledging
              if (recoveryStates.some(recovery => !recovery.complete || !recovery.acknowledged))
                return Promise.reject(
                  contractError(
                    'lifecycle.invalid-state',
                    'restoration',
                    `${SCOPE}.continuation.retained-v2.recovery.incomplete`
                  )
                )
              const attempt = (async () => {
                const response = await retainedV2Access.acknowledge(token)
                const parsed = parseContinuationClaimAcknowledgement(response)
                receipt = parsed
                state!.pending = undefined
                return parsed
              })()
              acknowledging = attempt
              attempt.then(
                () => undefined,
                () => {
                  if (acknowledging === attempt) acknowledging = undefined
                }
              )
              return attempt
            })()
          )
      })
      state!.pending = prefix
      return prefix
    })()
    state.preparing = preparing
    preparing.then(
      () => {
        state!.preparing = undefined
      },
      () => {
        state!.preparing = undefined
      }
    )
    return preparing
  }

  const control: NativeContinuationControlV1 = Object.freeze({
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
      rehydratePublicPromise(claimNativeContinuationBacklog(claimAccess, request, SCOPE)),
    claimRetainedV1: (request?: NativeContinuationClaimOptions) =>
      rehydratePublicPromise(claimNativeContinuationPrefixV1(retainedAccess, request, SCOPE)),
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
  if (retainedV2Access === undefined) return control
  return Object.freeze({
    ...control,
    claimRetainedV2: (request?: NativeContinuationClaimOptions) => rehydratePublicPromise(claimRetainedV2(request))
  })
}
