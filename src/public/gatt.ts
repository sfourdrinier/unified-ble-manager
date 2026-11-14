// src/public/gatt.ts

import type { ContinuationRecordingConfiguration } from '../backend-contract/continuation-recording-bounds'

import { registerOrderedStreamOrigin } from '../core/ordered-stream-origin'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { canonicalUuidInput } from '../backend-contract/primitives'
import { awaitWithOperationAdmission } from '../core/unified-ble-core-helpers'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { contractError, snapshotPlatformMetadataEntries } from '../backend-contract/errors'
import type {
  GattAccessRequirements,
  GattCharacteristicPropertyAvailability,
  GattDescriptorProperties,
  NotificationReceipt,
  NotificationProvenance
} from '../backend-contract/gatt'
import type {
  DiscoveredGattDatabaseHandle,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableGattDatabaseSnapshot,
  SubscriptionHandle
} from '../manager/consumer-handles'
import { normalizeOperationOptions, type OperationOptions } from './operation-options'
import { resolveStreamPolicy, type StreamPolicy } from './stream-presets'
import { collectCleanupPhases, rehydratePublicError, rehydratePublicPromise, runWithCleanup } from './error-bridge'
import { mapPublicBoundedAsyncStream, type PublicBoundedAsyncStream, type PublicStreamItem } from './streams'
import { toPublicCleanupRecord, toPublicNormalizedError, type CleanupRecord, type NormalizedBleError } from './cleanup'

export type {
  GattAccessRequirements,
  GattCharacteristicPropertyAvailability,
  NotificationReceipt,
  NotificationProvenance
} from '../backend-contract/gatt'

export type UuidInput = string | number

export interface OccurrenceSelector {
  readonly occurrence?: number
}

export interface GattPathSelector {
  readonly serviceOccurrence?: number
  readonly characteristicOccurrence?: number
}

export interface GattDatabaseChangedEvent {
  readonly previousGeneration: string
  readonly reason: 'service-changed' | 'reconnect' | 'backend-reset' | 'manual-rediscovery'
  readonly affectedHandleRange: { readonly start: number; readonly end: number } | null
}

export interface GattServiceReference {
  readonly uuid: string
  readonly occurrence: number
}

export interface GattCharacteristicProperties {
  readonly broadcast: boolean
  readonly read: boolean
  readonly writeWithResponse: boolean
  readonly writeWithoutResponse: boolean
  readonly authenticatedSignedWrites: boolean
  readonly notify: boolean
  readonly indicate: boolean
  readonly extendedProperties: boolean
  readonly reliableWrite: boolean
  readonly writableAuxiliaries: boolean
  readonly availability: GattCharacteristicPropertyAvailability
}

export interface GattWriteOptions extends OperationOptions {
  readonly response?: 'required' | 'not-required' | 'automatic'
}

export interface LongWriteOptions extends GattWriteOptions {
  readonly chunkSize?: number
}

export interface DescriptorWriteOptions extends OperationOptions {
  readonly response?: 'required' | 'automatic'
}

export interface GattSubscribeOptions extends OperationOptions {
  readonly delivery?: 'prefer-notification' | 'prefer-indication' | 'require-notification' | 'require-indication'
  readonly stream?: StreamPolicy
}

export interface GattRecordedSubscribeOptions extends OperationOptions {
  readonly delivery?: GattSubscribeOptions['delivery']
  readonly recording: ContinuationRecordingConfiguration
}

export type GattDelivery = 'notification' | 'indication' | 'unknown'

export interface GattValueEvent {
  readonly sourceGeneration?: string
  readonly sourceOrdinal?: string
  /**
   * Added in 5.2.3. Runtime values always normalize an omitted backend value
   * to `unknown`; optionality preserves the 5.2.1 source shape for consumers
   * that construct or forward this public type.
   */
  readonly provenance?: NotificationProvenance
  readonly value: Uint8Array
  readonly delivery: GattDelivery
  readonly observedAtMonotonicMs: number
  readonly sequence: number
  /** Original paired native clocks, when supplied by the host. */
  readonly receipt?: NotificationReceipt
}

export type GattValueStream = PublicBoundedAsyncStream<GattValueEvent>

export interface GattDatabaseSnapshot {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly characteristics: readonly GattCharacteristic[]
  readonly descriptors: readonly GattDescriptor[]
}

export interface GattOrderedMember {
  readonly memberId: string
  readonly characteristic: GattCharacteristic
  readonly delivery?: NonNullable<GattSubscribeOptions['delivery']>
}
export type GattOrderedMarkerWritePhase =
  | { readonly state: 'initiating' }
  | { readonly state: 'not-started' | 'started' | 'settled'; readonly error: NormalizedBleError | null }
export type GattOrderedMarkerWriteStatus =
  | { readonly state: 'pending' }
  | { readonly state: 'not-admitted'; readonly error: NormalizedBleError | null }
  | { readonly state: 'rejected'; readonly error: NormalizedBleError }
  | { readonly state: 'admitted'; readonly ordinal: string; readonly write: GattOrderedMarkerWritePhase }
export interface GattOrderedMarkerWrite {
  start(
    target: GattCharacteristic,
    value: Readonly<Uint8Array>,
    mode: 'with-response' | 'without-response',
    options?: OperationOptions
  ): Promise<GattOrderedMarkerWriteStatus>
  status(): Promise<GattOrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }>
  cancel(): Promise<GattOrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }>
  forget(): Promise<void>
}
export interface GattOrderedSourceGeneration {
  readonly kind: 'counter' | 'opaque'
  readonly value: string
}
export interface GattOrderedSourceMembership {
  readonly member: string
  readonly epoch: string
}
export type GattOrderedLossScope =
  | { readonly kind: 'member'; readonly memberId: string }
  | { readonly kind: 'group' }
  | { readonly kind: 'unknown' }
export type GattOrderedSourceRejection =
  | { readonly kind: 'value'; readonly bytes: string }
  | { readonly kind: 'loss'; readonly skippedRecords: string }
  | { readonly kind: 'fence'; readonly membership: GattOrderedSourceMembership }
  | { readonly kind: 'barrier'; readonly id: string }
  | { readonly kind: 'marker'; readonly markerId: string }
  | { readonly kind: 'ordinal-exhausted'; readonly bytes: string }
export interface GattOrderedRejectedLoss {
  readonly scope: GattOrderedLossScope
  readonly unit: 'values' | 'source-records'
  readonly count: string | null
  readonly bytes: string | null
}
export type GattOrderedItem =
  | {
      readonly kind: 'value'
      readonly collectionId: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string
      readonly memberId: string
      readonly ordinal: string
      readonly sourceGeneration?: GattOrderedSourceGeneration | null
      readonly sourceMembership?: GattOrderedSourceMembership | null
      readonly value: Uint8Array
      readonly delivery: import('../backend-contract/gatt').NotificationDelivery
      readonly provenance: NotificationProvenance
      readonly receipt?: NotificationReceipt
    }
  | {
      readonly kind: 'loss'
      readonly scope: GattOrderedLossScope
      readonly reason: 'overflow' | 'source-loss' | 'control-loss'
      readonly lostValues: string | null
      readonly lostBytes: string | null
      readonly lostSourceRecords: string | null
    }
  | {
      readonly kind: 'marker'
      readonly collectionId: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string
      readonly sourceGeneration?: GattOrderedSourceGeneration | null
      readonly sourceMembership?: GattOrderedSourceMembership | null
      readonly markerId: string
      readonly ordinal: string
    }
  | {
      readonly kind: 'failure-observation'
      readonly failure: NormalizedBleError | null
      readonly sourceRejection: GattOrderedSourceRejection | null
      readonly sourceFailureMember: GattOrderedSourceMembership | null
      readonly rejectedLoss: GattOrderedRejectedLoss | null
      readonly tailUncertain: boolean
    }
  | { readonly kind: 'failure-observations-omitted'; readonly count: string | null }
  | {
      readonly kind: 'terminal'
      readonly reason:
        | 'closed'
        | 'overflow'
        | 'source-failed'
        | 'owner-released'
        | 'connection-lost'
        | 'service-changed'
        | 'host-reset'
        | 'operation-aborted'
        | 'operation-timed-out'
        | 'ordinal-exhausted'
      readonly uncertainTail: boolean
      readonly error?: NormalizedBleError | null
      readonly secondaryError?: NormalizedBleError | null
      readonly additionalFailureCount?: string
      readonly failureCountOverflowed?: boolean
      readonly failureObservationsOmitted?: boolean
      readonly fenceError?: NormalizedBleError | null
      readonly rejectedValues?: string | null
      readonly rejectedLoss?: GattOrderedRejectedLoss | null
      readonly sourceRejection?: GattOrderedSourceRejection | null
      readonly sourceFailureMember?: GattOrderedSourceMembership | null
    }
export interface GattOrderedStream extends AsyncIterable<GattOrderedItem, undefined, undefined> {
  readonly limits: {
    readonly itemCapacity: number
    readonly byteCapacity: number
    readonly reservedControlCapacity: number
  }
  increaseByteCapacity?(
    maxBytes: number
  ): Promise<import('../core/ordered-notification-contract').OrderedByteCapacityChange>
  close(): Promise<GattOrderedCleanup>
  discard(): Promise<GattOrderedCleanup>
  reserveMarkerWrite?(): GattOrderedMarkerWrite
}
export type GattOrderedCleanup = Omit<
  import('../core/ordered-notification-contract').OrderedNotificationCleanup,
  'group' | 'members' | 'discarded'
> & {
  readonly group: CleanupRecord
  readonly members: readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]
  readonly discarded?: {
    readonly values: string
    readonly bytes: string
    readonly controls: string
    readonly uncertainTail: boolean
    readonly terminal?: GattOrderedItem | null
    readonly losses?: readonly Extract<GattOrderedItem, { readonly kind: 'loss' }>[]
    readonly markers?: readonly Extract<GattOrderedItem, { readonly kind: 'marker' }>[]
    readonly failureObservations?: readonly Extract<GattOrderedItem, { readonly kind: 'failure-observation' }>[]
    readonly failureObservationsOmitted?: readonly Extract<
      GattOrderedItem,
      { readonly kind: 'failure-observations-omitted' }
    >[]
    readonly trustedValues?: readonly {
      readonly memberId: string
      readonly ordinal: string
      readonly sourceMembership: GattOrderedSourceMembership
      readonly bytes: string
    }[]
  }
}
export type GattOrderedSetup =
  | {
      readonly state: 'ready'
      readonly sourceGeneration?: GattOrderedSourceGeneration | null
      readonly members: readonly {
        readonly memberId: string
        readonly requestedDelivery:
          | 'prefer-notification'
          | 'prefer-indication'
          | 'require-notification'
          | 'require-indication'
        readonly effectiveDelivery: import('../backend-contract/gatt').NotificationDelivery
      }[]
      readonly stream: GattOrderedStream
    }
  | {
      readonly state: 'failed'
      readonly sourceGeneration?: GattOrderedSourceGeneration | null
      readonly error: NormalizedBleError
      readonly members: readonly {
        readonly memberId: string
        readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
      }[]
      readonly observations: readonly Exclude<GattOrderedItem, { readonly kind: 'terminal' }>[]
      readonly uncertainTail: boolean
      readonly cleanup: GattOrderedCleanup
      readonly stream: GattOrderedStream
      readonly close: () => Promise<GattOrderedCleanup>
    }
type InternalOrderedRecordingStartResult = import('../core/ordered-recording-start').OrderedRecordingStartResult

type InternalOrderedMarkerWriteStatus = import('../core/ordered-marker-wire').OrderedMarkerWriteStatus
type InternalOrderedRecordingReclaimReceipt = import('../core/ordered-recording-reclaim').OrderedRecordingReclaimReceipt
type InternalOrderedRecordingReclaimStatus =
  import('../core/ordered-recording-reclaim').OrderedRecordingReclaimStatus<InternalOrderedRecordingReclaimReceipt>
type InternalOrderedObservation = Exclude<
  import('../core/ordered-notification-contract').OrderedNotificationItem,
  { readonly kind: 'terminal' }
>

/** Ordered recording start result with host-neutral cleanup evidence. */
export type GattOrderedRecordingFailureValue =
  | { readonly kind: 'int' | 'text'; readonly value: string }
  | { readonly kind: 'bool'; readonly value: boolean }
export interface GattOrderedRecordingPlatformDetail {
  readonly domain: string
  readonly code: string
  readonly message: string | null
  readonly metadata: Readonly<Record<string, GattOrderedRecordingFailureValue>>
  /** Ordered v2 metadata preserves duplicate and otherwise non-record-safe keys. */
  readonly metadataEntries?: readonly import('../backend-contract/errors').PlatformMetadataEntry[]
  readonly cleanup?: readonly GattOrderedRecordingPlatformDetail[]
  readonly cleanupFailures?: readonly GattOrderedRecordingFailure[]
}
export interface GattOrderedRecordingFailure {
  readonly code: string
  readonly domain: string
  readonly operation: string
  readonly detail: string | null
  readonly platformDomain: string | null
  readonly platformCode: string | null
  readonly platformMessage: string | null
  readonly platformMetadata: Readonly<Record<string, GattOrderedRecordingFailureValue>>
  /** Ordered v2 metadata preserves duplicate and otherwise non-record-safe keys. */
  readonly platformMetadataEntries?: readonly import('../backend-contract/errors').PlatformMetadataEntry[]
  readonly platformCleanup?: readonly GattOrderedRecordingPlatformDetail[]
  readonly platformCleanupFailures?: readonly GattOrderedRecordingFailure[]
}
export type GattOrderedRecordingStopFailure =
  | { readonly kind: 'source'; readonly source: GattOrderedRecordingFailure }
  | { readonly kind: 'storage'; readonly storage: GattOrderedRecordingStorageFailure }
  | {
      readonly kind: 'storage-cleanup'
      readonly storage: GattOrderedRecordingStorageFailure
      readonly source: GattOrderedRecordingFailure
    }
  | {
      readonly kind:
        | 'invalid-delivery'
        | 'invalid-identity'
        | 'worker-failed'
        | 'missing-terminal'
        | 'entropy-unavailable'
        | 'receipt-retired'
        | 'recording-not-accepting'
    }
export interface GattOrderedRecordingStorageFailure {
  readonly kind: string
  readonly detail: string
  readonly operation: string
  readonly sqliteExtendedCode: number | null
  readonly sqliteCode: string | null
}
export interface GattOrderedRecordingStartReceipt {
  readonly version: 1 | 2
  readonly recordingId: string
  readonly collectionId: string
  readonly state: 'active' | 'setup-failed' | 'failed'
  readonly sourceGeneration: GattOrderedSourceGeneration | null
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
    readonly delivery: import('../backend-contract/gatt').NotificationDelivery
    readonly cleanupReleased: boolean
    readonly cleanupError: GattOrderedRecordingFailure | null
  }[]
  readonly setupFailure: GattOrderedRecordingFailure | null
  readonly fenceFailure: GattOrderedRecordingFailure | null
  readonly error: GattOrderedRecordingStopFailure | null
  readonly inspectionError: GattOrderedRecordingStopFailure | null
  readonly driverError: GattOrderedRecordingStopFailure | null
}
export type GattOrderedRecordingStartResult =
  | { readonly kind: 'started'; readonly receipt: GattOrderedRecordingStartReceipt }
  | {
      readonly kind: 'owner-failed'
      readonly recordingId: string
      readonly collectionId: string
      readonly error: GattOrderedRecordingStopFailure
    }
  | {
      readonly kind: 'prepare-failed'
      readonly leaseCleanup?: CleanupRecord | { readonly state: 'pending' }
      readonly receipt: {
        readonly version: 1 | 2
        readonly state: 'prepare-failed'
        readonly recordingId: string
        readonly sourceStarted: false
        readonly journalEffect: 'none' | 'unknown'
        readonly error: GattOrderedRecordingStopFailure
      }
    }

export type GattOrderedRecordingReclaimReceipt = Omit<
  import('../core/ordered-recording-reclaim').OrderedRecordingReclaimReceipt,
  'error'
> & { readonly error: NormalizedBleError | null }

export type GattOrderedRecordingReclaimStatus =
  | GattOrderedRecordingReclaimReceipt
  | { readonly state: 'not-reserved'; readonly error: NormalizedBleError }
  | { readonly state: 'idle' | 'cancelled' | 'retiring' | 'retired' }

export interface GattOrderedRecordingReclaim {
  start(): Promise<GattOrderedRecordingReclaimStatus>
  status(): Promise<GattOrderedRecordingReclaimStatus>
  commit(): Promise<GattOrderedRecordingReclaimReceipt>
  cancel(): Promise<GattOrderedRecordingReclaimStatus>
  release(): Promise<GattOrderedRecordingReclaimReceipt>
  retire(): Promise<void>
}

export interface GattOrderedRecordingStart {
  start(
    members: readonly GattOrderedMember[],
    options: OperationOptions & {
      readonly recording: ContinuationRecordingConfiguration
      readonly stream?: StreamPolicy
    }
  ): Promise<GattOrderedRecordingStartResult>
  status(): Promise<
    | { readonly state: 'pending' | 'unknown' | 'failed' | 'retired' }
    | {
        readonly state: 'result'
        readonly result: GattOrderedRecordingStartResult
      }
  >
  retire(): Promise<void>
  recoveryHandle?(): import('../core/ordered-recording-reclaim').OrderedRecordingRecoveryHandle
  reserveMarkerWrite?(): GattOrderedMarkerWrite
}

export interface GattDatabase {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly changed: PublicBoundedAsyncStream<GattDatabaseChangedEvent>
  service(uuid: UuidInput, selector?: OccurrenceSelector): GattService
  servicesByUuid(uuid: UuidInput): readonly GattService[]
  characteristic(serviceUuid: UuidInput, characteristicUuid: UuidInput, selector?: GattPathSelector): GattCharacteristic
  snapshot(): GattDatabaseSnapshot
  reserveOrderedRecordingStart?(): GattOrderedRecordingStart
  reserveOrderedRecordingMarker?(recordingId: string): GattOrderedMarkerWrite
  reserveOrderedRecordingReclaim?(
    handle: import('../core/ordered-recording-reclaim').OrderedRecordingRecoveryHandle
  ): GattOrderedRecordingReclaim
  subscribeOrdered?(
    members: readonly GattOrderedMember[],
    options?: OperationOptions & { readonly stream?: StreamPolicy }
  ): Promise<GattOrderedSetup>
}

export interface GattService {
  readonly uuid: string
  readonly occurrence: number
  readonly primary: boolean | null
  readonly includedServices: readonly GattServiceReference[] | null
  /** Present when Windows kept the service but did not list its characteristics. */
  readonly restriction?: import('../backend-contract/gatt').GattServiceRestriction
  readonly characteristics: readonly GattCharacteristic[]
  characteristic(uuid: UuidInput, selector?: OccurrenceSelector): GattCharacteristic
  characteristicsByUuid(uuid: UuidInput): readonly GattCharacteristic[]
}

export interface GattCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly service: GattService
  readonly properties: GattCharacteristicProperties
  readonly access: GattAccessRequirements
  readonly descriptors: readonly GattDescriptor[]
  /** Reads the value. Use {@link GattCharacteristic.readReceipt} to learn what the platform says the value is. */
  read(options?: OperationOptions): Promise<Uint8Array>
  /**
   * Reads the value with its provenance. A read also runs while the
   * characteristic notifies; on Apple CoreBluetooth the value is then
   * `read-or-notification` and subscribers receive it too.
   */
  readReceipt(options?: OperationOptions): Promise<GattReadReceipt>
  write(value: Uint8Array, options?: GattWriteOptions): Promise<GattWriteReceipt>
  writeWhenReady(value: Uint8Array, options?: OperationOptions): Promise<GattWriteReceipt>
  /** Acquires BlueZ's optional eligible-characteristic FD; never falls back to writes. */
  acquireWrite(options?: OperationOptions): Promise<GattAcquiredWriter>
  /** Acquires the optional FD notification route with bounded owned delivery. */
  acquireNotifications(
    options?: OperationOptions & { readonly stream?: StreamPolicy }
  ): Promise<GattAcquiredNotifications>
  writeLong(value: Uint8Array, options?: LongWriteOptions): Promise<GattLongWriteReceipt>
  /**
   * A subscription acquired but not publishable is removed before rejection.
   * Failed provisional removal remains owned and is retried before this
   * characteristic admits a later subscription.
   */
  subscribe(options?: GattSubscribeOptions): Promise<GattSubscription>
  subscribeRecorded(options: GattRecordedSubscribeOptions): Promise<GattRecordedSubscription>
  withSubscription<T>(options: GattSubscribeOptions, action: (subscription: GattSubscription) => Promise<T>): Promise<T>
  descriptor(uuid: UuidInput, selector?: OccurrenceSelector): GattDescriptor
}

export interface GattAcquiredWriter {
  readonly mtuBytes: number
  write(value: Uint8Array, options?: OperationOptions): Promise<GattWriteReceipt>
  close(): Promise<CleanupRecord>
}
export interface GattAcquiredNotifications {
  readonly mtuBytes: number
  readonly values: PublicBoundedAsyncStream<GattValueEvent>
  close(): Promise<CleanupRecord>
}

export interface GattDescriptor {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristic: GattCharacteristic
  readonly properties: GattDescriptorProperties
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: DescriptorWriteOptions): Promise<GattWriteReceipt>
}

export type GattWriteReceipt = import('../manager/consumer-handles').PortableWriteReceipt
export type GattReadReceipt = import('../manager/consumer-handles').PortableReadReceipt
export type GattReadProvenance = GattReadReceipt['provenance']
export type GattLongWriteReceipt = import('../manager/consumer-handles').PortableLongWriteReceipt

export interface GattSubscription {
  readonly requestedDelivery: GattSubscribeOptions['delivery']
  readonly effectiveDelivery: GattDelivery
  readonly values: GattValueStream
  remove(): Promise<CleanupRecord>
}

export interface GattRecordedSubscription {
  readonly subscriptionId: string
  readonly recordingId: string
  readonly changed: PublicBoundedAsyncStream<{ readonly recordingId: string }>
  readonly requestedDelivery: GattSubscribeOptions['delivery']
  readonly effectiveDelivery: GattDelivery
  remove(): Promise<CleanupRecord>
}

export interface PublicGattDatabaseSource extends DiscoveredGattDatabaseHandle {
  readonly changed?: BoundedAsyncStream<GattDatabaseChangedEvent>
  assertCurrent?(): void
  readonly deliverySelection?: 'controllable' | 'unknown'
}

/** Retains unpublishable subscriptions until retry or manager teardown settles them. */
export class ProvisionalGattSubscriptionOwner {
  private readonly pending = new Set<Pick<SubscriptionHandle, 'remove'>>()
  private readonly scopes = new Map<Pick<SubscriptionHandle, 'remove'>, string>()
  private readonly removals = new Map<Pick<SubscriptionHandle, 'remove'>, Promise<CleanupRecord>>()
  private readonly releasedCleanup: CleanupRecord = { state: 'released', failures: [] }
  private released = false
  private readonly parentRelease: Promise<CleanupRecord>
  private confirmParentRelease!: (cleanup: CleanupRecord) => void

  constructor() {
    this.parentRelease = new Promise(resolve => {
      this.confirmParentRelease = resolve
    })
  }

  hasPending(): boolean {
    return this.pending.size > 0
  }

  async compensate(
    subscription: Pick<SubscriptionHandle, 'remove'>,
    path: PortableCurrentCharacteristicPath,
    primaryError: unknown
  ): Promise<never> {
    if (this.released) throw rehydratePublicError(primaryError)
    this.pending.add(subscription)
    this.scopes.set(subscription, provisionalScopeKey(path))
    return runWithCleanup(
      async (): Promise<never> => {
        throw rehydratePublicError(primaryError)
      },
      () => this.remove(subscription)
    )
  }

  async retryPending(scope?: PortableCurrentCharacteristicPath): Promise<CleanupRecord> {
    const phases: { readonly error?: unknown; readonly cleanup?: CleanupRecord }[] = []
    const key = scope === undefined ? null : provisionalScopeKey(scope)
    for (const subscription of [...this.pending]) {
      if (key !== null && this.scopes.get(subscription) !== key) continue
      try {
        phases.push({ cleanup: await this.remove(subscription) })
      } catch (error) {
        phases.push({ error })
      }
    }
    return collectCleanupPhases(phases)
  }

  /** A released manager lease is authoritative for every resource under it. */
  confirmManagerRelease(): void {
    if (this.released) return
    this.released = true
    this.pending.clear()
    this.scopes.clear()
    this.removals.clear()
    this.confirmParentRelease(this.releasedCleanup)
  }

  private remove(subscription: Pick<SubscriptionHandle, 'remove'>): Promise<CleanupRecord> {
    if (this.released) return Promise.resolve(this.releasedCleanup)
    const inFlight = this.removals.get(subscription)
    if (inFlight !== undefined) return inFlight
    const attempt = Promise.race([
      rehydrateCleanup(Promise.resolve().then(() => (this.released ? this.releasedCleanup : subscription.remove()))),
      this.parentRelease
    ]).then(cleanup => {
      if (cleanup.state === 'released') {
        this.pending.delete(subscription)
        this.scopes.delete(subscription)
      }
      return cleanup
    })
    this.removals.set(subscription, attempt)
    const forget = () => {
      if (this.removals.get(subscription) === attempt) this.removals.delete(subscription)
    }
    attempt.then(forget, forget)
    return attempt
  }
}

function provisionalScopeKey(path: PortableCurrentCharacteristicPath): string {
  return JSON.stringify([
    path.attachmentId,
    path.attachment?.backendInstanceId ?? null,
    path.attachment?.backendGeneration ?? null,
    path.peerId,
    path.connectionId,
    path.connectionGeneration,
    path.ownerLeaseId,
    path.databaseId,
    path.databaseGeneration,
    path.serviceUuid,
    path.serviceOccurrence,
    path.characteristicUuid,
    path.characteristicOccurrence
  ])
}

function assertSubscribeAdmission(options: ReturnType<typeof normalizeOperationOptions>, now: () => number): void {
  if (options.signal?.aborted === true) {
    throw contractError('operation.aborted', 'gatt', 'public-gatt.subscribe')
  }
  if (options.deadline !== null && options.deadline <= now()) {
    throw contractError('operation.timed-out', 'gatt', 'public-gatt.subscribe')
  }
}

export async function createPublicGattDatabase(
  source: PublicGattDatabaseSource,
  provisionalOwner = new ProvisionalGattSubscriptionOwner()
): Promise<GattDatabase> {
  const snapshot = await source.snapshot()
  return new PublicGattDatabase(source, snapshot, provisionalOwner)
}

const orderedCharacteristicPaths = new WeakMap<GattCharacteristic, PortableCurrentCharacteristicPath>()

function publicOrderedMarkerStatus(status: InternalOrderedMarkerWriteStatus): GattOrderedMarkerWriteStatus {
  switch (status.state) {
    case 'pending':
      return Object.freeze({ state: status.state })
    case 'not-admitted':
      return Object.freeze({
        state: status.state,
        error: status.error === null ? null : toPublicNormalizedError(status.error)
      })
    case 'rejected':
      return Object.freeze({ state: status.state, error: toPublicNormalizedError(status.error) })
    case 'admitted':
      if (
        status.write.state !== 'initiating' &&
        status.write.state !== 'not-started' &&
        status.write.state !== 'started' &&
        status.write.state !== 'settled'
      )
        throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-marker-status')
      return Object.freeze({
        state: status.state,
        ordinal: status.ordinal,
        write:
          status.write.state === 'initiating'
            ? Object.freeze({ state: status.write.state })
            : Object.freeze({
                state: status.write.state,
                error: status.write.error === null ? null : toPublicNormalizedError(status.write.error)
              })
      })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-marker-status')
  }
}

function isInternalOrderedMarkerWriteStatus(
  status: InternalOrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }
): status is InternalOrderedMarkerWriteStatus {
  return (
    status.state === 'pending' ||
    status.state === 'not-admitted' ||
    status.state === 'rejected' ||
    status.state === 'admitted'
  )
}

function publicOrderedRecordingReclaimReceipt(
  receipt: InternalOrderedRecordingReclaimReceipt
): GattOrderedRecordingReclaimReceipt {
  return Object.freeze({
    state: receipt.state,
    mappingCommitted: receipt.mappingCommitted,
    error: receipt.error === null ? null : toPublicNormalizedError(receipt.error)
  })
}

function publicOrderedRecordingReclaimStatus(
  status: InternalOrderedRecordingReclaimStatus
): GattOrderedRecordingReclaimStatus {
  switch (status.state) {
    case 'idle':
    case 'cancelled':
    case 'retiring':
    case 'retired':
      return Object.freeze({ state: status.state })
    case 'not-reserved':
      return Object.freeze({ state: status.state, error: toPublicNormalizedError(status.error) })
    case 'reserved':
    case 'pending':
    case 'ready':
    case 'committed':
    case 'releasing':
    case 'release-failed':
    case 'released':
    case 'failed':
      return publicOrderedRecordingReclaimReceipt(status)
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-reclaim')
  }
}

function publicOrderedRecordingStartResult(
  result: InternalOrderedRecordingStartResult
): GattOrderedRecordingStartResult {
  switch (result.kind) {
    case 'started':
      return Object.freeze({ kind: result.kind, receipt: publicOrderedRecordingStartReceipt(result.receipt) })
    case 'owner-failed':
      return Object.freeze({
        kind: result.kind,
        recordingId: result.recordingId,
        collectionId: result.collectionId,
        error: publicOrderedRecordingStopFailure(result.error)
      })
    case 'prepare-failed':
      return Object.freeze({
        kind: result.kind,
        ...(result.leaseCleanup === undefined
          ? {}
          : {
              leaseCleanup:
                result.leaseCleanup.state === 'pending'
                  ? pendingOrderedRecordingCleanup()
                  : toPublicCleanupRecord(result.leaseCleanup)
            }),
        receipt: Object.freeze({
          version: result.receipt.version,
          state: result.receipt.state,
          recordingId: result.receipt.recordingId,
          sourceStarted: result.receipt.sourceStarted,
          journalEffect: result.receipt.journalEffect,
          error: publicOrderedRecordingStopFailure(result.receipt.error)
        })
      })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-start')
  }
}

function pendingOrderedRecordingCleanup(): { readonly state: 'pending' } {
  return Object.freeze({ state: 'pending' })
}

function publicOrderedRecordingFailureValue(
  value: import('../core/ordered-recording').OrderedRecordingFailureValue
): GattOrderedRecordingFailureValue {
  switch (value.kind) {
    case 'int':
    case 'text':
      return Object.freeze({ kind: value.kind, value: value.value })
    case 'bool':
      return Object.freeze({ kind: value.kind, value: value.value })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-start')
  }
}

function publicOrderedRecordingMetadata(
  metadata: Readonly<Record<string, import('../core/ordered-recording').OrderedRecordingFailureValue>>
): Readonly<Record<string, GattOrderedRecordingFailureValue>> {
  return Object.freeze(
    Object.fromEntries(Object.entries(metadata).map(([key, value]) => [key, publicOrderedRecordingFailureValue(value)]))
  )
}

function publicOrderedRecordingPlatformDetail(
  detail: import('../core/ordered-recording').OrderedRecordingPlatformDetail
): GattOrderedRecordingPlatformDetail {
  return Object.freeze({
    domain: detail.domain,
    code: detail.code,
    message: detail.message,
    metadata: publicOrderedRecordingMetadata(detail.metadata),
    ...(detail.metadataEntries === undefined
      ? {}
      : {
          metadataEntries: snapshotPlatformMetadataEntries(
            detail.metadataEntries,
            'public-gatt.ordered-recording.metadata.entries'
          )
        }),
    ...(detail.cleanup === undefined
      ? {}
      : { cleanup: Object.freeze(detail.cleanup.map(publicOrderedRecordingPlatformDetail)) }),
    ...(detail.cleanupFailures === undefined
      ? {}
      : { cleanupFailures: Object.freeze(detail.cleanupFailures.map(publicOrderedRecordingFailure)) })
  })
}

function publicOrderedRecordingFailure(
  failure: import('../core/ordered-recording').OrderedRecordingFailure
): GattOrderedRecordingFailure {
  return Object.freeze({
    code: failure.code,
    domain: failure.domain,
    operation: failure.operation,
    detail: failure.detail,
    platformDomain: failure.platformDomain,
    platformCode: failure.platformCode,
    platformMessage: failure.platformMessage,
    platformMetadata: publicOrderedRecordingMetadata(failure.platformMetadata),
    ...(failure.platformMetadataEntries === undefined
      ? {}
      : {
          platformMetadataEntries: snapshotPlatformMetadataEntries(
            failure.platformMetadataEntries,
            'public-gatt.ordered-recording.metadata.entries'
          )
        }),
    ...(failure.platformCleanup === undefined
      ? {}
      : { platformCleanup: Object.freeze(failure.platformCleanup.map(publicOrderedRecordingPlatformDetail)) }),
    ...(failure.platformCleanupFailures === undefined
      ? {}
      : { platformCleanupFailures: Object.freeze(failure.platformCleanupFailures.map(publicOrderedRecordingFailure)) })
  })
}

function publicOrderedRecordingStorageFailure(
  failure: import('../core/continuation-recording').ContinuationRecordingFailure
): GattOrderedRecordingStorageFailure {
  return Object.freeze({
    kind: failure.kind,
    detail: failure.detail,
    operation: failure.operation,
    sqliteExtendedCode: failure.sqliteExtendedCode,
    sqliteCode: failure.sqliteCode
  })
}

function publicOrderedRecordingStopFailure(
  failure: import('../core/continuation-recording').OrderedRecordingStopFailure
): GattOrderedRecordingStopFailure {
  switch (failure.kind) {
    case 'source':
      return Object.freeze({ kind: failure.kind, source: publicOrderedRecordingFailure(failure.source) })
    case 'storage':
      return Object.freeze({ kind: failure.kind, storage: publicOrderedRecordingStorageFailure(failure.storage) })
    case 'storage-cleanup':
      return Object.freeze({
        kind: failure.kind,
        storage: publicOrderedRecordingStorageFailure(failure.storage),
        source: publicOrderedRecordingFailure(failure.source)
      })
    case 'invalid-delivery':
    case 'invalid-identity':
    case 'worker-failed':
    case 'missing-terminal':
    case 'entropy-unavailable':
    case 'receipt-retired':
    case 'recording-not-accepting':
      return Object.freeze({ kind: failure.kind })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-start')
  }
}

function publicOrderedRecordingStartReceipt(
  receipt: import('../core/ordered-recording-start').OrderedRecordingStartReceipt
): GattOrderedRecordingStartReceipt {
  const sourceGeneration = publicOrderedSourceGeneration(receipt.sourceGeneration)
  if (sourceGeneration === undefined)
    throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-start')
  return Object.freeze({
    version: receipt.version,
    recordingId: receipt.recordingId,
    collectionId: receipt.collectionId,
    state: receipt.state,
    sourceGeneration,
    members: Object.freeze(
      receipt.members.map(member =>
        Object.freeze({
          memberId: member.memberId,
          acquisition: member.acquisition,
          delivery: member.delivery,
          cleanupReleased: member.cleanupReleased,
          cleanupError: member.cleanupError === null ? null : publicOrderedRecordingFailure(member.cleanupError)
        })
      )
    ),
    setupFailure: receipt.setupFailure === null ? null : publicOrderedRecordingFailure(receipt.setupFailure),
    fenceFailure: receipt.fenceFailure === null ? null : publicOrderedRecordingFailure(receipt.fenceFailure),
    error: receipt.error === null ? null : publicOrderedRecordingStopFailure(receipt.error),
    inspectionError:
      receipt.inspectionError === null ? null : publicOrderedRecordingStopFailure(receipt.inspectionError),
    driverError: receipt.driverError === null ? null : publicOrderedRecordingStopFailure(receipt.driverError)
  })
}

function publicOrderedStreamLimits(
  limits: import('../backend-contract/streams').StreamLimits
): GattOrderedStream['limits'] {
  return Object.freeze({
    itemCapacity: limits.itemCapacity,
    byteCapacity: limits.byteCapacity,
    reservedControlCapacity: limits.reservedControlCapacity
  })
}

function publicOrderedSourceGeneration(
  generation: import('../core/ordered-notification-contract').OrderedSourceGeneration | null | undefined
): GattOrderedSourceGeneration | null | undefined {
  return generation === undefined
    ? undefined
    : generation === null
      ? null
      : Object.freeze({ kind: generation.kind, value: generation.value })
}

function publicOrderedSourceMembership(
  membership: import('../core/ordered-notification-contract').OrderedNotificationSourceMembership | null | undefined
): GattOrderedSourceMembership | null | undefined {
  return membership === undefined
    ? undefined
    : membership === null
      ? null
      : Object.freeze({ member: membership.member, epoch: membership.epoch })
}

function publicOrderedLossScope(
  scope: import('../core/ordered-notification-contract').OrderedNotificationLoss['scope']
): GattOrderedLossScope {
  switch (scope.kind) {
    case 'member':
      return Object.freeze({ kind: scope.kind, memberId: scope.memberId })
    case 'group':
    case 'unknown':
      return Object.freeze({ kind: scope.kind })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  }
}

function publicOrderedSourceRejection(
  rejection: import('../core/ordered-notification-contract').OrderedNotificationSourceRejection | null | undefined
): GattOrderedSourceRejection | null | undefined {
  if (rejection === undefined || rejection === null) return rejection
  switch (rejection.kind) {
    case 'value':
    case 'ordinal-exhausted':
      return Object.freeze({ kind: rejection.kind, bytes: rejection.bytes })
    case 'loss':
      return Object.freeze({ kind: rejection.kind, skippedRecords: rejection.skippedRecords })
    case 'fence': {
      const membership = publicOrderedSourceMembership(rejection.membership)
      if (membership === undefined || membership === null)
        throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
      return Object.freeze({ kind: rejection.kind, membership })
    }
    case 'barrier':
      return Object.freeze({ kind: rejection.kind, id: rejection.id })
    case 'marker':
      return Object.freeze({ kind: rejection.kind, markerId: rejection.markerId })
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  }
}

function publicOrderedRejectedLoss(
  loss: import('../core/ordered-notification-contract').OrderedNotificationRejectedLoss | null | undefined
): GattOrderedRejectedLoss | null | undefined {
  return loss === undefined
    ? undefined
    : loss === null
      ? null
      : Object.freeze({
          scope: publicOrderedLossScope(loss.scope),
          unit: loss.unit,
          count: loss.count,
          bytes: loss.bytes
        })
}

function publicOrderedLoss(
  item: Extract<import('../core/ordered-notification-contract').OrderedNotificationItem, { readonly kind: 'loss' }>
): Extract<GattOrderedItem, { readonly kind: 'loss' }> {
  return Object.freeze({
    kind: item.kind,
    scope: publicOrderedLossScope(item.scope),
    reason: item.reason,
    lostValues: item.lostValues,
    lostBytes: item.lostBytes,
    lostSourceRecords: item.lostSourceRecords
  })
}

function publicOrderedMarker(
  item: Extract<import('../core/ordered-notification-contract').OrderedNotificationItem, { readonly kind: 'marker' }>
): Extract<GattOrderedItem, { readonly kind: 'marker' }> {
  const sourceGeneration = publicOrderedSourceGeneration(item.sourceGeneration)
  const sourceMembership = publicOrderedSourceMembership(item.sourceMembership)
  return Object.freeze({
    kind: item.kind,
    collectionId: item.collectionId,
    connectionGeneration: item.connectionGeneration,
    databaseGeneration: item.databaseGeneration,
    ...(sourceGeneration === undefined ? {} : { sourceGeneration }),
    ...(sourceMembership === undefined ? {} : { sourceMembership }),
    markerId: item.markerId,
    ordinal: item.ordinal
  })
}

function publicNotificationReceipt(receipt: NotificationReceipt | undefined): NotificationReceipt | undefined {
  return receipt === undefined
    ? undefined
    : Object.freeze({
        receivedAtUtcMs: receipt.receivedAtUtcMs,
        receivedAtMonotonicMs: receipt.receivedAtMonotonicMs,
        clockId: receipt.clockId,
        origin: receipt.origin
      })
}

function publicOrderedCleanup(
  cleanup: import('../core/ordered-notification-contract').OrderedNotificationCleanup
): GattOrderedCleanup {
  return Object.freeze({
    group: toPublicCleanupRecord(cleanup.group),
    members: Object.freeze(
      cleanup.members.map(member =>
        Object.freeze({ memberId: member.memberId, cleanup: toPublicCleanupRecord(member.cleanup) })
      )
    ),
    ...(cleanup.discarded === undefined
      ? {}
      : {
          discarded: Object.freeze({
            values: cleanup.discarded.values,
            bytes: cleanup.discarded.bytes,
            controls: cleanup.discarded.controls,
            uncertainTail: cleanup.discarded.uncertainTail,
            ...(cleanup.discarded.terminal === undefined
              ? {}
              : {
                  terminal: cleanup.discarded.terminal === null ? null : publicOrderedItem(cleanup.discarded.terminal)
                }),
            ...(cleanup.discarded.losses === undefined
              ? {}
              : { losses: Object.freeze(cleanup.discarded.losses.map(publicOrderedLoss)) }),
            ...(cleanup.discarded.markers === undefined
              ? {}
              : { markers: Object.freeze(cleanup.discarded.markers.map(publicOrderedMarker)) }),
            ...(cleanup.discarded.failureObservations === undefined
              ? {}
              : {
                  failureObservations: Object.freeze(
                    cleanup.discarded.failureObservations.map(publicOrderedFailureObservation)
                  )
                }),
            ...(cleanup.discarded.failureObservationsOmitted === undefined
              ? {}
              : {
                  failureObservationsOmitted: Object.freeze(
                    cleanup.discarded.failureObservationsOmitted.map(item =>
                      Object.freeze({ kind: item.kind, count: item.count })
                    )
                  )
                }),
            ...(cleanup.discarded.trustedValues === undefined
              ? {}
              : {
                  trustedValues: Object.freeze(
                    cleanup.discarded.trustedValues.map(item => {
                      const sourceMembership = publicOrderedSourceMembership(item.sourceMembership)
                      if (sourceMembership === undefined || sourceMembership === null)
                        throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
                      return Object.freeze({
                        memberId: item.memberId,
                        ordinal: item.ordinal,
                        sourceMembership,
                        bytes: item.bytes
                      })
                    })
                  )
                })
          })
        })
  })
}

function publicOrderedItem(
  item: import('../core/ordered-notification-contract').OrderedNotificationItem
): GattOrderedItem {
  switch (item.kind) {
    case 'value': {
      const sourceGeneration = publicOrderedSourceGeneration(item.sourceGeneration)
      const sourceMembership = publicOrderedSourceMembership(item.sourceMembership)
      const receipt = publicNotificationReceipt(item.receipt)
      return Object.freeze({
        kind: item.kind,
        collectionId: item.collectionId,
        connectionGeneration: item.connectionGeneration,
        databaseGeneration: item.databaseGeneration,
        memberId: item.memberId,
        ordinal: item.ordinal,
        ...(sourceGeneration === undefined ? {} : { sourceGeneration }),
        ...(sourceMembership === undefined ? {} : { sourceMembership }),
        value: new Uint8Array(item.value),
        delivery: item.delivery,
        provenance: item.provenance,
        ...(receipt === undefined ? {} : { receipt })
      })
    }
    case 'loss':
      return publicOrderedLoss(item)
    case 'marker':
      return publicOrderedMarker(item)
    case 'failure-observation':
      return publicOrderedFailureObservation(item)
    case 'failure-observations-omitted':
      return Object.freeze({ kind: item.kind, count: item.count })
    case 'terminal': {
      const rejectedLoss = publicOrderedRejectedLoss(item.rejectedLoss)
      const sourceRejection = publicOrderedSourceRejection(item.sourceRejection)
      const sourceFailureMember = publicOrderedSourceMembership(item.sourceFailureMember)
      return Object.freeze({
        kind: item.kind,
        reason: item.reason,
        uncertainTail: item.uncertainTail,
        ...(item.error === undefined
          ? {}
          : { error: item.error === null ? null : toPublicNormalizedError(item.error) }),
        ...(item.secondaryError === undefined
          ? {}
          : { secondaryError: item.secondaryError === null ? null : toPublicNormalizedError(item.secondaryError) }),
        ...(item.additionalFailureCount === undefined ? {} : { additionalFailureCount: item.additionalFailureCount }),
        ...(item.failureCountOverflowed === undefined ? {} : { failureCountOverflowed: item.failureCountOverflowed }),
        ...(item.failureObservationsOmitted === undefined
          ? {}
          : { failureObservationsOmitted: item.failureObservationsOmitted }),
        ...(item.fenceError === undefined
          ? {}
          : { fenceError: item.fenceError === null ? null : toPublicNormalizedError(item.fenceError) }),
        ...(item.rejectedValues === undefined ? {} : { rejectedValues: item.rejectedValues }),
        ...(rejectedLoss === undefined ? {} : { rejectedLoss }),
        ...(sourceRejection === undefined ? {} : { sourceRejection }),
        ...(sourceFailureMember === undefined ? {} : { sourceFailureMember })
      })
    }
    default:
      throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  }
}

function publicOrderedObservation(
  item: InternalOrderedObservation
): Exclude<GattOrderedItem, { readonly kind: 'terminal' }> {
  const projected = publicOrderedItem(item)
  if (projected.kind === 'terminal') throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  return projected
}

function publicOrderedFailureObservation(
  item: Extract<
    import('../core/ordered-notification-contract').OrderedNotificationItem,
    { readonly kind: 'failure-observation' }
  >
): Extract<GattOrderedItem, { readonly kind: 'failure-observation' }> {
  const sourceRejection = publicOrderedSourceRejection(item.sourceRejection)
  const sourceFailureMember = publicOrderedSourceMembership(item.sourceFailureMember)
  const rejectedLoss = publicOrderedRejectedLoss(item.rejectedLoss)
  if (sourceRejection === undefined || sourceFailureMember === undefined || rejectedLoss === undefined)
    throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  return Object.freeze({
    kind: item.kind,
    failure: item.failure === null ? null : toPublicNormalizedError(item.failure),
    sourceRejection,
    sourceFailureMember,
    rejectedLoss,
    tailUncertain: item.tailUncertain
  })
}

function publicOrderedSetup(
  setup: import('../core/ordered-notification-contract').OrderedNotificationSetup<PortableCurrentCharacteristicPath>,
  stream: GattOrderedStream
): GattOrderedSetup {
  const sourceGeneration = publicOrderedSourceGeneration(setup.sourceGeneration)
  if (setup.state === 'ready')
    return Object.freeze({
      state: setup.state,
      ...(sourceGeneration === undefined ? {} : { sourceGeneration }),
      members: Object.freeze(
        setup.members.map(member =>
          Object.freeze({
            memberId: member.memberId,
            requestedDelivery: member.requestedDelivery,
            effectiveDelivery: member.effectiveDelivery
          })
        )
      ),
      stream
    })
  if (setup.state !== 'failed') throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered')
  return Object.freeze({
    state: setup.state,
    ...(sourceGeneration === undefined ? {} : { sourceGeneration }),
    error: toPublicNormalizedError(setup.error),
    members: Object.freeze(
      setup.members.map(member => Object.freeze({ memberId: member.memberId, acquisition: member.acquisition }))
    ),
    observations: Object.freeze(setup.observations.map(publicOrderedObservation)),
    uncertainTail: setup.uncertainTail,
    cleanup: publicOrderedCleanup(setup.cleanup),
    stream,
    close: async () => runPublicAsync(async () => publicOrderedCleanup(await setup.close()))
  })
}

function publicOrderedIteratorResult(
  result: IteratorResult<import('../core/ordered-notification-contract').OrderedNotificationItem, undefined>
): IteratorResult<GattOrderedItem, undefined> {
  return result.done
    ? Object.freeze({ done: true, value: undefined })
    : Object.freeze({ done: false, value: publicOrderedItem(result.value) })
}

class PublicGattDatabase implements GattDatabase {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly changed: PublicBoundedAsyncStream<GattDatabaseChangedEvent>
  private readonly serviceLookup: ReadonlyMap<string, readonly GattService[]>
  private readonly snapshotValue: GattDatabaseSnapshot

  constructor(
    private readonly source: PublicGattDatabaseSource,
    snapshot: PortableGattDatabaseSnapshot,
    provisionalOwner: ProvisionalGattSubscriptionOwner
  ) {
    validateTopology(snapshot)
    this.generation = snapshot.path.databaseGeneration
    const serviceRecords = recordsWithOccurrence(snapshot.services, record => record.path.serviceUuid)
    const serviceObjects = serviceRecords.map(record => {
      const characteristics = recordsWithOccurrence(
        snapshot.characteristics.filter(characteristic => sameService(characteristic.path, record.record.path)),
        characteristic => characteristic.path.characteristicUuid
      )
      const characteristicObjects = characteristics.map(characteristic => {
        const descriptors = recordsWithOccurrence(
          snapshot.descriptors.filter(descriptor => sameCharacteristic(descriptor.path, characteristic.record.path)),
          descriptor => descriptor.path.descriptorUuid
        )
        return { characteristic, descriptors }
      })
      return new PublicGattService(record, source, characteristicObjects, provisionalOwner)
    })
    this.services = Object.freeze(serviceObjects)
    this.serviceLookup = createLookup(this.services, service => service.uuid)
    this.snapshotValue = Object.freeze({
      generation: this.generation,
      services: this.services,
      characteristics: Object.freeze(this.services.flatMap(service => service.characteristics)),
      descriptors: Object.freeze(
        this.services.flatMap(service => service.characteristics.flatMap(characteristic => characteristic.descriptors))
      )
    })
    this.changed =
      source.changed === undefined
        ? emptyChangedStream()
        : mapPublicBoundedAsyncStream(source.changed, snapshotGattDatabaseChangedEvent)
    Object.freeze(this)
  }

  service(uuid: UuidInput, selector: OccurrenceSelector = {}): GattService {
    return selectOne(this.servicesByUuid(uuid), selector, 'public-gatt.database.service')
  }

  servicesByUuid(uuid: UuidInput): readonly GattService[] {
    return this.serviceLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }

  characteristic(
    serviceUuid: UuidInput,
    characteristicUuid: UuidInput,
    selector: GattPathSelector = {}
  ): GattCharacteristic {
    return this.service(serviceUuid, { occurrence: selector.serviceOccurrence }).characteristic(characteristicUuid, {
      occurrence: selector.characteristicOccurrence
    })
  }

  private orderedMembers(members: readonly GattOrderedMember[]) {
    if (!Array.isArray(members) || members.length === 0)
      throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered.members')
    const ids = new Set<string>()
    const characteristics = new Set<GattCharacteristic>()
    const entries = Array.from(members, member => {
      if (
        member === null ||
        typeof member !== 'object' ||
        typeof member.memberId !== 'string' ||
        member.memberId.length === 0 ||
        ids.has(member.memberId) ||
        characteristics.has(member.characteristic) ||
        !this.snapshotValue.characteristics.includes(member.characteristic)
      )
        throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered.member')
      const path = orderedCharacteristicPaths.get(member.characteristic)
      if (path === undefined) throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered.characteristic')
      const selected = resolveDelivery(member.characteristic.properties, member.delivery)
      if (this.source.deliverySelection === 'unknown' && member.delivery?.startsWith('require-'))
        throw contractError('capability.limited', 'gatt', 'public-gatt.ordered.delivery')
      ids.add(member.memberId)
      characteristics.add(member.characteristic)
      return {
        memberId: member.memberId,
        path,
        deliveryMode: member.delivery ?? (selected === 'indication' ? 'prefer-indication' : 'prefer-notification')
      }
    })
    return entries
  }

  private publicMarkerHandle(
    handle: import('../core/ordered-marker-wire').OrderedMarkerWriteHandle<PortableCurrentCharacteristicPath>
  ): GattOrderedMarkerWrite {
    let bound: GattCharacteristic | null = null
    let request: {
      readonly target: GattCharacteristic
      readonly value: Uint8Array
      readonly mode: 'with-response' | 'without-response'
    } | null = null
    let idleCancellation: GattOrderedMarkerWriteStatus | null = null
    let retired = false
    return {
      start: async (
        target: GattCharacteristic,
        value: Readonly<Uint8Array>,
        mode: 'with-response' | 'without-response',
        startOptions: OperationOptions = {}
      ) => {
        try {
          if (retired) throw contractError('lifecycle.invalid-state', 'gatt', 'public-gatt.ordered-marker.retired')
          if (idleCancellation !== null) return idleCancellation
          if (!(value instanceof Uint8Array) || (mode !== 'with-response' && mode !== 'without-response'))
            throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered-marker')
          if (bound !== null && bound !== target)
            throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered-marker.target')
          if (
            request !== null &&
            (request.target !== target ||
              request.mode !== mode ||
              request.value.length !== value.length ||
              request.value.some((byte, index) => byte !== value[index]))
          )
            throw contractError('lifecycle.invalid-state', 'gatt', 'public-gatt.ordered-marker.retry')
          // An admitted marker owns its exact receipt. Retrying that receipt is
          // an observation, not a new GATT operation, so a later source loss
          // must not make that exact receipt stale.
          if (request !== null) {
            const status = await handle.status()
            if (isInternalOrderedMarkerWriteStatus(status) && status.state === 'admitted')
              return publicOrderedMarkerStatus(status)
          }
          this.source.assertCurrent?.()
          if (!this.snapshotValue.characteristics.includes(target))
            throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered-marker.target')
          const path = orderedCharacteristicPaths.get(target)
          if (path === undefined) throw contractError('argument.invalid', 'gatt', 'public-gatt.ordered-marker.target')
          const controls = normalizeOperationOptions(startOptions, () => this.source.monotonicNow())
          const copied = new Uint8Array(value)
          bound = target
          request ??= { target, value: copied, mode }
          return publicOrderedMarkerStatus(await handle.start(path, copied, mode, controls))
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      status: async () => {
        try {
          const status = await handle.status()
          if (isInternalOrderedMarkerWriteStatus(status)) return publicOrderedMarkerStatus(status)
          if (status.state === 'idle' || status.state === 'retired') return Object.freeze({ state: status.state })
          throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-marker-status')
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      cancel: async () => {
        try {
          const raw = await handle.cancel()
          const result = isInternalOrderedMarkerWriteStatus(raw)
            ? publicOrderedMarkerStatus(raw)
            : raw.state === 'idle'
              ? Object.freeze({ state: 'idle' })
              : raw.state === 'retired'
                ? Object.freeze({ state: 'retired' })
                : (() => {
                    throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-marker-status')
                  })()
          if (bound === null && result.state === 'not-admitted') idleCancellation = result
          return result
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      forget: async () => {
        try {
          await handle.forget()
          retired = true
          idleCancellation = null
        } catch (error) {
          throw rehydratePublicError(error)
        }
      }
    }
  }

  reserveOrderedRecordingMarker(recordingId: string): GattOrderedMarkerWrite {
    try {
      this.source.assertCurrent?.()
      const reserve = this.source.reserveOrderedRecordingMarker
      if (!reserve) throw contractError('capability.unsupported', 'gatt', 'public-gatt.ordered-recording-marker')
      return this.publicMarkerHandle(reserve.call(this.source, recordingId))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  reserveOrderedRecordingReclaim(handle: import('../core/ordered-recording-reclaim').OrderedRecordingRecoveryHandle) {
    try {
      this.source.assertCurrent?.()
      const reserve = this.source.reserveOrderedRecordingReclaim
      if (!reserve) throw contractError('capability.unsupported', 'gatt', 'public-gatt.ordered-recording-reclaim')
      const reclaim = reserve.call(this.source, handle)
      return Object.freeze({
        start: () => runPublicAsync(async () => publicOrderedRecordingReclaimStatus(await reclaim.start())),
        status: () => runPublicAsync(async () => publicOrderedRecordingReclaimStatus(await reclaim.status())),
        commit: () => runPublicAsync(async () => publicOrderedRecordingReclaimReceipt(await reclaim.commit())),
        cancel: () => runPublicAsync(async () => publicOrderedRecordingReclaimStatus(await reclaim.cancel())),
        release: () => runPublicAsync(async () => publicOrderedRecordingReclaimReceipt(await reclaim.release())),
        retire: () => rehydratePublicPromise(reclaim.retire())
      })
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  reserveOrderedRecordingStart(): GattOrderedRecordingStart {
    const handle = (() => {
      try {
        this.source.assertCurrent?.()
        if (this.source.reserveOrderedRecordingStart === undefined)
          throw contractError('capability.unsupported', 'gatt', 'public-gatt.ordered-recording')
        return this.source.reserveOrderedRecordingStart()
      } catch (error) {
        throw rehydratePublicError(error)
      }
    })()
    const recoveryHandle = handle.recoveryHandle
    return Object.freeze({
      start: async (
        members: readonly GattOrderedMember[],
        options: OperationOptions & {
          readonly recording: ContinuationRecordingConfiguration
          readonly stream?: StreamPolicy
        }
      ) => {
        try {
          const now = () => this.source.monotonicNow()
          const operation = normalizeOperationOptions(options, now)
          assertSubscribeAdmission(operation, now)
          this.source.assertCurrent?.()
          return publicOrderedRecordingStartResult(
            await handle.start(this.orderedMembers(members), {
              ...operation,
              recording: options.recording,
              delivery: resolveStreamPolicy(options.stream ?? 'balanced')
            })
          )
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      status: async () => {
        try {
          const status = await handle.status()
          if (status.state === 'result')
            return Object.freeze({ state: status.state, result: publicOrderedRecordingStartResult(status.result) })
          if (
            status.state === 'pending' ||
            status.state === 'unknown' ||
            status.state === 'failed' ||
            status.state === 'retired'
          )
            return Object.freeze({ state: status.state })
          throw contractError('protocol.malformed', 'gatt', 'public-gatt.ordered-recording-start')
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      retire: async () => {
        try {
          await handle.retire()
        } catch (error) {
          throw rehydratePublicError(error)
        }
      },
      ...(recoveryHandle
        ? {
            recoveryHandle: () => {
              try {
                return recoveryHandle.call(handle)
              } catch (error) {
                throw rehydratePublicError(error)
              }
            }
          }
        : {}),
      ...(handle.reserveMarkerWrite
        ? {
            reserveMarkerWrite: () =>
              runPublic(() => {
                const marker = handle.reserveMarkerWrite?.()
                if (!marker)
                  throw rehydratePublicError(
                    contractError('capability.unsupported', 'gatt', 'public-gatt.ordered-marker')
                  )
                return this.publicMarkerHandle(marker)
              })
          }
        : {})
    })
  }

  async subscribeOrdered(
    members: readonly GattOrderedMember[],
    options: OperationOptions & { readonly stream?: StreamPolicy } = {}
  ): Promise<GattOrderedSetup> {
    try {
      const now = () => this.source.monotonicNow()
      const operation = normalizeOperationOptions(options, now)
      assertSubscribeAdmission(operation, now)
      this.source.assertCurrent?.()
      const entries = this.orderedMembers(members)
      if (this.source.subscribeOrdered === undefined)
        throw contractError('capability.unsupported', 'gatt', 'public-gatt.ordered')
      const setup = await this.source.subscribeOrdered(entries, {
        ...operation,
        delivery: resolveStreamPolicy(options.stream ?? 'balanced')
      })
      const source = setup.stream
      const reserve = source.reserveMarkerWrite
      const increase = source.increaseByteCapacity
      const stream: GattOrderedStream = {
        get limits() {
          return publicOrderedStreamLimits(source.limits)
        },
        ...(increase
          ? {
              increaseByteCapacity: async (maxBytes: number) => {
                try {
                  const result = await increase.call(source, maxBytes)
                  return Object.freeze({
                    previousMaxBytes: result.previousMaxBytes,
                    maxBytes: result.maxBytes,
                    maximumMaxBytes: result.maximumMaxBytes
                  })
                } catch (error) {
                  throw rehydratePublicError(error)
                }
              }
            }
          : {}),
        close: () => runPublicAsync(async () => publicOrderedCleanup(await source.close())),
        discard: () => runPublicAsync(async () => publicOrderedCleanup(await source.discard())),
        [Symbol.asyncIterator]: () =>
          runPublic(() => {
            const iterator = source[Symbol.asyncIterator]()
            const returnIterator = iterator.return?.bind(iterator)
            const throwIterator = iterator.throw?.bind(iterator)
            return {
              next: (...args: [] | [undefined]) =>
                runPublicAsync(async () => {
                  const result = await iterator.next(...args)
                  return publicOrderedIteratorResult(result)
                }),
              ...(returnIterator
                ? {
                    return: (value?: undefined) =>
                      runPublicAsync(async () => publicOrderedIteratorResult(await returnIterator(value)))
                  }
                : {}),
              ...(throwIterator
                ? {
                    throw: (error?: unknown) =>
                      runPublicAsync(async () => publicOrderedIteratorResult(await throwIterator(error)))
                  }
                : {})
            }
          }),
        ...(reserve
          ? {
              reserveMarkerWrite: () =>
                runPublic(() => {
                  return this.publicMarkerHandle(reserve.call(source))
                })
            }
          : {})
      }
      registerOrderedStreamOrigin(stream, source)
      return publicOrderedSetup(setup, stream)
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  snapshot(): GattDatabaseSnapshot {
    runPublic(() => this.source.assertCurrent?.())
    return this.snapshotValue
  }
}

class PublicGattService implements GattService {
  readonly uuid: string
  readonly occurrence: number
  readonly primary: boolean | null
  readonly includedServices: readonly GattServiceReference[] | null
  readonly restriction?: import('../backend-contract/gatt').GattServiceRestriction
  readonly characteristics: readonly GattCharacteristic[]
  private readonly characteristicLookup: ReadonlyMap<string, readonly GattCharacteristic[]>

  constructor(
    indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['services'][number]>,
    source: PublicGattDatabaseSource,
    characteristics: readonly {
      readonly characteristic: OccurrenceRecord<PortableGattDatabaseSnapshot['characteristics'][number]>
      readonly descriptors: readonly OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>[]
    }[],
    provisionalOwner: ProvisionalGattSubscriptionOwner
  ) {
    this.uuid = normalizeUuid(indexedRecord.record.path.serviceUuid)
    this.occurrence = indexedRecord.occurrence
    this.primary =
      indexedRecord.record.primary == null
        ? null
        : optionalBoolean(indexedRecord.record.primary, false, 'public-gatt.service.primary')
    this.includedServices =
      indexedRecord.record.includedServices == null
        ? null
        : Object.freeze(
            indexedRecord.record.includedServices.map(reference =>
              Object.freeze({ uuid: normalizeUuid(reference.uuid), occurrence: occurrenceNumber(reference.occurrence) })
            )
          )
    this.restriction = indexedRecord.record.restriction
    this.characteristics = Object.freeze(
      characteristics.map(
        entry => new PublicGattCharacteristic(this, source, entry.characteristic, entry.descriptors, provisionalOwner)
      )
    )
    this.characteristicLookup = createLookup(this.characteristics, characteristic => characteristic.uuid)
    Object.freeze(this)
  }

  characteristic(uuid: UuidInput, selector: OccurrenceSelector = {}): GattCharacteristic {
    return selectOne(this.characteristicsByUuid(uuid), selector, 'public-gatt.service.characteristic')
  }

  characteristicsByUuid(uuid: UuidInput): readonly GattCharacteristic[] {
    return this.characteristicLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }
}

class PublicGattCharacteristic implements GattCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly service: GattService
  readonly properties: GattCharacteristicProperties
  readonly access: GattAccessRequirements
  readonly descriptors: readonly GattDescriptor[]
  private readonly descriptorLookup: ReadonlyMap<string, readonly GattDescriptor[]>

  constructor(
    service: PublicGattService,
    private readonly source: PublicGattDatabaseSource,
    private readonly indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['characteristics'][number]>,
    descriptorRecords: readonly OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>[],
    private readonly provisionalOwner: ProvisionalGattSubscriptionOwner
  ) {
    this.service = service
    orderedCharacteristicPaths.set(this, indexedRecord.record.path)
    this.uuid = normalizeUuid(indexedRecord.record.path.characteristicUuid)
    this.occurrence = indexedRecord.occurrence
    this.properties = normalizeCharacteristicProperties(indexedRecord.record)
    this.access = normalizeAccess(indexedRecord.record.access, 'public-gatt.characteristic.access')
    this.descriptors = Object.freeze(
      descriptorRecords.map(recordValue => new PublicGattDescriptor(this, source, recordValue))
    )
    this.descriptorLookup = createLookup(this.descriptors, descriptor => descriptor.uuid)
    Object.freeze(this)
  }

  read(options: OperationOptions = {}): Promise<Uint8Array> {
    return this.run(() =>
      this.source.read(
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
    )
  }

  readReceipt(options: OperationOptions = {}): Promise<GattReadReceipt> {
    return this.run(() => {
      if (this.source.readReceipt === undefined) {
        throw contractError('capability.unsupported', 'gatt', 'public-gatt.characteristic.read-receipt')
      }
      return this.source.readReceipt(
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
    })
  }

  write(value: Uint8Array, options: GattWriteOptions = {}): Promise<GattWriteReceipt> {
    return this.run(() =>
      this.source.write(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: resolveWriteMode(this.properties, options.response)
      })
    )
  }

  writeWhenReady(value: Uint8Array, options: OperationOptions = {}): Promise<GattWriteReceipt> {
    return this.run(() => {
      if (!this.properties.writeWithoutResponse) {
        throw contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write-when-ready')
      }
      const writeWhenReady = this.source.writeWhenReady
      if (writeWhenReady === undefined) {
        throw contractError('capability.unsupported', 'connection', 'public-gatt.characteristic.write-when-ready')
      }
      return writeWhenReady.call(this.source, this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: 'without-response'
      })
    })
  }

  writeLong(value: Uint8Array, options: LongWriteOptions = {}): Promise<GattLongWriteReceipt> {
    return this.run(() =>
      this.source.writeLong(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: resolveWriteMode(this.properties, options.response),
        chunkSize: options.chunkSize
      })
    )
  }

  acquireWrite(options: OperationOptions = {}): Promise<GattAcquiredWriter> {
    return this.run(async () => {
      const acquire = this.source.acquireWrite
      if (acquire === undefined) throw contractError('capability.unsupported', 'gatt', 'public-gatt.acquire-write')
      const writer = await acquire.call(
        this.source,
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
      return Object.freeze({
        mtuBytes: writer.mtuBytes,
        write: (value: Uint8Array, writeOptions: OperationOptions = {}) =>
          this.run(() => {
            if (!(value instanceof Uint8Array))
              throw contractError('argument.invalid', 'gatt', 'public-gatt.acquired-write.bytes')
            const owned = new Uint8Array(value)
            return writer.write(
              owned,
              normalizeOperationOptions(writeOptions, () => this.source.monotonicNow())
            )
          }),
        close: () => rehydrateCleanup(writer.close())
      })
    })
  }

  acquireNotifications(
    options: OperationOptions & { readonly stream?: StreamPolicy } = {}
  ): Promise<GattAcquiredNotifications> {
    return this.run(async () => {
      const acquire = this.source.acquireNotifications
      if (acquire === undefined) throw contractError('capability.unsupported', 'gatt', 'public-gatt.acquire-notify')
      const subscription = await acquire.call(this.source, this.indexedRecord.record.path, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        delivery: resolveStreamPolicy(options.stream ?? 'balanced')
      })
      return Object.freeze({
        mtuBytes: subscription.mtuBytes,
        values: mapGattValueStream(subscription.values, () => this.source.monotonicNow()),
        close: () => rehydrateCleanup(subscription.close())
      })
    })
  }

  async subscribe(options: GattSubscribeOptions = {}): Promise<GattSubscription> {
    return this.run(async () => {
      const now = () => this.source.monotonicNow()
      const operation = normalizeOperationOptions(options, now)
      assertSubscribeAdmission(operation, now)
      await awaitWithOperationAdmission(
        runWithCleanup(
          async () => undefined,
          () => this.provisionalOwner.retryPending(this.indexedRecord.record.path)
        ),
        operation,
        now,
        'public-gatt.subscribe.provisional-cleanup'
      )
      assertSubscribeAdmission(operation, now)
      this.source.assertCurrent?.()
      const selectedDelivery = resolveDelivery(this.properties, options.delivery)
      if (
        this.source.deliverySelection === 'unknown' &&
        typeof options.delivery === 'string' &&
        options.delivery.startsWith('require-')
      ) {
        throw contractError('capability.limited', 'gatt', 'public-gatt.subscribe.delivery-selection')
      }
      const budget = resolveStreamPolicy(options.stream ?? 'balanced')
      const subscription = await this.source.subscribe(this.indexedRecord.record.path, {
        ...operation,
        delivery: budget,
        deliveryMode:
          options.delivery ??
          (selectedDelivery === 'notification'
            ? 'prefer-notification'
            : selectedDelivery === 'indication'
              ? 'prefer-indication'
              : undefined)
      })
      try {
        const effectiveDelivery = subscription.observedDelivery ?? 'unknown'
        if (
          effectiveDelivery !== 'notification' &&
          effectiveDelivery !== 'indication' &&
          effectiveDelivery !== 'unknown'
        ) {
          throw contractError('protocol.violation', 'gatt', 'public-gatt.subscribe.observed-delivery')
        }
        return Object.freeze({
          requestedDelivery: options.delivery,
          effectiveDelivery,
          values: mapGattValueStream(subscription.values, () => this.source.monotonicNow()),
          remove: () => rehydrateCleanup(subscription.remove())
        })
      } catch (error) {
        return this.provisionalOwner.compensate(subscription, this.indexedRecord.record.path, error)
      }
    })
  }

  async subscribeRecorded(options: GattRecordedSubscribeOptions): Promise<GattRecordedSubscription> {
    return this.run(async () => {
      const now = () => this.source.monotonicNow()
      const operation = normalizeOperationOptions(options, now)
      assertSubscribeAdmission(operation, now)
      await awaitWithOperationAdmission(
        runWithCleanup(
          async () => undefined,
          () => this.provisionalOwner.retryPending(this.indexedRecord.record.path)
        ),
        operation,
        now,
        'public-gatt.subscribe-recorded.provisional-cleanup'
      )
      assertSubscribeAdmission(operation, now)
      this.source.assertCurrent?.()
      const selected = resolveDelivery(this.properties, options.delivery)
      if (this.source.deliverySelection === 'unknown' && options.delivery?.startsWith('require-')) {
        throw contractError('capability.limited', 'gatt', 'public-gatt.subscribe-recorded.delivery-selection')
      }
      const subscribe = this.source.subscribeRecorded
      if (subscribe === undefined)
        throw contractError('capability.unsupported', 'gatt', 'public-gatt.subscribe-recorded')
      const subscription = await subscribe.call(this.source, this.indexedRecord.record.path, {
        ...operation,
        recording: options.recording,
        delivery: resolveStreamPolicy('balanced'),
        deliveryMode:
          options.delivery ??
          (selected === 'notification'
            ? 'prefer-notification'
            : selected === 'indication'
              ? 'prefer-indication'
              : undefined)
      })
      try {
        const effectiveDelivery = subscription.observedDelivery ?? 'unknown'
        if (
          effectiveDelivery !== 'notification' &&
          effectiveDelivery !== 'indication' &&
          effectiveDelivery !== 'unknown'
        ) {
          throw contractError('protocol.violation', 'gatt', 'public-gatt.subscribe-recorded.observed-delivery')
        }
        return Object.freeze({
          subscriptionId: subscription.subscriptionId,
          recordingId: subscription.recordingId,
          changed: mapPublicBoundedAsyncStream(subscription.changed, value =>
            Object.freeze({ recordingId: value.recordingId })
          ),
          requestedDelivery: options.delivery,
          effectiveDelivery,
          remove: () => rehydrateCleanup(subscription.remove())
        })
      } catch (error) {
        return this.provisionalOwner.compensate(subscription, this.indexedRecord.record.path, error)
      }
    })
  }

  async withSubscription<T>(
    options: GattSubscribeOptions,
    action: (subscription: GattSubscription) => Promise<T>
  ): Promise<T> {
    const subscription = await this.subscribe(options)
    return runWithCleanup(
      () => action(subscription),
      () => subscription.remove()
    )
  }

  descriptor(uuid: UuidInput, selector: OccurrenceSelector = {}): GattDescriptor {
    return selectOne(this.descriptorsByUuid(uuid), selector, 'public-gatt.characteristic.descriptor')
  }

  private descriptorsByUuid(uuid: UuidInput): readonly GattDescriptor[] {
    return this.descriptorLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }

  private async run<Value>(operation: () => Promise<Value>): Promise<Value> {
    try {
      this.source.assertCurrent?.()
      return await operation()
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
}

class PublicGattDescriptor implements GattDescriptor {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristic: GattCharacteristic
  readonly properties: GattDescriptorProperties

  constructor(
    characteristic: PublicGattCharacteristic,
    private readonly source: PublicGattDatabaseSource,
    private readonly indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>
  ) {
    this.characteristic = characteristic
    this.uuid = normalizeUuid(indexedRecord.record.path.descriptorUuid)
    this.occurrence = indexedRecord.occurrence
    this.properties = normalizeDescriptorProperties(indexedRecord.record.properties)
    Object.freeze(this)
  }

  read(options: OperationOptions = {}): Promise<Uint8Array> {
    return this.run(() =>
      this.source.readDescriptor(
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
    )
  }

  write(value: Uint8Array, options: DescriptorWriteOptions = {}): Promise<GattWriteReceipt> {
    return this.run(() => {
      if (this.uuid === normalizeUuid('2902')) {
        throw contractError('gatt.cccd-managed', 'gatt', 'public-gatt.descriptor.write-cccd')
      }
      if (!this.properties.write && this.properties.availability.write === 'known') {
        throw contractError('gatt.property-not-supported', 'gatt', 'public-gatt.descriptor.write')
      }
      return this.source.writeDescriptor(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: 'with-response'
      })
    })
  }

  private async run<Value>(operation: () => Promise<Value>): Promise<Value> {
    try {
      this.source.assertCurrent?.()
      return await operation()
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
}

function normalizeUuid(value: UuidInput): string {
  try {
    if (typeof value === 'string' && value.length === 0) {
      throw new Error('UUID input must be non-empty')
    }
    return String(canonicalUuidInput(value))
  } catch {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', 'public-gatt.uuid'))
  }
}

function occurrenceNumber(value: string): number {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.occurrence'))
  }
  const occurrence = Number(value)
  if (!Number.isSafeInteger(occurrence) || occurrence < 0) {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.occurrence'))
  }
  return occurrence
}

interface OccurrenceRecord<RecordValue> {
  readonly record: RecordValue
  readonly occurrence: number
}

function pathOccurrence(record: {
  readonly path: {
    readonly serviceOccurrence?: string
    readonly characteristicOccurrence?: string
    readonly descriptorOccurrence?: string
  }
}): string | undefined {
  if (record.path.descriptorOccurrence !== undefined) return record.path.descriptorOccurrence
  if (record.path.characteristicOccurrence !== undefined) return record.path.characteristicOccurrence
  return record.path.serviceOccurrence
}

function recordsWithOccurrence<
  RecordValue extends {
    readonly path: {
      readonly serviceUuid?: string
      readonly characteristicUuid?: string
      readonly descriptorUuid?: string
      readonly serviceOccurrence?: string
      readonly characteristicOccurrence?: string
      readonly descriptorOccurrence?: string
    }
  }
>(
  records: readonly RecordValue[],
  key: (record: RecordValue) => string | undefined
): readonly OccurrenceRecord<RecordValue>[] {
  const parsed = records.map(record => {
    const rawValue = key(record)
    const value = rawValue === undefined ? '' : normalizeUuid(rawValue)
    const rawOccurrence = pathOccurrence(record)
    return { record, value, numeric: rawOccurrence === undefined ? null : tryNumericOccurrence(rawOccurrence) }
  })
  const seen = new Map<string, Set<number>>()
  return parsed.map(entry => {
    const occurrence = entry.numeric
    if (occurrence === null) {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.occurrence'))
    }
    const used = seen.get(entry.value) ?? new Set<number>()
    if (used.has(occurrence)) {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.duplicate-occurrence'))
    }
    used.add(occurrence)
    seen.set(entry.value, used)
    return Object.freeze({ record: entry.record, occurrence })
  })
}

function tryNumericOccurrence(value: string): number | null {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) return null
  const occurrence = Number(value)
  return Number.isSafeInteger(occurrence) && occurrence >= 0 ? occurrence : null
}

function createLookup<Value>(
  values: readonly Value[],
  key: (value: Value) => string
): ReadonlyMap<string, readonly Value[]> {
  const lookup = new Map<string, Value[]>()
  for (const value of values) {
    const entries = lookup.get(key(value)) ?? []
    entries.push(value)
    lookup.set(key(value), entries)
  }
  return new Map([...lookup.entries()].map(([entryKey, entryValues]) => [entryKey, Object.freeze(entryValues)]))
}

function selectOne<Value extends { readonly occurrence: number }>(
  values: readonly Value[],
  selector: OccurrenceSelector,
  operation: string
): Value {
  if (selector.occurrence !== undefined && (!Number.isSafeInteger(selector.occurrence) || selector.occurrence < 0)) {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', `${operation}.occurrence`))
  }
  if (values.length === 0) {
    throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
  }
  if (selector.occurrence !== undefined) {
    const selected = values.find(value => value.occurrence === selector.occurrence)
    if (selected === undefined) throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
    return selected
  }
  if (values.length !== 1) throw rehydratePublicError(contractError('gatt.ambiguous-path', 'gatt', operation))
  const selected = values[0]
  if (selected === undefined) throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
  return selected
}

function sameService(
  characteristic: PortableCurrentCharacteristicPath,
  service: PortableGattDatabaseSnapshot['services'][number]['path']
): boolean {
  return (
    normalizeUuid(characteristic.serviceUuid) === normalizeUuid(service.serviceUuid) &&
    characteristic.serviceOccurrence === service.serviceOccurrence
  )
}

function sameCharacteristic(
  descriptor: PortableCurrentDescriptorPath,
  characteristic: PortableCurrentCharacteristicPath
): boolean {
  return (
    sameService(descriptor, characteristic) &&
    normalizeUuid(descriptor.characteristicUuid) === normalizeUuid(characteristic.characteristicUuid) &&
    descriptor.characteristicOccurrence === characteristic.characteristicOccurrence
  )
}

function validateTopology(snapshot: PortableGattDatabaseSnapshot): void {
  const serviceKeys = new Set<string>()
  for (const service of snapshot.services) {
    assertDatabasePath(service.path, snapshot.path, 'public-gatt.service-path')
    const key = `${normalizeUuid(service.path.serviceUuid)}|${service.path.serviceOccurrence}`
    if (serviceKeys.has(key)) {
      throw topologyViolation(
        'public-gatt.duplicate-service-path',
        `Duplicate service path ${normalizeUuid(service.path.serviceUuid)}#${service.path.serviceOccurrence} in the discovered GATT database.`,
        {
          serviceUuid: normalizeUuid(service.path.serviceUuid),
          serviceOccurrence: service.path.serviceOccurrence
        }
      )
    }
    serviceKeys.add(key)
  }
  const characteristicKeys = new Set<string>()
  for (const characteristic of snapshot.characteristics) {
    assertDatabasePath(characteristic.path, snapshot.path, 'public-gatt.characteristic-path')
    const parents = snapshot.services.filter(service => sameService(characteristic.path, service.path))
    if (parents.length !== 1) {
      throw topologyViolation(
        'public-gatt.characteristic-parent',
        `Characteristic path ${normalizeUuid(characteristic.path.serviceUuid)}#${characteristic.path.serviceOccurrence}/${normalizeUuid(characteristic.path.characteristicUuid)}#${characteristic.path.characteristicOccurrence} resolves to ${parents.length} parent services in the discovered GATT database.`,
        {
          serviceUuid: normalizeUuid(characteristic.path.serviceUuid),
          serviceOccurrence: characteristic.path.serviceOccurrence,
          characteristicUuid: normalizeUuid(characteristic.path.characteristicUuid),
          characteristicOccurrence: characteristic.path.characteristicOccurrence
        }
      )
    }
    const key = `${normalizeUuid(characteristic.path.serviceUuid)}|${characteristic.path.serviceOccurrence}|${normalizeUuid(
      characteristic.path.characteristicUuid
    )}|${characteristic.path.characteristicOccurrence}`
    if (characteristicKeys.has(key)) {
      throw topologyViolation(
        'public-gatt.duplicate-characteristic-path',
        `Duplicate characteristic path ${normalizeUuid(characteristic.path.serviceUuid)}#${characteristic.path.serviceOccurrence}/${normalizeUuid(characteristic.path.characteristicUuid)}#${characteristic.path.characteristicOccurrence} in the discovered GATT database.`,
        {
          serviceUuid: normalizeUuid(characteristic.path.serviceUuid),
          serviceOccurrence: characteristic.path.serviceOccurrence,
          characteristicUuid: normalizeUuid(characteristic.path.characteristicUuid),
          characteristicOccurrence: characteristic.path.characteristicOccurrence
        }
      )
    }
    characteristicKeys.add(key)
  }
  const descriptorKeys = new Set<string>()
  for (const descriptor of snapshot.descriptors) {
    assertDatabasePath(descriptor.path, snapshot.path, 'public-gatt.descriptor-path')
    const parents = snapshot.characteristics.filter(characteristic =>
      sameCharacteristic(descriptor.path, characteristic.path)
    )
    if (parents.length !== 1) {
      throw topologyViolation(
        'public-gatt.descriptor-parent',
        `Descriptor path ${normalizeUuid(descriptor.path.serviceUuid)}#${descriptor.path.serviceOccurrence}/${normalizeUuid(descriptor.path.characteristicUuid)}#${descriptor.path.characteristicOccurrence}/${normalizeUuid(descriptor.path.descriptorUuid)}#${descriptor.path.descriptorOccurrence} resolves to ${parents.length} parent characteristics in the discovered GATT database.`,
        {
          serviceUuid: normalizeUuid(descriptor.path.serviceUuid),
          serviceOccurrence: descriptor.path.serviceOccurrence,
          characteristicUuid: normalizeUuid(descriptor.path.characteristicUuid),
          characteristicOccurrence: descriptor.path.characteristicOccurrence,
          descriptorUuid: normalizeUuid(descriptor.path.descriptorUuid),
          descriptorOccurrence: descriptor.path.descriptorOccurrence
        }
      )
    }
    const key = `${normalizeUuid(descriptor.path.serviceUuid)}|${descriptor.path.serviceOccurrence}|${normalizeUuid(descriptor.path.characteristicUuid)}|${descriptor.path.characteristicOccurrence}|${normalizeUuid(descriptor.path.descriptorUuid)}|${descriptor.path.descriptorOccurrence}`
    if (descriptorKeys.has(key)) {
      throw topologyViolation(
        'public-gatt.duplicate-descriptor-path',
        `Duplicate descriptor path ${normalizeUuid(descriptor.path.serviceUuid)}#${descriptor.path.serviceOccurrence}/${normalizeUuid(descriptor.path.characteristicUuid)}#${descriptor.path.characteristicOccurrence}/${normalizeUuid(descriptor.path.descriptorUuid)}#${descriptor.path.descriptorOccurrence} in the discovered GATT database.`,
        {
          serviceUuid: normalizeUuid(descriptor.path.serviceUuid),
          serviceOccurrence: descriptor.path.serviceOccurrence,
          characteristicUuid: normalizeUuid(descriptor.path.characteristicUuid),
          characteristicOccurrence: descriptor.path.characteristicOccurrence,
          descriptorUuid: normalizeUuid(descriptor.path.descriptorUuid),
          descriptorOccurrence: descriptor.path.descriptorOccurrence
        }
      )
    }
    descriptorKeys.add(key)
  }
  for (const service of snapshot.services) {
    for (const included of service.includedServices ?? []) {
      const includedKey = `${normalizeUuid(included.uuid)}|${included.occurrence}`
      if (!serviceKeys.has(includedKey)) {
        throw rehydratePublicError(
          contractError('protocol.violation', 'gatt', 'public-gatt.included-service-unresolved')
        )
      }
    }
  }
}

/**
 * Finding 182 diagnosability: a topology rejection names the offending
 * path (uuids + occurrences) in the error's platform detail, never only a
 * bare code — against real hardware a bare
 * `duplicate-characteristic-path` names nothing to look at.
 */
function topologyViolation(
  operation: string,
  safeMessage: string,
  metadata: { readonly [key: string]: string }
): unknown {
  const code = operation.startsWith('public-gatt.') ? operation.slice('public-gatt.'.length) : operation
  return rehydratePublicError(
    contractError('protocol.violation', 'gatt', operation, {
      domain: 'gatt',
      code,
      safeMessage,
      metadata: Object.freeze({ ...metadata })
    })
  )
}

function assertDatabasePath(
  path:
    | PortableCurrentCharacteristicPath
    | PortableCurrentDescriptorPath
    | PortableGattDatabaseSnapshot['services'][number]['path'],
  database: PortableGattDatabaseSnapshot['path'],
  operation: string
): void {
  if (
    path.attachmentId !== database.attachmentId ||
    path.peerId !== database.peerId ||
    path.connectionId !== database.connectionId ||
    path.ownerLeaseId !== database.ownerLeaseId ||
    path.connectionGeneration !== database.connectionGeneration ||
    path.databaseId !== database.databaseId ||
    path.databaseGeneration !== database.databaseGeneration
  ) {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', operation))
  }
}

function assertBooleanProperty(value: unknown, operation: string): asserts value is boolean {
  if (typeof value !== 'boolean') {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', operation))
  }
}

function optionalBoolean(value: unknown, fallback: boolean, operation: string): boolean {
  if (value === undefined) return fallback
  assertBooleanProperty(value, operation)
  return value
}

function assertAvailabilityToken(value: unknown, operation: string): asserts value is 'known' | 'unknown' {
  if (value !== 'known' && value !== 'unknown') {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', operation))
  }
}

function optionalAvailabilityToken(
  value: unknown,
  fallback: 'known' | 'unknown',
  operation: string
): 'known' | 'unknown' {
  if (value === undefined) return fallback
  assertAvailabilityToken(value, operation)
  return value
}

function assertAccessToken(value: unknown, operation: string): asserts value is GattAccessRequirements['read'] {
  if (
    value !== 'none' &&
    value !== 'encrypted' &&
    value !== 'authenticated' &&
    value !== 'authorized' &&
    value !== 'unknown'
  ) {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', operation))
  }
}

function assertPlainRecord(value: unknown, operation: string): asserts value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw rehydratePublicError(contractError('protocol.violation', 'gatt', operation))
  }
}

function normalizeAccess(access: unknown, operation: string): GattAccessRequirements {
  if (access === undefined) return Object.freeze({ read: 'unknown', write: 'unknown' })
  assertPlainRecord(access, operation)
  const read = Reflect.get(access, 'read')
  const write = Reflect.get(access, 'write')
  assertAccessToken(read, `${operation}.read`)
  assertAccessToken(write, `${operation}.write`)
  return Object.freeze({ read, write })
}

function normalizeCharacteristicProperties(
  record: PortableGattDatabaseSnapshot['characteristics'][number]
): GattCharacteristicProperties {
  assertBooleanProperty(record.properties.read, 'public-gatt.characteristic.properties.read')
  assertBooleanProperty(record.properties.writeWithResponse, 'public-gatt.characteristic.properties.writeWithResponse')
  assertBooleanProperty(
    record.properties.writeWithoutResponse,
    'public-gatt.characteristic.properties.writeWithoutResponse'
  )
  assertBooleanProperty(record.properties.notify, 'public-gatt.characteristic.properties.notify')
  const broadcast = optionalBoolean(
    record.properties.broadcast,
    false,
    'public-gatt.characteristic.properties.broadcast'
  )
  const authenticatedSignedWrites = optionalBoolean(
    record.properties.authenticatedSignedWrites,
    false,
    'public-gatt.characteristic.properties.authenticatedSignedWrites'
  )
  const indicate = optionalBoolean(record.properties.indicate, false, 'public-gatt.characteristic.properties.indicate')
  const extendedProperties = optionalBoolean(
    record.properties.extendedProperties,
    false,
    'public-gatt.characteristic.properties.extendedProperties'
  )
  const reliableWrite = optionalBoolean(
    record.properties.reliableWrite,
    false,
    'public-gatt.characteristic.properties.reliableWrite'
  )
  const writableAuxiliaries = optionalBoolean(
    record.properties.writableAuxiliaries,
    false,
    'public-gatt.characteristic.properties.writableAuxiliaries'
  )
  const availabilityInput = record.properties.availability
  if (availabilityInput !== undefined) assertPlainRecord(availabilityInput, 'public-gatt.characteristic.availability')
  const availability: GattCharacteristicPropertyAvailability = Object.freeze({
    broadcast: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'broadcast'),
      record.properties.broadcast === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.broadcast'
    ),
    read: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'read'),
      'known',
      'public-gatt.characteristic.availability.read'
    ),
    writeWithResponse: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'writeWithResponse'),
      'known',
      'public-gatt.characteristic.availability.writeWithResponse'
    ),
    writeWithoutResponse: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'writeWithoutResponse'),
      'known',
      'public-gatt.characteristic.availability.writeWithoutResponse'
    ),
    authenticatedSignedWrites: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'authenticatedSignedWrites'),
      record.properties.authenticatedSignedWrites === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.authenticatedSignedWrites'
    ),
    notify: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'notify'),
      'known',
      'public-gatt.characteristic.availability.notify'
    ),
    indicate: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'indicate'),
      record.properties.indicate === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.indicate'
    ),
    extendedProperties: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'extendedProperties'),
      record.properties.extendedProperties === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.extendedProperties'
    ),
    reliableWrite: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'reliableWrite'),
      record.properties.reliableWrite === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.reliableWrite'
    ),
    writableAuxiliaries: optionalAvailabilityToken(
      availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'writableAuxiliaries'),
      record.properties.writableAuxiliaries === undefined ? 'unknown' : 'known',
      'public-gatt.characteristic.availability.writableAuxiliaries'
    )
  })
  return Object.freeze({
    broadcast,
    read: record.properties.read,
    writeWithResponse: record.properties.writeWithResponse,
    writeWithoutResponse: record.properties.writeWithoutResponse,
    authenticatedSignedWrites,
    notify: record.properties.notify,
    indicate,
    extendedProperties,
    reliableWrite,
    writableAuxiliaries,
    availability
  })
}

function normalizeDescriptorProperties(properties: unknown): GattDescriptorProperties {
  if (properties === undefined) {
    return Object.freeze({
      read: false,
      write: false,
      availability: Object.freeze({ read: 'unknown', write: 'unknown' }),
      access: Object.freeze({ read: 'unknown', write: 'unknown' })
    })
  }
  assertPlainRecord(properties, 'public-gatt.descriptor.properties')
  const read = Reflect.get(properties, 'read')
  const write = Reflect.get(properties, 'write')
  assertBooleanProperty(read, 'public-gatt.descriptor.properties.read')
  assertBooleanProperty(write, 'public-gatt.descriptor.properties.write')
  const availabilityInput = Reflect.get(properties, 'availability')
  if (availabilityInput !== undefined) {
    assertPlainRecord(availabilityInput, 'public-gatt.descriptor.availability')
  }
  return Object.freeze({
    read,
    write,
    availability: Object.freeze({
      read: optionalAvailabilityToken(
        availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'read'),
        'unknown',
        'public-gatt.descriptor.availability.read'
      ),
      write: optionalAvailabilityToken(
        availabilityInput === undefined ? undefined : Reflect.get(availabilityInput, 'write'),
        'unknown',
        'public-gatt.descriptor.availability.write'
      )
    }),
    access: normalizeAccess(Reflect.get(properties, 'access'), 'public-gatt.descriptor.access')
  })
}

function resolveWriteMode(
  properties: GattCharacteristicProperties,
  response: GattWriteOptions['response'] = 'automatic'
): 'with-response' | 'without-response' {
  if (response === 'required') {
    if (!properties.writeWithResponse)
      throw rehydratePublicError(
        contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write-required')
      )
    return 'with-response'
  }
  if (response === 'not-required') {
    if (!properties.writeWithoutResponse)
      throw rehydratePublicError(
        contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write-not-required')
      )
    return 'without-response'
  }
  if (response !== 'automatic' && response !== undefined) {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', 'public-gatt.characteristic.write-response'))
  }
  if (properties.writeWithResponse) return 'with-response'
  if (properties.writeWithoutResponse) return 'without-response'
  throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write'))
}

function resolveDelivery(
  properties: GattCharacteristicProperties,
  requested: GattSubscribeOptions['delivery']
): GattDelivery {
  if (requested === 'require-notification' && !properties.notify) {
    throw rehydratePublicError(
      contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe.notification')
    )
  }
  if (requested === 'require-indication' && !properties.indicate) {
    throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe.indication'))
  }
  if (requested === 'require-notification') return 'notification'
  if (requested === 'require-indication') return 'indication'
  if (requested === 'prefer-indication' && properties.indicate) return 'indication'
  if (requested !== undefined && requested !== 'prefer-notification' && requested !== 'prefer-indication') {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', 'public-gatt.subscribe.delivery'))
  }
  if (properties.notify) return 'notification'
  if (properties.indicate) return 'indication'
  throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe'))
}

function mapGattValueStream(
  source: import('../manager/consumer-handles').PortableBoundedAsyncStream<
    import('../manager/consumer-handles').PortableNotificationValue
  >,
  now: () => number
): GattValueStream {
  let sequence = 1
  return mapPublicBoundedAsyncStream(source, value => {
    const delivery = value.delivery
    if (delivery !== 'notification' && delivery !== 'indication' && delivery !== 'unknown') {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.notification.delivery'))
    }
    if (
      (value.sourceGeneration === undefined) !== (value.sourceOrdinal === undefined) ||
      (value.sourceGeneration !== undefined &&
        (!isCanonicalIngressOrdinal(value.sourceGeneration) || !isCanonicalIngressOrdinal(value.sourceOrdinal)))
    ) {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.notification.source-order'))
    }
    const provenance = value.provenance ?? 'unknown'
    if (provenance !== 'notification' && provenance !== 'read-or-notification' && provenance !== 'unknown') {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.notification.provenance'))
    }
    const observedAtMonotonicMs = value.observedAtMonotonicMs ?? now()
    if (!Number.isFinite(observedAtMonotonicMs) || observedAtMonotonicMs < 0) {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.notification.timestamp'))
    }
    const valueSequence = value.sequence ?? sequence++
    if (!Number.isSafeInteger(valueSequence) || valueSequence < 1) {
      throw rehydratePublicError(contractError('protocol.violation', 'gatt', 'public-gatt.notification.sequence'))
    }
    return Object.freeze({
      value: new Uint8Array(value.value),
      delivery,
      provenance,
      ...(value.sourceGeneration === undefined
        ? {}
        : { sourceGeneration: value.sourceGeneration, sourceOrdinal: value.sourceOrdinal }),
      observedAtMonotonicMs,
      sequence: valueSequence,
      ...(value.receipt === undefined ? {} : { receipt: Object.freeze({ ...value.receipt }) })
    })
  })
}

function emptyChangedStream(): PublicBoundedAsyncStream<GattDatabaseChangedEvent> {
  const emptyCleanup = toPublicCleanupRecord({ state: 'released', failures: [] })
  return Object.freeze({
    limits: Object.freeze({ itemCapacity: 1, byteCapacity: 2, reservedControlCapacity: 1 }),
    overflowPolicy: 'error',
    [Symbol.asyncIterator]() {
      return {
        async next(): Promise<IteratorResult<PublicStreamItem<GattDatabaseChangedEvent>, undefined>> {
          return { done: true, value: undefined }
        },
        async return(): Promise<IteratorResult<PublicStreamItem<GattDatabaseChangedEvent>, undefined>> {
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    },
    close: async () => emptyCleanup
  })
}

function snapshotGattDatabaseChangedEvent(event: GattDatabaseChangedEvent): GattDatabaseChangedEvent {
  return Object.freeze({
    previousGeneration: event.previousGeneration,
    reason: event.reason,
    affectedHandleRange:
      event.affectedHandleRange === null
        ? null
        : Object.freeze({
            start: event.affectedHandleRange.start,
            end: event.affectedHandleRange.end
          })
  })
}

function rehydrateCleanup(operation: Promise<CleanupRecord>): Promise<CleanupRecord> {
  return operation.then(toPublicCleanupRecord).catch(error => {
    throw rehydratePublicError(error)
  })
}

async function runPublicAsync<Value>(operation: () => Promise<Value>): Promise<Value> {
  try {
    return await operation()
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function runPublic<Value>(operation: () => Value): Value {
  try {
    return operation()
  } catch (error) {
    throw rehydratePublicError(error)
  }
}
