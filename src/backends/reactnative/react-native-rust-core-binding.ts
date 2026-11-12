// src/backends/reactnative/react-native-rust-core-binding.ts
//
// The production binding over the `UnifiedBleRustCore` TurboModule
// (src/NativeUnifiedBleRustCore.ts). Session operations cross the strict
// `ubm-mobile-wire/3` codec in ./rust-core-wire. OS accessory setup and the
// `ubm-accessory-authorized/1` saved directory are separate versioned native
// controls, not Rust session envelopes. Neither path guesses a peer identity.
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
  parseScanEndVersion,
  failureEnvelopeError,
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
  type WireDrainBatch,
  type WireJsonObject,
  type WireOp,
  type WireOpResults,
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

function hasRetainedContinuationV1(native: Spec): native is Spec & {
  prepareRetainedContinuationClaimV1(maxItems: number, maxBytes: number): Promise<string>
  acknowledgeRetainedContinuationClaimV1(token: string): Promise<string>
  retireRetainedContinuationClaimReceiptV1(token: string): Promise<string>
} {
  return (
    'prepareRetainedContinuationClaimV1' in native &&
    typeof native.prepareRetainedContinuationClaimV1 === 'function' &&
    'acknowledgeRetainedContinuationClaimV1' in native &&
    typeof native.acknowledgeRetainedContinuationClaimV1 === 'function' &&
    'retireRetainedContinuationClaimReceiptV1' in native &&
    typeof native.retireRetainedContinuationClaimReceiptV1 === 'function'
  )
}

/**
 * A V2 claim can contain a source-failure recovery record. Advertising only
 * its prepare/claim half would strand that record, so these four methods are
 * one atomic capability rather than two independently optional features.
 */
function hasRetainedContinuationV2(native: Spec): native is Spec & {
  prepareRetainedContinuationClaimV2(maxItems: number, maxBytes: number): Promise<string>
  acknowledgeRetainedContinuationClaimV2(token: string): Promise<string>
  readPeerFailureRecovery(recoveryRecord: string, offset: string): Promise<string>
  acknowledgePeerFailureRecovery(recoveryRecord: string): Promise<string>
} {
  return (
    'prepareRetainedContinuationClaimV2' in native &&
    typeof native.prepareRetainedContinuationClaimV2 === 'function' &&
    'acknowledgeRetainedContinuationClaimV2' in native &&
    typeof native.acknowledgeRetainedContinuationClaimV2 === 'function' &&
    'readPeerFailureRecovery' in native &&
    typeof native.readPeerFailureRecovery === 'function' &&
    'acknowledgePeerFailureRecovery' in native &&
    typeof native.acknowledgePeerFailureRecovery === 'function'
  )
}

/** Recovery is atomic: a module that cannot list, bounded-read and ACK must not advertise any partial route. */
function hasTerminalEvidenceRecovery(native: Spec): native is Spec & {
  terminalEvidenceRecoveryOpenSelected(): Promise<string>
  terminalEvidenceRecoveryList(leaseId: string, maxItems: number, maxBytes: number): Promise<string>
  terminalEvidenceRecoveryRead(handle: string, offset: string, maxBytes: number): Promise<string>
  terminalEvidenceRecoveryAcknowledge(handle: string): Promise<string>
} {
  return (
    'terminalEvidenceRecoveryOpenSelected' in native &&
    typeof native.terminalEvidenceRecoveryOpenSelected === 'function' &&
    'terminalEvidenceRecoveryList' in native &&
    typeof native.terminalEvidenceRecoveryList === 'function' &&
    'terminalEvidenceRecoveryRead' in native &&
    typeof native.terminalEvidenceRecoveryRead === 'function' &&
    'terminalEvidenceRecoveryAcknowledge' in native &&
    typeof native.terminalEvidenceRecoveryAcknowledge === 'function'
  )
}

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

  const createSession = (
    sessionId: string,
    identity: NativeBuildIdentityRecord,
    scanEndVersion: number
  ): ReactNativeRustCoreSession => {
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
        if (envelope.kind === 'failure') throw failureEnvelopeError(envelope)
        return unwrap(parseOpValue(op, envelope.value))
      },
      drain: async (maxItems: number, maxBytes: number): Promise<WireDrainBatch> => {
        if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.drain`)
        const text = await call('drain', () => native.drain(sessionId, maxItems, maxBytes))
        const batch = unwrap(parseDrainText(text, lastOrdinal, scanEndVersion))
        const last = batch.records[batch.records.length - 1]
        if (last !== undefined) lastOrdinal = last.ordinal
        return batch
      },
      ...(typeof native.orderedRecordingVersion === 'function' &&
      typeof native.orderedRecordingReserve === 'function' &&
      typeof native.orderedRecordingStartStatus === 'function' &&
      typeof native.orderedRecordingRetire === 'function' &&
      typeof native.orderedRecordingStart === 'function' &&
      typeof native.orderedRecordingContext === 'function' &&
      typeof native.orderedRecordingStop === 'function'
        ? {
            orderedRecordingVersion: async (): Promise<number> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-version`)
              const version = await call('ordered-recording-version', () => native.orderedRecordingVersion())
              if (!Number.isSafeInteger(version) || version < 0)
                throw contractError('protocol.malformed', 'core', `${OPERATION}.ordered-recording-version`)
              return version
            },
            orderedRecordingReserve: async (id: string): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-reserve`)
              return call('ordered-recording-reserve', () => native.orderedRecordingReserve(sessionId, id))
            },
            orderedRecordingStartStatus: async (handle: string): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-status`)
              return call('ordered-recording-status', () => native.orderedRecordingStartStatus(sessionId, handle))
            },
            orderedRecordingRetire: async (handle: string): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-retire`)
              return call('ordered-recording-retire', () => native.orderedRecordingRetire(sessionId, handle))
            },
            orderedRecordingStart: async (
              handle: string,
              id: string,
              maxBytes: string,
              maxRecords: string,
              declaration: string,
              budgetMs: number
            ): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-start`)
              return call('ordered-recording-start', () =>
                native.orderedRecordingStart(sessionId, handle, id, maxBytes, maxRecords, declaration, budgetMs)
              )
            },
            orderedRecordingContext: async (id: string): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-context`)
              return call('ordered-recording-context', () => native.orderedRecordingContext(sessionId, id))
            },
            orderedRecordingStop: async (id: string): Promise<string> => {
              if (closed) throw contractError('lifecycle.destroyed', 'core', `${OPERATION}.ordered-recording-stop`)
              return call('ordered-recording-stop', () => native.orderedRecordingStop(sessionId, id))
            }
          }
        : {}),
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

  const terminalEvidenceRecoveryListWithStatus = native.terminalEvidenceRecoveryListWithStatus

  return Object.freeze({
    configureTerminalEvidenceMaximumBytes: async (maximumBytes: string): Promise<void> => {
      if (!/^[1-9][0-9]*$/.test(maximumBytes))
        throw contractError(
          'argument.invalid',
          'core',
          'react-native-rust-core.terminal-evidence.maximum-durable-bytes'
        )
      await call('terminal-evidence-configure', () => native.configureTerminalEvidenceMaximumBytes(maximumBytes))
    },
    ...(typeof native.chooseAccessory === 'function' &&
    typeof native.cancelAccessoryChoice === 'function' &&
    typeof native.accessoryChooserAvailable === 'function'
      ? {
          accessoryChooserAvailable: async (): Promise<boolean> => {
            await verifyBinary()
            const value = await call('accessory-availability', () => native.accessoryChooserAvailable())
            if (typeof value !== 'boolean')
              throw contractError('protocol.malformed', 'chooser', 'accessory.availability')
            return value
          },
          chooseAccessory: async (requestId: string, optionsJson: string, timeoutMs: number): Promise<string> => {
            await verifyBinary()
            return call('accessory-choose', () => native.chooseAccessory(requestId, optionsJson, timeoutMs))
          },
          cancelAccessoryChoice: async (requestId: string): Promise<void> => {
            await call('accessory-cancel', () => native.cancelAccessoryChoice(requestId))
          }
        }
      : {}),
    ...(options.platform === 'apple' && typeof native.authorizedAccessories === 'function'
      ? {
          authorizedAccessories: async (): Promise<string> => {
            await verifyBinary()
            const value = await call('accessory-authorized', () => native.authorizedAccessories())
            if (typeof value !== 'string') throw contractError('protocol.malformed', 'chooser', 'accessory.authorized')
            return value
          }
        }
      : {}),
    verifyNativeIdentity: async (): Promise<void> => {
      await verifyBinary()
    },
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
        // Older producers have no negotiation method and continue to speak v1.
        // The optional method must atomically select the version for this session.
        const negotiate: unknown = Reflect.get(native, 'negotiateScanEndVersion')
        let scanEndVersion = 1
        if (negotiate !== undefined) {
          if (typeof negotiate !== 'function')
            throw contractError('protocol.malformed', 'boundary', `${OPERATION}.scan-end-version`)
          const reply: unknown = await call('scan-end-version', () =>
            Promise.resolve(Reflect.apply(negotiate, native, [sessionId, '[1,2]']))
          )
          scanEndVersion = unwrap(parseScanEndVersion(reply))
        }
        return createSession(sessionId, identity, scanEndVersion)
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
    },
    // An older native module predates the standing order: the methods are
    // omitted then, and the provider fails fast instead of a silent
    // record-only or an invented empty backlog.
    ...(typeof native.declareBackgroundContinuation === 'function'
      ? {
          declareBackgroundContinuation: async (declarationJson: string): Promise<void> => {
            if (declarationJson.length === 0) {
              throw contractError('argument.invalid', 'restoration', `${OPERATION}.continuation.empty`)
            }
            await call('declare-background-continuation', () => native.declareBackgroundContinuation(declarationJson))
          }
        }
      : {}),
    ...(typeof native.onRecoveryHint === 'function'
      ? {
          onRecoveryHint: (listener: () => void): (() => void) => {
            const subscription = native.onRecoveryHint(() => listener())
            return () => subscription.remove()
          }
        }
      : {}),
    ...(typeof native.orderedRecordingOfflineContext === 'function'
      ? {
          orderedRecordingOfflineContext: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('ordered-recording-offline-context', () => native.orderedRecordingOfflineContext(id))
          }
        }
      : {}),
    ...(typeof native.orderedRecordingOfflineStatus === 'function'
      ? {
          orderedRecordingOfflineStatus: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('ordered-recording-offline-status', () => native.orderedRecordingOfflineStatus(id))
          }
        }
      : {}),
    ...(typeof native.orderedRecordingOfflineStop === 'function'
      ? {
          orderedRecordingOfflineStop: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('ordered-recording-offline-stop', () => native.orderedRecordingOfflineStop(id))
          }
        }
      : {}),
    ...(typeof native.continuationRecordingStatus === 'function'
      ? {
          continuationRecordingStatus: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('recording-status', () => native.continuationRecordingStatus(id))
          }
        }
      : {}),
    ...(typeof native.continuationRecordingPrepare === 'function'
      ? {
          continuationRecordingPrepare: async (id: string, maxItems: number, maxBytes: number): Promise<string> => {
            await verifyBinary()
            return call('recording-prepare', () => native.continuationRecordingPrepare(id, maxItems, maxBytes))
          }
        }
      : {}),
    ...(typeof native.continuationRecordingAcknowledge === 'function'
      ? {
          continuationRecordingAcknowledge: async (id: string, token: string): Promise<string> => {
            await verifyBinary()
            return call('recording-acknowledge', () => native.continuationRecordingAcknowledge(id, token))
          }
        }
      : {}),
    ...(typeof native.continuationRecordingStop === 'function'
      ? {
          continuationRecordingStop: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('recording-stop', () => native.continuationRecordingStop(id))
          }
        }
      : {}),
    ...(typeof native.continuationRecordingClear === 'function'
      ? {
          continuationRecordingClear: async (id: string): Promise<string> => {
            await verifyBinary()
            return call('recording-clear', () => native.continuationRecordingClear(id))
          }
        }
      : {}),
    ...(typeof native.notificationFailureSpoolConfigure === 'function'
      ? {
          notificationFailureSpoolConfigure: async (
            recordingId: string,
            maxBytes: string,
            maxRecords: string
          ): Promise<string> => {
            await verifyBinary()
            return call('notification-failure-spool-configure', () =>
              native.notificationFailureSpoolConfigure(recordingId, maxBytes, maxRecords)
            )
          }
        }
      : {}),
    ...(options.platform === 'android' && hasRetainedContinuationV1(native)
      ? {
          prepareRetainedContinuationClaimV1: async (items: number, bytes: number): Promise<string> => {
            await verifyBinary()
            return call('prepare-retained-continuation-v1', () =>
              native.prepareRetainedContinuationClaimV1(items, bytes)
            )
          },
          acknowledgeRetainedContinuationClaimV1: async (token: string): Promise<string> => {
            await verifyBinary()
            return call('acknowledge-retained-continuation-v1', () =>
              native.acknowledgeRetainedContinuationClaimV1(token)
            )
          },
          retireRetainedContinuationClaimReceiptV1: async (token: string): Promise<string> => {
            await verifyBinary()
            return call('retire-retained-continuation-v1', () => native.retireRetainedContinuationClaimReceiptV1(token))
          }
        }
      : {}),
    ...(hasRetainedContinuationV2(native)
      ? {
          prepareRetainedContinuationClaimV2: async (items: number, bytes: number): Promise<string> => {
            await verifyBinary()
            return call('prepare-retained-continuation-v2', () =>
              native.prepareRetainedContinuationClaimV2(items, bytes)
            )
          },
          acknowledgeRetainedContinuationClaimV2: async (token: string): Promise<string> => {
            await verifyBinary()
            return call('acknowledge-retained-continuation-v2', () =>
              native.acknowledgeRetainedContinuationClaimV2(token)
            )
          },
          readPeerFailureRecovery: async (record: string, offset: string): Promise<string> => {
            await verifyBinary()
            return call('read-peer-failure-recovery', () => native.readPeerFailureRecovery(record, offset))
          },
          acknowledgePeerFailureRecovery: async (record: string): Promise<string> => {
            await verifyBinary()
            return call('acknowledge-peer-failure-recovery', () => native.acknowledgePeerFailureRecovery(record))
          }
        }
      : {}),
    ...(hasTerminalEvidenceRecovery(native)
      ? {
          terminalEvidenceRecoveryOpenSelected: async (): Promise<string> => {
            await verifyBinary()
            return call('terminal-evidence-recovery-open', () => native.terminalEvidenceRecoveryOpenSelected())
          },
          terminalEvidenceRecoveryList: async (
            leaseId: string,
            maxItems: number,
            maxBytes: number
          ): Promise<string> => {
            await verifyBinary()
            return call('terminal-evidence-recovery-list', () =>
              native.terminalEvidenceRecoveryList(leaseId, maxItems, maxBytes)
            )
          },
          ...(typeof terminalEvidenceRecoveryListWithStatus === 'function'
            ? {
                terminalEvidenceRecoveryListWithStatus: async (
                  leaseId: string,
                  maxItems: number,
                  maxBytes: number
                ): Promise<string> => {
                  await verifyBinary()
                  return call('terminal-evidence-recovery-list-with-status', () =>
                    terminalEvidenceRecoveryListWithStatus(leaseId, maxItems, maxBytes)
                  )
                }
              }
            : {}),
          terminalEvidenceRecoveryRead: async (handle: string, offset: string, maxBytes: number): Promise<string> => {
            await verifyBinary()
            return call('terminal-evidence-recovery-read', () =>
              native.terminalEvidenceRecoveryRead(handle, offset, maxBytes)
            )
          },
          terminalEvidenceRecoveryAcknowledge: async (handle: string): Promise<string> => {
            await verifyBinary()
            return call('terminal-evidence-recovery-acknowledge', () =>
              native.terminalEvidenceRecoveryAcknowledge(handle)
            )
          }
        }
      : {}),
    ...(typeof native.prepareContinuationClaim === 'function' &&
    typeof native.acknowledgeContinuationClaim === 'function' &&
    typeof native.continuationStatus === 'function'
      ? {
          prepareContinuationClaim: async (maxItems: number, maxBytes: number): Promise<string> => {
            if (!Number.isSafeInteger(maxItems) || maxItems < 1 || !Number.isSafeInteger(maxBytes) || maxBytes < 1) {
              throw contractError('argument.invalid', 'restoration', `${OPERATION}.continuation.claim-bounds`)
            }
            return call('prepare-continuation-claim', () => native.prepareContinuationClaim(maxItems, maxBytes))
          },
          acknowledgeContinuationClaim: async (claimToken: string): Promise<string> => {
            if (claimToken.length === 0 || claimToken.length > 256) {
              throw contractError('argument.invalid', 'restoration', `${OPERATION}.continuation.claim-token`)
            }
            return call('acknowledge-continuation-claim', () => native.acknowledgeContinuationClaim(claimToken))
          },
          continuationStatus: async (): Promise<string> => {
            return call('continuation-status', () => native.continuationStatus())
          }
        }
      : {})
  })
}

/** Checks a write receipt against the requested mode (exported for the provider). */
export function checkedWriteReceipt(
  receipt: WireOpResults['gatt.write'],
  mode: 'with-response' | 'without-response'
): WireOpResults['gatt.write'] {
  return unwrap(checkWriteReceipt(receipt, mode))
}
