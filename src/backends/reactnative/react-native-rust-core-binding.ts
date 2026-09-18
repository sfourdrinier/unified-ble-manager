// src/backends/reactnative/react-native-rust-core-binding.ts
//
// The production binding over the `UnifiedBleRustCore` TurboModule
// (src/NativeUnifiedBleRustCore.ts). Every argument and result crosses the
// strict `ubm-mobile-wire/1` codec in ./rust-core-wire; nothing here infers a
// value or substitutes an identity.
//
// Admission (PR210-15, PR210-18) happens in this order, before any radio work:
//   1. the binary's own `nativeBuildIdentity()`, `contractRevision()` and
//      `wireRevision()` must equal what this package was sealed with;
//   2. `openSession` admits the lease;
//   3. the admission record must repeat the same identity and revisions.
// Any failure after step 2 closes the lease before the failure propagates; a
// failed close is reported with the original failure as cleanup debt.
//
// Wakes: one `onSessionWake` subscription per binding, routed by session id,
// held only while sessions are open (PR210-17). A wake that arrives before a
// session registers its listener is kept for that session, so no wake is lost.

import { TurboModuleRegistry } from 'react-native'

import { BackendContractError, contractError, type PlatformErrorDetail } from '../../backend-contract/errors'
import type { NativeBuildBinding } from '../../generated/native-build-identity'
import { EXPECTED_NATIVE_BUILD_IDENTITY, type ExpectedNativeBuildIdentity } from '../../generated/native-build-identity'
import {
  nativeBuildIdentitiesEqual,
  nativeBuildIdentityMismatches,
  parseNativeBuildIdentityText,
  readNativeBuildIdentity,
  type NativeBuildIdentityRecord
} from '../../native-build-identity-check'
import type { Spec } from '../../NativeUnifiedBleRustCore'
import type {
  ReactNativeRustCoreBinding,
  ReactNativeRustCoreSession,
  RustCoreRestorationIdentityRequest
} from './react-native-rust-core'
import {
  checkRandomByteLength,
  checkWriteReceipt,
  parseAdmissionText,
  parseDrainText,
  parseInvokeEnvelope,
  parseOpValue,
  parseRandomBytesText,
  parseRemoteFailureText,
  parseConfiguredRestorationIdentityText,
  parseRestorationIdentityText,
  remoteFailureError,
  serializeInvokeArgs,
  WIRE_REVISION,
  type WireCommit,
  type WireDrainBatch,
  type WireJsonObject,
  type WireOp,
  type WireOpResults,
  type WireRemoteFailure,
  type WireRestorationIdentity,
  type WireResult
} from './rust-core-wire'

export type ReactNativeRustCoreBindingPlatform = 'android' | 'apple'

export interface ReactNativeRustCoreBindingOptions {
  /** Selects the sealed identity: `jni` for Android, `uniffi` for Apple. */
  readonly platform: ReactNativeRustCoreBindingPlatform
  /**
   * The native module. Absent (production), the `UnifiedBleRustCore`
   * TurboModule is resolved with `TurboModuleRegistry.get`; a missing module
   * fails with `capability.unsupported`. Tests pass a deterministic module.
   */
  readonly native?: Spec
  /** The sealed identity to require; defaults to the package's generated one. */
  readonly expectedIdentity?: ExpectedNativeBuildIdentity
}

const OPERATION = 'react-native-rust-core'

function bindingFor(platform: ReactNativeRustCoreBindingPlatform): NativeBuildBinding {
  return platform === 'android' ? 'jni' : 'uniffi'
}

function resolveNative(options: ReactNativeRustCoreBindingOptions): Spec {
  if (options.native !== undefined) return options.native
  const native = TurboModuleRegistry.get<Spec>('UnifiedBleRustCore')
  if (native == null) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  return native
}

function unwrap<Value>(result: WireResult<Value>): Value {
  if (!result.ok) throw result.error
  return result.value
}

function rejectionMessage(error: unknown): string | null {
  if (typeof error === 'string') return error
  if (typeof error === 'object' && error !== null) {
    const message: unknown = Reflect.get(error, 'message')
    if (typeof message === 'string') return message
  }
  return null
}

/**
 * The contract error a native rejection carries. The facade rejects with
 * Rust's structured failure JSON; any other rejection is itself a protocol
 * fault, reported with its text rather than mapped to a guessed identity.
 */
function nativeRejection(error: unknown, operation: string): BackendContractError {
  const message = rejectionMessage(error)
  const parsed = parseRemoteFailureText(message, `${operation}.rejection`)
  if (parsed.ok) return remoteFailureError(parsed.value)
  return contractError('protocol.malformed', 'core', `${OPERATION}.${operation}.rejection`, {
    domain: 'react-native-rust-core',
    code: 'unstructured-native-rejection',
    safeMessage: (message ?? String(error)).slice(0, 1024),
    metadata: Object.freeze({})
  })
}

/** The failure an envelope reports, with the owner's commit state on writes. */
function failureWithCommit(failure: WireRemoteFailure, commit: WireCommit | null): BackendContractError {
  const error = remoteFailureError(failure)
  if (commit === null) return error
  return new BackendContractError({
    ...error.normalized,
    retryability: commit === 'uncertain' ? 'never' : error.normalized.retryability,
    commit
  })
}

function identityFailure(safeMessage: string, fields: readonly string[]): BackendContractError {
  return contractError('protocol.incompatible', 'core', `${OPERATION}.native-identity`, {
    domain: 'react-native-rust-core',
    code: 'native-identity',
    safeMessage,
    metadata: Object.freeze({ fields: Object.freeze([...fields]) })
  })
}

/**
 * Keeps the original failure's identity and adds the failed close as cleanup
 * debt: the caller learns both that admission failed and that a native lease
 * may remain.
 */
function withCleanupDebt(original: unknown, sessionId: string, closeError: unknown): unknown {
  if (!(original instanceof BackendContractError)) return original
  const debt = closeError instanceof BackendContractError ? closeError.normalized : null
  const platform: PlatformErrorDetail = {
    domain: 'react-native-rust-core',
    code: 'cleanup-debt',
    safeMessage: `session ${sessionId} was not closed after admission failed; its native lease may remain`,
    metadata: Object.freeze({
      sessionId,
      original: original.normalized.platform?.safeMessage ?? null,
      closeCode: debt?.code ?? 'unknown',
      closeOperation: debt?.operation ?? 'unknown'
    })
  }
  return new BackendContractError({ ...original.normalized, platform })
}

class WakeRouter {
  private subscription: { remove(): void } | null = null
  private readonly listeners = new Map<string, () => void>()
  private readonly pending = new Set<string>()
  private users = 0

  constructor(private readonly native: Spec) {}

  /** Held from before `openSession` until the session closes (or admission fails). */
  retain(): void {
    this.users += 1
    if (this.subscription === null) {
      this.subscription = this.native.onSessionWake(event => this.route(event.sessionId))
    }
  }

  release(sessionId: string | null): void {
    if (sessionId !== null) {
      this.listeners.delete(sessionId)
      this.pending.delete(sessionId)
    }
    this.users -= 1
    if (this.users === 0 && this.subscription !== null) {
      this.subscription.remove()
      this.subscription = null
      this.pending.clear()
    }
  }

  listen(sessionId: string, listener: () => void): () => void {
    if (this.listeners.has(sessionId)) {
      throw contractError('lifecycle.invalid-state', 'core', `${OPERATION}.wake-listener`)
    }
    this.listeners.set(sessionId, listener)
    if (this.pending.delete(sessionId)) listener()
    return () => {
      if (this.listeners.get(sessionId) === listener) this.listeners.delete(sessionId)
    }
  }

  private route(sessionId: string): void {
    const listener = this.listeners.get(sessionId)
    if (listener === undefined) {
      this.pending.add(sessionId)
      return
    }
    listener()
  }
}

/** Creates the production binding over the native module. */
export function createReactNativeRustCoreBinding(
  options: ReactNativeRustCoreBindingOptions
): ReactNativeRustCoreBinding {
  const native = resolveNative(options)
  const binding = bindingFor(options.platform)
  const expected = options.expectedIdentity ?? EXPECTED_NATIVE_BUILD_IDENTITY
  const wakes = new WakeRouter(native)

  const call = async <Value>(operation: string, run: () => Promise<Value>): Promise<Value> => {
    try {
      return await run()
    } catch (error) {
      throw nativeRejection(error, operation)
    }
  }

  const verifyBinary = async (): Promise<NativeBuildIdentityRecord> => {
    const reported = await call('native-build-identity', () => native.nativeBuildIdentity())
    const identity = parseNativeBuildIdentityText(reported)
    if (identity === null) {
      throw identityFailure('the native binary reported a malformed build identity', ['nativeBuildIdentity'])
    }
    const mismatches = [...nativeBuildIdentityMismatches(identity, binding, expected, false)]
    const contractRevision = await call('contract-revision', () => native.contractRevision())
    if (contractRevision !== expected.contractRevision) mismatches.push('contractRevision()')
    const wireRevision = await call('wire-revision', () => native.wireRevision())
    if (wireRevision !== WIRE_REVISION) mismatches.push('wireRevision()')
    if (mismatches.length > 0) {
      throw identityFailure(
        `the native ${binding} binary differs from the packaged identity in: ${mismatches.join(', ')}`,
        mismatches
      )
    }
    return identity
  }

  const admit = (text: string, identity: NativeBuildIdentityRecord, sessionId: string): void => {
    const admission = unwrap(parseAdmissionText(text))
    const mismatches: string[] = []
    if (admission.sessionId !== sessionId) mismatches.push('sessionId')
    if (admission.contractRevision !== expected.contractRevision) mismatches.push('contractRevision')
    if (admission.wireRevision !== WIRE_REVISION) mismatches.push('wireRevision')
    const admitted = readNativeBuildIdentity(admission.buildIdentity)
    if (admitted === null || !nativeBuildIdentitiesEqual(admitted, identity)) mismatches.push('buildIdentity')
    if (mismatches.length > 0) {
      throw identityFailure(
        `the admission record differs from the verified binary in: ${mismatches.join(', ')}`,
        mismatches
      )
    }
  }

  const createSession = (sessionId: string, identity: NativeBuildIdentityRecord): ReactNativeRustCoreSession => {
    let lastOrdinal: number | null = null
    let closing: Promise<void> | null = null
    let closed = false
    return Object.freeze({
      sessionId,
      buildIdentity: identity,
      invoke: async <Op extends WireOp>(op: Op, args: WireJsonObject): Promise<WireOpResults[Op]> => {
        if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.${op}`)
        const argsText = unwrap(serializeInvokeArgs(args))
        const text = await call(op, () => native.invoke(sessionId, op, argsText))
        const envelope = unwrap(parseInvokeEnvelope(text, op))
        if (envelope.kind === 'failure') throw failureWithCommit(envelope.failure, envelope.commit)
        return unwrap(parseOpValue(op, envelope.value))
      },
      drain: async (maxItems: number, maxBytes: number): Promise<WireDrainBatch> => {
        if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.drain`)
        const text = await call('drain', () => native.drain(sessionId, maxItems, maxBytes))
        const batch = unwrap(parseDrainText(text, lastOrdinal))
        const last = batch.records[batch.records.length - 1]
        if (last !== undefined) lastOrdinal = last.ordinal
        return batch
      },
      onWake: (listener: () => void) => wakes.listen(sessionId, listener),
      close: (): Promise<void> => {
        if (closed) return Promise.resolve()
        if (closing === null) {
          closing = call('close-session', () => native.closeSession(sessionId)).then(
            () => {
              closed = true
              wakes.release(sessionId)
            },
            error => {
              closing = null
              throw error
            }
          )
        }
        return closing
      }
    })
  }

  return Object.freeze({
    openSession: async (owner: string): Promise<ReactNativeRustCoreSession> => {
      if (owner.length === 0) throw contractError('argument.invalid', 'core', `${OPERATION}.owner`)
      wakes.retain()
      let sessionId: string | null = null
      try {
        const identity = await verifyBinary()
        const text = await call('open-session', () => native.openSession(owner, WIRE_REVISION))
        const admission = parseAdmissionText(text)
        if (!admission.ok) {
          // The owner admitted a lease this side cannot name, so it cannot be
          // closed from here: report the leaked lease with the fault.
          throw withCleanupDebt(admission.error, 'unreadable', null)
        }
        sessionId = admission.value.sessionId
        admit(text, identity, sessionId)
        return createSession(sessionId, identity)
      } catch (error) {
        if (sessionId === null) {
          wakes.release(null)
          throw error
        }
        const admitted = sessionId
        try {
          await call('close-session', () => native.closeSession(admitted))
        } catch (closeError) {
          wakes.release(admitted)
          throw withCleanupDebt(error, admitted, closeError)
        }
        wakes.release(admitted)
        throw error
      }
    },
    randomBytes: async (length: number): Promise<Uint8Array> => {
      const checked = unwrap(checkRandomByteLength(length))
      const text = await call('random-bytes', () => native.randomBytes(checked))
      return unwrap(parseRandomBytesText(text, checked))
    },
    configuredRestorationIdentity: async (): Promise<WireRestorationIdentity | null> => {
      const text = await call('restoration-identity', () => native.restorationIdentity('{}'))
      return unwrap(parseConfiguredRestorationIdentityText(text))
    },
    restorationIdentity: async (request: RustCoreRestorationIdentityRequest): Promise<WireRestorationIdentity> => {
      const requestText = unwrap(
        serializeInvokeArgs({ restorationId: request.restorationId, generation: request.generation })
      )
      const text = await call('restoration-identity', () => native.restorationIdentity(requestText))
      return unwrap(parseRestorationIdentityText(text))
    }
  })
}

/** Checks a write receipt against the requested mode (exported for the provider). */
export function checkedWriteReceipt(
  receipt: WireOpResults['gatt.write'],
  mode: 'with-response' | 'without-response'
): WireOpResults['gatt.write'] {
  return unwrap(checkWriteReceipt(receipt, mode))
}
