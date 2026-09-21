// src/backends/winrt/winrt-backend.ts

import type {
  AdapterBackend,
  BackendAttachment,
  BackendAttachmentRequest,
  BackendConnection,
  BackendEvent,
  BleCentralBackend,
  ConnectionBackend,
  ConnectionLease,
  ConnectionOptions,
  GattBackend,
  ResourceCounters,
  ScanLease,
  ScannerBackend
} from '../../backend-contract/backend'
import {
  deviceIdentity,
  type AdvertisementObservation,
  type OwnerScanOptions
} from '../../backend-contract/advertisement'
import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry,
  type FeatureRegistry
} from '../../backend-contract/capabilities'
import {
  BackendContractError,
  contractError,
  type CleanupFailure,
  type CleanupRecord
} from '../../backend-contract/errors'
import type { CharacteristicPath, DescriptorPath } from '../../backend-contract/gatt'
import { attachmentRecordsEqual } from '../../backend-contract/identity'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'
import type {
  AdapterStateSnapshot,
  AdapterStateWatch,
  AttachmentRecord,
  HostNeutralBackendIdentity
} from '../../backend-contract/identity'
import type { BackendOperationDispatch } from '../../backend-contract/operations'
import {
  capacity,
  canonicalUuid,
  createAttachmentBoundIdFactory,
  monotonicTimestamp,
  negotiateCoreVersions,
  opaqueId,
  type BackendInstanceId,
  type ClientId,
  type ConnectionId,
  type GenerationId,
  type LeaseId,
  type OwnedBytes,
  type PeerId,
  type ScanSessionId,
  type ScanShareToken,
  type Uuid
} from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream, type CoreStreamTerminalReason } from '../../core/bounded-stream'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import {
  WinRtConnection,
  WinRtConnectionLease,
  WinRtGattDatabase,
  WinRtBackendSubscription,
  WinRtScanLease,
  advertisementByteLength,
  cleanupFailure,
  matchesScan,
  releasedCleanup
} from './winrt-handles'
import { WinRtGattOperations } from './winrt-gatt-operations'
import { isWinRtSecurityBoundary, WinRtSecurityBackend } from './winrt-security'
import {
  assertWinRtOperationAdmission,
  broadcastWinRtEvent,
  combineWinRtCleanup,
  WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
  timedOutWinRtCleanup,
  waitForWinRtSettlement,
  waitForWinRtValue,
  winRtPlatformError,
  winRtResourceCounters
} from './winrt-backend-helpers'
import { assertWinRtAdapterReady, winRtAdapterIsReady, winRtAdapterState } from './winrt-adapter-state'
import { WinRtOperationDispatcher, type WinRtOperationDispatch } from './winrt-operation-dispatcher'
import {
  WINRT_BACKEND_ID,
  WINRT_IMPLEMENTATION_VERSION,
  WINRT_PLATFORM_ID,
  adapterDescriptor,
  winRtCompatibility
} from './winrt-provider'
import {
  validateWinRtConnectionLossRecord,
  validateWinRtDatabaseChangedRecord,
  validateWinRtScanTerminalRecord,
  validateWinRtAdapterSnapshot
} from './winrt-boundary'
import type {
  WinRtAdapterRecord,
  WinRtAsyncOperation,
  WinRtAdapterSnapshot,
  WinRtBoundary,
  WinRtCancellationState,
  WinRtCharacteristicAddress,
  WinRtConnectionLossRecord,
  WinRtDatabaseChangedRecord,
  WinRtScanTerminalRecord
} from './winrt-boundary'
import { invalidateWinRtPhysicalSubscription } from './winrt-subscription-runtime'
import { diagnosticWinRtScanPlan, planWinRtScan } from './winrt-scan-planner'
import { trustedServiceUuidFilter } from '../scan-planning/service-uuid-scan-planner'

const eventLimits = Object.freeze({
  itemCapacity: capacity(64),
  byteCapacity: capacity(64 * 1024),
  reservedControlCapacity: capacity(1)
})
const adapterStateLimits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})
type WinRtDisconnectOutcome = { readonly state: 'released' } | { readonly state: 'failed'; readonly error: unknown }

function createWinRtFeatureRegistry(securityAvailable: boolean): FeatureRegistry {
  const registrations = [
    createBackendOperationCapabilityRegistration({
      implementationVersion: WINRT_IMPLEMENTATION_VERSION,
      sourceDigest: 'winrt-direct-connection-v1',
      tckSuiteId: 'capability.catalog-v2',
      requiredScenarioIds: ['scenario.scan-connect-discover-read-notify-destroy']
    })
  ]
  if (securityAvailable) {
    registrations.push(
      ...[
        BUILT_IN_FEATURE_IDS.securityState,
        BUILT_IN_FEATURE_IDS.securityPair,
        BUILT_IN_FEATURE_IDS.securityCancelPairing,
        BUILT_IN_FEATURE_IDS.securityUnpair
      ].map(id =>
        createBackendOperationCapabilityRegistration({
          id,
          implementationVersion: WINRT_IMPLEMENTATION_VERSION,
          sourceDigest: `winrt-${id.replace(':', '-')}-v1`,
          tckSuiteId: 'tck.feature.security.winrt',
          requiredScenarioIds: ['security.state-pair-cancel-unpair'],
          operation: `${id}.invoke-without-security-backend`
        })
      )
    )
  }
  return createFeatureRegistry(registrations)
}

function pendingWinRtConnectionCleanup(operation = 'winrt.connection.dispatcher-idle'): CleanupRecord {
  return timedOutWinRtCleanup('connection', operation)
}

function winRtEnablementCleanupTimedOut(cleanup: CleanupRecord): boolean {
  return cleanup.failures.some(failure => failure.error.operation === 'winrt.gatt.start-notify.enablement')
}

function pendingWinRtOperationCleanup(operation: string): CleanupRecord {
  return timedOutWinRtCleanup('operation-quarantine', operation)
}

export interface WinRtScanConsumer {
  readonly scanSessionId: ScanSessionId<string, string>
  readonly leaseId: LeaseId<string, string>
  readonly shareToken: ScanShareToken<string, string> | null
  readonly options: OwnerScanOptions<string, string>
  readonly stream: CoreBoundedStream<AdvertisementObservation<string>>
  abort: (() => void) | null
  deadlineTimer: ReturnType<typeof setTimeout> | null
  released: boolean
}

interface WinRtScanGroup {
  readonly ownerLeaseId: LeaseId<string, string>
  readonly shareToken: ScanShareToken<string, string> | null
  readonly scanToken: string
  /** The owner-selected session remains stable even if the owner releases before joined consumers. */
  readonly scanSessionId: ScanSessionId<string, string>
  readonly consumers: Map<string, WinRtScanConsumer>
  state: 'starting' | 'active' | 'stopping' | 'cleanup-pending'
  /** A matching native terminal proves this group no longer owns a watcher. */
  nativeTerminalReceived: boolean
  startTerminalError: Error | null
  startTerminalShouldTerminalize: boolean
  startInvocationActive: boolean
  startDispatch: WinRtOperationDispatch<void> | null
  stopResult: Promise<CleanupRecord> | null
  stopSettlement: Promise<CleanupRecord> | null
}

interface WinRtPendingConnect {
  dispatch: WinRtOperationDispatch<void> | null
  physicalSettlement: Promise<void> | null
  terminalError: Error | null
}

interface WinRtPendingConnectionOperation {
  readonly operationName: string
  readonly physicalSettlement: Promise<void>
}

/** A fully validated native advertisement that can safely enter backend state and public observations. */
interface ValidatedWinRtAdvertisement {
  readonly scanToken: string
  readonly nativePeerId: string
  readonly localName: string | null
  readonly rssi: number | null
  readonly serviceUuids: readonly Uuid[] | null
  readonly connectable: boolean | null
}

const minimumWinRtRssi = -32768
const maximumWinRtRssi = 32767

function invalidWinRtAdvertisement(message: string): Error {
  return new Error(`WinRT advertisement ${message}`)
}

function requiredWinRtAdvertisementField(advertisement: object, name: string): unknown {
  if (!Object.prototype.hasOwnProperty.call(advertisement, name)) {
    throw invalidWinRtAdvertisement(`is missing required field ${name}`)
  }
  return Reflect.get(advertisement, name)
}

function nullableWinRtAdvertisementString(value: unknown, name: string): string | null {
  if (value === null) {
    return null
  }
  if (typeof value !== 'string') {
    throw invalidWinRtAdvertisement(`field ${name} must be a string or null`)
  }
  return value
}

function nullableWinRtAdvertisementRssi(value: unknown): number | null {
  if (value === null) {
    return null
  }
  if (
    typeof value !== 'number' ||
    !Number.isSafeInteger(value) ||
    value < minimumWinRtRssi ||
    value > maximumWinRtRssi
  ) {
    throw invalidWinRtAdvertisement('field rssi must be a signed 16-bit integer or null')
  }
  return value
}

function nullableWinRtAdvertisementServiceUuids(value: unknown): readonly Uuid[] | null {
  if (value === null) {
    return null
  }
  if (!Array.isArray(value)) {
    throw invalidWinRtAdvertisement('field serviceUuids must be an array or null')
  }
  const serviceUuids: Uuid[] = []
  for (const serviceUuid of value) {
    if (typeof serviceUuid !== 'string') {
      throw invalidWinRtAdvertisement('field serviceUuids must contain only strings')
    }
    serviceUuids.push(canonicalUuid(serviceUuid))
  }
  return Object.freeze(serviceUuids)
}

function nullableWinRtAdvertisementConnectable(value: unknown): boolean | null {
  if (value === null) {
    return null
  }
  if (typeof value !== 'boolean') {
    throw invalidWinRtAdvertisement('field connectable must be a boolean or null')
  }
  return value
}

/**
 * Treats the native callback payload as untrusted runtime input before it can
 * affect peer identity maps, observation matching, or scan delivery.
 */
function validateWinRtAdvertisement(advertisement: unknown): ValidatedWinRtAdvertisement {
  if (typeof advertisement !== 'object' || advertisement === null || Array.isArray(advertisement)) {
    throw invalidWinRtAdvertisement('must be a non-array object')
  }
  const scanToken = requiredWinRtAdvertisementField(advertisement, 'scanToken')
  if (typeof scanToken !== 'string' || scanToken.length === 0) {
    throw invalidWinRtAdvertisement('field scanToken must be a non-empty string')
  }
  const nativePeerId = requiredWinRtAdvertisementField(advertisement, 'nativePeerId')
  if (typeof nativePeerId !== 'string' || nativePeerId.length === 0) {
    throw invalidWinRtAdvertisement('field nativePeerId must be a non-empty string')
  }
  return Object.freeze({
    scanToken,
    nativePeerId,
    localName: nullableWinRtAdvertisementString(
      requiredWinRtAdvertisementField(advertisement, 'localName'),
      'localName'
    ),
    rssi: nullableWinRtAdvertisementRssi(requiredWinRtAdvertisementField(advertisement, 'rssi')),
    serviceUuids: nullableWinRtAdvertisementServiceUuids(
      requiredWinRtAdvertisementField(advertisement, 'serviceUuids')
    ),
    connectable: nullableWinRtAdvertisementConnectable(requiredWinRtAdvertisementField(advertisement, 'connectable'))
  })
}

export interface WinRtConnectionRecord {
  readonly attachment: AttachmentRecord<string>
  readonly nativePeerId: string
  readonly peerId: PeerId<string>
  readonly connectionId: ConnectionId<string, string>
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  /**
   * The generation the native link was dialled with (FX1B). Joined leases
   * carry their own `connectionGeneration` but share the dial generation,
   * so a link drop still matches after the dialling owner released.
   */
  readonly dialGeneration: GenerationId<'connection-generation', string>
  readonly ownerLeaseId: LeaseId<string, string>
  state: 'connecting' | 'connected' | 'disconnecting' | 'disconnected' | 'lost'
  gattRevision: number
  database: WinRtGattDatabase | null
  lease: WinRtConnectionLease | null
  pendingConnect: WinRtPendingConnect | null
  readonly pendingOperations: Map<BackendOperationDispatch<string, unknown>['handle'], WinRtPendingConnectionOperation>
  disconnectResult: Promise<CleanupRecord> | null
  disconnectSettlement: Promise<WinRtDisconnectOutcome> | null
}

export interface WinRtPhysicalSubscription {
  readonly key: string
  readonly address: WinRtCharacteristicAddress
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  readonly mode: 'notify' | 'indicate'
  readonly consumers: Set<WinRtBackendSubscription>
  readonly pendingConsumers: Set<WinRtPendingSubscription>
  readonly stagedValues: OwnedBytes[]
  state: 'enabling' | 'ready' | 'removing' | 'cleanup-pending'
  enableConfirmed: boolean
  enableOutcome: 'pending' | 'enabled' | 'failed'
  invalidated: boolean
  enablement: Promise<void> | null
  nativeEnable: WinRtAsyncOperation<void> | null
  /** Records invalidation received during synchronous startNotify before its operation handle is available. */
  enableCancellationRequested: boolean
  enableCancellation: Promise<WinRtCancellationState> | null
  removal: Promise<CleanupRecord> | null
  removalSettlement: Promise<CleanupRecord> | null
  removalPhase: 'pre-enable' | 'post-enable' | null
  stagedBytes: number
  stagingOverflowed: boolean
}

export interface WinRtPendingSubscription {
  state: 'pending' | 'cancelled' | 'invalidated' | 'completed'
  physical: WinRtPhysicalSubscription | null
  invalidation: Promise<void>
  invalidationError: Error | null
  invalidate(error: Error): void
}

let nextBackendInstance = 1

function allocateBackendInstance(): number {
  const allocated = nextBackendInstance
  nextBackendInstance += 1
  return allocated
}

/**
 * First-party Windows central backend. It owns one selected WinRT adapter and
 * retains native operation ownership until cancellation is acknowledged or a
 * late native completion has been quarantined.
 */
export interface BackendStreamOwnershipSnapshot {
  readonly stateWatchers: number
  readonly eventStreams: number
}

const winRtStreamOwnershipInspectors = new WeakMap<WinRtBackend, () => BackendStreamOwnershipSnapshot>()

export function inspectWinRtStreamOwnershipForTests(backend: WinRtBackend): BackendStreamOwnershipSnapshot {
  const inspect = winRtStreamOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('winrt stream ownership inspector is missing')
  }
  return inspect()
}

export class WinRtBackend implements BleCentralBackend<string, HostNeutralBackendIdentity<string>> {
  readonly features: FeatureRegistry
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly security: WinRtSecurityBackend | undefined
  readonly dispatcher: WinRtOperationDispatcher
  readonly subscriptions = new Map<string, WinRtPhysicalSubscription>()
  connectionOwnsGeneration(
    nativePeerId: string,
    connectionGeneration: GenerationId<'connection-generation', string>
  ): boolean {
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId === nativePeerId && record.connectionGeneration === connectionGeneration) {
        return true
      }
    }
    return false
  }
  private readonly backendInstanceId: BackendInstanceId<string>
  private readonly eventStreams = new Set<CoreBoundedStream<BackendEvent<string>>>()
  private readonly stateStreams = new Set<CoreBoundedStream<AdapterStateSnapshot<string>>>()
  private readonly peerIdsByNativeId = new Map<string, PeerId<string>>()
  private readonly nativeIdsByPeerId = new Map<string, string>()
  /**
   * One record per lease, keyed by connection id (UNIFIED_SEMANTICS §3/§8,
   * FX1B). Leases of one peer share the single native link; the map holds a
   * join family of `connected` records, a single transitional record, or
   * terminal garbage awaiting deletion — never a mix.
   */
  private readonly connectionsById = new Map<string, WinRtConnectionRecord>()
  private readonly removeConnectionListener: () => void
  private readonly removeDatabaseListener: () => void
  private readonly removeScanTerminalListener: () => void
  private readonly removeAdapterStateListener: () => void
  private adapterStateSnapshot: WinRtAdapterSnapshot
  private attached = false
  private admissionClosed = false
  private destroyed = false
  private destroyResult: Promise<CleanupRecord> | null = null
  private adapterLossCleanup: Promise<void> | null = null
  private readonly adapterLossTerminalizedConnections = new WeakSet<WinRtConnectionRecord>()
  private adapterLossPending = false
  private scanGroup: WinRtScanGroup | null = null
  private backendGeneration = 1
  private adapterGeneration = 1
  private nextPeer = 1
  private nextScan = 1
  private nextConnection = 1
  private nextLease = 1
  nextDatabase = 1
  nextSubscription = 1
  private nextIngressOrdinal = 1
  readonly gattOperations: WinRtGattOperations

  constructor(
    readonly boundary: WinRtBoundary,
    readonly selectedAdapter: WinRtAdapterRecord,
    readonly now: () => number,
    private readonly hostKind: 'node' | 'desktop-native'
  ) {
    this.backendInstanceId = opaqueId(`winrt-backend-${allocateBackendInstance()}`, 'backend-instance', 'winrt')
    this.adapterStateSnapshot = validateWinRtAdapterSnapshot(boundary.adapterSnapshot())
    this.dispatcher = new WinRtOperationDispatcher({
      now,
      onLateSuccess: operation => console.info(`[WinRtBackend] Late WinRT completion quarantined: ${operation}`),
      onLateFailure: (operation, error) =>
        console.error(`[WinRtBackend] Late WinRT completion failed: ${operation}`, error),
      onCancellationFailure: (operation, error) =>
        console.error(`[WinRtBackend] WinRT cancellation acknowledgement failed: ${operation}`, error)
    })
    this.gattOperations = new WinRtGattOperations(this)
    this.adapter = Object.freeze({
      currentState: async () => winRtAdapterState(this.adapterStateSnapshot, this.backendGeneration, this.now),
      watchState: async () => this.watchAdapterState()
    })
    this.scanner = Object.freeze({
      plan: (query: NormalizedScanQuery) => diagnosticWinRtScanPlan(query),
      start: this.startScan.bind(this),
      join: this.joinScan.bind(this)
    })
    this.connections = Object.freeze({
      connect: this.connect.bind(this)
    })
    this.gatt = Object.freeze({
      discover: this.gattOperations.discover.bind(this.gattOperations),
      read: this.gattOperations.read.bind(this.gattOperations),
      write: this.gattOperations.write.bind(this.gattOperations),
      readDescriptor: this.gattOperations.readDescriptor.bind(this.gattOperations),
      writeDescriptor: this.gattOperations.writeDescriptor.bind(this.gattOperations),
      subscribe: this.gattOperations.subscribe.bind(this.gattOperations),
      unsubscribe: this.gattOperations.unsubscribe.bind(this.gattOperations)
    })
    this.security = isWinRtSecurityBoundary(boundary)
      ? new WinRtSecurityBackend(
          boundary,
          now,
          this.dispatcher,
          (peerId, operation) => this.nativePeerIdForPeerId(peerId, operation),
          nativePeerId => this.peerIdsByNativeId.get(nativePeerId) ?? null
        )
      : undefined
    this.features = createWinRtFeatureRegistry(this.security !== undefined)
    this.removeConnectionListener = boundary.onConnectionLost(record => {
      try {
        this.handleConnectionLoss(validateWinRtConnectionLossRecord(record))
      } catch (error) {
        console.error('[WinRtBackend.onConnectionLost] Dropped malformed native connection-loss record:', error)
      }
    })
    this.removeDatabaseListener = boundary.onDatabaseChanged(record => {
      try {
        this.handleDatabaseChanged(validateWinRtDatabaseChangedRecord(record))
      } catch (error) {
        console.error('[WinRtBackend.onDatabaseChanged] Dropped malformed native database-change record:', error)
      }
    })
    this.removeScanTerminalListener = boundary.onScanTerminal(record => {
      this.handleScanTerminal(record)
    })
    this.removeAdapterStateListener = boundary.onAdapterState(state => {
      try {
        this.handleAdapterState(validateWinRtAdapterSnapshot(state))
      } catch (error) {
        console.error('[WinRtBackend.onAdapterState] Dropped malformed native adapter-state record:', error)
      }
    })
    winRtStreamOwnershipInspectors.set(this, () => ({
      stateWatchers: this.stateStreams.size,
      eventStreams: this.eventStreams.size
    }))
  }

  get identity(): HostNeutralBackendIdentity<string> {
    const attachment = this.attachment()
    return Object.freeze({
      registeredBackendId: WINRT_BACKEND_ID,
      registeredPlatformId: WINRT_PLATFORM_ID,
      attachment,
      versions: negotiateCoreVersions(winRtCompatibility, winRtCompatibility),
      runtime: Object.freeze({
        hostKind: this.hostKind,
        implementationVersion: WINRT_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          boundary: 'winrt-direct-v2',
          deployment: this.selectedAdapter.deployment
        })
      })
    })
  }

  async attach(
    request: BackendAttachmentRequest
  ): Promise<BackendAttachment<string, HostNeutralBackendIdentity<string>>> {
    this.assertUsable('winrt.attach')
    if (this.attached) {
      throw contractError('lifecycle.invalid-state', 'core', 'winrt.attach')
    }
    negotiateCoreVersions(winRtCompatibility, request.coreCompatibility)
    assertWinRtAdapterReady(this.adapterStateSnapshot, 'winrt.attach')
    this.attached = true
    const identity = this.identity
    return Object.freeze({ attachment: identity.attachment, identity })
  }

  events(): BoundedAsyncStream<BackendEvent<string>> {
    this.assertUsable('winrt.events')
    const stream = new OwnedCoreBoundedStream<BackendEvent<string>>(eventLimits, 'error', () =>
      this.eventStreams.delete(stream)
    )
    this.eventStreams.add(stream)
    return stream
  }

  resourceCounters(): ResourceCounters {
    return winRtResourceCounters(
      this.scanGroup === null ? 0 : 1,
      this.scanGroup?.consumers.size ?? 0,
      this.connectionsById.values(),
      this.subscriptions.values(),
      this.dispatcher.activeCount()
    )
  }

  destroy(): Promise<CleanupRecord> {
    if (this.destroyResult === null) {
      this.destroyResult = this.destroyInternal().then(result => {
        if (result.state === 'release-failed') {
          this.destroyResult = null
        }
        return result
      })
    }
    return this.destroyResult
  }

  attachment(): AttachmentRecord<string> {
    const descriptor = adapterDescriptor({ ...this.selectedAdapter, state: this.adapterStateSnapshot }, this.now)
    const backendGeneration = opaqueId(String(this.backendGeneration), 'backend-generation', 'winrt')
    return Object.freeze({
      attachmentId: opaqueId(
        `${String(this.backendInstanceId)}:${this.backendGeneration}:${this.adapterGeneration}`,
        'attachment',
        'winrt'
      ),
      backendInstanceId: this.backendInstanceId,
      backendGeneration,
      adapter: Object.freeze({
        ...descriptor,
        state: Object.freeze({ ...descriptor.state, backendGeneration }),
        adapterGeneration: opaqueId(String(this.adapterGeneration), 'adapter-generation', 'winrt')
      })
    })
  }

  identifiers() {
    const attachment = this.attachment()
    return createAttachmentBoundIdFactory({
      attachmentId: attachment.attachmentId,
      backendInstanceId: attachment.backendInstanceId,
      backendGeneration: attachment.backendGeneration,
      adapterId: attachment.adapter.adapterId,
      adapterGeneration: attachment.adapter.adapterGeneration
    })
  }

  /** Captures adapter-reset causality before a native GATT start can synchronously re-enter this backend. */
  captureAdapterResetEpoch(): number {
    return this.adapterGeneration
  }

  assertUsable(operation: string): void {
    if (this.admissionClosed || this.destroyed) {
      throw contractError('lifecycle.destroyed', 'core', operation)
    }
    if (this.adapterLossPending) {
      throw contractError('lifecycle.invalid-state', 'core', operation)
    }
  }

  /** Applies the common lifecycle and radio-readiness gate to every GATT admission path. */
  assertGattUsable(operation: string): void {
    this.assertUsable(operation)
    assertWinRtAdapterReady(this.adapterStateSnapshot, operation)
  }

  requireConnection(connection: BackendConnection<string, string>, operation: string): WinRtConnectionRecord {
    const record = this.connectionsById.get(String(connection.connectionId))
    if (
      record === undefined ||
      record.connectionId !== connection.connectionId ||
      record.connectionGeneration !== connection.connectionGeneration ||
      !attachmentRecordsEqual(connection.attachment, this.attachment()) ||
      connection.attachmentId !== this.attachment().attachmentId
    ) {
      throw contractError('connection.stale', 'connection', operation)
    }
    if (record.state !== 'connected') {
      throw contractError(record.state === 'lost' ? 'connection.lost' : 'connection.stale', 'connection', operation)
    }
    return record
  }

  databaseForPath(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): WinRtGattDatabase {
    for (const record of this.connectionsById.values()) {
      const database = record.database
      if (database !== null && database !== undefined && database.matchesPath(path)) {
        database.assertCurrent(operation)
        return database
      }
    }
    throw contractError('gatt.stale-handle', 'gatt', operation)
  }

  /** Retains the logical operation in the connection generation that owns its native work. */
  trackConnectionOperation<Result>(
    record: WinRtConnectionRecord,
    gattRevision: number,
    dispatch: WinRtOperationDispatch<Result>,
    operationName: string,
    adapterResetEpoch: number
  ): BackendOperationDispatch<string, Result> {
    record.pendingOperations.set(dispatch.handle, {
      operationName,
      physicalSettlement: dispatch.physicalSettlement
    })
    dispatch.physicalSettlement.then(() => {
      if (record.pendingOperations.get(dispatch.handle)?.physicalSettlement === dispatch.physicalSettlement) {
        record.pendingOperations.delete(dispatch.handle)
      }
    })
    if (this.adapterGeneration !== adapterResetEpoch) {
      this.dispatcher.terminalize(
        dispatch.handle,
        contractError('operation.reset', 'adapter', `${operationName}.adapter-loss-during-start`)
      )
    } else if (record.state !== 'connected') {
      this.dispatcher.terminalize(
        dispatch.handle,
        contractError('operation.disconnected', 'connection', 'winrt.gatt.connection-lost-during-start')
      )
    } else if (record.gattRevision !== gattRevision) {
      this.dispatcher.terminalize(
        dispatch.handle,
        contractError('gatt.stale-handle', 'gatt', `${operationName}.services-changed`)
      )
    }
    return dispatch
  }

  descriptorDatabaseForPath(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): WinRtGattDatabase {
    return this.databaseForPath(path, operation)
  }

  disconnect(
    record: WinRtConnectionRecord,
    operation: string,
    invalidateChildren = true,
    waitForOperations = true
  ): Promise<CleanupRecord> {
    if (record.disconnectResult !== null) {
      return record.disconnectResult
    }
    let resolveCleanup: (cleanup: CleanupRecord) => void = () => undefined
    let rejectCleanup: (error: Error) => void = () => undefined
    const sharedCleanup = new Promise<CleanupRecord>((resolve, reject) => {
      resolveCleanup = resolve
      rejectCleanup = reject
    })
    record.disconnectResult = sharedCleanup
    this.disconnectInternal(record, operation, invalidateChildren, waitForOperations).then(
      cleanup => {
        if (cleanup.state === 'release-failed' && record.disconnectResult === sharedCleanup) {
          record.disconnectResult = null
        }
        resolveCleanup(cleanup)
      },
      error => {
        if (record.disconnectResult === sharedCleanup) {
          record.disconnectResult = null
        }
        rejectCleanup(
          error instanceof Error ? error : new Error('WinRT connection cleanup rejected with a non-Error value')
        )
      }
    )
    return sharedCleanup
  }

  private async disconnectInternal(
    record: WinRtConnectionRecord,
    operation: string,
    invalidateChildren: boolean,
    waitForOperations: boolean
  ): Promise<CleanupRecord> {
    this.terminalizeConnectionOperations(record, () =>
      contractError('operation.disconnected', 'connection', 'winrt.gatt.connection-release')
    )
    const invalidation = invalidateChildren
      ? await this.invalidateConnectionChildren(
          record,
          'owner-released',
          contractError('operation.disconnected', 'gatt', 'winrt.gatt.subscribe.connection-release')
        )
      : releasedCleanup
    const pendingDisconnect = await this.waitForRetainedDisconnect(record, operation)
    if (pendingDisconnect.state === 'release-failed') {
      return combineWinRtCleanup(invalidation, pendingDisconnect)
    }
    if (!(await this.waitForPendingConnectBeforeDisconnect(record))) {
      return combineWinRtCleanup(invalidation, pendingWinRtConnectionCleanup())
    }
    const enablementTimedOut = winRtEnablementCleanupTimedOut(invalidation)
    if (!enablementTimedOut && !(await this.waitForConnectionOperationsBeforeDisconnect(record))) {
      if (record.state === 'disconnecting') {
        record.state = 'connected'
      }
      return combineWinRtCleanup(invalidation, pendingWinRtConnectionCleanup())
    }
    const nativeDisconnectRequired = record.state !== 'disconnected' && record.state !== 'lost'
    if (nativeDisconnectRequired) {
      record.state = 'disconnecting'
      const nativeCleanup = await this.startNativeDisconnect(record, operation)
      if (nativeCleanup.state === 'release-failed') {
        return combineWinRtCleanup(invalidation, nativeCleanup)
      }
    }
    if (waitForOperations && !enablementTimedOut && !(await this.waitForConnectionOperations(record))) {
      return combineWinRtCleanup(invalidation, pendingWinRtConnectionCleanup())
    }
    if (invalidation.state === 'release-failed') {
      if (winRtEnablementCleanupTimedOut(invalidation)) {
        this.retireDisconnectedRecord(record)
      }
      return invalidation
    }
    this.retireDisconnectedRecord(record)
    // An explicit disconnect drops the shared link: every joined lease ends
    // with it. The radio fired once above; siblings end locally.
    const siblingFailures: CleanupFailure[] = []
    for (const sibling of this.recordsForNativePeer(record.nativePeerId)) {
      if (sibling === record) {
        continue
      }
      const siblingInvalidation = await this.invalidateConnectionChildren(
        sibling,
        'owner-released',
        contractError('operation.disconnected', 'gatt', 'winrt.gatt.subscribe.connection-release')
      )
      siblingFailures.push(...siblingInvalidation.failures)
      if (siblingInvalidation.state !== 'release-failed') {
        this.retireDisconnectedRecord(sibling)
      }
    }
    if (siblingFailures.length === 0) {
      return invalidation
    }
    return Object.freeze({ state: 'release-failed', failures: Object.freeze(siblingFailures) })
  }

  private retireDisconnectedRecord(record: WinRtConnectionRecord): void {
    record.state = 'disconnected'
    record.lease?.markReleased()
    if (this.connectionsById.get(String(record.connectionId)) === record) {
      this.connectionsById.delete(String(record.connectionId))
    }
  }

  async releaseConnectionLease(lease: WinRtConnectionLease): Promise<CleanupRecord> {
    const record = lease.record
    // A lease that shares its peer's link with other live leases ends
    // locally without radio work; the final holder drives the radio.
    if (this.liveLinkHolders(record.nativePeerId, record).length > 0) {
      return this.releaseSharedLease(record)
    }
    return this.disconnect(record, 'winrt.connection.release')
  }

  async stopScanConsumer(consumer: WinRtScanConsumer): Promise<CleanupRecord> {
    const group = this.scanGroup
    if (group === null || !group.consumers.has(String(consumer.leaseId))) {
      this.releaseScanAdmission(consumer)
      consumer.stream.closeWithReason('owner-released')
      return releasedCleanup
    }
    if (!consumer.released) {
      consumer.released = true
      consumer.stream.closeWithReason('owner-released')
    }
    this.releaseScanAdmission(consumer)
    const remaining = [...group.consumers.values()].some(candidate => !candidate.released)
    if (remaining) {
      group.consumers.delete(String(consumer.leaseId))
      return releasedCleanup
    }
    if (group.state === 'starting') {
      // A start that has not physically settled must be stopped after, not before, its late success.
      group.consumers.delete(String(consumer.leaseId))
      group.state = 'stopping'
      return releasedCleanup
    }
    return this.stopScanGroup(group, 'winrt.scan.stop')
  }

  /** Owns the single physical watcher teardown and leaves failures retryable on the group. */
  private stopScanGroup(group: WinRtScanGroup, operation: string): Promise<CleanupRecord> {
    if (group.stopResult !== null) {
      return group.stopResult
    }
    group.state = 'stopping'
    const deadline = this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS
    if (group.stopSettlement !== null) {
      const retry = waitForWinRtValue(group.stopSettlement, deadline, this.now).then(result =>
        result.state === 'timed-out' ? timedOutWinRtCleanup('scan', operation) : result.value
      )
      group.stopResult = retry
      retry.then(() => {
        if (group.stopResult === retry) {
          group.stopResult = null
        }
      })
      return retry
    }
    let nativeStop: WinRtAsyncOperation<void>
    try {
      nativeStop = this.boundary.stopScan(group.scanToken)
    } catch (error) {
      group.state = 'cleanup-pending'
      return Promise.resolve(cleanupFailure('scan', operation, error))
    }
    const settlement = nativeStop.completion.then(
      () => {
        group.consumers.clear()
        group.stopSettlement = null
        group.stopResult = null
        if (this.scanGroup === group) {
          this.scanGroup = null
        }
        return releasedCleanup
      },
      error => {
        group.stopSettlement = null
        group.stopResult = null
        if (this.scanGroup === group) {
          group.state = 'cleanup-pending'
        }
        return cleanupFailure('scan', operation, error)
      }
    )
    group.stopSettlement = settlement
    const stop = waitForWinRtValue(settlement, deadline, this.now).then(result =>
      result.state === 'timed-out' ? timedOutWinRtCleanup('scan', operation) : result.value
    )
    group.stopResult = stop
    stop.then(() => {
      if (group.stopResult === stop) {
        group.stopResult = null
      }
    })
    return stop
  }

  /** Retires a scan group only after its native start has proved that no watcher remains owned. */
  private retireScanGroup(group: WinRtScanGroup, reason: CoreStreamTerminalReason = 'owner-released'): void {
    for (const consumer of group.consumers.values()) {
      this.releaseScanAdmission(consumer)
      consumer.released = true
      consumer.stream.closeWithReason(reason)
    }
    group.consumers.clear()
    if (this.scanGroup === group) {
      this.scanGroup = null
    }
  }

  private scanGroupStopResult(group: WinRtScanGroup): Promise<CleanupRecord> | null {
    return group.stopResult
  }

  private scanGroupState(group: WinRtScanGroup): WinRtScanGroup['state'] {
    return group.state
  }

  /** Retries an abandoned late-start cleanup before admitting another native watcher. */
  private async reconcilePendingScanGroup(): Promise<void> {
    const group = this.scanGroup
    if (group === null || group.state === 'active' || group.state === 'starting') {
      return
    }
    if (group.state === 'stopping') {
      if (group.stopResult !== null) {
        const cleanup = await group.stopResult
        if (cleanup.state === 'release-failed') {
          throw contractError('platform.failure', 'cleanup', 'winrt.scan.reconcile-stop')
        }
        return
      }
      if (group.stopSettlement !== null) {
        const cleanup = await this.stopScanGroup(group, 'winrt.scan.reconcile-stop')
        if (cleanup.state === 'release-failed') {
          throw contractError('platform.failure', 'cleanup', 'winrt.scan.reconcile-stop')
        }
        return
      }
      if (group.startDispatch !== null) {
        await group.startDispatch.physicalSettlement
      }
      if (this.scanGroup !== group) {
        return
      }
      const reconciledStopResult = this.scanGroupStopResult(group)
      if (reconciledStopResult !== null) {
        const cleanup = await reconciledStopResult
        if (cleanup.state === 'release-failed') {
          throw contractError('platform.failure', 'cleanup', 'winrt.scan.reconcile-stop')
        }
        return
      }
      const reconciledState = this.scanGroupState(group)
      if (reconciledState === 'cleanup-pending') {
        const cleanup = await this.stopScanGroup(group, 'winrt.scan.retry-late-start-cleanup')
        if (cleanup.state === 'release-failed') {
          throw contractError('platform.failure', 'cleanup', 'winrt.scan.retry-late-start-cleanup')
        }
        return
      }
      this.retireScanGroup(group)
      return
    }
    const cleanup = await this.stopScanGroup(group, 'winrt.scan.retry-late-start-cleanup')
    if (cleanup.state === 'release-failed') {
      throw contractError('platform.failure', 'cleanup', 'winrt.scan.retry-late-start-cleanup')
    }
  }

  private watchAdapterState(): AdapterStateWatch<string> {
    const stream = new OwnedCoreBoundedStream<AdapterStateSnapshot<string>>(adapterStateLimits, 'latest', () =>
      this.stateStreams.delete(stream)
    )
    this.stateStreams.add(stream)
    return Object.freeze({
      initial: winRtAdapterState(this.adapterStateSnapshot, this.backendGeneration, this.now),
      transitions: stream
    })
  }

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    this.assertUsable('winrt.scan.start')
    assertWinRtAdapterReady(this.adapterStateSnapshot, 'winrt.scan.start')
    assertWinRtOperationAdmission(options, this.now, 'winrt.scan.start')
    const serviceUuids = trustedServiceUuidFilter(options, planWinRtScan, 'winrt.scan').serviceUuids
    await this.reconcilePendingScanGroup()
    this.assertUsable('winrt.scan.start')
    assertWinRtAdapterReady(this.adapterStateSnapshot, 'winrt.scan.start')
    assertWinRtOperationAdmission(options, this.now, 'winrt.scan.start')
    if (this.scanGroup !== null) {
      throw contractError('scan.already-active', 'scan', 'winrt.scan.start')
    }
    const ids = this.identifiers()
    const ordinal = this.nextScan
    this.nextScan += 1
    const consumer: WinRtScanConsumer = {
      scanSessionId: ids.scanSessionId(`winrt-scan-session-${ordinal}`),
      leaseId: ids.leaseId(`winrt-scan-lease-${ordinal}`),
      shareToken: options.sharing.allowSharing ? ids.scanShareToken(`winrt-scan-share-${ordinal}`) : null,
      options,
      stream: new CoreBoundedStream(options.delivery, options.delivery.overflowPolicy),
      abort: null,
      deadlineTimer: null,
      released: false
    }
    const group: WinRtScanGroup = {
      ownerLeaseId: consumer.leaseId,
      shareToken: consumer.shareToken,
      scanToken: `winrt-scan-token-${String(this.backendInstanceId)}-${ordinal}`,
      scanSessionId: consumer.scanSessionId,
      consumers: new Map([[String(consumer.leaseId), consumer]]),
      state: 'starting',
      nativeTerminalReceived: false,
      startTerminalError: null,
      startTerminalShouldTerminalize: false,
      startInvocationActive: true,
      startDispatch: null,
      stopResult: null,
      stopSettlement: null
    }
    this.scanGroup = group
    this.bindScanAdmission(consumer)
    let dispatch: WinRtOperationDispatch<void>
    try {
      dispatch = this.dispatcher.dispatch(
        options,
        'winrt.scan.start',
        () =>
          this.boundary.startScan(group.scanToken, serviceUuids, advertisement =>
            this.handleAdvertisement(advertisement)
          ),
        async () => {
          if (group.nativeTerminalReceived) {
            return
          }
          const cleanup = await this.stopScanGroup(group, 'winrt.scan.late-start-cleanup')
          if (cleanup.state === 'release-failed') {
            throw contractError('platform.failure', 'cleanup', 'winrt.scan.late-start-cleanup')
          }
        },
        () => {
          if (this.scanGroup === group && (group.state === 'starting' || group.state === 'stopping')) {
            this.retireScanGroup(group)
          }
        }
      )
    } finally {
      group.startInvocationActive = false
    }
    group.startDispatch = dispatch
    if (group.startTerminalShouldTerminalize && group.startTerminalError !== null) {
      this.dispatcher.terminalize(dispatch.handle, group.startTerminalError)
    }
    try {
      await dispatch.completion
    } catch (error) {
      this.releaseScanAdmission(consumer)
      consumer.released = true
      consumer.stream.closeWithReason('owner-released')
      group.consumers.delete(String(consumer.leaseId))
      if (this.scanGroup === group) {
        if (this.isPublicOperationCancellation(error)) {
          // Native start can still succeed after a logical cancellation. Keep the group owned until
          // its late completion either proves no watcher exists or compensating stop succeeds.
          group.state = 'stopping'
        } else {
          await dispatch.physicalSettlement
          if (this.scanGroup === group && (group.state === 'starting' || group.state === 'stopping')) {
            this.retireScanGroup(group)
          }
        }
      }
      throw winRtPlatformError('scan.start-failed', 'scan', 'winrt.scan.start', error)
    }
    if (group.nativeTerminalReceived) {
      consumer.released = true
      this.releaseScanAdmission(consumer)
      consumer.stream.closeWithReason('source-failed')
      throw group.startTerminalError ?? contractError('scan.start-failed', 'scan', 'winrt.scan.start')
    }
    if (group.state !== 'starting' || consumer.released) {
      consumer.released = true
      this.releaseScanAdmission(consumer)
      consumer.stream.closeWithReason('owner-released')
      const cleanup = await this.stopScanGroup(group, 'winrt.scan.start-terminal-cleanup')
      if (cleanup.state === 'release-failed') {
        throw contractError('platform.failure', 'cleanup', 'winrt.scan.start-terminal-cleanup')
      }
      throw group.startTerminalError ?? contractError('operation.aborted', 'scan', 'winrt.scan.start')
    }
    group.state = 'active'
    return new WinRtScanLease(this, consumer)
  }

  private async joinScan(
    ownerLeaseId: LeaseId<string, string>,
    token: ScanShareToken<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    this.assertUsable('winrt.scan.join')
    const group = this.scanGroup
    const owner = group?.consumers.get(String(ownerLeaseId))
    if (
      group === null ||
      group.state !== 'active' ||
      group.ownerLeaseId !== ownerLeaseId ||
      group.shareToken !== token ||
      owner === undefined ||
      owner.released
    ) {
      throw contractError('ownership.denied', 'scan', 'winrt.scan.join')
    }
    const ids = this.identifiers()
    assertWinRtOperationAdmission(owner.options, this.now, 'winrt.scan.join')
    const ordinal = this.nextScan
    this.nextScan += 1
    const consumer: WinRtScanConsumer = {
      scanSessionId: owner.scanSessionId,
      leaseId: ids.leaseId(`winrt-scan-lease-${ordinal}`),
      shareToken: null,
      options: owner.options,
      stream: new CoreBoundedStream(owner.options.delivery, owner.options.delivery.overflowPolicy),
      abort: null,
      deadlineTimer: null,
      released: false
    }
    group.consumers.set(String(consumer.leaseId), consumer)
    this.bindScanAdmission(consumer)
    return new WinRtScanLease(this, consumer)
  }

  /**
   * Every lease record for one native peer. Joins share the single native
   * link; see the `connectionsById` invariant.
   */
  private recordsForNativePeer(nativePeerId: string): WinRtConnectionRecord[] {
    return [...this.connectionsById.values()].filter(record => record.nativePeerId === nativePeerId)
  }

  /**
   * The peer's live link, if any: a `connected` record whose lease still
   * holds it. A connect joins this link with an independent generation
   * instead of re-dialling the radio.
   */
  private liveSharedLink(nativePeerId: string): WinRtConnectionRecord | null {
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId !== nativePeerId) continue
      // A record with teardown in flight (`disconnectResult`) is not a live
      // link: a connect joins only a settled `connected` lease, otherwise the
      // joiner would land on a dying link.
      if (record.state === 'connected' && record.lease !== null && record.disconnectResult === null) {
        return record
      }
    }
    return null
  }

  /**
   * Live holders of the peer's link besides `except`: `connected`,
   * `disconnecting` or `lost`-retained records still name the native link.
   * A non-final release ends locally while any holder remains.
   */
  private liveLinkHolders(nativePeerId: string, except: WinRtConnectionRecord): WinRtConnectionRecord[] {
    return this.recordsForNativePeer(nativePeerId).filter(
      record =>
        record !== except &&
        (record.state === 'connected' || record.state === 'disconnecting' || record.state === 'lost')
    )
  }

  private joinSharedLink(peerId: PeerId<string>, shared: WinRtConnectionRecord): WinRtConnectionLease {
    const ids = this.identifiers()
    const record: WinRtConnectionRecord = {
      attachment: this.attachment(),
      nativePeerId: shared.nativePeerId,
      peerId,
      connectionId: ids.connectionId(`winrt-connection-${this.nextConnection}`),
      connectionGeneration: opaqueId(String(this.nextConnection), 'connection-generation', 'winrt'),
      dialGeneration: shared.dialGeneration,
      ownerLeaseId: ids.leaseId(`winrt-connection-lease-${this.nextLease}`),
      state: 'connected',
      gattRevision: 0,
      database: null,
      lease: null,
      pendingConnect: null,
      pendingOperations: new Map(),
      disconnectResult: null,
      disconnectSettlement: null
    }
    this.nextConnection += 1
    this.nextLease += 1
    this.connectionsById.set(String(record.connectionId), record)
    const connection = new WinRtConnection(this, record)
    const lease = new WinRtConnectionLease(this, record, connection)
    record.lease = lease
    return lease
  }

  /**
   * Ends one joined lease locally: only its own generation's subscriptions
   * are removed (another lease's enablement is untouched); the native link
   * is untouched.
   */
  private async releaseSharedLease(record: WinRtConnectionRecord): Promise<CleanupRecord> {
    const invalidation = await this.invalidateJoinChildren(
      record,
      'owner-released',
      contractError('operation.disconnected', 'gatt', 'winrt.gatt.subscribe.connection-release')
    )
    if (invalidation.state === 'release-failed') {
      return invalidation
    }
    record.state = 'disconnected'
    record.lease?.markReleased()
    record.lease = null
    if (this.connectionsById.get(String(record.connectionId)) === record) {
      this.connectionsById.delete(String(record.connectionId))
    }
    return invalidation
  }

  private async invalidateJoinChildren(
    record: WinRtConnectionRecord,
    reason: 'connection-lost' | 'owner-released',
    pendingSubscriptionError: Error
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    record.database?.invalidate()
    record.database = null
    for (const physical of [...this.subscriptions.values()]) {
      if (String(physical.connectionGeneration) !== String(record.connectionGeneration)) {
        continue
      }
      for (const consumer of physical.consumers) {
        consumer.stream.closeWithReason(reason)
        consumer.removed = true
      }
      physical.consumers.clear()
      const cleanup = await invalidateWinRtPhysicalSubscription(this, physical, pendingSubscriptionError)
      failures.push(...cleanup.failures)
    }
    return failures.length === 0
      ? releasedCleanup
      : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
  }

  private async connect(
    peerId: PeerId<string>,
    _clientId: ClientId<string, string>,
    options: ConnectionOptions
  ): Promise<ConnectionLease<string, string, string>> {
    this.assertUsable('winrt.connect')
    assertWinRtAdapterReady(this.adapterStateSnapshot, 'winrt.connect')
    assertWinRtOperationAdmission(options, this.now, 'winrt.connect')
    if (options.intent === 'when-available') {
      throw contractError('capability.unsupported', 'connection', 'winrt.connect.when-available')
    }
    const nativePeerId = this.nativeIdsByPeerId.get(String(peerId))
    if (nativePeerId === undefined) {
      throw contractError('connection.not-found', 'connection', 'winrt.connect.peer')
    }
    // Same-peer join (UNIFIED_SEMANTICS §3/§8, FX1B, Android reference): a
    // live link is leased, never re-dialled. Only a peer with no live link
    // reaches the radio; a transitional record arbitrates
    // `connection.already-owned` until its teardown completes.
    const shared = this.liveSharedLink(nativePeerId)
    if (shared !== null) {
      return this.joinSharedLink(peerId, shared)
    }
    for (const retained of this.recordsForNativePeer(nativePeerId)) {
      // A retired record whose lease object is still live blocks a new dial:
      // its release retry owns the peer until it settles.
      if (retained.state === 'disconnected' && retained.lease === null) {
        continue
      }
      if ((retained.state !== 'disconnecting' && retained.state !== 'lost') || retained.lease !== null) {
        throw contractError('connection.already-owned', 'connection', 'winrt.connect.owner')
      }
      await this.retryRetainedConnectionCleanup(retained)
    }
    for (const terminal of this.recordsForNativePeer(nativePeerId)) {
      if (terminal.state === 'disconnected' || terminal.state === 'lost') {
        this.connectionsById.delete(String(terminal.connectionId))
      }
    }
    this.assertUsable('winrt.connect')
    assertWinRtAdapterReady(this.adapterStateSnapshot, 'winrt.connect')
    assertWinRtOperationAdmission(options, this.now, 'winrt.connect')
    if (this.nativeIdsByPeerId.get(String(peerId)) !== nativePeerId) {
      throw contractError('connection.not-found', 'connection', 'winrt.connect.peer-generation')
    }
    const ids = this.identifiers()
    const pendingConnect: WinRtPendingConnect = {
      dispatch: null,
      physicalSettlement: null,
      terminalError: null
    }
    const connectionGeneration = opaqueId(String(this.nextConnection), 'connection-generation', 'winrt')
    const record: WinRtConnectionRecord = {
      attachment: this.attachment(),
      nativePeerId,
      peerId,
      connectionId: ids.connectionId(`winrt-connection-${this.nextConnection}`),
      connectionGeneration,
      dialGeneration: connectionGeneration,
      ownerLeaseId: ids.leaseId(`winrt-connection-lease-${this.nextLease}`),
      state: 'connecting',
      gattRevision: 0,
      database: null,
      lease: null,
      pendingConnect,
      pendingOperations: new Map(),
      disconnectResult: null,
      disconnectSettlement: null
    }
    this.nextConnection += 1
    this.nextLease += 1
    this.connectionsById.set(String(record.connectionId), record)
    const dispatch = this.dispatcher.dispatch(
      options,
      'winrt.connect',
      () => this.boundary.connect(nativePeerId, String(record.connectionGeneration)),
      async () => {
        await this.cleanupLateConnect(record, 'winrt.connect.late-success-cleanup')
      },
      () => {
        this.removeConnectingRecord(record)
      }
    )
    pendingConnect.dispatch = dispatch
    pendingConnect.physicalSettlement = dispatch.physicalSettlement
    if (pendingConnect.terminalError !== null) {
      this.dispatcher.terminalize(dispatch.handle, pendingConnect.terminalError)
    }
    try {
      await dispatch.completion
    } catch (error) {
      if (!this.isPublicOperationCancellation(error)) {
        record.pendingConnect = null
        this.removeConnectingRecord(record)
      }
      throw winRtPlatformError('connection.failed', 'connection', 'winrt.connect', error)
    }
    record.pendingConnect = null
    if (record.state !== 'connecting') {
      throw contractError('operation.cancelled-by-destroy', 'connection', 'winrt.connect')
    }
    record.state = 'connected'
    const connection = new WinRtConnection(this, record)
    const lease = new WinRtConnectionLease(this, record, connection)
    record.lease = lease
    return lease
  }

  private removeConnectingRecord(record: WinRtConnectionRecord): void {
    if (this.connectionsById.get(String(record.connectionId)) !== record || record.state !== 'connecting') {
      return
    }
    record.state = 'disconnected'
    this.connectionsById.delete(String(record.connectionId))
  }

  /** Retries a failed compensating disconnect before another owner can claim the late-connected peer. */
  private async retryRetainedConnectionCleanup(record: WinRtConnectionRecord): Promise<void> {
    const cleanupOperation =
      record.state === 'lost' ? 'winrt.connect.retry-lost-cleanup' : 'winrt.connect.retry-late-cleanup'
    const cleanup = await this.disconnect(record, cleanupOperation)
    if (cleanup.state === 'released') {
      return
    }
    console.error('[WinRtBackend.connect] Late native connect cleanup retry failed:', cleanup.failures)
    throw contractError('platform.failure', 'cleanup', cleanupOperation)
  }

  /** Disconnects a link that completed after the public connect operation already became terminal. */
  private async cleanupLateConnect(record: WinRtConnectionRecord, operation: string): Promise<void> {
    const invalidation = await this.invalidateConnectionChildren(
      record,
      'owner-released',
      contractError('operation.cancelled-by-destroy', 'connection', operation)
    )
    record.state = 'disconnecting'
    const cleanup = await this.startNativeDisconnect(record, operation)
    if (cleanup.state === 'release-failed') {
      record.state = 'disconnecting'
      this.connectionsById.set(String(record.connectionId), record)
      console.error('[WinRtBackend.connect] Late native connect cleanup requires retry:', cleanup.failures)
      throw contractError('platform.failure', 'cleanup', operation)
    }
    if (invalidation.state === 'release-failed') {
      record.state = 'disconnected'
      this.connectionsById.set(String(record.connectionId), record)
      console.error('[WinRtBackend.connect] Late native connect cleanup requires retry:', invalidation.failures)
      throw contractError('platform.failure', 'cleanup', operation)
    }
    record.state = 'disconnected'
    record.lease = null
    if (this.connectionsById.get(String(record.connectionId)) === record) {
      this.connectionsById.delete(String(record.connectionId))
    }
  }

  private isPublicOperationCancellation(error: unknown): boolean {
    return (
      error instanceof BackendContractError &&
      (error.normalized.code === 'operation.aborted' ||
        error.normalized.code === 'operation.timed-out' ||
        error.normalized.code === 'operation.cancelled-by-destroy' ||
        error.normalized.code === 'operation.reset')
    )
  }

  private handleAdvertisement(advertisement: unknown): void {
    if (this.destroyed) {
      return
    }
    let validatedAdvertisement: ValidatedWinRtAdvertisement
    try {
      validatedAdvertisement = validateWinRtAdvertisement(advertisement)
    } catch (error) {
      this.reportMalformedAdvertisement(error)
      return
    }
    const group = this.scanGroup
    if (group === null || group.state !== 'active' || validatedAdvertisement.scanToken !== group.scanToken) {
      return
    }
    const peerId = this.peerIdForNativeId(validatedAdvertisement.nativePeerId)
    const absent = (reason: string) =>
      Object.freeze({ state: 'absent' as const, reason, provenance: 'not-provided' as const })
    const observation: AdvertisementObservation<string> = Object.freeze({
      device: deviceIdentity(peerId, this.backendInstanceId, null),
      provenance: 'platform-raw',
      sourceTimestamp: absent('winrt-source-timestamp-not-provided'),
      receivedAtMonotonicMs: monotonicTimestamp(this.now()),
      ingressOrdinal: this.nextIngressOrdinal,
      scanSessionId: group.scanSessionId,
      localName:
        validatedAdvertisement.localName === null
          ? absent('winrt-not-provided')
          : Object.freeze({ state: 'present', value: validatedAdvertisement.localName, provenance: 'observed' }),
      rssi:
        validatedAdvertisement.rssi === null
          ? absent('winrt-not-provided')
          : Object.freeze({ state: 'present', value: validatedAdvertisement.rssi, provenance: 'observed' }),
      txPower: absent('winrt-not-provided'),
      connectable:
        validatedAdvertisement.connectable === null
          ? absent('winrt-not-provided')
          : Object.freeze({ state: 'present', value: validatedAdvertisement.connectable, provenance: 'observed' }),
      appearance: absent('winrt-not-provided'),
      serviceUuids:
        validatedAdvertisement.serviceUuids === null
          ? absent('winrt-not-provided')
          : Object.freeze({
              state: 'present',
              value: validatedAdvertisement.serviceUuids,
              provenance: 'observed'
            }),
      solicitedServiceUuids: absent('winrt-not-provided'),
      overflowServiceUuids: absent('winrt-not-provided'),
      serviceData: absent('winrt-not-provided'),
      manufacturerData: absent('winrt-not-provided'),
      rawRecord: absent('winrt-raw-advertisement-not-provided'),
      scanResponseRecord: absent('winrt-scan-response-not-provided')
    })
    this.nextIngressOrdinal += 1
    for (const consumer of [...group.consumers.values()]) {
      if (consumer.released || consumer.stream.isTerminal() || !matchesScan(consumer.options, observation)) {
        continue
      }
      const push = consumer.stream.emit(observation, advertisementByteLength(observation), String(peerId))
      if (push.terminated) {
        this.stopScanConsumer(consumer)
          .then(result => {
            if (result.state === 'release-failed') {
              console.error('[WinRtBackend.handleAdvertisement] Overflow scan cleanup requires retry:', result.failures)
            }
          })
          .catch(error => {
            console.error('[WinRtBackend.handleAdvertisement] Overflow scan cleanup rejected:', error)
          })
      }
    }
  }

  private handleScanTerminal(record: unknown): void {
    if (this.destroyed) {
      return
    }
    let terminal: WinRtScanTerminalRecord
    try {
      terminal = validateWinRtScanTerminalRecord(record)
    } catch (error) {
      const group = this.scanGroup
      if (group !== null && (group.state === 'starting' || group.state === 'active')) {
        const terminalError = winRtPlatformError('scan.start-failed', 'scan', 'winrt.scan.malformed-terminal', error)
        this.terminalizeScanGroupForNativeFailure(group, terminalError, true)
      }
      this.reportMalformedScanTerminal(error)
      return
    }
    const group = this.scanGroup
    if (group === null || group.scanToken !== terminal.scanToken) {
      return
    }
    if (
      this.adapterLossPending ||
      !winRtAdapterIsReady(this.adapterStateSnapshot) ||
      group.state === 'stopping' ||
      group.state === 'cleanup-pending'
    ) {
      return
    }
    const terminalError = this.scanTerminalError(terminal)
    if (group.state !== 'starting' && group.state !== 'active') {
      return
    }
    this.terminalizeScanGroupForNativeFailure(group, terminalError, false)
    this.reportScanTerminal(terminal, terminalError)
  }

  /** Terminal records own the watcher until its exact native stop retry succeeds. */
  private terminalizeScanGroupForNativeFailure(
    group: WinRtScanGroup,
    terminalError: Error,
    reconcileNativeOwnership: boolean
  ): void {
    group.nativeTerminalReceived = true
    group.startTerminalError ??= terminalError
    group.startTerminalShouldTerminalize = group.state === 'starting'
    group.state = 'cleanup-pending'
    for (const consumer of group.consumers.values()) {
      this.releaseScanAdmission(consumer)
      consumer.released = true
      consumer.stream.closeWithReason('source-failed')
    }
    group.consumers.clear()
    if (group.startDispatch !== null && group.startTerminalShouldTerminalize) {
      this.dispatcher.terminalize(group.startDispatch.handle, group.startTerminalError)
    }
    if (!reconcileNativeOwnership) {
      // A validated watcher terminal proves Windows has stopped the exact
      // watcher.  Retire the logical owner without issuing a second Stop call
      // against an already-retired native token.
      if (this.scanGroup === group) {
        this.scanGroup = null
      }
      return
    }
    this.stopScanGroup(group, 'winrt.scan.terminal-cleanup').then(
      cleanup => {
        if (cleanup.state === 'release-failed') {
          console.error('[WinRtBackend.handleScanTerminal] Native scan cleanup remains retryable:', cleanup.failures)
        }
      },
      cleanupError => {
        console.error('[WinRtBackend.handleScanTerminal] Native scan cleanup dispatch rejected:', cleanupError)
      }
    )
  }

  private scanTerminalError(record: WinRtScanTerminalRecord): Error {
    return winRtPlatformError(
      'scan.start-failed',
      'scan',
      record.status === 'aborted' ? 'winrt.scan.aborted' : 'winrt.scan.terminal',
      new Error(`WinRT scan terminated with ${record.error}`)
    )
  }

  private reportMalformedScanTerminal(error: unknown): void {
    const report = error instanceof Error ? error : new Error('WinRT scan terminal validation failed')
    try {
      console.error(
        '[WinRtBackend.handleScanTerminal] Malformed native scan terminal terminalized the active scan:',
        report
      )
    } catch {
      // The active group was terminalized before reporting; a diagnostic sink
      // failure cannot reopen its retained cleanup ownership.
    }
  }

  private reportScanTerminal(record: WinRtScanTerminalRecord, error: Error): void {
    try {
      console.error('[WinRtBackend.handleScanTerminal] Native scan terminated:', {
        scanToken: record.scanToken,
        status: record.status,
        error: record.error,
        normalized: error
      })
    } catch {
      // The scan is already terminal; diagnostics must not re-open ownership or throw into native code.
    }
  }

  /** A reporting failure cannot be allowed to escape through the native callback stack. */
  private reportMalformedAdvertisement(error: unknown): void {
    const report =
      error instanceof Error ? error : invalidWinRtAdvertisement('validation failed with a non-Error value')
    try {
      console.error('[WinRtBackend.handleAdvertisement] Dropped malformed native advertisement:', report)
    } catch {
      // The record is already dropped; a broken diagnostic sink must not destabilize the healthy scan callback.
    }
  }

  peerIdForNativeId(nativePeerId: string): PeerId<string> {
    const existing = this.peerIdsByNativeId.get(nativePeerId)
    if (existing !== undefined) {
      return existing
    }
    const peerId = opaqueId(`winrt-peer-${this.nextPeer}`, 'peer', String(this.backendInstanceId))
    this.nextPeer += 1
    this.peerIdsByNativeId.set(nativePeerId, peerId)
    this.nativeIdsByPeerId.set(String(peerId), nativePeerId)
    return peerId
  }

  nativePeerIdForPeerId(peerId: string, operation: string): string {
    const nativePeerId = this.nativeIdsByPeerId.get(String(peerId))
    if (nativePeerId === undefined) throw contractError('connection.not-found', 'connection', operation)
    return nativePeerId
  }

  private handleConnectionLoss(event: WinRtConnectionLossRecord): void {
    if (this.destroyed) {
      return
    }
    // A link drop ends every lease with `connection.lost`. The native event
    // names the generation the link was dialled with; once any live lease
    // still names that dial generation, every live lease of the peer ends
    // with its own generation.
    const live = this.recordsForNativePeer(event.nativePeerId).filter(
      record => record.state !== 'lost' && record.state !== 'disconnected'
    )
    const owner = live.find(record => String(record.dialGeneration) === event.connectionGeneration)
    if (owner === undefined) {
      return
    }
    for (const record of live) {
      this.handleRecordConnectionLoss(record)
    }
    if (event.safeReason !== null) {
      console.info('[WinRtBackend.connection-loss] WinRT reported connection loss:', event.safeReason)
    }
  }

  private handleRecordConnectionLoss(record: WinRtConnectionRecord): void {
    if (this.adapterLossPending) {
      const previous = record.state === 'disconnecting' ? 'disconnecting' : 'connected'
      this.terminalizeAdapterLossConnection(record, previous)
      return
    }
    const connectionPath = Object.freeze({
      attachment: record.attachment,
      attachmentId: record.attachment.attachmentId,
      peerId: record.peerId,
      connectionId: record.connectionId,
      ownerLeaseId: record.ownerLeaseId,
      connectionGeneration: record.connectionGeneration
    })
    const pendingConnect = record.pendingConnect
    if (record.state === 'connecting' && pendingConnect !== null) {
      this.terminalizePendingConnect(
        record,
        contractError('operation.disconnected', 'connection', 'winrt.connect.connection-lost')
      )
    }
    this.terminalizeConnectionOperations(record, () =>
      contractError('operation.disconnected', 'connection', 'winrt.gatt.connection-lost')
    )
    record.state = 'lost'
    const existingDisconnect = record.disconnectResult
    const connectionLossCleanup = existingDisconnect ?? this.finishConnectionLoss(record, pendingConnect)
    if (existingDisconnect === null) {
      record.disconnectResult = connectionLossCleanup
    }
    connectionLossCleanup.then(
      cleanup => {
        if (cleanup.state === 'release-failed') {
          if (record.disconnectResult === connectionLossCleanup) {
            record.disconnectResult = null
          }
          console.error('[WinRtBackend.connection-loss] Resource cleanup requires retry:', cleanup.failures)
        }
      },
      error => {
        if (record.disconnectResult === connectionLossCleanup) {
          record.disconnectResult = null
        }
        console.error('[WinRtBackend.connection-loss] Resource cleanup rejected:', error)
      }
    )
    const attachment = this.attachment()
    broadcastWinRtEvent(this.eventStreams, {
      attachment,
      attachmentId: attachment.attachmentId,
      kind: 'connection-lost',
      connection: connectionPath,
      ingressOrdinal: this.nextIngressOrdinal
    })
    this.nextIngressOrdinal += 1
    record.lease = null
  }

  private terminalizeAdapterLossConnection(
    record: WinRtConnectionRecord,
    previous: 'connected' | 'disconnecting'
  ): void {
    if (this.adapterLossTerminalizedConnections.has(record)) return
    this.adapterLossTerminalizedConnections.add(record)
    const connection = Object.freeze({
      attachment: record.attachment,
      attachmentId: record.attachment.attachmentId,
      peerId: record.peerId,
      connectionId: record.connectionId,
      ownerLeaseId: record.ownerLeaseId,
      connectionGeneration: record.connectionGeneration
    })
    record.state = 'lost'
    record.database?.invalidate()
    record.database = null
    record.lease?.markReleased()
    record.lease = null
    if (this.connectionsById.get(String(record.connectionId)) === record) {
      this.connectionsById.delete(String(record.connectionId))
    }
    broadcastWinRtEvent(this.eventStreams, {
      attachment: record.attachment,
      attachmentId: record.attachment.attachmentId,
      kind: 'connection-state-changed',
      connection,
      previous,
      current: 'lost',
      reason: 'adapter',
      ingressOrdinal: this.nextIngressOrdinal
    })
    this.nextIngressOrdinal += 1
  }

  private async finishConnectionLoss(
    record: WinRtConnectionRecord,
    pendingConnect: WinRtPendingConnect | null
  ): Promise<CleanupRecord> {
    const cleanup = await this.invalidateConnectionChildren(
      record,
      'connection-lost',
      contractError('operation.disconnected', 'gatt', 'winrt.gatt.subscribe.connection-lost')
    )
    if (pendingConnect !== null) {
      if (pendingConnect.physicalSettlement === null) {
        return combineWinRtCleanup(cleanup, pendingWinRtConnectionCleanup('winrt.connect.connection-loss'))
      }
      const settled = await waitForWinRtValue(
        pendingConnect.physicalSettlement,
        this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
        this.now
      )
      if (settled.state === 'timed-out') {
        return combineWinRtCleanup(cleanup, pendingWinRtConnectionCleanup('winrt.connect.connection-loss'))
      }
      if (record.state === 'disconnecting') {
        return cleanupFailure(
          'connection',
          'winrt.connect.late-success-cleanup',
          new Error('WinRT late connect cleanup remains physically owned and requires retry')
        )
      }
    }
    if (!(await this.waitForConnectionOperations(record))) {
      return combineWinRtCleanup(cleanup, pendingWinRtConnectionCleanup('winrt.connection.dispatcher-idle'))
    }
    if (cleanup.state === 'release-failed') {
      return cleanup
    }
    if (
      this.connectionsById.get(String(record.connectionId)) === record &&
      (record.state === 'lost' || record.state === 'disconnected')
    ) {
      this.connectionsById.delete(String(record.connectionId))
    }
    return releasedCleanup
  }

  private terminalizePendingConnect(record: WinRtConnectionRecord, error: Error): void {
    const pendingConnect = record.pendingConnect
    if (pendingConnect === null) {
      return
    }
    pendingConnect.terminalError = error
    if (pendingConnect.dispatch !== null) {
      this.dispatcher.terminalize(pendingConnect.dispatch.handle, error)
    }
  }

  private terminalizeConnectionOperations(
    record: WinRtConnectionRecord,
    errorForOperation: (operationName: string) => Error
  ): void {
    for (const [handle, operation] of record.pendingOperations) {
      this.dispatcher.terminalize(handle, errorForOperation(operation.operationName))
    }
  }

  private async waitForConnectionOperations(record: WinRtConnectionRecord): Promise<boolean> {
    return this.waitForConnectionOperationsBeforeDisconnect(record)
  }

  private async waitForPendingConnectBeforeDisconnect(record: WinRtConnectionRecord): Promise<boolean> {
    const pendingConnect = record.pendingConnect
    if (pendingConnect === null || pendingConnect.physicalSettlement === null) {
      return true
    }
    const settled = await waitForWinRtValue(
      pendingConnect.physicalSettlement,
      this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
      this.now
    )
    return settled.state === 'settled'
  }

  private async waitForRetainedDisconnect(record: WinRtConnectionRecord, operation: string): Promise<CleanupRecord> {
    const settlement = record.disconnectSettlement
    if (settlement === null) {
      return releasedCleanup
    }
    const settled = await waitForWinRtValue(settlement, this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS, this.now)
    if (settled.state === 'timed-out') {
      return pendingWinRtConnectionCleanup(`${operation}.disconnect`)
    }
    return settled.value.state === 'released'
      ? releasedCleanup
      : cleanupFailure('connection', operation, settled.value.error)
  }

  private async startNativeDisconnect(record: WinRtConnectionRecord, operation: string): Promise<CleanupRecord> {
    let native: WinRtAsyncOperation<void>
    try {
      native = this.boundary.disconnect(record.nativePeerId)
    } catch (error) {
      return cleanupFailure('connection', operation, error)
    }
    const settlement = this.trackNativeDisconnect(record, native.completion)
    const settled = await waitForWinRtValue(settlement, this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS, this.now)
    if (settled.state === 'timed-out') {
      return pendingWinRtConnectionCleanup(`${operation}.disconnect`)
    }
    return settled.value.state === 'released'
      ? releasedCleanup
      : cleanupFailure('connection', operation, settled.value.error)
  }

  private trackNativeDisconnect(
    record: WinRtConnectionRecord,
    completion: Promise<void>
  ): Promise<WinRtDisconnectOutcome> {
    const settlement: Promise<WinRtDisconnectOutcome> = completion.then(
      () => {
        const outcome: WinRtDisconnectOutcome = { state: 'released' }
        if (record.disconnectSettlement === settlement) {
          record.disconnectSettlement = null
          if (record.state !== 'lost') record.state = 'disconnected'
        }
        return outcome
      },
      error => {
        const outcome: WinRtDisconnectOutcome = { state: 'failed', error }
        if (record.disconnectSettlement === settlement) {
          record.disconnectSettlement = null
          if (record.state === 'disconnecting') {
            record.state = 'connected'
          }
        }
        return outcome
      }
    )
    record.disconnectSettlement = settlement
    return settlement
  }

  private async waitForConnectionOperationsBeforeDisconnect(record: WinRtConnectionRecord): Promise<boolean> {
    if (record.pendingOperations.size === 0) {
      return true
    }
    const deadline = this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS
    while (record.pendingOperations.size > 0) {
      const pending = [...record.pendingOperations.values()]
      const settled = await waitForWinRtSettlement(
        Promise.all(pending.map(operation => operation.physicalSettlement)),
        deadline,
        this.now
      )
      if (settled === 'timed-out') {
        return false
      }
    }
    return true
  }

  private handleDatabaseChanged(event: WinRtDatabaseChangedRecord): void {
    if (this.destroyed) {
      return
    }
    // Services changed on the shared link: the event names the dial
    // generation; every connected lease's snapshot ends with its own path.
    // Subscription teardown stays peer-wide and idempotent.
    const live = this.recordsForNativePeer(event.nativePeerId).filter(record => record.state === 'connected')
    const owner = live.find(record => String(record.dialGeneration) === event.connectionGeneration)
    if (owner === undefined) {
      return
    }
    const invalidated: WinRtGattDatabase[] = []
    for (const record of live) {
      record.gattRevision += 1
      const database = record.database
      database?.invalidate()
      record.database = null
      this.terminalizeConnectionOperations(record, operationName =>
        contractError('gatt.stale-handle', 'gatt', `${operationName}.services-changed`)
      )
      if (database !== null && database !== undefined) {
        invalidated.push(database)
      }
    }
    this.invalidateConnectionChildren(
      owner,
      'connection-lost',
      contractError('gatt.stale-handle', 'gatt', 'winrt.gatt.subscribe.database-changed')
    ).then(
      cleanup => {
        if (cleanup.state === 'release-failed') {
          console.error('[WinRtBackend.database-changed] Subscription cleanup requires retry:', cleanup.failures)
        }
      },
      error => console.error('[WinRtBackend.database-changed] Subscription cleanup rejected:', error)
    )
    if (invalidated.length === 0) {
      return
    }
    const attachment = this.attachment()
    for (const database of invalidated) {
      broadcastWinRtEvent(this.eventStreams, {
        attachment,
        attachmentId: attachment.attachmentId,
        kind: 'database-changed',
        database: database.path,
        ingressOrdinal: this.nextIngressOrdinal
      })
      this.nextIngressOrdinal += 1
    }
  }

  private handleAdapterState(state: WinRtAdapterSnapshot): void {
    if (this.destroyed) {
      return
    }
    const previous = this.adapterStateSnapshot
    const wasReady = winRtAdapterIsReady(previous)
    this.adapterStateSnapshot = state
    if (wasReady && !winRtAdapterIsReady(state)) {
      this.backendGeneration += 1
      this.adapterGeneration += 1
      this.adapterLossPending = true
      this.security?.resetForAdapterLoss()
      this.dispatcher.cancelAll('reset').catch(error => {
        console.error('[WinRtBackend.adapter-state] Native operation cancellation failed:', error)
      })
    }
    if (this.adapterLossPending) {
      this.startAdapterLossCleanup()
    }
    const snapshot = winRtAdapterState(this.adapterStateSnapshot, this.backendGeneration, this.now)
    // A re-announced record that changes nothing observable is not a
    // transition: emitting it would duplicate the watch's initial snapshot
    // for every subscriber. Bookkeeping, the backend event and generation
    // advances are untouched.
    const unchanged =
      state.availability === previous.availability &&
      state.authorization === previous.authorization &&
      state.power === previous.power &&
      state.safeReason === previous.safeReason
    if (!unchanged) {
      for (const stream of this.stateStreams) {
        stream.emit(snapshot, 64, 'adapter-state')
      }
    }
    const attachment = this.attachment()
    broadcastWinRtEvent(this.eventStreams, {
      attachment,
      attachmentId: attachment.attachmentId,
      kind: 'adapter-state',
      ingressOrdinal: this.nextIngressOrdinal
    })
    this.nextIngressOrdinal += 1
  }

  private startAdapterLossCleanup(): void {
    if (this.adapterLossCleanup !== null) {
      return
    }
    this.adapterLossCleanup = this.releaseAdapterLossResources().then(
      async cleanup => {
        if (cleanup.state === 'release-failed') {
          this.adapterLossCleanup = null
          console.error('[WinRtBackend.adapter-state] Adapter loss cleanup requires retry:', cleanup.failures)
          return
        }
        await this.dispatcher.waitForIdle()
        const retainedResources = this.adapterLossRetainedResourcesFailure()
        if (retainedResources !== null) {
          this.adapterLossCleanup = null
          console.error('[WinRtBackend.adapter-state] Adapter loss cleanup requires retry:', retainedResources.failures)
          return
        }
        this.adapterLossPending = false
        this.peerIdsByNativeId.clear()
        this.nativeIdsByPeerId.clear()
        this.security?.adapterRecovered()
        this.adapterLossCleanup = null
      },
      error => {
        this.adapterLossCleanup = null
        console.error('[WinRtBackend.adapter-state] Adapter loss cleanup rejected:', error)
      }
    )
  }

  /** Does not reopen adapter admission while any physical resource remains accounted for after cleanup. */
  private adapterLossRetainedResourcesFailure(): CleanupRecord | null {
    const failures: CleanupFailure[] = []
    if (this.scanGroup !== null) {
      failures.push(
        ...cleanupFailure(
          'scan',
          'winrt.adapter-loss.late-scan-cleanup',
          new Error('WinRT scan watcher remains physically owned after adapter-loss cleanup')
        ).failures
      )
    }
    for (const record of this.connectionsById.values()) {
      if (record.state === 'connecting' || record.state === 'connected' || record.state === 'disconnecting') {
        failures.push(
          ...cleanupFailure(
            'connection',
            'winrt.adapter-loss.late-connect-cleanup',
            new Error('WinRT connection remains physically owned after adapter-loss cleanup')
          ).failures
        )
      }
    }
    for (const physical of this.subscriptions.values()) {
      failures.push(
        ...cleanupFailure(
          'subscription',
          'winrt.adapter-loss.subscription-cleanup',
          new Error(`WinRT CCCD enablement ${physical.key} remains physically owned after adapter-loss cleanup`)
        ).failures
      )
    }
    const retainedResources = Object.entries(this.resourceCounters())
      .filter(([, count]) => Number(count) !== 0)
      .map(([resource]) => resource)
    if (retainedResources.length === 0 && failures.length === 0) {
      return null
    }
    if (failures.length === 0) {
      failures.push(
        ...cleanupFailure(
          'backend',
          'winrt.adapter-loss.retained-resources',
          new Error(`WinRT adapter-loss cleanup retained resources: ${retainedResources.join(', ')}`)
        ).failures
      )
    }
    return Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
  }

  /** Releases every stale physical resource and preserves a failed resource for a later adapter-state retry. */
  private async releaseAdapterLossResources(): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    const subscriptionCleanups: Promise<CleanupRecord>[] = []
    for (const physical of [...this.subscriptions.values()]) {
      for (const consumer of physical.consumers) {
        consumer.stream.closeWithReason('connection-lost')
        consumer.removed = true
      }
      physical.consumers.clear()
      subscriptionCleanups.push(
        invalidateWinRtPhysicalSubscription(
          this,
          physical,
          contractError('adapter.unavailable', 'adapter', 'winrt.gatt.subscribe.adapter-loss')
        )
      )
    }
    const group = this.scanGroup
    if (group !== null) {
      for (const consumer of group.consumers.values()) {
        this.releaseScanAdmission(consumer)
        consumer.stream.closeWithReason('connection-lost')
        consumer.released = true
      }
      if (group.state === 'starting') {
        group.startTerminalError = contractError('operation.reset', 'scan', 'winrt.scan.start.adapter-loss')
        group.state = 'stopping'
      }
      if (group.stopResult !== null) {
        failures.push(...(await group.stopResult).failures)
      } else if (group.state === 'active' || group.state === 'cleanup-pending') {
        const cleanup = await this.stopScanGroup(group, 'winrt.adapter-loss.stop-scan')
        failures.push(...cleanup.failures)
      } else if (group.state === 'stopping') {
        if (group.stopSettlement !== null) {
          failures.push(...(await this.stopScanGroup(group, 'winrt.adapter-loss.stop-scan')).failures)
        } else if (group.startDispatch !== null) {
          await group.startDispatch.physicalSettlement
        }
        if (this.scanGroup === group) {
          const lateStopResult = this.scanGroupStopResult(group)
          const lateState = this.scanGroupState(group)
          if (lateStopResult !== null) {
            failures.push(...(await lateStopResult).failures)
          } else if (lateState === 'cleanup-pending') {
            // The late-start compensating stop already failed; retain ownership for the next
            // adapter-loss cleanup pass so the failure remains observable and retryable.
          } else if (!group.startInvocationActive) {
            this.retireScanGroup(group, 'connection-lost')
          }
        }
      }
    }
    for (const cleanup of subscriptionCleanups) {
      failures.push(...(await cleanup).failures)
    }
    // One native disconnect per peer: joined leases share the link, so only
    // the first record of a peer drives the radio; every record of a
    // released link is terminalized with its own generation. Peers are
    // snapshotted before the radio fires because retirement deletes records
    // from the map. A failed radio disconnect retains every record of the
    // peer for the next retry.
    const peerReleased = new Map<string, boolean>()
    for (const record of [...this.connectionsById.values()]) {
      if (record.state === 'connected' || record.state === 'disconnecting') {
        const previous = record.state
        const peers = this.recordsForNativePeer(record.nativePeerId)
        let released = peerReleased.get(record.nativePeerId)
        if (released === undefined) {
          const disconnectCleanup = await this.disconnect(record, 'winrt.adapter-loss.disconnect', false, false)
          failures.push(...disconnectCleanup.failures)
          released = disconnectCleanup.state !== 'release-failed'
          peerReleased.set(record.nativePeerId, released)
        }
        if (!released) {
          // A failed native disconnect retains the connected record, lease, and physical-link counter
          // for the next adapter-state cleanup retry.
          continue
        }
        for (const peerRecord of peers) {
          if (peerRecord.state === 'lost') {
            continue
          }
          this.terminalizeAdapterLossConnection(peerRecord, peerRecord === record ? previous : 'connected')
        }
      }
      if (record.state === 'connecting' && record.pendingConnect !== null) {
        this.terminalizePendingConnect(
          record,
          contractError('operation.reset', 'connection', 'winrt.connect.adapter-loss')
        )
      }
      if (record.state !== 'lost') {
        record.state = 'lost'
        record.database?.invalidate()
        record.database = null
        record.lease?.markReleased()
        record.lease = null
        if (this.connectionsById.get(String(record.connectionId)) === record) {
          this.connectionsById.delete(String(record.connectionId))
        }
      }
    }
    return failures.length === 0
      ? releasedCleanup
      : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
  }

  private async invalidateConnectionChildren(
    record: WinRtConnectionRecord,
    reason: 'connection-lost' | 'owner-released',
    pendingSubscriptionError: Error
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    record.database?.invalidate()
    record.database = null
    for (const physical of [...this.subscriptions.values()]) {
      const samePeer = physical.address.nativePeerId === record.nativePeerId
      if (!samePeer) {
        continue
      }
      for (const consumer of physical.consumers) {
        consumer.stream.closeWithReason(reason)
        consumer.removed = true
      }
      physical.consumers.clear()
      const cleanup = await invalidateWinRtPhysicalSubscription(this, physical, pendingSubscriptionError)
      failures.push(...cleanup.failures)
    }
    return failures.length === 0
      ? releasedCleanup
      : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
  }

  private releaseScanAdmission(consumer: WinRtScanConsumer): void {
    if (consumer.abort !== null) {
      consumer.options.signal?.removeEventListener('abort', consumer.abort)
      consumer.abort = null
    }
    if (consumer.deadlineTimer !== null) {
      clearTimeout(consumer.deadlineTimer)
      consumer.deadlineTimer = null
    }
  }

  private bindScanAdmission(consumer: WinRtScanConsumer): void {
    const closeForAbort = (): void => {
      this.stopScanConsumer(consumer).then(
        result => {
          if (result.state === 'release-failed') {
            console.error('[WinRtBackend.scan] Abort cleanup requires retry:', result.failures)
          }
        },
        error => console.error('[WinRtBackend.scan] Abort cleanup rejected:', error)
      )
    }
    consumer.abort = closeForAbort
    consumer.options.signal?.addEventListener('abort', closeForAbort, { once: true })
    if (consumer.options.deadline !== null) {
      consumer.deadlineTimer = setTimeout(closeForAbort, Math.max(0, consumer.options.deadline - this.now()))
    }
  }

  private async destroyInternal(): Promise<CleanupRecord> {
    this.admissionClosed = true
    this.security?.close()
    const failures: CleanupFailure[] = []
    try {
      await this.dispatcher.cancelAll('destroyed')
    } catch (error) {
      failures.push(...cleanupFailure('operation', 'winrt.destroy.cancel-operations', error).failures)
    }
    const subscriptionCleanups: Promise<CleanupRecord>[] = []
    for (const physical of [...this.subscriptions.values()]) {
      for (const consumer of physical.consumers) {
        consumer.removed = true
        consumer.stream.closeWithReason('owner-released')
      }
      physical.consumers.clear()
      subscriptionCleanups.push(
        invalidateWinRtPhysicalSubscription(
          this,
          physical,
          contractError('operation.cancelled-by-destroy', 'gatt', 'winrt.gatt.subscribe.destroy')
        )
      )
    }
    const adapterLossCleanup = this.adapterLossCleanup
    if (
      adapterLossCleanup !== null &&
      (await waitForWinRtSettlement(adapterLossCleanup, this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS, this.now)) ===
        'timed-out'
    ) {
      failures.push(...pendingWinRtOperationCleanup('winrt.destroy.adapter-loss-idle').failures)
    }
    if (
      (await waitForWinRtSettlement(
        this.dispatcher.waitForIdle(),
        this.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
        this.now
      )) === 'timed-out'
    ) {
      failures.push(...pendingWinRtOperationCleanup('winrt.destroy.dispatcher-idle').failures)
    }
    await Promise.resolve()
    const group = this.scanGroup
    if (group !== null) {
      for (const consumer of group.consumers.values()) {
        consumer.released = true
        this.releaseScanAdmission(consumer)
        consumer.stream.closeWithReason('owner-released')
      }
      if (group.stopResult !== null) {
        failures.push(...(await group.stopResult).failures)
      } else if (group.state === 'active' || group.state === 'cleanup-pending') {
        failures.push(...(await this.stopScanGroup(group, 'winrt.destroy.scan')).failures)
      } else if (group.stopSettlement !== null) {
        failures.push(...(await this.stopScanGroup(group, 'winrt.destroy.scan-retry')).failures)
      } else {
        if (group.startDispatch !== null) {
          await group.startDispatch.physicalSettlement
        }
        if (this.scanGroup === group) {
          const destroyStopResult = this.scanGroupStopResult(group)
          const destroyState = this.scanGroupState(group)
          if (destroyStopResult !== null) {
            failures.push(...(await destroyStopResult).failures)
          } else if (destroyState === 'cleanup-pending') {
            failures.push(...(await this.stopScanGroup(group, 'winrt.destroy.scan-retry')).failures)
          } else {
            this.retireScanGroup(group)
          }
        }
      }
    }
    for (const cleanup of subscriptionCleanups) {
      failures.push(...(await cleanup).failures)
    }
    for (const record of [...this.connectionsById.values()]) {
      failures.push(...(await this.disconnect(record, 'winrt.destroy.connection')).failures)
    }
    const nonZeroCounters = Object.entries(this.resourceCounters()).filter(([, value]) => Number(value) !== 0)
    if (nonZeroCounters.length > 0) {
      failures.push(
        ...cleanupFailure(
          'backend',
          'winrt.destroy.resource-counters',
          new Error(`WinRT cleanup retained counters: ${nonZeroCounters.map(([name]) => name).join(', ')}`)
        ).failures
      )
    }
    if (failures.length > 0) {
      return Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
    }
    try {
      await this.boundary.destroy().completion
    } catch (error) {
      return cleanupFailure('boundary', 'winrt.destroy.boundary', error)
    }
    this.destroyed = true
    this.removeConnectionListener()
    this.removeDatabaseListener()
    this.removeScanTerminalListener()
    this.removeAdapterStateListener()
    for (const stream of [...this.eventStreams]) {
      stream.closeWithReason('owner-released')
    }
    this.eventStreams.clear()
    for (const stream of [...this.stateStreams]) {
      stream.closeWithReason('owner-released')
    }
    this.stateStreams.clear()
    return releasedCleanup
  }
}
