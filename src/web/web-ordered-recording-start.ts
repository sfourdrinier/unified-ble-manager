import {
  webRecordingContractError as contractError,
  trustedWebRecordingError,
  webStorageError,
  webHostErrorEvidence
} from './web-recording-errors'
import {
  isRecordingId,
  MAX_RECORDING_BYTES,
  MAX_RECORDING_RECORDS,
  MIN_RECORDING_BYTES,
  MIN_RECORDING_RECORDS
} from '../backend-contract/continuation-recording-bounds'
import type { GattDatabase } from '../backend-contract/gatt'
import { parseOrderedRecordingStartResult, type OrderedRecordingStartResult } from '../core/ordered-recording-start'
import type { OrderedRecordingHostStopResult } from '../core/continuation-recording'
import type { OrderedRecordingFailure } from '../core/ordered-recording'
import { createWebAuthorityToken } from './web-authority-reservation'
import { createWebAuthorityRecordingJournalBridge } from './web-authority-recording-journal-bridge'
import type { WebBluetoothBackend } from './web-bluetooth-backend'
import type { WebGattDatabase } from './web-bluetooth-handles'
import { WebOrderedRecordingSupervisor } from './web-ordered-recording-supervisor'
import type { WebRecordingSupervisorStatus } from './web-ordered-recording-supervisor'
import type { WebRecordingAuthorityOptions } from './web-recording-authority'
import type {
  WebRecordingLiveIdentity,
  WebRecordingLiveOwner,
  WebRecordingLiveOwners
} from './web-recording-live-owners'
import {
  createWebRecordingMarkers,
  registerWebRecordingMarkers,
  unregisterWebRecordingMarkers
} from './web-recording-markers'
import { WebRecordingMemberCleanupError } from './web-bluetooth-gatt'
import { webCleanupFailure } from './web-bluetooth-errors'
import { webRecordingOriginalFailure, webRecordingReconciliationFailure } from './web-recording-journal-bridge'
import { createWebRecordingPeerIdentityStore } from './web-recording-peer-identity'
import { WebRecordingReadinessError, webRecordingReadinessFailure } from './web-indexeddb-transaction'
import { WebIngressRegistrationCleanupError } from './web-notification-ingress'

export interface WebRecordingStartDependencies extends WebRecordingAuthorityOptions {
  /** Aggregate retained quota for opted-in source-native terminal evidence. */
  readonly terminalEvidenceMaximumDurableBytes?: number
  /** SHA-256 of exact terminal-evidence bytes, supplied by this storage partition. */
  readonly terminalEvidenceDigest?: (value: Readonly<Uint8Array>) => Promise<string>
  readonly crypto: { getRandomValues(bytes: Uint8Array): Uint8Array }
  readonly liveOwners: WebRecordingLiveOwners
  readonly forRecording?: () => WebRecordingStartDependencies
  readonly preparePartition?: (signal?: AbortSignal) => Promise<void>
}

type Handle = ReturnType<NonNullable<GattDatabase<string, string, string>['reserveOrderedRecordingStart']>>
let nextCollection = 1n
const invalid = () => contractError('lifecycle.invalid-state', 'restoration', 'web-recording.start')
const sourceFailure = (error: unknown): OrderedRecordingFailure => {
  error = webRecordingOriginalFailure(error)
  const safe =
    webRecordingReadinessFailure(error) ??
    trustedWebRecordingError(error) ??
    webStorageError(error, 'indexeddb-transaction')
  const normalized = safe.normalized
  const platform = normalized?.platform
  const metadata: Record<string, import('../core/ordered-recording').OrderedRecordingFailureValue> = {}
  for (const [key, value] of Object.entries(platform?.metadata ?? {})) {
    if (key === '__proto__' || key === 'constructor' || key === 'prototype') {
      if (platform?.metadataEntries !== undefined) continue
      throw contractError('protocol.malformed', 'boundary', 'web-recording.failure-metadata')
    }
    if (typeof value === 'string') metadata[key] = { kind: 'text', value }
    else if (typeof value === 'boolean') metadata[key] = { kind: 'bool', value }
    else if (typeof value === 'number' && Number.isSafeInteger(value))
      metadata[key] = { kind: 'int', value: String(value) }
    else metadata[key] = { kind: 'text', value: JSON.stringify(value) }
  }
  return {
    code: normalized?.code ?? 'platform.failure',
    domain: normalized?.domain ?? 'gatt',
    operation: normalized?.operation ?? 'web-recording.start',
    detail: safe.message,
    platformDomain: normalized?.platform?.domain ?? null,
    platformCode: normalized?.platform?.code ?? null,
    platformMessage: normalized?.platform?.safeMessage ?? null,
    platformMetadata: metadata,
    ...(platform?.metadataEntries === undefined ? {} : { platformMetadataEntries: platform.metadataEntries })
  }
}
const stopFailure = { kind: 'worker-failed' } as const

function runtimeFailure(
  error: unknown,
  phase: WebRecordingSupervisorStatus['runtimeFailurePhase']
): OrderedRecordingFailure {
  const seen = new Set<unknown>()
  let remaining = 32
  const find = (value: unknown, depth: number): OrderedRecordingFailure | undefined => {
    if (depth >= 8 || remaining-- <= 0 || seen.has(value)) return undefined
    seen.add(value)
    const original = webRecordingOriginalFailure(value)
    if (original !== value) return find(original, depth + 1)
    if (webRecordingReadinessFailure(value) !== undefined || trustedWebRecordingError(value) !== undefined)
      return sourceFailure(value)
    try {
      if (value instanceof WebRecordingMemberCleanupError) {
        const failure = value.cleanup.failures[0]?.error
        if (failure !== undefined) return sourceFailure(contractError(failure.code, failure.domain, failure.operation))
      }
      if (value instanceof WebIngressRegistrationCleanupError) {
        // The first entry is the installation failure; subsequent entries are
        // rollback failures. Preserve that distinction at every nested level.
        const relevant = phase === 'cleanup' ? value.latestCleanupErrors.slice(0, 16) : value.errors.slice(0, 1)
        for (const child of relevant) {
          const found = find(child, depth + 1)
          if (found !== undefined) return found
        }
        return undefined
      }
      if (value instanceof AggregateError) {
        for (const child of value.errors.slice(0, 16)) {
          const found = find(child, depth + 1)
          if (found !== undefined) return found
        }
      }
    } catch {
      /* Untrusted thrown values are redacted below. */
    }
    return undefined
  }
  return (
    find(error, 0) ??
    (phase === 'cleanup'
      ? sourceFailure(contractError('platform.failure', 'cleanup', 'web-recording.cleanup'))
      : phase === 'setup'
        ? sourceFailure(contractError('platform.failure', 'gatt', 'web-recording.setup'))
        : sourceFailure(error))
  )
}

export function createWebOrderedRecordingStart(
  backend: WebBluetoothBackend,
  database: WebGattDatabase,
  baseDependencies: WebRecordingStartDependencies,
  now: () => number
): Handle {
  const dependencies = baseDependencies.forRecording?.() ?? baseDependencies
  const preparePartition = dependencies.preparePartition
  const recordingDatabase =
    preparePartition === undefined && dependencies.database.ready === undefined
      ? dependencies.database
      : {
          ready: async () => {
            const cancellation = new AbortController()
            let timer: ReturnType<typeof setTimeout> | undefined
            try {
              await Promise.race([
                preparePartition === undefined
                  ? (dependencies.database.ready?.() ?? Promise.resolve())
                  : preparePartition.call(dependencies, cancellation.signal),
                new Promise<never>((_resolve, reject) => {
                  timer = setTimeout(() => {
                    cancellation.abort()
                    reject(contractError('operation.timed-out', 'restoration', 'web-recording.indexeddb-open'))
                  }, 5000)
                })
              ])
            } catch (error) {
              throw new WebRecordingReadinessError(error)
            } finally {
              clearTimeout(timer)
            }
          },
          transaction: dependencies.database.transaction.bind(dependencies.database)
        }
  let peerAuthorityCurrent = (): boolean => false
  const peerDatabase = {
    ...recordingDatabase,
    ready: async () => {
      if (!peerAuthorityCurrent()) throw invalid()
      await recordingDatabase.ready?.()
      if (!peerAuthorityCurrent()) throw invalid()
    },
    transaction: (...args: Parameters<typeof recordingDatabase.transaction>) => {
      if (!peerAuthorityCurrent()) throw invalid()
      return recordingDatabase.transaction(...args)
    }
  }
  let phase: 'pending' | 'unknown' | 'failed' | 'retired' | 'result' = 'pending'
  let result: OrderedRecordingStartResult | null = null
  let dispatched = false
  let supervisor: WebOrderedRecordingSupervisor | null = null
  let operation: Promise<OrderedRecordingStartResult> | null = null
  let markers: ReturnType<typeof createWebRecordingMarkers> | null = null
  let identity: WebRecordingLiveIdentity | null = null
  let markersRegistered = false
  return {
    async start(members, options) {
      if (dispatched || phase === 'retired') throw invalid()
      if (options.signal?.aborted) throw contractError('operation.aborted', 'gatt', 'web-recording.start')
      if (options.deadline !== null && now() >= options.deadline)
        throw contractError('operation.timed-out', 'gatt', 'web-recording.start')
      const config = options.recording
      if (
        !isRecordingId(config.id) ||
        !Number.isSafeInteger(config.maxBytes) ||
        config.maxBytes < MIN_RECORDING_BYTES ||
        config.maxBytes > MAX_RECORDING_BYTES ||
        !Number.isSafeInteger(config.maxRecords) ||
        config.maxRecords < MIN_RECORDING_RECORDS ||
        config.maxRecords > MAX_RECORDING_RECORDS
      )
        throw contractError('argument.invalid', 'restoration', 'web-recording.start')
      const peer = backend.peerReferenceFor(database.record.peerId)
      if (peer === null) throw invalid()
      // The supervisor, not an individual waiter's signal, owns dispatched leases.
      const owner = backend.createRecordingSourceOwner(
        database,
        members,
        { ...options, signal: null, deadline: null },
        2
      )
      const collectionId = String(nextCollection++)
      const generation = createWebAuthorityToken(dependencies.crypto)
      const declaration = {
        ...config,
        collectionId,
        sourceGeneration: generation,
        context: {
          kind: 'ordered-recording' as const,
          version: owner.recordingVersion,
          peerId: peer.browserDeviceId,
          connectionGeneration: String(database.path.connectionGeneration),
          databaseGeneration: String(database.path.databaseGeneration),
          maxItems: Number(options.delivery.itemCapacity),
          maxBytes: Number(options.delivery.byteCapacity),
          maxControls: Number(options.delivery.reservedControlCapacity),
          members: members.map((member, index) => ({
            memberId: String(index),
            publicMemberId: member.memberId,
            selector: {
              serviceUuid: String(member.path.serviceUuid),
              serviceOccurrence: String(member.path.serviceOccurrence),
              characteristicUuid: String(member.path.characteristicUuid),
              characteristicOccurrence: String(member.path.characteristicOccurrence)
            },
            delivery: null,
            preference: member.deliveryMode.endsWith('indication') ? ('indication' as const) : ('notification' as const)
          }))
        }
      }
      const mapStop = (state: WebRecordingSupervisorStatus, forObservation = false): OrderedRecordingHostStopResult => {
        // The observation is written before authority release and is only
        // exposed as final after the separate release receipt is established.
        const released =
          state.authorityReleased ||
          (forObservation &&
            state.sourceReleased &&
            state.members.every(member => member.cleanup === 'not-needed' || member.cleanup === 'released'))
        const complete = state.terminalCommitted && state.sealed
        const failure = state.failure
        const previousReconciliation = webRecordingReconciliationFailure(failure)
        const reconciliationFailure =
          state.storageOutcomeUnknown && state.storageUncertaintyEvidence.length > 1
            ? {
                ...webHostErrorEvidence(state.storageUncertaintyEvidence),
                ...(previousReconciliation === null ? {} : { previousReconciliation })
              }
            : (previousReconciliation ??
              (state.storageOutcomeUnknown ? webHostErrorEvidence(state.storageUncertaintyEvidence) : null))
        const setupFailure =
          state.setupFailure === null ? null : runtimeFailure(state.setupFailure, state.setupFailurePhase)
        const sourceFailed =
          state.terminalReason === 'source-failed' ||
          state.terminalReason === 'host-reset' ||
          state.terminalReason === 'overflow' ||
          state.terminalReason === 'ordinal-exhausted' ||
          state.terminalReason === 'operation-aborted' ||
          state.terminalReason === 'operation-timed-out'
        const terminalFailure = () => {
          const reason = state.terminalReason
          const code =
            reason === 'overflow'
              ? 'stream.overflow'
              : reason === 'ordinal-exhausted'
                ? 'stream.closed'
                : reason === 'host-reset'
                  ? 'backend.reset'
                  : reason === 'operation-aborted'
                    ? 'operation.aborted'
                    : reason === 'operation-timed-out'
                      ? 'operation.timed-out'
                      : 'platform.failure'
          return sourceFailure(contractError(code, 'stream', `web-recording.${reason ?? 'source-failed'}`))
        }
        return {
          recording:
            state.state === 'waiting-capacity'
              ? { version: owner.recordingVersion, state: 'waiting-capacity', error: null }
              : state.storageOutcomeUnknown || failure !== null || (complete && state.workerStoppedSource)
                ? {
                    version: owner.recordingVersion,
                    state: 'failed',
                    error:
                      reconciliationFailure !== null
                        ? {
                            kind: 'storage',
                            storage: {
                              kind: 'reconciliation-unknown',
                              detail: JSON.stringify({
                                ...reconciliationFailure,
                                ...(setupFailure === null
                                  ? {}
                                  : {
                                      setupFailure: {
                                        code: setupFailure.code,
                                        domain: setupFailure.domain,
                                        operation: setupFailure.operation
                                      }
                                    })
                              }),
                              operation: 'web-recording.reconcile',
                              sqliteExtendedCode: null,
                              sqliteCode: null
                            }
                          }
                        : setupFailure !== null
                          ? { kind: 'source', source: setupFailure }
                          : state.observationFailure && failure !== null
                            ? {
                                kind: 'storage',
                                storage: {
                                  kind: 'indexeddb-transaction',
                                  detail: JSON.stringify(webHostErrorEvidence([failure])),
                                  operation: 'web-recording.terminal-observation',
                                  sqliteExtendedCode: null,
                                  sqliteCode: null
                                }
                              }
                            : stopFailure
                  }
                : complete && sourceFailed
                  ? {
                      version: owner.recordingVersion,
                      state: 'failed',
                      error: { kind: 'source', source: terminalFailure() }
                    }
                  : complete
                    ? {
                        version: owner.recordingVersion,
                        state: state.terminalReason === 'closed' ? 'finished' : 'interrupted',
                        error: null
                      }
                    : {
                        version: owner.recordingVersion,
                        state:
                          state.state === 'waiting-source' || state.state === 'preparing'
                            ? 'waiting-source'
                            : 'waiting-drain',
                        error: null
                      },
          hostCleanup:
            state.cleanupFailure instanceof WebRecordingMemberCleanupError
              ? state.cleanupFailure.cleanup
              : state.cleanupFailure !== null
                ? webCleanupFailure('recording', 'web-recording.cleanup')
                : released
                  ? { state: 'released', failures: [] }
                  : { state: 'pending' }
        }
      }
      const live: WebRecordingLiveOwner = {
        recordingVersion: owner.recordingVersion,
        get supervisor() {
          if (supervisor === null) throw invalid()
          return supervisor
        },
        async status() {
          // The journal does not retain a cumulative loss count after reader
          // acknowledgement. Do not manufacture a complete status from a row.
          throw contractError('capability.unavailable', 'restoration', 'web-recording.live-status')
        },
        runtimeDiagnostic() {
          if (supervisor === null) throw invalid()
          const state = supervisor.status()
          const failure = state.runtimeFailure
          if (failure === null) return { state: state.state, lastFailure: null }
          const safe = runtimeFailure(failure.error, state.runtimeFailurePhase)
          return {
            state: state.state,
            lastFailure: {
              persisted: false,
              error: {
                code: safe.code,
                domain: safe.domain,
                operation: safe.operation,
                platformCode: safe.platformCode
              }
            }
          }
        },
        stopResult: () => {
          if (supervisor === null) throw invalid()
          return mapStop(supervisor.status())
        },
        async stopOrdered() {
          if (supervisor === null) throw invalid()
          supervisor.stop()
          return mapStop(await supervisor.retry())
        },
        async retryFailedCleanup() {
          if (supervisor === null) throw invalid()
          return mapStop(await supervisor.retryFailedCleanup())
        },
        async spaceAvailable() {
          if (supervisor === null) throw invalid()
          await supervisor.retry(false)
        },
        terminalRetained: observed => bridge.terminalRetained(observed),
        retirementFailed: error => supervisor?.noteRetirementFailure(error),
        unboundDeclarationRemoved: () => bridge.unboundDeclarationRemoved(),
        hasPendingMarkerReceipt: () => markers?.hasActiveReceipt() === true,
        dispose: () => {
          if (markers !== null && markersRegistered) unregisterWebRecordingMarkers(database, config.id, markers)
        }
      }
      const bridge = createWebAuthorityRecordingJournalBridge(
        {
          database: recordingDatabase,
          locks: dependencies.locks,
          workerUrl: dependencies.workerUrl,
          crypto: dependencies.crypto,
          digest: dependencies.digest,
          onLoss: dependencies.onLoss,
          signal: dependencies.signal
        },
        declaration,
        dependencies.crypto,
        {
          store: createWebRecordingPeerIdentityStore(peerDatabase),
          reference: { version: 1, scope: 'origin', backendId: peer.backendId, opaqueId: peer.browserDeviceId }
        },
        () => {
          if (markers === null) throw invalid()
          registerWebRecordingMarkers(database, config.id, markers)
          markersRegistered = true
        },
        declared => {
          dependencies.liveOwners.install(declared, live)
          identity = declared
        }
      )
      peerAuthorityCurrent = () => bridge.canAcknowledge()
      markers = createWebRecordingMarkers(
        database,
        owner,
        () => bridge.canAcknowledge(),
        collectionId,
        generation,
        now,
        () => bridge.markerLedger(),
        () => createWebAuthorityToken(dependencies.crypto),
        async () => {
          if (identity !== null) await dependencies.liveOwners.retireIfEligible(identity, live)
        }
      )
      const journal =
        dependencies.preparePartition === undefined
          ? bridge
          : {
              ...bridge,
              prepare: async (signal?: AbortSignal) => {
                await dependencies.preparePartition?.(signal)
                await bridge.prepare(signal)
              }
            }
      supervisor = new WebOrderedRecordingSupervisor(
        owner,
        journal,
        async () => {
          if (supervisor === null) throw invalid()
          return { result: mapStop(supervisor.status(), true) }
        },
        async () => {
          if (identity !== null) await dependencies.liveOwners.retireIfEligible(identity, live)
        }
      )
      owner.onMarkerEvidenceProgress = () => supervisor?.markerEvidenceProgress()
      dispatched = true
      const current = supervisor
      const mapStart = (state: WebRecordingSupervisorStatus): OrderedRecordingStartResult => {
        if (state.sourceStarted === false)
          return {
            kind: 'prepare-failed',
            leaseCleanup:
              state.sourceReleased && state.authorityReleased
                ? { state: 'released', failures: [] }
                : state.cleanupFailure !== null
                  ? webCleanupFailure('recording', 'web-recording.cleanup')
                  : { state: 'pending' },
            receipt: {
              version: owner.recordingVersion,
              state: 'prepare-failed',
              recordingId: config.id,
              sourceStarted: false,
              journalEffect: 'unknown',
              error:
                state.setupFailure === null
                  ? stopFailure
                  : { kind: 'source', source: runtimeFailure(state.setupFailure, state.setupFailurePhase) }
            }
          }
        return {
          kind: 'started',
          receipt: {
            version: owner.recordingVersion,
            recordingId: config.id,
            collectionId,
            state: state.state === 'active' ? 'active' : 'setup-failed',
            sourceGeneration: { kind: 'opaque', value: generation },
            members: members.map((_, index) => {
              const member = state.members[index]
              return {
                memberId: String(index),
                acquisition:
                  member?.acquisition === 'acquired'
                    ? 'acquired'
                    : member?.acquisition === 'enabling' || member?.acquisition === 'failed'
                      ? 'indeterminate'
                      : 'not-started',
                delivery: 'unknown',
                cleanupReleased: member?.cleanup === 'released' || member?.cleanup === 'not-needed',
                cleanupError: member?.cleanupFailure ? runtimeFailure(member.cleanupFailure, 'cleanup') : null
              }
            }),
            setupFailure:
              state.setupFailure === null ? null : runtimeFailure(state.setupFailure, state.setupFailurePhase),
            fenceFailure: null,
            error: null,
            inspectionError: null,
            driverError: null
          }
        }
      }
      const startCancellation = new AbortController()
      const propagateAbort = () => startCancellation.abort()
      options.signal?.addEventListener('abort', propagateAbort, { once: true })
      if (options.signal?.aborted) startCancellation.abort()
      const startWaiter = current.start(startCancellation.signal)
      operation = startWaiter
        .catch(async error => {
          if (startCancellation.signal.aborted) return current.start()
          throw error
        })
        .then(
          state => {
            result = parseOrderedRecordingStartResult(
              mapStart(state),
              config.id,
              members.length,
              owner.recordingVersion
            )
            phase = 'result'
            return result
          },
          error => {
            phase = 'failed'
            throw error
          }
        )
        .finally(() => options.signal?.removeEventListener('abort', propagateAbort))
      operation.catch(() => undefined)
      const remaining = options.deadline === null ? null : Math.max(0, Number(options.deadline) - now())
      const limit = remaining === null ? 5000 : Math.min(5000, remaining)
      let timer: ReturnType<typeof setTimeout> | undefined
      try {
        return await Promise.race([
          operation,
          new Promise<never>((_resolve, reject) => {
            timer = setTimeout(() => {
              phase = 'unknown'
              // An internal bound only loses the reply. A caller's deadline,
              // like its signal, may cancel pre-admission ownership.
              if (remaining !== null && remaining <= 5000) startCancellation.abort()
              reject(contractError('operation.timed-out', 'gatt', 'web-recording.start'))
            }, limit)
          }),
          new Promise<never>((_resolve, reject) => {
            if (options.signal === null) return
            const signal = options.signal
            const abort = () => reject(contractError('operation.aborted', 'gatt', 'web-recording.start'))
            if (signal.aborted) abort()
            else {
              signal.addEventListener('abort', abort, { once: true })
              operation?.finally(() => signal.removeEventListener('abort', abort)).catch(() => undefined)
            }
          })
        ])
      } finally {
        if (timer !== undefined) clearTimeout(timer)
      }
    },
    reserveMarkerWrite() {
      if (markers === null || !markersRegistered) throw invalid()
      return markers.reserveMarkerWrite()
    },
    async status() {
      return phase === 'result' && result !== null
        ? { state: 'result', result }
        : { state: phase === 'result' ? 'failed' : phase }
    },
    async retire() {
      if (operation !== null && (phase === 'pending' || phase === 'unknown')) throw invalid()
      phase = 'retired'
      result = null
    }
  }
}
