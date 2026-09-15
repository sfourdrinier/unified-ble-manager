// src/tck/race-bounds-cleanup-vectors.ts
//
// New race/bounds/cleanup vectors for real gaps (UBM 5.0 TCK card, Phase 2).
// Each vector runs through the public manager plus the deterministic test
// boundary only. Vectors never manufacture facts: they observe public
// outcomes (rejections, cleanups, counters, terminals) and report holds.
// No replacement DSL, no second TCK.

import type { BleCentralBackend } from '../backend-contract/backend'
import type { CleanupRecord } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import type { SerializableRecord } from '../backend-contract/primitives'
import { createAttachmentBoundIdFactory, version, versionRange } from '../backend-contract/primitives'
import type { BackendCompatibilityOffer } from '../backend-contract/primitives'
import {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} from '../manager/ble-manager'
import type { BackendTckFactory } from './contracts'
import {
  connectAndDiscover,
  notificationInput,
  operationOptions,
  rejectsWithCode,
  scanOptions,
  subscriptionOptions
} from './runner-public-scenario-support'
import type { PublicManager } from './runner-public-scenarios'

export const RACE_BOUNDS_CLEANUP_VECTOR_IDS = Object.freeze([
  'cleanup.failed-cleanup-retains-and-reports',
  'cleanup.duplicate-destroy-is-idempotent',
  'generation.stale-path-rejects-before-dispatch',
  'completion.duplicate-completion-settles-once',
  'bounds.subscription-overflow-is-bounded-and-terminal',
  'cancel.admission-completion-boundary-settles-once',
  'invalidation.service-change-invalidates-generation'
])

export type RaceBoundsCleanupVectorId = (typeof RACE_BOUNDS_CLEANUP_VECTOR_IDS)[number]

export interface RaceBoundsCleanupObservation {
  readonly vectorId: RaceBoundsCleanupVectorId
  readonly holds: boolean
  readonly detail: SerializableRecord
}

function compatibility(): BackendCompatibilityOffer {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function isVectorId(value: string): value is RaceBoundsCleanupVectorId {
  return (RACE_BOUNDS_CLEANUP_VECTOR_IDS as readonly string[]).includes(value)
}

export async function runRaceBoundsCleanupVector<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(factory: BackendTckFactory<Attachment, Identity, Backend>, vectorId: string): Promise<RaceBoundsCleanupObservation> {
  if (!isVectorId(vectorId)) {
    throw new Error(`race-bounds-cleanup vector is not registered: ${vectorId}`)
  }
  const fixture = await factory.create(
    Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
  )
  try {
    const result = await runVector(factory, fixture, vectorId)
    return result
  } finally {
    await fixture.dispose()
  }
}

async function runVector<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  _factory: BackendTckFactory<Attachment, Identity, Backend>,
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>,
  vectorId: RaceBoundsCleanupVectorId
): Promise<RaceBoundsCleanupObservation> {
  if (vectorId === 'cleanup.failed-cleanup-retains-and-reports') {
    return vectorFailedCleanupRetainsAndReports(fixture)
  }
  if (vectorId === 'cleanup.duplicate-destroy-is-idempotent') {
    return vectorDuplicateDestroyIsIdempotent(fixture)
  }
  if (vectorId === 'generation.stale-path-rejects-before-dispatch') {
    return vectorStalePathRejectsBeforeDispatch(fixture)
  }
  if (vectorId === 'completion.duplicate-completion-settles-once') {
    return vectorDuplicateCompletionSettlesOnce(fixture)
  }
  if (vectorId === 'bounds.subscription-overflow-is-bounded-and-terminal') {
    return vectorOverflowIsBoundedAndTerminal(fixture)
  }
  if (vectorId === 'cancel.admission-completion-boundary-settles-once') {
    return vectorCancelAcrossAdmissionCompletion(fixture)
  }
  return vectorServiceChangeInvalidatesGeneration(fixture)
}

async function createOwningManager<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>,
  label: string
): Promise<PublicManager<Attachment, Identity>> {
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const attachment = attached.attachment.attachment
  const ids = createAttachmentBoundIdFactory({
    attachmentId: attachment.attachmentId,
    backendInstanceId: attachment.backendInstanceId,
    backendGeneration: attachment.backendGeneration,
    adapterId: attachment.adapter.adapterId,
    adapterGeneration: attachment.adapter.adapterGeneration
  })
  const authority = createManagerOwnershipAuthority(attached)
  return createBleManager(
    {
      attachedBackend: attached,
      clientId: ids.clientId(`tck-${label}-client`),
      managerId: ids.managerId(`tck-${label}-manager`),
      ownerMode: 'owning'
    },
    authority,
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => fixture.controller.now() }
  )
}

function makeObservation(vectorId: RaceBoundsCleanupVectorId, holds: boolean, detail: SerializableRecord) {
  return Object.freeze({ vectorId, holds, detail: Object.freeze(detail) })
}

/**
 * Compares cleanup records by contract value (release state plus the ordered
 * failure identities), never by object identity: the idempotency contract is
 * "both released + admission rejected", not memoization of one record.
 */
export function cleanupRecordsDeepEqual(first: CleanupRecord, second: CleanupRecord): boolean {
  if (first.state !== second.state || first.failures.length !== second.failures.length) {
    return false
  }
  return first.failures.every((failure, index) => {
    const other = second.failures[index]
    return other !== undefined && failure.resourceKind === other.resourceKind && failure.error.code === other.error.code
  })
}

async function vectorFailedCleanupRetainsAndReports<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'cleanup.failed-cleanup-retains-and-reports' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    const characteristic = connected.snapshot.characteristics[0]
    if (characteristic === undefined) {
      throw new Error('failed-cleanup vector discovery returned no characteristic')
    }
    await fixture.controller.perform(
      'queue-operation-completion',
      Object.freeze({ stage: 'subscribe', delayMilliseconds: 10 })
    )
    const subscriptionPromise = connected.database.subscribe(
      characteristic.path,
      subscriptionOptions('drop-oldest', 4, 128)
    )
    await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 10 }))
    const subscription = await fixture.controller.settle(subscriptionPromise)
    await fixture.controller.perform('inject-unsubscribe-failure', Object.freeze({}))
    let failureReported = false
    let silentCleanRelease = false
    try {
      const cleanup = await fixture.controller.settle(subscription.remove())
      failureReported = cleanup.state !== 'released' || cleanup.failures.length > 0
      silentCleanRelease = cleanup.state === 'released' && cleanup.failures.length === 0
    } catch {
      failureReported = true
      silentCleanRelease = false
    }
    // Retained, not dropped: the failed cleanup keeps the subscription tracked
    // (still consuming its consumer slot) instead of silently releasing it.
    const retainedSubscriptionConsumers = Number(fixture.backend.resourceCounters().subscriptionConsumers)
    const retained = failureReported && retainedSubscriptionConsumers > 0
    // The injected failure is one-shot: a follow-up remove() completes the
    // release cleanly, proving the retained resource is still manageable.
    const followUp = await fixture.controller.settle(subscription.remove())
    const followUpReleased = followUp.state === 'released' && followUp.failures.length === 0
    const holds = failureReported && !silentCleanRelease && retained && followUpReleased
    await fixture.controller.settle(connected.connection.release().catch(() => ({ state: 'released', failures: [] })))
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, {
      failureReported,
      silentCleanRelease,
      retained,
      retainedSubscriptionConsumers,
      followUpReleased
    })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}

async function vectorDuplicateDestroyIsIdempotent<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'cleanup.duplicate-destroy-is-idempotent' as const
  const manager = await createOwningManager(fixture, vectorId)
  const firstPromise = manager.destroy()
  const secondPromise = manager.destroy()
  const first = await fixture.controller.settle(firstPromise)
  const second = await fixture.controller.settle(secondPromise)
  const equalCleanupRecords = cleanupRecordsDeepEqual(first, second)
  const bothReleased = first.state === 'released' && second.state === 'released'
  let admissionRejected = false
  try {
    admissionRejected = await fixture.controller.settle(
      rejectsWithCode(manager.scan(scanOptions(false)), 'lifecycle.destroyed')
    )
  } catch {
    admissionRejected = false
  }
  const holds = equalCleanupRecords && bothReleased && admissionRejected
  return makeObservation(vectorId, holds, { equalCleanupRecords, bothReleased, admissionRejected })
}

async function vectorStalePathRejectsBeforeDispatch<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'generation.stale-path-rejects-before-dispatch' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    const characteristic = connected.snapshot.characteristics[0]
    if (characteristic === undefined) {
      throw new Error('stale-path vector discovery returned no characteristic')
    }
    const currentReads = await fixture.controller.settle(connected.database.read(characteristic.path, operationOptions))
    const currentStillReads = currentReads.byteLength > 0
    await fixture.controller.perform(
      'trigger-services-changed',
      Object.freeze({ peerId: String(connected.connection.peerId) })
    )
    const before = manager
      .traces()
      .filter(entry => entry.resource === 'operation' && entry.transition === 'dispatched').length
    const staleRejected = await fixture.controller.settle(
      rejectsWithCode(connected.database.read(characteristic.path, operationOptions), 'gatt.stale-handle')
    )
    const after = manager
      .traces()
      .filter(entry => entry.resource === 'operation' && entry.transition === 'dispatched').length
    const didNotDispatch = before === after
    const holds = staleRejected && didNotDispatch && currentStillReads
    await fixture.controller.settle(connected.connection.release())
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, { staleRejected, didNotDispatch, currentStillReads })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}

async function vectorDuplicateCompletionSettlesOnce<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'completion.duplicate-completion-settles-once' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    const characteristic = connected.snapshot.characteristics[0]
    if (characteristic === undefined) {
      throw new Error('duplicate-completion vector discovery returned no characteristic')
    }
    // Two backend completions are queued for ONE read. The read is aborted after
    // dispatch, so the first completion arrives late, after the caller already
    // settled with the abort: the caller count is re-read after the late
    // delivery, so a double settlement would be observed. The abort-then-late
    // ordering must pass through quarantine exactly once (proving the late
    // path was exercised, not skipped), and the leftover completion proves
    // the channel stays healthy for the next operation.
    await fixture.controller.perform(
      'queue-operation-completion',
      Object.freeze({ stage: 'read', delayMilliseconds: 10 })
    )
    await fixture.controller.perform(
      'queue-operation-completion',
      Object.freeze({ stage: 'read', delayMilliseconds: 10 })
    )
    const cancellation = new AbortController()
    const read = connected.database.read(characteristic.path, {
      signal: cancellation.signal,
      deadline: null
    })
    let callerSettlements = 0
    const trackedSettlement = rejectsWithCode(
      read.then(
        value => {
          callerSettlements += 1
          return value
        },
        error => {
          callerSettlements += 1
          throw error
        }
      ),
      'operation.aborted'
    )
    await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 0 }))
    cancellation.abort()
    await fixture.controller.flush()
    const callerAborted = await fixture.controller.settle(trackedSettlement)
    await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 10 }))
    await fixture.controller.flush()
    // The late duplicate has now been delivered: the caller count is re-read
    // here (not just at abort time) so a double settlement would be observed.
    const settledOnce = callerSettlements === 1 && callerAborted
    const traces = manager.traces().filter(entry => entry.resource === 'operation')
    const late = traces.filter(
      entry => entry.transition === 'late-success' || entry.transition === 'late-failure'
    ).length
    const exactlyOneLateAcknowledgement = late === 1
    const noDoubleSettlement = settledOnce && exactlyOneLateAcknowledgement
    const followUp = await fixture.controller.settle(connected.database.read(characteristic.path, operationOptions))
    const followUpHealthy = followUp.byteLength > 0
    const holds = settledOnce && noDoubleSettlement && followUpHealthy
    await fixture.controller.settle(connected.connection.release())
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, {
      settledOnce,
      callerAborted,
      noDoubleSettlement,
      lateAcknowledgements: late,
      exactlyOneLateAcknowledgement,
      followUpHealthy
    })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}

async function vectorOverflowIsBoundedAndTerminal<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'bounds.subscription-overflow-is-bounded-and-terminal' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    const characteristic = connected.snapshot.characteristics[0]
    if (characteristic === undefined) {
      throw new Error('overflow vector discovery returned no characteristic')
    }
    // Post-R12 limits: the byte budget must exceed the 64-byte control reserve
    // (frozen validateStreamLimits fails closed with stream.quota otherwise),
    // so the data pool is 128 - 64 while itemCapacity 1 still forces the item
    // overflow. The terminal drop counts are asserted exactly (R-TCK2).
    const probe = await fixture.controller.settle(
      connected.database.subscribe(characteristic.path, subscriptionOptions('error', 1, 128))
    )
    await fixture.controller.perform('emit-notification', notificationInput(characteristic.path, new Uint8Array([4])))
    await fixture.controller.perform('emit-notification', notificationInput(characteristic.path, new Uint8Array([5])))
    await fixture.controller.flush()
    const iterator = probe.values[Symbol.asyncIterator]()
    const terminal = await fixture.controller.settle(iterator.next())
    const complete = await fixture.controller.settle(iterator.next())
    const exactTerminal =
      !terminal.done &&
      terminal.value.kind === 'terminal' &&
      terminal.value.reason === 'overflow' &&
      Number(terminal.value.droppedItems) === 1 &&
      Number(terminal.value.droppedBytes) === 1 &&
      Number(terminal.value.replacedItems) === 0
    const oneTerminal = complete.done === true
    const cleanup = await fixture.controller.settle(probe.remove())
    const cleanupReleased = cleanup.state === 'released' && cleanup.failures.length === 0
    await fixture.controller.perform('emit-notification', notificationInput(characteristic.path, new Uint8Array([6])))
    await fixture.controller.flush()
    const noLateValue = (await fixture.controller.settle(iterator.next())).done === true
    const holds = exactTerminal && oneTerminal && cleanupReleased && noLateValue
    await fixture.controller.settle(connected.connection.release())
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, { exactTerminal, oneTerminal, cleanupReleased, noLateValue })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}

async function vectorCancelAcrossAdmissionCompletion<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'cancel.admission-completion-boundary-settles-once' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    const characteristic = connected.snapshot.characteristics[0]
    if (characteristic === undefined) {
      throw new Error('cancel-boundary vector discovery returned no characteristic')
    }
    await fixture.controller.perform(
      'queue-operation-completion',
      Object.freeze({ stage: 'write', delayMilliseconds: 10 })
    )
    const cancellation = new AbortController()
    const write = connected.database.write(characteristic.path, new Uint8Array([91]), {
      signal: cancellation.signal,
      deadline: null,
      mode: 'with-response'
    })
    let callerSettlements = 0
    const settlement = rejectsWithCode(
      write.then(
        value => {
          callerSettlements += 1
          return value
        },
        error => {
          callerSettlements += 1
          throw error
        }
      ),
      'operation.aborted'
    )
    await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 0 }))
    cancellation.abort()
    await fixture.controller.flush()
    const callerCancelled = await fixture.controller.settle(settlement)
    await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 10 }))
    await fixture.controller.flush()
    // Re-read after the late completion is delivered: a double settlement
    // would increment the counter here, and the late acknowledgement must be
    // recorded exactly once rather than lost.
    const settledOnce = callerSettlements === 1 && callerCancelled
    const late = manager
      .traces()
      .filter(entry => entry.resource === 'operation')
      .filter(entry => entry.transition === 'late-success' || entry.transition === 'late-failure').length
    const lateAcknowledgedOnce = late === 1
    const persisted = await fixture.controller.settle(connected.database.read(characteristic.path, operationOptions))
    const completionBoundaryClean = persisted.byteLength > 0
    const holds = settledOnce && lateAcknowledgedOnce && completionBoundaryClean
    await fixture.controller.settle(connected.connection.release())
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, {
      settledOnce,
      callerCancelled,
      lateAcknowledgements: late,
      lateAcknowledgedOnce,
      completionBoundaryClean
    })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}

async function vectorServiceChangeInvalidatesGeneration<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: import('./contracts').BackendTckFixture<Attachment, Identity, Backend>
): Promise<RaceBoundsCleanupObservation> {
  const vectorId = 'invalidation.service-change-invalidates-generation' as const
  const manager = await createOwningManager(fixture, vectorId)
  try {
    const connected = await connectAndDiscover(manager, fixture, {
      id: 'scenario.scan-connect-discover-read-notify-destroy',
      execution: 'base',
      requiredFacts: [],
      requiredControllerActions: []
    })
    await fixture.controller.perform(
      'trigger-services-changed',
      Object.freeze({ peerId: String(connected.connection.peerId) })
    )
    const snapshotInvalidated = await fixture.controller.settle(
      rejectsWithCode(connected.database.snapshot(), 'gatt.stale-handle')
    )
    const rediscovered = await fixture.controller.settle(connected.connection.discover(operationOptions))
    const rediscoverySnapshot = await rediscovered.snapshot()
    const newGenerationValue = await fixture.controller.settle(
      rediscovered.read(
        rediscoverySnapshot.characteristics[0]?.path ??
          connected.snapshot.characteristics[0]?.path ??
          (() => {
            throw new Error('no characteristic')
          })(),
        operationOptions
      )
    )
    // Strict: a zero-length read must not satisfy the new-generation check.
    const newGenerationByteLength = newGenerationValue.byteLength
    const newGenerationReads = rediscoverySnapshot.characteristics.length > 0 && newGenerationByteLength > 0
    const holds = snapshotInvalidated && newGenerationReads
    await fixture.controller.settle(connected.connection.release())
    await fixture.controller.settle(manager.destroy())
    return makeObservation(vectorId, holds, { snapshotInvalidated, newGenerationReads, newGenerationByteLength })
  } catch (error) {
    await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
    throw error
  }
}
