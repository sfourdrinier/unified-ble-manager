// src/core/subscription-registry.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import {
  isRecordingId,
  MIN_RECORDING_BYTES,
  MAX_RECORDING_BYTES,
  MIN_RECORDING_RECORDS,
  MAX_RECORDING_RECORDS
} from '../backend-contract/continuation-recording-bounds'
import { ownBytes } from '../backend-contract/primitives'
import type { BackendSubscription, BleCentralBackend } from '../backend-contract/backend'
import type { CleanupFailure, CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { CharacteristicPath, ConnectionPath, DatabasePath, NotificationValue } from '../backend-contract/gatt'
import type { BackendIdentity } from '../backend-contract/identity'
import type {
  PublicOperationOptions,
  RecordedSubscriptionOptions,
  SubscriptionOptions
} from '../backend-contract/operations'
import type { AttachmentBoundIdFactory, AttachmentId, ByteLimit, SubscriptionId } from '../backend-contract/primitives'
import type { BoundedAsyncStream, StreamItem } from '../backend-contract/streams'
import { AggregateStreamQuota } from './aggregate-stream-quota'
import { CoreBoundedStream } from './bounded-stream'
import type { TerminalEvidenceReservation } from '../backend-contract/terminal-evidence'
import type { TerminalEvidenceCommit } from '../backend-contract/terminal-evidence'
import { hydrateTerminalEvidence } from './terminal-evidence-codec'
import { assertSuccessfulOperationTerminal, awaitWithOperationAdmission } from './unified-ble-core-helpers'
import { CoreOperationCoordinator, type CoreOperationDispatch, type CoreOperationResult } from './operation-coordinator'
import { ResourceLedger } from './resource-ledger'
import { CoreTraceRecorder } from './trace-recorder'
import {
  characteristicPathKey,
  characteristicPathsEqual,
  connectionPathsEqual,
  databasePathsEqual
} from './gatt-path-equality'

type CurrentCharacteristicPath<Attachment extends string> = CharacteristicPath<
  Attachment,
  string,
  string,
  string,
  string,
  'current'
>

interface PhysicalSubscription<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  readonly key: string
  readonly path: CurrentCharacteristicPath<Attachment>
  readonly queueKey: string
  readonly recording: RecordedSubscriptionOptions['recording'] | null
  readonly incarnation: string
  readonly consumers: Set<CoreSubscription<Attachment, Identity>>
  backend: BackendSubscription<Attachment, string, string, string, string> | null
  ready: Promise<void> | null
  closed: boolean
  released: boolean
  releaseInFlight: Promise<CleanupRecord> | null
  releasedDuringEnable: boolean
  backendInvalidated: boolean
  lateSubscriptionPending: boolean
  pump: Promise<void> | null
  terminalEvidence: TerminalEvidenceReservation | null
  terminalEvidenceScope: {
    readonly managerId: string
    readonly operationId: string
    readonly incarnation: string
  } | null
  readonly terminalEvidenceReaders: Set<CoreBoundedStream<NotificationValue>>
  terminalEvidenceReleaseInFlight: Promise<CleanupRecord> | null
  terminalEvidenceReleaseFailure: NormalizedBleError | null
}

export interface SubscriptionRegistryRuntime<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  readonly backend: BleCentralBackend<Attachment, Identity>
  readonly attachmentId: AttachmentId<Attachment>
  readonly idFactory: AttachmentBoundIdFactory<Attachment>
  readonly operationCoordinator: CoreOperationCoordinator<Attachment>
  readonly aggregateQuota: AggregateStreamQuota
  readonly resourceLedger: ResourceLedger
  readonly trace: CoreTraceRecorder
  readonly now: () => number
  readonly maximumValueBytes: ByteLimit
  readonly terminalEvidence?: { readonly maximumCanonicalBytes: number }
  readonly managerId: string
  isPathCurrent(path: CurrentCharacteristicPath<Attachment>): boolean
}

/** Public subscription handle with a consumer-owned bounded value stream. */
export class CoreSubscription<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  private removal: Promise<CleanupRecord> | null = null
  private state: 'enabling' | 'ready' | 'removed' | 'invalid' = 'enabling'
  observedDelivery: NotificationValue['delivery'] = 'unknown'

  constructor(
    readonly subscriptionId: SubscriptionId<Attachment, string, string, string, string, string>,
    readonly path: CurrentCharacteristicPath<Attachment>,
    readonly values: BoundedAsyncStream<NotificationValue>,
    private readonly removeSubscription: (
      subscription: CoreSubscription<Attachment, Identity>
    ) => Promise<CleanupRecord>
  ) {}

  isReady(): boolean {
    return this.state === 'ready'
  }

  isActive(): boolean {
    return this.state === 'enabling' || this.state === 'ready'
  }

  markReady(): void {
    if (this.state === 'enabling') {
      this.state = 'ready'
    }
  }

  invalidate(
    reason: 'connection-lost' | 'owner-released' | 'source-failed' | 'service-changed',
    evidence?: TerminalEvidenceCommit
  ): void {
    if (!this.isActive()) {
      return
    }
    this.state = 'invalid'
    if (this.values instanceof CoreBoundedStream) {
      this.values.closeWithReason(reason, evidence === undefined ? null : undefined, evidence)
    }
  }

  remove(): Promise<CleanupRecord> {
    if (this.removal === null) {
      const removal = this.removeSubscription(this)
      this.removal = removal
      removal.then(
        result => {
          if (result.state === 'release-failed') {
            this.removal = null
          }
        },
        () => {
          this.removal = null
        }
      )
    }
    return this.removal
  }

  markRemoved(): void {
    this.state = 'removed'
  }
}

/**
 * Owns one physical CCCD enablement for each exact current path and requested
 * delivery mode. Each
 * consumer gets its own bounded stream; removing one never disables another.
 */
export class SubscriptionRegistry<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  private readonly physicalByPath = new Map<string, PhysicalSubscription<Attachment, Identity>>()
  private nextSubscription = 1
  private nextLateCompensation = 1

  constructor(private readonly runtime: SubscriptionRegistryRuntime<Attachment, Identity>) {}

  async subscribe(
    path: CurrentCharacteristicPath<Attachment>,
    options: SubscriptionOptions,
    queueKey: string
  ): Promise<CoreSubscription<Attachment, Identity>> {
    return this.subscribeOwned(path, options, queueKey, null)
  }

  async subscribeRecorded(
    path: CurrentCharacteristicPath<Attachment>,
    options: RecordedSubscriptionOptions,
    queueKey: string
  ): Promise<import('../backend-contract/gatt').RecordedSubscription<Attachment>> {
    const recording = options.recording
    if (
      typeof recording !== 'object' ||
      recording === null ||
      !isRecordingId(recording.id) ||
      !Number.isSafeInteger(recording.maxBytes) ||
      recording.maxBytes < MIN_RECORDING_BYTES ||
      recording.maxBytes > MAX_RECORDING_BYTES ||
      !Number.isSafeInteger(recording.maxRecords) ||
      recording.maxRecords < MIN_RECORDING_RECORDS ||
      recording.maxRecords > MAX_RECORDING_RECORDS
    ) {
      throw contractError('argument.invalid', 'gatt', 'subscription-registry.recording')
    }
    if (this.runtime.backend.gatt.subscribeRecorded === undefined) {
      throw contractError('capability.unsupported', 'gatt', 'subscription-registry.recorded')
    }
    const key = `recorded:${this.nextSubscription}`
    const subscription = await this.subscribeOwned(path, options, queueKey, options.recording)
    const changed = this.physicalByPath.get(key)?.backend?.changed
    if (changed === undefined) {
      await subscription.remove()
      throw contractError('protocol.violation', 'gatt', 'subscription-registry.recorded-changed')
    }
    return Object.freeze({
      subscriptionId: subscription.subscriptionId,
      path: subscription.path,
      recordingId: options.recording.id,
      changed,
      observedDelivery: subscription.observedDelivery,
      remove: () => subscription.remove()
    })
  }

  private async subscribeOwned(
    path: CurrentCharacteristicPath<Attachment>,
    options: SubscriptionOptions,
    queueKey: string,
    recording: RecordedSubscriptionOptions['recording'] | null
  ): Promise<CoreSubscription<Attachment, Identity>> {
    this.assertCurrent(path)
    this.assertAdmission(options)
    const incarnation = String(this.nextSubscription)
    const key =
      recording === null ? physicalSubscriptionKey(path, options.deliveryMode) : `recorded:${this.nextSubscription}`
    const stream = new CoreBoundedStream<NotificationValue>(options.delivery, options.delivery.overflowPolicy)
    if (this.runtime.terminalEvidence !== undefined && this.runtime.backend.gatt.readTerminalEvidence !== undefined) {
      stream.setTerminalEvidenceHydrator(notice =>
        hydrateTerminalEvidence(this.runtime.backend.gatt, notice, this.runtime.terminalEvidence!.maximumCanonicalBytes)
      )
    }
    this.runtime.aggregateQuota.register(stream)
    const subscription = new CoreSubscription(
      this.runtime.idFactory.subscriptionId(`subscription-${this.nextSubscription}`),
      path,
      stream,
      current => this.remove(current, key)
    )
    this.nextSubscription += 1
    this.runtime.resourceLedger.increment('subscriptionConsumers')
    const existing = this.physicalByPath.get(key)
    if (existing !== undefined) {
      if (existing.closed) {
        const cleanup = await this.disable(existing)
        if (cleanup.state === 'release-failed') {
          this.closeConsumer(subscription, 'owner-released')
          throw new BackendContractError(
            cleanup.failures[0]?.error ??
              contractError('platform.failure', 'gatt', 'subscription-registry.closed-physical').normalized
          )
        }
        this.closeConsumer(subscription, 'owner-released')
        return this.subscribe(path, options, queueKey)
      }
      existing.consumers.add(subscription)
      // Native CCCD enablement may yield a value before the shared enable promise settles.
      subscription.markReady()
      try {
        await this.waitForReady(existing, options)
        subscription.observedDelivery = existing.backend?.observedDelivery ?? 'unknown'
        return subscription
      } catch (error) {
        const admissionError =
          error instanceof Error
            ? error
            : contractError('platform.failure', 'gatt', 'subscription-registry.shared-enable')
        if (this.isOwnAdmissionTerminal(admissionError, options)) {
          const cleanup = await this.remove(subscription, key)
          if (cleanup.state === 'release-failed') {
            throw new BackendContractError(
              cleanup.failures[0]?.error ??
                contractError('platform.failure', 'cleanup', 'subscription-registry.joiner-cleanup').normalized
            )
          }
          throw admissionError
        }
        const cleanup = await this.failEnableConsumers(existing)
        if (cleanup.state === 'release-failed') {
          throw new BackendContractError(
            cleanup.failures[0]?.error ??
              contractError('platform.failure', 'cleanup', 'subscription-registry.shared-enable-compensation')
                .normalized
          )
        }
        throw this.normalizeEnableError(admissionError)
      }
    }
    const physical: PhysicalSubscription<Attachment, Identity> = {
      key,
      path,
      queueKey,
      recording,
      incarnation,
      consumers: new Set([subscription]),
      backend: null,
      ready: null,
      closed: false,
      released: false,
      releaseInFlight: null,
      releasedDuringEnable: false,
      backendInvalidated: false,
      lateSubscriptionPending: false,
      pump: null,
      terminalEvidence: null,
      terminalEvidenceScope: null,
      terminalEvidenceReaders: new Set(),
      terminalEvidenceReleaseInFlight: null,
      terminalEvidenceReleaseFailure: null
    }
    this.physicalByPath.set(key, physical)
    // The consumer is not public until the await below resolves, so readiness here safely retains early native values.
    subscription.markReady()
    physical.ready = this.enable(physical, options)
    try {
      await physical.ready
      return subscription
    } catch (error) {
      const cleanup = await this.failEnableConsumers(physical)
      if (cleanup.state === 'release-failed') {
        throw new BackendContractError(
          cleanup.failures[0]?.error ??
            contractError('platform.failure', 'cleanup', 'subscription-registry.enable-compensation').normalized
        )
      }
      throw this.normalizeEnableError(
        error instanceof Error ? error : contractError('platform.failure', 'gatt', 'subscription-registry.enable')
      )
    }
  }

  async remove(subscription: CoreSubscription<Attachment, Identity>, physicalKey: string): Promise<CleanupRecord> {
    const physical = this.physicalByPath.get(physicalKey)
    this.closeConsumer(subscription, 'owner-released')
    if (physical === undefined) {
      return { state: 'released', failures: [] }
    }
    physical.consumers.delete(subscription)
    if (physical.consumers.size > 0) {
      return { state: 'released', failures: [] }
    }
    return this.disable(physical)
  }

  async invalidatePath(
    path: CurrentCharacteristicPath<Attachment>,
    reason: 'connection-lost' | 'owner-released' | 'source-failed' | 'service-changed',
    evidence?: TerminalEvidenceCommit
  ): Promise<CleanupRecord> {
    const physicals = [...this.physicalByPath.values()].filter(candidate =>
      characteristicPathsEqual(candidate.path, path)
    )
    if (physicals.length === 0) {
      return { state: 'released', failures: [] }
    }
    const failures: CleanupFailure[] = []
    for (const physical of physicals) {
      const cleanup = await this.invalidatePhysical(physical, reason, evidence)
      failures.push(...cleanup.failures)
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  /** Authoritative end of this exact lease generation, before cancelling child work. */
  confirmConnectionReleased(path: ConnectionPath<Attachment, string>): void {
    for (const physical of this.physicalByPath.values()) {
      if (connectionPathsEqual(physical.path, path)) physical.backendInvalidated = true
    }
  }

  async invalidateDatabase(
    database: DatabasePath<Attachment, string, string>,
    reason: 'connection-lost' | 'owner-released' | 'source-failed' | 'service-changed'
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const physical of [...this.physicalByPath.values()]) {
      if (!matchesDatabasePath(physical.path, database)) {
        continue
      }
      for (const subscription of [...physical.consumers]) {
        this.closeConsumer(subscription, reason)
        physical.consumers.delete(subscription)
      }
      const result = await this.disable(physical)
      failures.push(...result.failures)
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  async destroy(): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const physical of [...this.physicalByPath.values()]) {
      for (const subscription of [...physical.consumers]) {
        this.closeConsumer(subscription, 'owner-released')
        physical.consumers.delete(subscription)
      }
      const result = await this.disable(physical)
      failures.push(...result.failures)
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private async enable(
    physical: PhysicalSubscription<Attachment, Identity>,
    options: SubscriptionOptions
  ): Promise<void> {
    if (this.runtime.terminalEvidence !== undefined) {
      const reserve = this.runtime.backend.gatt.reserveTerminalEvidence
      if (reserve === undefined)
        throw contractError('capability.unsupported', 'gatt', 'subscription-registry.terminal-evidence')
      const scope = { managerId: this.runtime.managerId, operationId: physical.key, incarnation: physical.incarnation }
      physical.terminalEvidence = await reserve.call(this.runtime.backend.gatt, {
        producer: 'subscription',
        maximumBytes: this.runtime.terminalEvidence.maximumCanonicalBytes,
        formatVersion: 1,
        scope
      })
      physical.terminalEvidenceScope = scope
    }
    const result = await this.runtime.operationCoordinator.run({
      queueKey: physical.queueKey,
      fairnessKey: 'subscription',
      options,
      mayCommit: false,
      dispatch: correlation => {
        this.assertCurrent(physical.path)
        return this.subscribeDispatch(physical, options, correlation)
      },
      onQuarantined: () => {
        physical.lateSubscriptionPending = true
      },
      onLateSuccess: backendSubscription => this.compensateLateSubscription(physical, backendSubscription),
      onLateFailure: () => {
        physical.lateSubscriptionPending = false
        return this.failEnableConsumers(physical).then(() => undefined)
      }
    })
    const backendSubscription = requireOperationValue(result, 'subscription-registry.enable')
    if (physical.closed || physical.consumers.size === 0 || !this.runtime.isPathCurrent(physical.path)) {
      physical.backend = backendSubscription
      this.runtime.resourceLedger.increment('physicalCccdEnablements')
      await this.unsubscribeBackend(physical, backendSubscription)
      this.runtime.resourceLedger.decrement('physicalCccdEnablements')
      physical.backend = null
      physical.releasedDuringEnable = true
      throw contractError('operation.disconnected', 'gatt', 'subscription-registry.enable-invalidated')
    }
    physical.lateSubscriptionPending = false
    physical.backend = backendSubscription
    for (const consumer of physical.consumers) {
      consumer.observedDelivery = backendSubscription.observedDelivery ?? 'unknown'
    }
    this.runtime.resourceLedger.increment('physicalCccdEnablements')
    physical.pump = this.pumpNotifications(physical, backendSubscription.notifications)
    this.runtime.trace.record({
      timestamp: this.runtime.now(),
      resource: 'subscription',
      transition: 'ready',
      operation: null,
      cause: null,
      queuedOperations: 0,
      dispatchedOperations: 0,
      quarantinedOperations: 0
    })
  }

  private subscribeDispatch(
    physical: PhysicalSubscription<Attachment, Identity>,
    options: SubscriptionOptions,
    correlation: import('../backend-contract/primitives').OperationCorrelation<Attachment, string>
  ): CoreOperationDispatch<BackendSubscription<Attachment, string, string, string, string>> {
    const recorded = this.runtime.backend.gatt.subscribeRecorded
    const dispatch =
      physical.recording === null
        ? this.runtime.backend.gatt.subscribe(physical.path, {
            operation: { ...options, correlation },
            options,
            ...(physical.terminalEvidenceScope === null
              ? {}
              : { terminalEvidenceScope: physical.terminalEvidenceScope })
          })
        : recorded === undefined
          ? (() => {
              throw contractError('capability.unsupported', 'gatt', 'subscription-registry.recorded')
            })()
          : recorded.call(this.runtime.backend.gatt, physical.path, {
              operation: { ...options, correlation },
              options: { ...options, recording: physical.recording },
              ...(physical.terminalEvidenceScope === null
                ? {}
                : { terminalEvidenceScope: physical.terminalEvidenceScope })
            })
    return {
      completion: dispatch.completion.then(async subscription => {
        try {
          if (!characteristicPathsEqual(subscription.path, physical.path)) {
            throw contractError('protocol.violation', 'gatt', 'subscription-registry.subscribe-path')
          }
          assertSuccessfulOperationTerminal(
            subscription.terminal,
            correlation,
            'subscription-registry.subscribe-terminal'
          )
          return subscription
        } catch (error) {
          await this.compensateMalformedSubscription(physical, subscription)
          throw error
        }
      }),
      requestCancellation: () => dispatch.requestCancellation().then(() => undefined)
    }
  }

  private async compensateMalformedSubscription(
    physical: PhysicalSubscription<Attachment, Identity>,
    backendSubscription: BackendSubscription<Attachment, string, string, string, string>
  ): Promise<void> {
    physical.lateSubscriptionPending = false
    physical.backend = backendSubscription
    this.runtime.resourceLedger.increment('physicalCccdEnablements')
    const options: PublicOperationOptions = { signal: null, deadline: null }
    const correlation = this.runtime.idFactory.operationCorrelation(
      `malformed-subscription-compensation-${this.nextLateCompensation}`
    )
    this.nextLateCompensation += 1
    try {
      await this.unsubscribeDispatch(backendSubscription, options, correlation).completion
      this.runtime.resourceLedger.decrement('physicalCccdEnablements')
      physical.backend = null
      physical.releasedDuringEnable = true
      this.runtime.trace.record({
        timestamp: this.runtime.now(),
        resource: 'subscription',
        transition: 'malformed-subscription-compensated',
        operation: null,
        cause: null,
        queuedOperations: 0,
        dispatchedOperations: 0,
        quarantinedOperations: 0
      })
    } catch (error) {
      const normalized =
        error instanceof BackendContractError
          ? error.normalized
          : contractError('platform.failure', 'cleanup', 'subscription-registry.malformed-subscription').normalized
      this.runtime.trace.record({
        timestamp: this.runtime.now(),
        resource: 'subscription',
        transition: 'malformed-subscription-compensation-failed',
        operation: null,
        cause: normalized.code,
        queuedOperations: 0,
        dispatchedOperations: 0,
        quarantinedOperations: 0
      })
    }
  }

  private async disable(physical: PhysicalSubscription<Attachment, Identity>): Promise<CleanupRecord> {
    if (physical.released) {
      return { state: 'released', failures: [] }
    }
    if (physical.releaseInFlight !== null) {
      return physical.releaseInFlight
    }
    physical.closed = true
    const release = this.disablePhysical(physical)
    physical.releaseInFlight = release
    release.then(
      result => {
        physical.releaseInFlight = null
        if (result.state === 'released' && physical.terminalEvidence === null) {
          physical.released = true
          this.physicalByPath.delete(physical.key)
        }
      },
      () => {
        physical.releaseInFlight = null
      }
    )
    return release
  }

  private async disablePhysical(physical: PhysicalSubscription<Attachment, Identity>): Promise<CleanupRecord> {
    if (physical.ready !== null) {
      try {
        await physical.ready
      } catch (error) {
        if (physical.releasedDuringEnable) {
          return this.releaseTerminalEvidenceIfReady(physical)
        }
        if (physical.backendInvalidated) {
          this.releaseInvalidatedBackend(physical)
          return this.releaseTerminalEvidenceIfReady(physical)
        }
        if (physical.backend !== null) {
          const cleanup = await this.unsubscribePhysical(physical, physical.backend)
          return cleanup.state === 'release-failed' ? cleanup : this.releaseTerminalEvidenceIfReady(physical)
        }
        if (error instanceof BackendContractError) {
          return cleanupFailure('subscription', error.normalized)
        }
        return cleanupFailure(
          'subscription',
          contractError('platform.failure', 'cleanup', 'subscription-registry.ready').normalized
        )
      }
    }
    if (physical.backendInvalidated) {
      this.releaseInvalidatedBackend(physical)
      return this.releaseTerminalEvidenceIfReady(physical)
    }
    const backendSubscription = physical.backend
    if (backendSubscription === null) {
      return this.releaseTerminalEvidenceIfReady(physical)
    }
    const cleanup = await this.unsubscribePhysical(physical, backendSubscription)
    return cleanup.state === 'release-failed' ? cleanup : this.releaseTerminalEvidenceIfReady(physical)
  }

  private releaseInvalidatedBackend(physical: PhysicalSubscription<Attachment, Identity>): CleanupRecord {
    if (physical.backend !== null) {
      this.runtime.resourceLedger.decrement('physicalCccdEnablements')
      physical.backend = null
    }
    return { state: 'released', failures: [] }
  }

  private async unsubscribePhysical(
    physical: PhysicalSubscription<Attachment, Identity>,
    backendSubscription: BackendSubscription<Attachment, string, string, string, string>
  ): Promise<CleanupRecord> {
    const options: PublicOperationOptions = { signal: null, deadline: null }
    const result = await this.runtime.operationCoordinator.runCleanup({
      queueKey: physical.queueKey,
      fairnessKey: 'subscription',
      options,
      mayCommit: false,
      dispatch: correlation => this.unsubscribeDispatch(backendSubscription, options, correlation)
    })
    if (result.outcome !== 'succeeded') {
      // A stale path or interrupted operation says nothing about whether the
      // native CCCD was disabled. Only independently confirmed connection-end
      // invalidation can discharge a refused unsubscribe.
      if (physical.backendInvalidated) {
        this.runtime.trace.record({
          timestamp: this.runtime.now(),
          resource: 'subscription',
          transition: 'backend-invalidated-during-disable',
          operation: null,
          cause: result.error.code,
          queuedOperations: 0,
          dispatchedOperations: 0,
          quarantinedOperations: 0
        })
        return this.releaseInvalidatedBackend(physical)
      }
      return cleanupFailure('subscription', result.error)
    }
    this.runtime.resourceLedger.decrement('physicalCccdEnablements')
    physical.backend = null
    return { state: 'released', failures: [] }
  }

  private unsubscribeDispatch(
    backendSubscription: BackendSubscription<Attachment, string, string, string, string>,
    options: PublicOperationOptions,
    correlation: import('../backend-contract/primitives').OperationCorrelation<Attachment, string>
  ): CoreOperationDispatch<import('../backend-contract/operations').OperationTerminalRecord<Attachment, string>> {
    const dispatch = this.runtime.backend.gatt.unsubscribe(backendSubscription, { ...options, correlation })
    return {
      completion: dispatch.completion.then(terminal => {
        assertSuccessfulOperationTerminal(terminal, correlation, 'subscription-registry.unsubscribe-terminal')
        return terminal
      }),
      requestCancellation: () => dispatch.requestCancellation().then(() => undefined)
    }
  }

  private async unsubscribeBackend(
    physical: PhysicalSubscription<Attachment, Identity>,
    backendSubscription: BackendSubscription<Attachment, string, string, string, string>
  ): Promise<void> {
    const options: PublicOperationOptions = { signal: null, deadline: null }
    const result = await this.runtime.operationCoordinator.runCleanup({
      queueKey: physical.queueKey,
      fairnessKey: 'subscription',
      options,
      mayCommit: false,
      dispatch: correlation => this.unsubscribeDispatch(backendSubscription, options, correlation)
    })
    if (result.outcome !== 'succeeded') {
      throw new BackendContractError(result.error)
    }
  }

  private async compensateLateSubscription(
    physical: PhysicalSubscription<Attachment, Identity>,
    backendSubscription: BackendSubscription<Attachment, string, string, string, string>
  ): Promise<void> {
    physical.lateSubscriptionPending = false
    physical.backend = backendSubscription
    this.runtime.resourceLedger.increment('physicalCccdEnablements')
    const options: PublicOperationOptions = { signal: null, deadline: null }
    const correlation = this.runtime.idFactory.operationCorrelation(
      `late-subscription-compensation-${this.nextLateCompensation}`
    )
    this.nextLateCompensation += 1
    try {
      await this.unsubscribeDispatch(backendSubscription, options, correlation).completion
      this.runtime.resourceLedger.decrement('physicalCccdEnablements')
      physical.backend = null
      physical.releasedDuringEnable = true
      this.runtime.trace.record({
        timestamp: this.runtime.now(),
        resource: 'subscription',
        transition: 'late-subscription-compensated',
        operation: null,
        cause: null,
        queuedOperations: 0,
        dispatchedOperations: 0,
        quarantinedOperations: 0
      })
      this.finalizeUnusedPhysical(physical)
    } catch (error) {
      const normalized =
        error instanceof BackendContractError
          ? error.normalized
          : contractError('platform.failure', 'cleanup', 'subscription-registry.late-subscription').normalized
      this.runtime.trace.record({
        timestamp: this.runtime.now(),
        resource: 'subscription',
        transition: 'late-subscription-compensation-failed',
        operation: null,
        cause: normalized.code,
        queuedOperations: 0,
        dispatchedOperations: 0,
        quarantinedOperations: 0
      })
      throw new BackendContractError(normalized)
    }
  }

  private async pumpNotifications(
    physical: PhysicalSubscription<Attachment, Identity>,
    source: BoundedAsyncStream<NotificationValue>
  ): Promise<void> {
    try {
      for await (const item of source) {
        if (physical.closed) {
          return
        }
        if (await this.forwardNotificationItem(physical, item)) {
          return
        }
      }
      await this.invalidatePhysical(physical, 'source-failed')
    } catch (error) {
      const normalized =
        error instanceof BackendContractError
          ? error.normalized
          : contractError('platform.failure', 'gatt', 'subscription-registry.notification-pump').normalized
      this.runtime.trace.record({
        timestamp: this.runtime.now(),
        resource: 'subscription',
        transition: 'notification-source-failed',
        operation: null,
        cause: normalized.code,
        queuedOperations: 0,
        dispatchedOperations: 0,
        quarantinedOperations: 0
      })
      await this.invalidatePhysical(physical, 'source-failed')
    }
  }

  private async forwardNotificationItem(
    physical: PhysicalSubscription<Attachment, Identity>,
    item: StreamItem<NotificationValue>
  ): Promise<boolean> {
    if (item.kind === 'overflow') {
      const terminated: typeof physical.consumers extends Set<infer S> ? S[] : never = []
      for (const subscription of [...physical.consumers]) {
        if (!subscription.isReady()) {
          continue
        }
        const stream = subscription.values
        if (!(stream instanceof CoreBoundedStream)) {
          throw contractError('lifecycle.invariant-violation', 'gatt', 'subscription-registry.stream')
        }
        stream.observeSourceOverflow(item)
      }
      for (const subscription of terminated) await subscription.remove()
      return false
    }
    if (item.kind !== 'value') {
      if (item.kind === 'terminal') {
        if (
          physical.terminalEvidence !== null &&
          (item.evidence === undefined ||
            item.evidence.producer !== 'subscription' ||
            !sameTerminalEvidenceScope(item.evidence.scope, physical.terminalEvidenceScope))
        ) {
          throw contractError('protocol.violation', 'gatt', 'subscription-registry.terminal-evidence-missing')
        }
        const reason =
          item.reason === 'connection-lost'
            ? 'connection-lost'
            : item.reason === 'service-changed'
              ? 'service-changed'
              : item.reason === 'owner-released'
                ? 'owner-released'
                : 'source-failed'
        if (reason === 'connection-lost' || reason === 'service-changed') {
          if (reason === 'connection-lost') this.confirmConnectionReleased(physical.path)
          await this.invalidatePath(physical.path, reason, item.evidence)
        } else {
          await this.invalidatePhysical(physical, reason, item.evidence)
        }
        return true
      }
      return false
    }
    const value = ownBytes(item.value.value, this.runtime.maximumValueBytes)
    for (const subscription of [...physical.consumers]) {
      if (!subscription.isReady()) {
        continue
      }
      const stream = subscription.values
      if (!(stream instanceof CoreBoundedStream)) {
        throw contractError('lifecycle.invariant-violation', 'gatt', 'subscription-registry.stream')
      }
      const outcome = this.runtime.aggregateQuota.emit(
        stream,
        {
          value: ownBytes(value, this.runtime.maximumValueBytes),
          delivery: item.value.delivery,
          ...(item.value.sourceGeneration === undefined
            ? {}
            : { sourceGeneration: item.value.sourceGeneration, sourceOrdinal: item.value.sourceOrdinal }),
          ...(item.value.provenance === undefined ? {} : { provenance: item.value.provenance }),
          ...(item.value.receipt === undefined ? {} : { receipt: Object.freeze({ ...item.value.receipt }) })
        },
        value.byteLength
      )
      if (outcome.terminated) {
        await subscription.remove()
      }
    }
    return false
  }

  private async waitForReady(
    physical: PhysicalSubscription<Attachment, Identity>,
    options: SubscriptionOptions
  ): Promise<void> {
    if (physical.ready === null) {
      throw contractError('lifecycle.invariant-violation', 'gatt', 'subscription-registry.missing-ready')
    }
    await awaitWithOperationAdmission(physical.ready, options, this.runtime.now, 'subscription-registry.join')
  }

  private closeConsumer(
    subscription: CoreSubscription<Attachment, Identity>,
    reason: 'connection-lost' | 'owner-released' | 'source-failed' | 'service-changed',
    evidence?: TerminalEvidenceCommit
  ): void {
    if (!subscription.isActive()) {
      return
    }
    subscription.invalidate(reason, evidence)
    subscription.markRemoved()
    this.runtime.resourceLedger.decrement('subscriptionConsumers')
    if (subscription.values instanceof CoreBoundedStream) {
      this.runtime.aggregateQuota.unregister(subscription.values)
    }
  }

  private async invalidatePhysical(
    physical: PhysicalSubscription<Attachment, Identity>,
    reason: 'connection-lost' | 'owner-released' | 'source-failed' | 'service-changed',
    evidence?: TerminalEvidenceCommit
  ): Promise<CleanupRecord> {
    if (evidence !== undefined) this.retainTerminalEvidenceForConsumers(physical)
    for (const subscription of [...physical.consumers]) {
      this.closeConsumer(subscription, reason, evidence)
      physical.consumers.delete(subscription)
    }
    // Database invalidation ends usability, not the native enablement. Keep
    // its exact backend identity until unsubscribe succeeds; only a confirmed
    // connection end discharges that connection's physical obligations.
    return this.disable(physical)
  }

  private async failEnableConsumers(physical: PhysicalSubscription<Attachment, Identity>): Promise<CleanupRecord> {
    physical.closed = true
    for (const consumer of [...physical.consumers]) {
      this.closeConsumer(consumer, 'owner-released')
      physical.consumers.delete(consumer)
    }
    const cleanup = await this.releaseTerminalEvidenceIfReady(physical)
    this.finalizeUnusedPhysical(physical)
    return cleanup
  }

  private finalizeUnusedPhysical(physical: PhysicalSubscription<Attachment, Identity>): void {
    if (
      physical.consumers.size > 0 ||
      physical.backend !== null ||
      physical.lateSubscriptionPending ||
      physical.terminalEvidence !== null
    ) {
      return
    }
    physical.released = true
    this.physicalByPath.delete(physical.key)
  }

  /**
   * A terminal may be read after its physical CCCD has been disabled. Keep the
   * reservation until every consumer stream has delivered that terminal (and
   * any retained views have drained), then retire it exactly once.
   */
  private retainTerminalEvidenceForConsumers(physical: PhysicalSubscription<Attachment, Identity>): void {
    if (physical.terminalEvidence === null) return
    for (const consumer of physical.consumers) {
      if (consumer.values instanceof CoreBoundedStream) {
        physical.terminalEvidenceReaders.add(consumer.values)
      }
    }
    for (const stream of [...physical.terminalEvidenceReaders]) {
      stream.onRetainedValuesDrained(() => {
        physical.terminalEvidenceReaders.delete(stream)
        this.releaseTerminalEvidenceIfReady(physical)
          .then(() => this.finalizeUnusedPhysical(physical))
          .catch(() => undefined)
      })
    }
  }

  /**
   * Retains a failed release on the physical object. A repeated remove,
   * invalidation, or destroy calls this again, so a storage outage cannot turn
   * a reserved record into an unreachable leak.
   */
  private async releaseTerminalEvidenceIfReady(
    physical: PhysicalSubscription<Attachment, Identity>
  ): Promise<CleanupRecord> {
    if (physical.terminalEvidence === null || physical.terminalEvidenceReaders.size > 0) {
      return physical.terminalEvidenceReleaseFailure === null
        ? { state: 'released', failures: [] }
        : cleanupFailure('subscription', physical.terminalEvidenceReleaseFailure)
    }
    if (physical.terminalEvidenceReleaseInFlight !== null) return physical.terminalEvidenceReleaseInFlight
    const reservation = physical.terminalEvidence
    const release = (async (): Promise<CleanupRecord> => {
      try {
        await reservation.release()
        if (physical.terminalEvidence === reservation) {
          physical.terminalEvidence = null
          physical.terminalEvidenceScope = null
          physical.terminalEvidenceReleaseFailure = null
        }
        this.finalizeUnusedPhysical(physical)
        return { state: 'released', failures: [] }
      } catch (error) {
        const normalized =
          error instanceof BackendContractError
            ? error.normalized
            : contractError('platform.failure', 'cleanup', 'subscription-registry.terminal-evidence-release').normalized
        physical.terminalEvidenceReleaseFailure = normalized
        this.runtime.trace.record({
          timestamp: this.runtime.now(),
          resource: 'subscription',
          transition: 'terminal-evidence-release-failed',
          operation: null,
          cause: normalized.code,
          queuedOperations: 0,
          dispatchedOperations: 0,
          quarantinedOperations: 0
        })
        return cleanupFailure('subscription', normalized)
      }
    })()
    physical.terminalEvidenceReleaseInFlight = release
    release
      .finally(() => {
        if (physical.terminalEvidenceReleaseInFlight === release) physical.terminalEvidenceReleaseInFlight = null
      })
      .catch(() => undefined)
    return release
  }

  private normalizeEnableError(error: Error): BackendContractError {
    if (error instanceof BackendContractError) {
      return error
    }
    return contractError('gatt.subscribe-failed', 'gatt', 'subscription-registry.enable')
  }

  private assertCurrent(path: CurrentCharacteristicPath<Attachment>): void {
    if (!this.runtime.isPathCurrent(path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'subscription-registry.path')
    }
  }

  private assertAdmission(options: SubscriptionOptions): void {
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'gatt', 'subscription-registry.subscribe')
    }
    if (options.deadline !== null && options.deadline <= this.runtime.now()) {
      throw contractError('operation.timed-out', 'gatt', 'subscription-registry.subscribe')
    }
  }

  private isOwnAdmissionTerminal(error: Error, options: SubscriptionOptions): boolean {
    if (!(error instanceof BackendContractError)) {
      return false
    }
    return (
      (error.normalized.code === 'operation.aborted' && options.signal?.aborted === true) ||
      (error.normalized.code === 'operation.timed-out' &&
        options.deadline !== null &&
        options.deadline <= this.runtime.now())
    )
  }
}

function exactPathKey<Attachment extends string>(path: CurrentCharacteristicPath<Attachment>): string {
  return characteristicPathKey(path)
}

function physicalSubscriptionKey<Attachment extends string>(
  path: CurrentCharacteristicPath<Attachment>,
  deliveryMode: SubscriptionOptions['deliveryMode']
): string {
  // A hard requirement must not share the preference or automatic entry.
  // Identical require-* values still share one physical enable. A hard
  // request that misses that key reaches the core, which accepts the join
  // or returns capability.limited without disabling the first CCCD.
  const physicalMode =
    deliveryMode === 'require-notification' || deliveryMode === 'require-indication'
      ? deliveryMode
      : deliveryMode === 'prefer-notification'
        ? 'notification'
        : deliveryMode === 'prefer-indication'
          ? 'indication'
          : 'automatic'
  return `${exactPathKey(path)}|${physicalMode}`
}

function matchesDatabasePath<Attachment extends string>(
  characteristic: CurrentCharacteristicPath<Attachment>,
  database: DatabasePath<Attachment, string, string>
): boolean {
  return databasePathsEqual(characteristic, database)
}

function sameTerminalEvidenceScope(
  actual: { readonly managerId: string; readonly operationId: string; readonly incarnation: string },
  expected: { readonly managerId: string; readonly operationId: string; readonly incarnation: string } | null
): boolean {
  return (
    expected !== null &&
    actual.managerId === expected.managerId &&
    actual.operationId === expected.operationId &&
    actual.incarnation === expected.incarnation
  )
}

function requireOperationValue<Attachment extends string, Value>(
  result: CoreOperationResult<Attachment, Value>,
  operation: string
): Value {
  if (result.outcome === 'succeeded') {
    return result.value
  }
  throw new BackendContractError(result.error ?? contractError('platform.failure', 'core', operation).normalized)
}

function cleanupFailure(resourceKind: string, error: NormalizedBleError): CleanupRecord {
  return { state: 'release-failed', failures: [{ resourceKind, error }] }
}
