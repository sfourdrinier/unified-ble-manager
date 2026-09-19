// src/backends/reactnative/react-native-rust-core-manager.ts
//
// R01 native-owned React Native manager: implements the internal manager
// surface over an admitted Rust core session plus the binding-backed backend,
// WITHOUT constructing the TypeScript scheduling/lifecycle core
// (`UnifiedBleCore.attach` is never called; the module-graph test forbids
// runtime imports of `core/unified-ble-core`, `core/core-gatt-handles`, and
// `manager/ble-manager`). Deadline/overflow/teardown authority lives behind
// `session.invoke/close`; this file is a thin bridge: option mapping, lease
// wrapping, lifecycle-event projection, and destroy-time release.
//
// Pure rule/plumbing helpers ARE reused from `core/` (`createCoreFeatureRegistry`,
// `scheduleCoreDeadline`, `retryableCleanup`, lifecycle-rule predicates, path
// equality): they construct no lifecycle state. Every other decision crosses
// the native session. Members with no native route fail closed with frozen
// identities (see the connection-controls and ownership-transfer members).

import {
  assertBackendEvent,
  attachBackend,
  type AttachedBackend,
  type BackendAttachment,
  type BackendConnection,
  type BackendEvent,
  type BleCentralBackend,
  type ConnectionLease,
  type ConnectionOptions,
  type ConnectionState,
  type OwnerMode,
  type ResourceCounters,
  type ScanLease
} from '../../backend-contract/backend'
import type { OwnerScanOptions, ScanOptions } from '../../backend-contract/advertisement'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'
import { BUILT_IN_FEATURE_IDS } from '../../backend-contract/capabilities'
import type { CapabilityDescriptor, FeatureId, FeatureState } from '../../backend-contract/capabilities'
import { MAXIMUM_REQUESTED_ATT_MTU, MINIMUM_ATT_MTU } from '../../backend-contract/connection-controls'
import type { SecurityBackend } from '../../backend-contract/security'
import {
  assertBackendLifecycleTransition,
  isConnectionLossCause,
  lifecycleCauseFromBackendDisconnect
} from '../../core/connection-lifecycle-rules'
import type {
  ConnectionLifecycleCause,
  ConnectionLifecycleEvent,
  ConnectionLifecycleTerminalCause
} from '../../backend-contract/connection-lifecycle'
import type {
  ConnectionPhyObservation,
  ConnectionPhyRequest,
  ConnectionPriority,
  ConnectionPriorityRequest,
  ConnectionWriteReadinessWatch,
  EffectiveMtuMeasurement,
  MtuNegotiation,
  PhyPreference,
  RssiMeasurement
} from '../../backend-contract/connection-controls'
import { CoreBoundedStream } from '../../core/bounded-stream'
import { createCoreFeatureRegistry, observeMaximumWriteLength, planLongWrite } from '../../core/core-capabilities'
import type { LongWriteFeatureOutput } from '../../backend-contract/capabilities'
import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { CleanupFailure, CleanupRecord } from '../../backend-contract/errors'
import type {
  CharacteristicPath,
  ConnectionPath,
  DatabasePath,
  DescriptorPath,
  GattDatabaseChangedEvent,
  GattDatabaseSnapshot,
  MaximumWriteLengthObservation,
  Subscription as BackendSubscription
} from '../../backend-contract/gatt'
import { connectionPathsEqual, databasePathsEqual } from '../../core/gatt-path-equality'
import type { NativeBackendIdentity } from '../../backend-contract/identity'
import type { AdapterStateSnapshot, AttachmentRecord } from '../../backend-contract/identity'
import type {
  AttachmentId,
  BackendCompatibilityOffer,
  ByteLimit,
  ClientId,
  ConnectionId,
  GenerationId,
  LeaseId,
  ManagerId,
  OperationCorrelation,
  OwnedBytes,
  PeerId
} from '../../backend-contract/primitives'
import { capacity, deadline, ownBytes } from '../../backend-contract/primitives'
import { utf8ByteLength } from '../../backend-contract/serializable'
import type {
  BackendOperationDispatch,
  CharacteristicRead,
  OperationOptions,
  LongWriteChunkProgress,
  LongWritePolicy,
  LongWriteReceipt,
  PublicOperationOptions,
  SubscriptionOptions,
  WriteMode,
  WritePolicy,
  WriteReceipt
} from '../../backend-contract/operations'
import type { RestorationAdoptionRequest, RestorationAdoptionResult } from '../../backend-contract/restoration'
import type { ScanPlan } from '../../backend-contract/scan-planning'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import type { DiagnosticTraceDocument } from '../../diagnostics/trace-format'
import type { CoreTraceRecord, CoreTraceRecorder } from '../../core/trace-recorder'
import {
  recordBackendDiagnostic,
  retryableCleanup,
  scheduleCoreDeadline,
  type CoreDeadlineHandle,
  type CoreDeadlineScheduler
} from '../../core/unified-ble-core-helpers'
import type {
  BleManager as InternalBleManager,
  Connection as InternalConnection,
  DiscoveredGattDatabase as InternalDiscoveredGattDatabase,
  ScanSession as InternalScanSession,
  Subscription as InternalSubscription
} from '../../manager/ble-manager'
import type {
  DeadlineHandle,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableOperationOptions,
  PortableSubscriptionOptions,
  PortableWritePolicy
} from '../../manager/consumer-handles'
import type {
  OwnershipRoleTransitionCapability,
  OwnershipTransferGrant
} from '../../manager/manager-ownership-authority'
import type { ReactNativeRestorationCoordinator } from './react-native-restoration'
import type { ReactNativeRustCoreBackend } from './react-native-rust-core-provider'

type CurrentCharacteristicPath = CharacteristicPath<string, string, string, string, string, 'current'>
type CurrentDescriptorPath = DescriptorPath<string, string, string, string, string, string, 'current'>

// Lifecycle-event window, mirrored from the core handle implementation
// (`core-gatt-handles.ts` keeps these private): 8 retained transitions is a
// transport budget, not lifecycle authority.
const CONNECTION_LIFECYCLE_ITEM_CAPACITY = 8
const CONNECTION_LIFECYCLE_RESERVED_CONTROL_CAPACITY = 256

/** Inputs that bind one native-owned manager to one opened backend. */
export interface ReactNativeRustCoreManagerOptions {
  readonly backend: ReactNativeRustCoreBackend
  readonly coreCompatibility: BackendCompatibilityOffer
  readonly clientId: ClientId<string, string>
  readonly managerId: ManagerId<string, string>
  readonly ownerMode: OwnerMode
  readonly restoration?: {
    readonly client: { readonly clientId: ClientId<string, string>; readonly hostSessionScope: string }
    readonly coordinator: ReactNativeRestorationCoordinator
  }
  readonly now: () => number
  readonly timer?: CoreDeadlineScheduler
  readonly maximumValueBytes: ByteLimit
  /**
   * The bounded diagnostic trace the backend records into
   * (`diagnostics.traceMaximumRecords` / `traceMaximumBytes`).
   */
  readonly trace: CoreTraceRecorder
}

/**
 * Creates the native-owned manager. The backend MUST already be opened
 * (session admitted, restoration activated). The backend keeps owning the
 * session lifecycle (invoke/close); the manager owns leases, lifecycle
 * projection, and destroy-time release. No `UnifiedBleCore` is constructed
 * on this path, by design.
 */
export async function createReactNativeRustCoreManager(
  options: ReactNativeRustCoreManagerOptions
): Promise<InternalBleManager<string, NativeBackendIdentity<string>>> {
  const attached = await attachBackend(
    options.backend as unknown as BleCentralBackend<string, NativeBackendIdentity<string>>,
    options.coreCompatibility
  )
  const manager = new ReactNativeRustCoreManager(options, attached)
  manager.startBackendEventPump()
  // Structural cast: the class below implements every public member of the
  // internal manager shape without extending it (the constructor demands a
  // `UnifiedBleCore`, which this path must never build). The prototype-parity
  // test fails loudly on any drift.
  return manager as unknown as InternalBleManager<string, NativeBackendIdentity<string>>
}

type ManagerState = 'ready' | 'destroying' | 'destroyed'

class ReactNativeRustCoreManager {
  private managerState: ManagerState = 'ready'
  private readonly featuresRegistry: ReturnType<typeof createCoreFeatureRegistry>
  private readonly connections = new Map<string, NativeConnection>()
  private readonly connectionReleases = new Map<string, Promise<CleanupRecord>>()
  private readonly discoveries = new Map<
    string,
    { connection: NativeConnection; promise: Promise<NativeGattDatabase> }
  >()
  private readonly openScans = new Set<{ stop(): Promise<CleanupRecord> }>()
  private readonly openAdapterWatches = new Set<{ close(): Promise<unknown> }>()
  private resourceReleaseResult: Promise<CleanupRecord> | null = null
  private destroyResult: Promise<CleanupRecord> | null = null
  private eventsPumpStarted = false
  private attachment: BackendAttachment<string, NativeBackendIdentity<string>>
  private readonly attachmentListeners = new Set<
    (previous: AttachmentRecord<string>, current: AttachmentRecord<string>) => void
  >()

  constructor(
    private readonly options: ReactNativeRustCoreManagerOptions,
    private readonly attached: AttachedBackend<string, NativeBackendIdentity<string>>
  ) {
    this.featuresRegistry = createCoreFeatureRegistry(options.backend.features)
    this.attachment = attached.attachment
  }

  private get backend(): ReactNativeRustCoreBackend {
    return this.options.backend
  }

  get state(): ManagerState {
    return this.managerState
  }

  get identity(): NativeBackendIdentity<string> {
    return this.attachment.identity
  }

  get attachmentId(): AttachmentId<string> {
    return this.attachment.attachment.attachmentId
  }

  /** The manager followed its backend to a new attachment after an adapter loss. */
  onAttachmentAdvanced(
    listener: (previous: AttachmentRecord<string>, current: AttachmentRecord<string>) => void
  ): () => void {
    this.attachmentListeners.add(listener)
    return () => {
      this.attachmentListeners.delete(listener)
    }
  }

  get managerId(): ManagerId<string, string> {
    return this.options.managerId
  }

  get clientId(): ClientId<string, string> {
    return this.options.clientId
  }

  get ownerMode(): OwnerMode {
    return this.options.ownerMode
  }

  get attachedBackend(): AttachedBackend<string, NativeBackendIdentity<string>> {
    return this.attached
  }

  get features() {
    return this.featuresRegistry
  }

  securityBackend(): SecurityBackend | undefined {
    this.assertReady('security.backend')
    return this.backend.security ?? undefined
  }

  supports(id: FeatureId): boolean {
    const descriptor = this.capability(id)
    return descriptor !== null && (descriptor.state === 'supported' || descriptor.state === 'limited')
  }

  capability(id: FeatureId): CapabilityDescriptor | null {
    for (const descriptor of this.features.descriptors) {
      if (descriptor.id === id) {
        return descriptor
      }
    }
    return null
  }

  capabilities(): readonly CapabilityDescriptor[] {
    return this.features.descriptors
  }

  /** The registered state of one feature (`unsupported` when not registered). */
  featureState(id: FeatureId): FeatureState {
    return this.features.registrations.find(candidate => candidate.id === id)?.state ?? 'unsupported'
  }

  get backendConnections(): ReactNativeRustCoreBackend['connections'] {
    return this.backend.connections
  }

  get backendGatt(): ReactNativeRustCoreBackend['gatt'] {
    return this.backend.gatt
  }

  /** The next public correlation (legacy `operation-{n}`). */
  publicCorrelation(): OperationCorrelation<string, string> {
    return this.backend.publicCorrelation()
  }

  /** Operation options with the next public correlation (legacy `operation-{n}`). */
  operationOptions(options: PublicOperationOptions): OperationOptions<string, string> {
    return Object.freeze({
      signal: options.signal,
      deadline: options.deadline,
      correlation: this.backend.publicCorrelation()
    })
  }

  adoptRestoration(request: RestorationAdoptionRequest<string>): Promise<RestorationAdoptionResult<string>> {
    if (this.managerState !== 'ready') {
      throw contractError('lifecycle.destroyed', 'restoration', 'rust-core-manager.adopt-restoration')
    }
    const restoration = this.options.restoration
    if (restoration === undefined) {
      throw contractError('capability.unsupported', 'restoration', 'rust-core-manager.adopt-restoration')
    }
    return restoration.coordinator.adopt(restoration.client, request)
  }

  async scan(options: ScanOptions<string, string>): Promise<InternalScanSession<string>> {
    this.assertReady('scan')
    this.assertOperationAdmission(options, 'scan')
    let lease: ScanLease<string, string>
    try {
      lease =
        options.sharing.mode === 'owner'
          ? await this.backend.scanner.start(
              { ...options, sharing: options.sharing } as OwnerScanOptions<string, string>,
              this.options.clientId
            )
          : await this.backend.scanner.join(options.sharing.sharedLeaseId, options.sharing.token, this.options.clientId)
    } catch (error) {
      throw error instanceof BackendContractError
        ? error
        : contractError('scan.start-failed', 'scan', 'rust-core-manager.scan')
    }
    if (this.managerState !== 'ready') {
      await lease.stop().catch(() => undefined)
      throw contractError('lifecycle.destroyed', 'core', 'scan')
    }
    const tracked = { stop: () => lease.stop() }
    this.openScans.add(tracked)
    const originalStop = lease.stop.bind(lease)
    // The scan stays tracked until the owner confirms release: a failed stop
    // is retried by the next stop() or by destroy (PR210-09).
    const stopOnce = async (): Promise<CleanupRecord> => {
      const record = await originalStop()
      if (record.state === 'released') this.openScans.delete(tracked)
      return record
    }
    // The lease already carries the scan-session surface (ids, observation
    // stream, stop); the cast retypes it without wrapping behavior.
    return { ...lease, stop: stopOnce } as unknown as InternalScanSession<string>
  }

  planScan(query: NormalizedScanQuery): ScanPlan | null {
    this.assertReady('scan.plan')
    return this.backend.scanner.plan?.(query) ?? null
  }

  async connect(
    peerId: PeerId<string>,
    options: ConnectionOptions
  ): Promise<InternalConnection<string, NativeBackendIdentity<string>>> {
    this.assertReady('connect')
    this.assertOperationAdmission(options, 'connect')
    let lease: ConnectionLease<string, string, string>
    try {
      lease = await this.backend.connections.connect(peerId, this.options.clientId, options)
    } catch (error) {
      throw error instanceof BackendContractError
        ? error
        : contractError('connection.failed', 'connection', 'rust-core-manager.connect')
    }
    if (this.managerState !== 'ready') {
      await lease.release().catch(() => undefined)
      throw contractError('lifecycle.destroyed', 'core', 'connect')
    }
    const connection = new NativeConnection(this, lease)
    this.connections.set(String(lease.connection.connectionId), connection)
    return connection as unknown as InternalConnection<string, NativeBackendIdentity<string>>
  }

  destroy(): Promise<CleanupRecord> {
    if (this.destroyResult === null) {
      const destruction = this.ownerMode === 'owning' ? this.destroyOwningManager() : this.destroyBorrowingManager()
      this.destroyResult = retryableCleanup(destruction, () => {
        this.destroyResult = null
      })
    }
    return this.destroyResult
  }

  private async destroyBorrowingManager(): Promise<CleanupRecord> {
    return this.releaseOwnedResources()
  }

  private async destroyOwningManager(): Promise<CleanupRecord> {
    const ownCleanup = await this.releaseOwnedResources()
    // Finding 185: a debt above must not strand the backend session — an
    // unreleased scan membership would survive the destroyed manager and
    // brick every later scan on the process host with scan.already-active.
    // The backend dispose retries the native release; both records merge so
    // no debt is swallowed.
    const backendCleanup = await this.backend.destroy()
    const failures: CleanupFailure[] = [...ownCleanup.failures, ...backendCleanup.failures]
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  revokeForOwnerDestroy(): Promise<CleanupRecord> {
    return this.releaseOwnedResources()
  }

  // Ownership transfer (PR210-71). The legacy factory built this manager with
  // `createBleManagerFromProvider`, whose attachment authority was issued
  // internally and never returned: no application could obtain a grant for
  // it or register a borrower. This manager is no authority participant
  // either, so a borrower cannot be admitted against it, and these members
  // answer the identities the legacy authority answered
  // (`__tests__/backends/reactnative/rust-core-legacy-parity.test.js`).
  async transferOwnership(_grant: OwnershipTransferGrant<string>): Promise<CleanupRecord> {
    throw contractError('ownership.denied', 'core', 'manager-ownership-authority.transfer-grant')
  }

  acceptsOwnershipTransfer(): boolean {
    return false
  }

  becomeOwnershipTransferDestination(_capability: OwnershipRoleTransitionCapability): void {
    throw contractError('ownership.denied', 'core', 'manager-ownership-authority.role-capability')
  }

  relinquishOwnershipTransferSource(_capability: OwnershipRoleTransitionCapability): void {
    throw contractError('ownership.denied', 'core', 'manager-ownership-authority.role-capability')
  }

  /** The bounded trace of every operation this manager's backend sent to the owner. */
  traces(): readonly CoreTraceRecord[] {
    return Object.freeze(this.options.trace.snapshot())
  }

  traceDocument(): DiagnosticTraceDocument {
    return this.options.trace.snapshotDocument()
  }

  monotonicNow(): number {
    return this.options.now()
  }

  scheduleDeadline(deadlineAt: number, action: () => void): CoreDeadlineHandle {
    return scheduleCoreDeadline(deadlineAt, action, this.options.timer, this.options.now)
  }

  localResourceCounters(): ResourceCounters {
    return this.backend.resourceCounters()
  }

  adapterState(): Promise<AdapterStateSnapshot<string>> {
    this.assertReady('adapter-state')
    return this.backend.adapter.currentState()
  }

  async adapterStates(options: { readonly signal?: AbortSignal | null } = {}): Promise<{
    readonly initial: AdapterStateSnapshot<string>
    readonly values: BoundedAsyncStream<AdapterStateSnapshot<string>>
    stop(): Promise<CleanupRecord>
  }> {
    this.assertReady('adapter-states')
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'adapter', 'adapter-states')
    }
    const watch = await this.backend.adapter.watchState()
    if (this.managerState !== 'ready') {
      await watch.transitions.close().catch(() => undefined)
      throw contractError('lifecycle.destroyed', 'core', 'adapter-states')
    }
    const tracked = { close: () => watch.transitions.close() }
    this.openAdapterWatches.add(tracked)
    return {
      initial: watch.initial,
      values: watch.transitions,
      stop: async () => {
        try {
          await watch.transitions.close()
        } finally {
          this.openAdapterWatches.delete(tracked)
        }
        return { state: 'released', failures: [] }
      }
    }
  }

  async discoverOnConnection(
    connection: NativeConnection,
    options: PublicOperationOptions
  ): Promise<InternalDiscoveredGattDatabase<string, NativeBackendIdentity<string>>> {
    this.assertReady('discover')
    this.assertOperationAdmission(options, 'discover')
    const key = String(connection.connectionId)
    const registered = this.discoveries.get(key)
    if (registered !== undefined && registered.connection === connection) {
      let existing: NativeGattDatabase
      try {
        existing = await registered.promise
      } catch {
        throw contractError('gatt.stale-handle', 'gatt', 'rust-core-manager.discover.concurrent')
      }
      if (connection.database === existing) {
        return this.wrapDiscovered(existing)
      }
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-manager.discover.replaced')
    }
    const discovery = this.runDiscovery(connection, options)
    this.discoveries.set(key, { connection, promise: discovery })
    try {
      return this.wrapDiscovered(await discovery)
    } finally {
      if (this.discoveries.get(key)?.promise === discovery) {
        this.discoveries.delete(key)
      }
    }
  }

  async rediscoverOnConnection(
    connection: NativeConnection,
    options: PublicOperationOptions,
    reason: Extract<GattDatabaseChangedEvent['reason'], 'service-changed' | 'manual-rediscovery'>
  ): Promise<InternalDiscoveredGattDatabase<string, NativeBackendIdentity<string>>> {
    this.assertReady('discover')
    this.assertOperationAdmission(options, 'discover')
    const existing = connection.database
    if (existing !== null && [...this.discoveries.values()].some(entry => entry.connection === connection)) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-manager.rediscover.concurrent')
    }
    if (existing !== null) {
      await connection.invalidateDatabase('owner-released', reason)
    }
    return this.discoverOnConnection(connection, options)
  }

  async cleanupInvalidDatabase(
    database: NativeGattDatabase,
    _reason: 'connection-lost' | 'owner-released'
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const subscription of database.drainSubscriptions()) {
      try {
        const record = await subscription.removeBackend()
        failures.push(...record.failures)
      } catch (error) {
        failures.push(asCleanupFailure('subscription', error))
      }
    }
    database.connection.completeDatabaseCleanup(database)
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private async runDiscovery(
    connection: NativeConnection,
    options: PublicOperationOptions
  ): Promise<NativeGattDatabase> {
    connection.assertCurrent()
    const backendDatabase = await this.backend.gatt.discover(connection.resource, options)
    if (this.managerState !== 'ready' || !connection.isCurrent()) {
      throw contractError('lifecycle.destroyed', 'core', 'discover')
    }
    const database = new NativeGattDatabase(this, backendDatabase, connection, this.options)
    connection.setDatabase(database)
    return database
  }

  private async wrapDiscovered(
    database: NativeGattDatabase
  ): Promise<InternalDiscoveredGattDatabase<string, NativeBackendIdentity<string>>> {
    const snapshot = await database.snapshot()
    return new NativeDiscoveredGattDatabase(database, snapshot) as unknown as InternalDiscoveredGattDatabase<
      string,
      NativeBackendIdentity<string>
    >
  }

  /** Releases scans, connections, and watches; the backend + session close at destroy. */
  /**
   * The owner advanced its generations after an adapter loss. 5.0 keeps the
   * manager (legacy destroyed it, `unified-ble-core.ts` `releaseResources('backend-restart')`):
   * every connection of the old generation ends `adapter-loss` and is
   * released; scans and streams already ended with the owner's records, and
   * a new connection can follow the adapter's return. A release that fails
   * stays in `connections`, so `destroy()` retries and reports it.
   */
  /**
   * Binds the backend's current attachment when the same backend instance on
   * the same adapter advanced its generation, so later events of the new
   * generation reach the manager; answers whether the manager is bound to the
   * backend's current attachment.
   */
  private followBackendGeneration(): boolean {
    const held = this.attachment.attachment
    const current = this.backend.identity.attachment
    if (current.attachmentId === held.attachmentId) {
      return true
    }
    if (current.backendInstanceId !== held.backendInstanceId || current.adapter.adapterId !== held.adapter.adapterId) {
      return false
    }
    this.attachment = Object.freeze({ ...this.attachment, attachment: current, identity: this.backend.identity })
    for (const listener of [...this.attachmentListeners]) {
      try {
        listener(held, current)
      } catch (error) {
        console.error('[ReactNativeRustCoreManager.followBackendGeneration] An attachment listener failed:', error)
      }
    }
    return true
  }

  private releaseAfterAdapterLoss(): void {
    for (const connection of [...this.connections.values()]) {
      connection.finishLifecycle('adapter-loss', null)
      this.releaseConnection(connection, 'adapter-loss').catch(error => {
        // The connection stays owned; destroy() retries and reports it.
        console.error('[ReactNativeRustCoreManager] Adapter-loss connection release failed:', error)
      })
    }
  }

  private releaseOwnedResources(cause: ConnectionLifecycleTerminalCause = 'manager-destroyed'): Promise<CleanupRecord> {
    if (this.resourceReleaseResult === null) {
      this.managerState = 'destroying'
      this.resourceReleaseResult = retryableCleanup(this.releaseOwnedResourcesInternal(cause), () => {
        this.resourceReleaseResult = null
      })
    }
    return this.resourceReleaseResult
  }

  private async releaseOwnedResourcesInternal(cause: ConnectionLifecycleTerminalCause): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const connection of [...this.connections.values()]) {
      connection.finishLifecycle(cause, null)
    }
    for (const scan of [...this.openScans]) {
      try {
        const record = await scan.stop()
        failures.push(...record.failures)
        if (record.state === 'released') this.openScans.delete(scan)
      } catch (error) {
        failures.push(asCleanupFailure('scan', error))
      }
    }
    for (const connection of [...this.connections.values()]) {
      try {
        const record = await this.releaseConnection(connection, cause)
        failures.push(...record.failures)
      } catch (error) {
        failures.push(asCleanupFailure('connection', error))
      }
    }
    for (const watch of [...this.openAdapterWatches]) {
      await watch.close().catch(() => undefined)
    }
    this.openAdapterWatches.clear()
    this.managerState = 'destroyed'
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  async releaseConnection(
    connection: NativeConnection,
    cause: ConnectionLifecycleTerminalCause
  ): Promise<CleanupRecord> {
    const key = String(connection.resource.connectionId)
    const inFlight = this.connectionReleases.get(key)
    if (inFlight !== undefined) {
      return inFlight
    }
    if (connection.isReleased()) {
      return { state: 'released', failures: [] }
    }
    const release = this.releaseConnectionCurrent(connection, cause)
    this.connectionReleases.set(key, release)
    try {
      return await release
    } finally {
      if (this.connectionReleases.get(key) === release) {
        this.connectionReleases.delete(key)
      }
    }
  }

  private async releaseConnectionCurrent(
    connection: NativeConnection,
    cause: ConnectionLifecycleTerminalCause
  ): Promise<CleanupRecord> {
    const disconnect = cause === 'requested-disconnect'
    const reason = isConnectionLossCause(cause) ? 'connection-lost' : 'owner-released'
    const children = await connection.cleanupChildren(reason)
    let backendResult: CleanupRecord
    try {
      backendResult =
        disconnect && connection.isCurrent() ? await connection.resource.disconnect() : await connection.lease.release()
    } catch (error) {
      backendResult = { state: 'release-failed', failures: [asCleanupFailure('connection', error)] }
    }
    connection.finishLifecycle(cause, null)
    if (backendResult.state !== 'released') {
      // The backend kept the lease; the connection stays registered so a
      // retried release reaches the same native identity (PR210-09).
      return { state: 'release-failed', failures: [...children.failures, ...backendResult.failures] }
    }
    connection.markReleased()
    this.connections.delete(String(connection.resource.connectionId))
    if (children.state !== 'released') {
      return { state: 'release-failed', failures: [...children.failures, ...backendResult.failures] }
    }
    return backendResult
  }

  startBackendEventPump(): void {
    if (this.eventsPumpStarted) {
      return
    }
    this.eventsPumpStarted = true
    const events = this.backend.events()
    const pump = (async () => {
      try {
        for await (const item of events) {
          if (this.managerState !== 'ready') {
            return
          }
          if (item.kind !== 'value') {
            await this.releaseOwnedResources('backend-failure')
            return
          }
          assertBackendEvent(item.value)
          this.applyBackendEvent(item.value)
        }
      } catch {
        await this.releaseOwnedResources('backend-failure').catch(() => undefined)
      }
    })()
    pump.catch(() => undefined)
  }

  private applyBackendEvent(event: BackendEvent<string>): void {
    if (event.kind === 'diagnostic-warning') {
      recordBackendDiagnostic(this.options.trace, this.options.now, event)
      return
    }
    if (event.kind === 'backend-restarted' || event.kind === 'backend-restarting') {
      if (event.attachment.adapter.adapterId === this.attachment.attachment.adapter.adapterId) {
        if (this.followBackendGeneration()) {
          this.releaseAfterAdapterLoss()
        } else {
          // A different backend instance replaced this one: nothing the
          // manager holds survives it, so it ends as legacy did.
          this.releaseOwnedResources('backend-restart').catch(error => {
            console.error('[ReactNativeRustCoreManager] Backend-restart release failed:', error)
          })
        }
      }
      return
    }
    if (event.attachmentId !== this.attachmentId) {
      this.followBackendGeneration()
      if (event.attachmentId !== this.attachmentId) {
        return
      }
    }
    if (event.kind === 'database-changed') {
      for (const connection of this.connections.values()) {
        const database = connection.database
        if (database !== null && database.matchesDatabasePath(event.database)) {
          database.markInvalid('service-changed')
        }
      }
      return
    }
    if (event.kind === 'connection-lost' || event.kind === 'disconnected') {
      const connection = this.connections.get(String(event.connection.connectionId))
      if (connection !== undefined && connection.matchesConnectionPath(event.connection)) {
        const cause =
          event.kind === 'connection-lost' ? 'peer-link-loss' : lifecycleCauseFromBackendDisconnect(event.reason)
        connection.finishLifecycle(cause, event.ingressOrdinal)
        this.releaseConnection(connection, cause).catch(() => undefined)
      }
      return
    }
    if (event.kind === 'connection-state-changed') {
      const connection = this.connections.get(String(event.connection.connectionId))
      if (connection !== undefined && connection.matchesConnectionPath(event.connection)) {
        if (event.current === 'disconnected' || event.current === 'lost') {
          if (event.reason === null) {
            throw contractError(
              'lifecycle.invariant-violation',
              'connection',
              'backend-event-terminal-transition-reason'
            )
          }
          const cause = lifecycleCauseFromBackendDisconnect(event.reason)
          connection.finishBackendLifecycle(event.previous, event.current, cause, event.ingressOrdinal)
          this.releaseConnection(connection, cause).catch(() => undefined)
        } else {
          connection.applyBackendTransition(event.previous, event.current, event.ingressOrdinal)
        }
      }
    }
  }

  private assertReady(operation: string): void {
    if (this.managerState !== 'ready') {
      throw contractError(
        this.managerState === 'destroyed' ? 'lifecycle.destroyed' : 'lifecycle.invalid-state',
        'core',
        operation
      )
    }
  }

  private assertOperationAdmission(options: PublicOperationOptions, operation: string): void {
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', operation)
    }
    if (options.deadline !== null && options.deadline !== undefined && options.deadline <= this.options.now()) {
      throw contractError('operation.timed-out', 'core', operation)
    }
  }
}

type NativeGattDatabaseHandle = import('../../backend-contract/gatt').GattDatabase<string, string, string>

class NativeConnection {
  private released = false
  private readonly lifecycleStream: CoreBoundedStream<ConnectionLifecycleEvent<string>>
  private lifecycleState: ConnectionState = 'connecting'
  private lifecycleFinished = false
  private nextLifecycleSequence = 1
  private lastBackendIngressOrdinal: number | null = null
  database: NativeGattDatabase | null = null
  private pendingDatabaseCleanup: NativeGattDatabase | null = null

  constructor(
    private readonly manager: ReactNativeRustCoreManager,
    readonly lease: ConnectionLease<string, string, string>
  ) {
    const maximumEventBytes = maximumLifecycleEventByteLength(lease)
    this.lifecycleStream = new CoreBoundedStream<ConnectionLifecycleEvent<string>>(
      Object.freeze({
        itemCapacity: capacity(CONNECTION_LIFECYCLE_ITEM_CAPACITY),
        byteCapacity: capacity(
          maximumEventBytes * CONNECTION_LIFECYCLE_ITEM_CAPACITY + CONNECTION_LIFECYCLE_RESERVED_CONTROL_CAPACITY
        ),
        reservedControlCapacity: capacity(CONNECTION_LIFECYCLE_RESERVED_CONTROL_CAPACITY)
      }),
      'drop-oldest'
    )
    this.emitLifecycle('connected', 'connected', null)
  }

  get resource(): BackendConnection<string, string> {
    return this.lease.connection
  }

  get peerId(): PeerId<string> {
    return this.resource.peerId
  }

  get connectionId(): ConnectionId<string, string> {
    return this.resource.connectionId
  }

  get ownerLeaseId(): LeaseId<string, string> {
    return this.lease.leaseId
  }

  get connectionGeneration(): GenerationId<'connection-generation', string> {
    return this.resource.connectionGeneration
  }

  get events(): BoundedAsyncStream<ConnectionLifecycleEvent<string>> {
    return this.lifecycleStream
  }

  async discover(
    options: PortableOperationOptions
  ): Promise<InternalDiscoveredGattDatabase<string, NativeBackendIdentity<string>>> {
    return this.manager.discoverOnConnection(this, toPublicOperationOptions(options))
  }

  async rediscoverGatt(
    options: PortableOperationOptions,
    reason: Extract<GattDatabaseChangedEvent['reason'], 'service-changed' | 'manual-rediscovery'>
  ): Promise<InternalDiscoveredGattDatabase<string, NativeBackendIdentity<string>>> {
    return this.manager.rediscoverOnConnection(this, toPublicOperationOptions(options), reason)
  }

  release(): Promise<CleanupRecord> {
    return this.manager.releaseConnection(this, 'released')
  }

  disconnect(): Promise<CleanupRecord> {
    return this.manager.releaseConnection(this, 'requested-disconnect')
  }

  async readRssi(options: PortableOperationOptions): Promise<RssiMeasurement<string, string>> {
    return this.control(BUILT_IN_FEATURE_IDS.connectionRssi, options, 'read-rssi', (connections, operation) =>
      requireControl(connections.readRssi, 'read-rssi')(this.resource, { operation })
    )
  }

  async requestMtu(requestedMtu: number, options: PortableOperationOptions): Promise<MtuNegotiation<string, string>> {
    if (
      !Number.isSafeInteger(requestedMtu) ||
      requestedMtu < MINIMUM_ATT_MTU ||
      requestedMtu > MAXIMUM_REQUESTED_ATT_MTU
    ) {
      throw contractError('argument.invalid', 'connection', 'rust-core-manager.connection.request-mtu')
    }
    return this.control(BUILT_IN_FEATURE_IDS.connectionRequestMtu, options, 'request-mtu', (connections, operation) =>
      requireControl(connections.requestMtu, 'request-mtu')(this.resource, { operation, requestedMtu })
    )
  }

  async effectiveMtu(): Promise<EffectiveMtuMeasurement<string, string>> {
    return this.control(
      BUILT_IN_FEATURE_IDS.connectionEffectiveMtu,
      { signal: null, deadline: null },
      'effective-mtu',
      (connections, operation) =>
        requireControl(connections.effectiveMtu, 'effective-mtu')(this.resource, { operation })
    )
  }

  async requestPriority(
    priority: ConnectionPriority,
    options: PortableOperationOptions
  ): Promise<ConnectionPriorityRequest<string, string>> {
    if (priority !== 'low-power' && priority !== 'balanced' && priority !== 'high-throughput') {
      throw contractError('argument.invalid', 'connection', 'rust-core-manager.connection.request-priority')
    }
    return this.control(
      BUILT_IN_FEATURE_IDS.connectionPriority,
      options,
      'request-priority',
      (connections, operation) =>
        requireControl(connections.requestPriority, 'request-priority')(this.resource, { operation, priority })
    )
  }

  async readPhy(options: PortableOperationOptions): Promise<ConnectionPhyObservation<string, string>> {
    return this.control(BUILT_IN_FEATURE_IDS.connectionPhy, options, 'read-phy', (connections, operation) =>
      requireControl(connections.readPhy, 'read-phy')(this.resource, { operation })
    )
  }

  async requestPhy(
    preference: PhyPreference,
    options: PortableOperationOptions
  ): Promise<ConnectionPhyRequest<string, string>> {
    if (preference.tx === undefined && preference.rx === undefined) {
      throw contractError('argument.invalid', 'connection', 'rust-core-manager.connection.request-phy')
    }
    return this.control(BUILT_IN_FEATURE_IDS.connectionPhy, options, 'request-phy', (connections, operation) =>
      requireControl(connections.requestPhy, 'request-phy')(this.resource, { operation, preference })
    )
  }

  async maximumWriteLength(
    mode: WriteMode,
    options: PortableOperationOptions
  ): Promise<MaximumWriteLengthObservation<string>> {
    if (mode !== 'with-response' && mode !== 'without-response') {
      throw contractError('argument.invalid', 'gatt', 'rust-core-manager.connection.maximum-write-length')
    }
    const measured = await this.control(
      BUILT_IN_FEATURE_IDS.maximumWriteLength,
      options,
      'maximum-write-length',
      (connections, operation) =>
        requireControl(connections.maximumWriteLength, 'maximum-write-length')(this.resource, { operation, mode })
    )
    return Object.freeze({
      connectionId: measured.connectionId,
      connectionGeneration: measured.connectionGeneration,
      mode: measured.mode,
      maximumWriteLength: measured.maximumWriteLength,
      observedAtMonotonicMs: measured.observedAtMonotonicMs
    })
  }

  async writeWithoutResponseReadiness(
    _options?: PortableOperationOptions
  ): Promise<ConnectionWriteReadinessWatch<string>> {
    throw contractError('capability.unsupported', 'connection', 'rust-core-manager.connection.write-readiness')
  }

  /**
   * One connection control through the backend: refused before any native
   * call when the backend does not register the capability, the connection
   * is no longer current, or the caller already aborted or expired.
   */
  private async control<Result extends { readonly terminal: unknown }>(
    featureId: FeatureId,
    options: PortableOperationOptions,
    name: string,
    dispatch: (
      connections: ReactNativeRustCoreBackend['connections'],
      operation: OperationOptions<string, string>
    ) => BackendOperationDispatch<string, Result>
  ): Promise<Result> {
    const operationName = `rust-core-manager.connection.${name}`
    const state = this.manager.featureState(featureId)
    if (state !== 'supported' && state !== 'limited') {
      throw contractError(
        state === 'unavailable' ? 'capability.unavailable' : 'capability.unsupported',
        'connection',
        operationName
      )
    }
    this.assertCurrent()
    const publicOptions = toPublicOperationOptions(options)
    if (publicOptions.signal?.aborted === true) throw contractError('operation.aborted', 'connection', operationName)
    const operation = this.manager.operationOptions(publicOptions)
    return dispatch(this.manager.backendConnections, operation).completion
  }

  isCurrent(): boolean {
    return !this.released && this.resource.state === 'connected'
  }

  isReleased(): boolean {
    return this.released
  }

  assertCurrent(): void {
    if (!this.isCurrent()) {
      throw contractError('connection.stale', 'connection', 'rust-core-connection.current')
    }
  }

  setDatabase(database: NativeGattDatabase): void {
    if (this.pendingDatabaseCleanup !== null) {
      throw contractError('lifecycle.invariant-violation', 'gatt', 'rust-core-connection.pending-database-cleanup')
    }
    this.database = database
  }

  clearDatabase(database: NativeGattDatabase): void {
    if (this.database === database) {
      this.database = null
    }
  }

  invalidateDatabase(
    reason: 'connection-lost' | 'owner-released',
    changeReason: GattDatabaseChangedEvent['reason'] | null = null
  ): Promise<CleanupRecord> {
    const database = this.database ?? this.pendingDatabaseCleanup
    if (database === null) {
      return Promise.resolve({ state: 'released', failures: [] })
    }
    if (!this.isPendingDatabaseCleanup(database)) {
      database.markInvalid(changeReason)
      if (this.retainPendingDatabaseCleanup(database)) {
        // retained
      }
    }
    return this.manager.cleanupInvalidDatabase(database, reason === 'connection-lost' ? 'connection-lost' : reason)
  }

  retainPendingDatabaseCleanup(database: NativeGattDatabase): boolean {
    if (this.pendingDatabaseCleanup === database) {
      return false
    }
    if (this.pendingDatabaseCleanup !== null) {
      throw contractError('lifecycle.invariant-violation', 'gatt', 'rust-core-connection.multiple-pending-databases')
    }
    this.clearDatabase(database)
    this.pendingDatabaseCleanup = database
    return true
  }

  isPendingDatabaseCleanup(database: NativeGattDatabase): boolean {
    return this.pendingDatabaseCleanup === database
  }

  completeDatabaseCleanup(database: NativeGattDatabase): void {
    if (this.pendingDatabaseCleanup === database) {
      this.pendingDatabaseCleanup = null
    }
  }

  async cleanupChildren(reason: 'connection-lost' | 'owner-released'): Promise<CleanupRecord> {
    return this.invalidateDatabase(reason)
  }

  markReleased(): void {
    this.released = true
  }

  applyBackendTransition(
    previous: ConnectionState,
    current: Exclude<ConnectionState, 'disconnected' | 'lost'>,
    backendIngressOrdinal: number
  ): void {
    if (this.lifecycleFinished) {
      return
    }
    assertBackendLifecycleTransition(
      this.lifecycleState,
      previous,
      current,
      backendIngressOrdinal,
      this.lastBackendIngressOrdinal
    )
    this.acceptBackendIngressOrdinal(backendIngressOrdinal)
    this.emitLifecycle(current, 'backend-transition', backendIngressOrdinal)
  }

  finishBackendLifecycle(
    previous: ConnectionState,
    current: Extract<ConnectionState, 'disconnected' | 'lost'>,
    cause: ConnectionLifecycleTerminalCause,
    backendIngressOrdinal: number
  ): void {
    if (this.lifecycleFinished) {
      return
    }
    assertBackendLifecycleTransition(
      this.lifecycleState,
      previous,
      current,
      backendIngressOrdinal,
      this.lastBackendIngressOrdinal
    )
    if (current !== lifecycleTerminalState(cause)) {
      throw contractError('lifecycle.invariant-violation', 'connection', 'connection-lifecycle.terminal-cause')
    }
    this.acceptBackendIngressOrdinal(backendIngressOrdinal)
    this.completeLifecycle(cause, backendIngressOrdinal)
  }

  finishLifecycle(cause: ConnectionLifecycleTerminalCause, backendIngressOrdinal: number | null): void {
    if (this.lifecycleFinished) {
      return
    }
    if (backendIngressOrdinal !== null) {
      this.acceptBackendIngressOrdinal(backendIngressOrdinal)
    }
    this.completeLifecycle(cause, backendIngressOrdinal)
  }

  matchesConnectionPath(path: DatabasePath<string, string, string> | ConnectionPath<string, string>): boolean {
    return connectionPathsEqual(path, {
      attachment: this.resource.attachment,
      attachmentId: this.resource.attachmentId,
      peerId: this.resource.peerId,
      connectionId: this.resource.connectionId,
      connectionGeneration: this.resource.connectionGeneration,
      ownerLeaseId: this.lease.leaseId
    })
  }

  private acceptBackendIngressOrdinal(backendIngressOrdinal: number): void {
    if (this.lastBackendIngressOrdinal !== null && backendIngressOrdinal <= this.lastBackendIngressOrdinal) {
      throw contractError('lifecycle.invariant-violation', 'connection', 'connection-lifecycle.ingress-order')
    }
    this.lastBackendIngressOrdinal = backendIngressOrdinal
  }

  private completeLifecycle(cause: ConnectionLifecycleTerminalCause, backendIngressOrdinal: number | null): void {
    const current = lifecycleTerminalState(cause)
    this.emitLifecycle(current, cause, backendIngressOrdinal)
    this.lifecycleFinished = true
    this.lifecycleStream.finishWithReason(lifecycleTerminalReason(current))
  }

  private emitLifecycle(
    current: ConnectionState,
    cause: ConnectionLifecycleCause,
    backendIngressOrdinal: number | null
  ): void {
    if (!Number.isSafeInteger(this.nextLifecycleSequence)) {
      throw contractError('lifecycle.invariant-violation', 'connection', 'connection-lifecycle.sequence')
    }
    const previous = this.lifecycleState
    this.lifecycleState = current
    const event: ConnectionLifecycleEvent<string> = Object.freeze({
      kind: 'connection-lifecycle',
      attachment: this.resource.attachment,
      attachmentId: this.resource.attachmentId,
      peerId: this.resource.peerId,
      connectionId: this.resource.connectionId,
      connectionGeneration: this.resource.connectionGeneration,
      ownerLeaseId: this.lease.leaseId,
      sequence: this.nextLifecycleSequence,
      backendIngressOrdinal,
      previous,
      current,
      cause
    })
    this.nextLifecycleSequence += 1
    this.lifecycleStream.emit(event, lifecycleEventByteLength(event))
  }
}

class NativeGattDatabase {
  private valid = true
  private readonly changedStream = new CoreBoundedStream<GattDatabaseChangedEvent>(
    {
      itemCapacity: capacity(4),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1)
    },
    'drop-oldest'
  )
  private readonly subscriptions = new Set<NativeSubscription>()

  constructor(
    private readonly manager: ReactNativeRustCoreManager,
    readonly backendDatabase: NativeGattDatabaseHandle,
    readonly connection: NativeConnection,
    private readonly options: ReactNativeRustCoreManagerOptions
  ) {}

  get path(): DatabasePath<string, string, string> {
    return this.backendDatabase.path
  }

  get changed(): BoundedAsyncStream<GattDatabaseChangedEvent> {
    return this.changedStream
  }

  monotonicNow(): number {
    return this.manager.monotonicNow()
  }

  scheduleDeadline(deadlineAt: number, action: () => void): CoreDeadlineHandle {
    return this.manager.scheduleDeadline(deadlineAt, action)
  }

  async snapshot(): Promise<GattDatabaseSnapshot<string, string, string>> {
    this.assertCurrent()
    let snapshot: GattDatabaseSnapshot<string, string, string>
    try {
      snapshot = await this.backendDatabase.snapshot()
    } catch (error) {
      if (error instanceof BackendContractError) {
        throw error
      }
      throw contractError('platform.failure', 'gatt', 'rust-core-gatt-database.snapshot')
    }
    this.assertSnapshot(snapshot)
    return snapshot
  }

  async readReceipt(path: CurrentCharacteristicPath, options: PublicOperationOptions): Promise<CharacteristicRead> {
    this.assertPath(path)
    this.assertOperationAdmission(options, 'read')
    return this.backendDatabase.read(path, options)
  }

  async write(
    path: CurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<WriteReceipt<string, string>> {
    this.assertPath(path)
    this.assertOperationAdmission(options, 'write')
    const owned = ownBytes(bytes, this.options.maximumValueBytes)
    return this.backendDatabase.write(path, owned, options)
  }

  async writeWhenReady(
    _path: CurrentCharacteristicPath,
    _bytes: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<WriteReceipt<string, string>> {
    if (options.mode !== 'without-response') {
      throw contractError('argument.invalid', 'gatt', 'rust-core-manager.write-when-ready.mode')
    }
    const registration = this.manager.features.registrations.find(
      candidate => candidate.id === BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness
    )
    if (registration?.state === 'unavailable') {
      throw contractError('capability.unavailable', 'connection', 'rust-core-manager.write-when-ready')
    }
    throw contractError('capability.unsupported', 'connection', 'rust-core-manager.write-when-ready')
  }

  async maximumWriteLength(
    path: CurrentCharacteristicPath,
    mode: WriteMode
  ): Promise<MaximumWriteLengthObservation<string>> {
    this.assertPath(path)
    const observation = await observeMaximumWriteLength(this.manager.features, path, mode)
    this.assertPath(path)
    return observation
  }

  async writeLong(
    path: CurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: LongWritePolicy
  ): Promise<LongWriteReceipt<string, string>> {
    this.assertPath(path)
    this.assertOperationAdmission(options, 'write-long')
    const owned = ownBytes(bytes, this.options.maximumValueBytes)
    const correlation = this.mintWriteLongCorrelation()
    let plan: LongWriteFeatureOutput
    try {
      plan = await this.planLongWrite(path, owned.byteLength, options)
    } catch (error) {
      throw error instanceof BackendContractError
        ? error
        : contractError('platform.failure', 'gatt', 'rust-core-manager.write-long.plan')
    }
    const progress = createLongWriteProgress(owned.byteLength, plan.maximumWriteLength, plan.totalChunks)
    let failed: BackendContractError | null = null
    for (let index = 0; index < progress.chunks.length; index += 1) {
      if (isAborted(options.signal) || this.manager.state !== 'ready') {
        failed = contractError('operation.aborted', 'gatt', 'rust-core-manager.write-long.cancelled-between-chunks')
        break
      }
      const chunk = progress.chunks[index]
      if (chunk === undefined) {
        break
      }
      try {
        this.assertPath(path)
      } catch (error) {
        failed = asBackendError(error, 'rust-core-manager.write-long.assert-path')
        break
      }
      progress.activeChunkIndex = index
      try {
        await this.manager.backendGatt.write(path, {
          operation: { signal: options.signal ?? null, deadline: options.deadline ?? null, correlation },
          bytes: owned.subarray(chunk.byteOffset, chunk.byteOffset + chunk.byteLength),
          mode: options.mode
        }).completion
      } catch (error) {
        progress.markDispatchedChunkUncertain(index)
        failed = asBackendError(error, 'rust-core-manager.write-long.chunk')
        break
      }
      if (isAborted(options.signal)) {
        failed = contractError('operation.aborted', 'gatt', 'rust-core-manager.write-long.cancelled-after-chunk')
        break
      }
      progress.chunks[index] = { ...chunk, state: 'confirmed' }
      progress.activeChunkIndex = null
    }
    return receiptFromLongWriteProgress(progress, correlation, failed)
  }

  async readDescriptor(path: CurrentDescriptorPath, options: PublicOperationOptions): Promise<OwnedBytes> {
    this.assertDescriptorPath(path)
    this.assertOperationAdmission(options, 'read-descriptor')
    return this.backendDatabase.readDescriptor(path, options)
  }

  async writeDescriptor(
    path: CurrentDescriptorPath,
    bytes: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<WriteReceipt<string, string>> {
    this.assertDescriptorPath(path)
    this.assertOperationAdmission(options, 'write-descriptor')
    const owned = ownBytes(bytes, this.options.maximumValueBytes)
    return this.backendDatabase.writeDescriptor(path, owned, options)
  }

  async subscribe(
    path: CurrentCharacteristicPath,
    options: SubscriptionOptions
  ): Promise<BackendSubscription<string, string, string, string, string, string>> {
    this.assertPath(path)
    this.assertOperationAdmission(options, 'subscribe')
    return this.backendDatabase.subscribe(path, options)
  }

  trackSubscription(subscription: NativeSubscription): void {
    this.subscriptions.add(subscription)
  }

  untrackSubscription(subscription: NativeSubscription): void {
    this.subscriptions.delete(subscription)
  }

  drainSubscriptions(): NativeSubscription[] {
    const pending = [...this.subscriptions]
    this.subscriptions.clear()
    return pending
  }

  isCurrent(): boolean {
    return this.valid && this.connection.isCurrent()
  }

  isAttached(): boolean {
    return this.connection.database === this
  }

  assertCurrent(): void {
    if (!this.isCurrent()) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-gatt-database.current')
    }
  }

  assertPath(path: CurrentCharacteristicPath): void {
    this.assertCurrent()
    if (!this.matchesDatabasePath(path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-gatt-database.path')
    }
  }

  matchesDatabasePath(path: DatabasePath<string, string, string>): boolean {
    return databasePathsEqual(path, this.path)
  }

  markInvalid(reason: GattDatabaseChangedEvent['reason'] | null = null): void {
    if (!this.valid) {
      return
    }
    this.valid = false
    this.connection.clearDatabase(this)
    if (reason !== null) {
      this.changedStream.emit(
        Object.freeze({
          previousGeneration: String(this.path.databaseGeneration),
          reason,
          affectedHandleRange: null
        }),
        128
      )
    }
    this.changedStream.finishWithReason('closed')
  }

  private assertDescriptorPath(path: CurrentDescriptorPath): void {
    this.assertCurrent()
    if (!this.matchesDatabasePath(path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-gatt-database.path')
    }
  }

  private assertSnapshot(snapshot: GattDatabaseSnapshot<string, string, string>): void {
    this.assertSnapshotPath(snapshot.path)
    for (const service of snapshot.services) {
      this.assertSnapshotPath(service.path)
    }
    for (const characteristic of snapshot.characteristics) {
      this.assertSnapshotPath(characteristic.path)
    }
    for (const descriptor of snapshot.descriptors) {
      this.assertSnapshotPath(descriptor.path)
    }
  }

  private assertSnapshotPath(path: DatabasePath<string, string, string>): void {
    if (!this.matchesDatabasePath(path)) {
      throw contractError('protocol.violation', 'gatt', 'rust-core-gatt-database.snapshot-path')
    }
  }

  private assertOperationAdmission(options: PublicOperationOptions, operation: string): void {
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'gatt', operation)
    }
    if (options.deadline !== null && options.deadline !== undefined && options.deadline <= this.options.now()) {
      throw contractError('operation.timed-out', 'gatt', operation)
    }
  }

  private async planLongWrite(
    path: CurrentCharacteristicPath,
    totalBytes: number,
    options: LongWritePolicy
  ): Promise<LongWriteFeatureOutput> {
    const observed = await observeMaximumWriteLength(this.manager.features, path, options.mode)
    this.assertPath(path)
    const maximumWriteLength = resolveLongWriteChunkSize(observed.maximumWriteLength, options.chunkSize)
    const plan = await planLongWrite(
      this.manager.features,
      String(path.connectionId),
      String(path.connectionGeneration),
      options.mode,
      totalBytes,
      maximumWriteLength
    )
    this.assertPath(path)
    return plan
  }

  /** One legacy core correlation for the whole long write; its chunks carry it. */
  private mintWriteLongCorrelation(): OperationCorrelation<string, string> {
    return this.manager.publicCorrelation()
  }
}

class NativeDiscoveredGattDatabase {
  constructor(
    private readonly database: NativeGattDatabase,
    private readonly discoverySnapshot: GattDatabaseSnapshot<string, string, string>
  ) {}

  get path(): DatabasePath<string, string, string> {
    return this.database.path
  }

  assertCurrent(): void {
    this.database.assertCurrent()
  }

  get changed(): BoundedAsyncStream<GattDatabaseChangedEvent> {
    return this.database.changed
  }

  monotonicNow(): number {
    return this.database.monotonicNow()
  }

  scheduleDeadline(deadlineAt: number, action: () => void): DeadlineHandle {
    return this.database.scheduleDeadline(deadlineAt, action)
  }

  snapshot(): Promise<GattDatabaseSnapshot<string, string, string>> {
    return this.database.snapshot()
  }

  async read(path: PortableCurrentCharacteristicPath, options: PortableOperationOptions): Promise<OwnedBytes> {
    return (await this.readReceipt(path, options)).value
  }

  async readReceipt(
    path: PortableCurrentCharacteristicPath,
    options: PortableOperationOptions
  ): Promise<CharacteristicRead> {
    return this.database.readReceipt(this.resolveCharacteristicPath(path), toPublicOperationOptions(options))
  }

  async write(
    path: PortableCurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<WriteReceipt<string, string>> {
    return this.database.write(this.resolveCharacteristicPath(path), bytes, toPublicWritePolicy(options))
  }

  async writeWhenReady(
    path: PortableCurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<WriteReceipt<string, string>> {
    return this.database.writeWhenReady(this.resolveCharacteristicPath(path), bytes, toPublicWritePolicy(options))
  }

  async maximumWriteLength(
    path: PortableCurrentCharacteristicPath,
    mode: WriteMode
  ): Promise<MaximumWriteLengthObservation<string>> {
    return this.database.maximumWriteLength(this.resolveCharacteristicPath(path), mode)
  }

  async writeLong(
    path: PortableCurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<LongWriteReceipt<string, string>> {
    return this.database.writeLong(this.resolveCharacteristicPath(path), bytes, toPublicLongWritePolicy(options))
  }

  async readDescriptor(path: PortableCurrentDescriptorPath, options: PortableOperationOptions): Promise<OwnedBytes> {
    return this.database.readDescriptor(this.resolveDescriptorPath(path), toPublicOperationOptions(options))
  }

  async writeDescriptor(
    path: PortableCurrentDescriptorPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<WriteReceipt<string, string>> {
    return this.database.writeDescriptor(this.resolveDescriptorPath(path), bytes, toPublicWritePolicy(options))
  }

  async subscribe(
    path: PortableCurrentCharacteristicPath,
    options: PortableSubscriptionOptions
  ): Promise<InternalSubscription<string, NativeBackendIdentity<string>>> {
    const backendSubscription = await this.database.subscribe(
      this.resolveCharacteristicPath(path),
      toPublicSubscriptionOptions(options)
    )
    const subscription = new NativeSubscription(this.database, backendSubscription)
    this.database.trackSubscription(subscription)
    return subscription as unknown as InternalSubscription<string, NativeBackendIdentity<string>>
  }

  private resolveCharacteristicPath(path: PortableCurrentCharacteristicPath): CurrentCharacteristicPath {
    const characteristic = this.discoverySnapshot.characteristics.find(candidate =>
      characteristicAddressMatches(candidate.path, path)
    )
    if (characteristic === undefined) {
      throw contractError('gatt.not-found', 'gatt', 'rust-core-discovered-gatt.resolve-characteristic-path')
    }
    if (!characteristicPathMatches(characteristic.path, path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-discovered-gatt.resolve-characteristic-path')
    }
    return characteristic.path as CurrentCharacteristicPath
  }

  private resolveDescriptorPath(path: PortableCurrentDescriptorPath): CurrentDescriptorPath {
    const descriptor = this.discoverySnapshot.descriptors.find(candidate =>
      descriptorAddressMatches(candidate.path, path)
    )
    if (descriptor === undefined) {
      throw contractError('gatt.not-found', 'gatt', 'rust-core-discovered-gatt.resolve-descriptor-path')
    }
    if (!descriptorPathMatches(descriptor.path, path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'rust-core-discovered-gatt.resolve-descriptor-path')
    }
    return descriptor.path as CurrentDescriptorPath
  }
}

class NativeSubscription {
  private removal: Promise<CleanupRecord> | null = null

  constructor(
    private readonly database: NativeGattDatabase,
    private readonly backendSubscription: BackendSubscription<string, string, string, string, string, string>
  ) {}

  get subscriptionId() {
    return this.backendSubscription.subscriptionId
  }

  get path() {
    return this.backendSubscription.path
  }

  get values() {
    return this.backendSubscription.values
  }

  remove(): Promise<CleanupRecord> {
    if (this.removal === null) {
      const removal = this.removeBackend()
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

  /** The subscription stays tracked until the owner confirms release, so a failed removal can be retried. */
  async removeBackend(): Promise<CleanupRecord> {
    const record = await this.backendSubscription.remove()
    if (record.state === 'released') this.database.untrackSubscription(this)
    return record
  }
}

function isAborted(signal: AbortSignal | null | undefined): boolean {
  return signal?.aborted === true
}

function asCleanupFailure(resourceKind: string, error: unknown): CleanupFailure {
  if (error instanceof BackendContractError) {
    return { resourceKind, error: error.normalized }
  }
  return {
    resourceKind,
    error: contractError('platform.failure', 'core', 'rust-core-manager.release').normalized
  }
}

function asBackendError(error: unknown, operation: string): BackendContractError {
  if (error instanceof BackendContractError) {
    return error
  }
  return contractError('platform.failure', 'gatt', operation)
}

function toPublicOperationOptions(options: PortableOperationOptions): PublicOperationOptions {
  return {
    signal: options.signal,
    deadline: options.deadline === null ? null : deadline(options.deadline)
  }
}

function toPublicWritePolicy(options: PortableWritePolicy): WritePolicy {
  return { ...toPublicOperationOptions(options), mode: options.mode }
}

function toPublicLongWritePolicy(options: PortableWritePolicy): LongWritePolicy {
  return { ...toPublicWritePolicy(options), chunkSize: options.chunkSize }
}

function toPublicSubscriptionOptions(options: PortableSubscriptionOptions): SubscriptionOptions {
  return {
    ...toPublicOperationOptions(options),
    delivery: {
      itemCapacity: capacity(options.delivery.itemCapacity),
      byteCapacity: capacity(options.delivery.byteCapacity),
      reservedControlCapacity: capacity(options.delivery.reservedControlCapacity),
      overflowPolicy: options.delivery.overflowPolicy
    },
    deliveryMode: options.deliveryMode
  }
}

function attachmentMatches(
  current: CurrentCharacteristicPath['attachment'],
  portable: PortableCurrentCharacteristicPath['attachment']
): boolean {
  return (
    current.attachmentId === portable.attachmentId &&
    current.backendInstanceId === portable.backendInstanceId &&
    current.backendGeneration === portable.backendGeneration &&
    current.adapter.adapterId === portable.adapter.adapterId &&
    current.adapter.displayName === portable.adapter.displayName &&
    current.adapter.adapterGeneration === portable.adapter.adapterGeneration &&
    current.adapter.state.availability === portable.adapter.state.availability &&
    current.adapter.state.authorization === portable.adapter.state.authorization &&
    current.adapter.state.power === portable.adapter.state.power &&
    current.adapter.state.backendGeneration === portable.adapter.state.backendGeneration &&
    current.adapter.state.updatedAt === portable.adapter.state.updatedAt &&
    current.adapter.state.safeReason === portable.adapter.state.safeReason &&
    current.adapter.limitations.length === portable.adapter.limitations.length &&
    current.adapter.limitations.every((limitation, index) => limitation === portable.adapter.limitations[index])
  )
}

function characteristicPathMatches(
  current: CurrentCharacteristicPath,
  portable: PortableCurrentCharacteristicPath
): boolean {
  return (
    attachmentMatches(current.attachment, portable.attachment) &&
    current.attachmentId === portable.attachmentId &&
    current.peerId === portable.peerId &&
    current.connectionId === portable.connectionId &&
    current.ownerLeaseId === portable.ownerLeaseId &&
    current.connectionGeneration === portable.connectionGeneration &&
    current.databaseId === portable.databaseId &&
    current.databaseGeneration === portable.databaseGeneration &&
    current.serviceUuid === portable.serviceUuid &&
    current.serviceOccurrence === portable.serviceOccurrence &&
    current.characteristicUuid === portable.characteristicUuid &&
    current.characteristicOccurrence === portable.characteristicOccurrence &&
    current.validity === portable.validity
  )
}

function characteristicAddressMatches(
  current: CurrentCharacteristicPath,
  portable: PortableCurrentCharacteristicPath
): boolean {
  return (
    current.serviceUuid === portable.serviceUuid &&
    current.serviceOccurrence === portable.serviceOccurrence &&
    current.characteristicUuid === portable.characteristicUuid &&
    current.characteristicOccurrence === portable.characteristicOccurrence
  )
}

function descriptorPathMatches(current: CurrentDescriptorPath, portable: PortableCurrentDescriptorPath): boolean {
  return (
    characteristicPathMatches(current, portable) &&
    current.descriptorUuid === portable.descriptorUuid &&
    current.descriptorOccurrence === portable.descriptorOccurrence
  )
}

function descriptorAddressMatches(current: CurrentDescriptorPath, portable: PortableCurrentDescriptorPath): boolean {
  return (
    characteristicAddressMatches(current, portable) &&
    current.descriptorUuid === portable.descriptorUuid &&
    current.descriptorOccurrence === portable.descriptorOccurrence
  )
}

function resolveLongWriteChunkSize(observedMaximum: number, requestedChunkSize: number | undefined): number {
  if (requestedChunkSize === undefined) return observedMaximum
  if (!Number.isSafeInteger(requestedChunkSize) || requestedChunkSize < 1 || requestedChunkSize > observedMaximum) {
    throw contractError('argument.invalid', 'gatt', 'rust-core-manager.write-long.chunk-size')
  }
  return requestedChunkSize
}

function maximumLifecycleEventByteLength(lease: ConnectionLease<string, string, string>): number {
  return lifecycleEventByteLength({
    kind: 'connection-lifecycle',
    attachment: lease.connection.attachment,
    attachmentId: lease.connection.attachmentId,
    peerId: lease.connection.peerId,
    connectionId: lease.connection.connectionId,
    connectionGeneration: lease.connection.connectionGeneration,
    ownerLeaseId: lease.leaseId,
    sequence: Number.MAX_SAFE_INTEGER,
    backendIngressOrdinal: Number.MAX_SAFE_INTEGER,
    previous: 'connecting',
    current: 'disconnecting',
    cause: 'backend-failure'
  })
}

function lifecycleEventByteLength(event: ConnectionLifecycleEvent<string>): number {
  return utf8ByteLength(JSON.stringify(event))
}

function lifecycleTerminalState(cause: ConnectionLifecycleTerminalCause): 'disconnected' | 'lost' {
  return cause === 'peer-link-loss' || cause === 'adapter-loss' ? 'lost' : 'disconnected'
}

function lifecycleTerminalReason(current: 'disconnected' | 'lost'): 'connection-lost' | 'owner-released' {
  return current === 'lost' ? 'connection-lost' : 'owner-released'
}

interface MutableLongWriteProgress {
  readonly totalBytes: number
  readonly maximumWriteLength: number
  readonly chunks: LongWriteChunkProgress[]
  activeChunkIndex: number | null
  boundaryReached: boolean
  markBoundary(): void
  markDispatchedChunkUncertain(index: number): void
}

function createLongWriteProgress(
  totalBytes: number,
  maximumWriteLength: number,
  totalChunks: number
): MutableLongWriteProgress {
  if (
    !Number.isSafeInteger(maximumWriteLength) ||
    maximumWriteLength < 1 ||
    !Number.isSafeInteger(totalChunks) ||
    totalChunks < 1 ||
    totalChunks !== Math.max(1, Math.ceil(totalBytes / maximumWriteLength))
  ) {
    throw contractError('protocol.violation', 'gatt', 'rust-core-manager.write-long-plan')
  }
  const chunks: LongWriteChunkProgress[] = []
  for (let index = 0; index < totalChunks; index += 1) {
    const byteOffset = index * maximumWriteLength
    chunks.push({
      index,
      byteOffset,
      byteLength: totalBytes === 0 ? 0 : Math.min(maximumWriteLength, totalBytes - byteOffset),
      state: 'not-started'
    })
  }
  return {
    totalBytes,
    maximumWriteLength,
    chunks,
    activeChunkIndex: null,
    boundaryReached: false,
    markBoundary() {
      this.boundaryReached = true
      if (this.activeChunkIndex !== null) {
        this.markDispatchedChunkUncertain(this.activeChunkIndex)
      }
    },
    markDispatchedChunkUncertain(index: number) {
      const current = this.chunks[index]
      if (current !== undefined && current.state === 'not-started') {
        this.chunks[index] = { ...current, state: 'uncertain' }
      }
      if (this.activeChunkIndex === index) {
        this.activeChunkIndex = null
      }
    }
  }
}

function receiptFromLongWriteProgress(
  progress: MutableLongWriteProgress,
  correlation: OperationCorrelation<string, string>,
  failed: BackendContractError | null
): LongWriteReceipt<string, string> {
  const completedChunks = progress.chunks.filter(chunk => chunk.state === 'confirmed').length
  const committedBytes = progress.chunks
    .filter(chunk => chunk.state === 'confirmed')
    .reduce((total, chunk) => total + chunk.byteLength, 0)
  const uncertainChunk = progress.chunks.find(chunk => chunk.state === 'uncertain')
  const failedChunk = uncertainChunk ?? progress.chunks.find(chunk => chunk.state === 'not-started')
  if (failed === null) {
    return Object.freeze({
      terminal: Object.freeze({ correlation, outcome: 'succeeded', cause: null }),
      commitState: 'confirmed',
      planState: 'planned',
      totalBytes: progress.totalBytes,
      chunkSize: progress.maximumWriteLength,
      totalChunks: progress.chunks.length,
      chunks: Object.freeze(progress.chunks.map(chunk => Object.freeze({ ...chunk }))),
      completedChunks,
      committedBytes,
      failedChunkIndex: null
    })
  }
  return Object.freeze({
    terminal: Object.freeze({ correlation, outcome: 'failed', cause: failed.normalized.code }),
    commitState: 'confirmed',
    planState: 'planned',
    totalBytes: progress.totalBytes,
    chunkSize: progress.maximumWriteLength,
    totalChunks: progress.chunks.length,
    chunks: Object.freeze(progress.chunks.map(chunk => Object.freeze({ ...chunk }))),
    completedChunks,
    committedBytes,
    failedChunkIndex: failedChunk?.index ?? null
  })
}

function requireControl<Method>(method: Method | undefined, name: string): Method {
  if (method === undefined) {
    throw contractError('capability.unsupported', 'connection', `rust-core-manager.connection.${name}`)
  }
  return method
}
