// src/ipc/public-manager.ts

import { BackendContractError, BLE_ERROR_CODES, contractError, type CleanupRecord } from '../backend-contract/errors'
import type { ConnectionLifecycleCause } from '../backend-contract/connection-lifecycle'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import type {
  PortableBoundedAsyncStream,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableNotificationValue,
  PortableOperationOptions,
  PortableWritePolicy,
  SubscriptionHandle
} from '../manager/consumer-handles'
import type {
  BleConnectionControls,
  BleConnectionEvent,
  BleConnection,
  BleManager,
  BlePeer,
  BleControlObservationMetadata,
  ConnectionParametersObservation,
  ConnectionPriority,
  ConnectionPriorityResult,
  ConnectOptions,
  FindOptions,
  MaximumWriteLengthObservation,
  MtuNegotiation,
  MtuObservation,
  PeerAddress,
  PhyObservation,
  PhyPreference,
  PhyUpdateResult,
  PublicScanObservation,
  RediscoverGattOptions,
  RssiObservation,
  ScanOptions,
  ScanSession,
  SubrateMode,
  SubrateResult,
  WriteMode,
  WriteReadinessEvent
} from '../public/ble-manager'
import {
  assertPublicConnectOptions,
  assertPublicChooseOptions,
  assertPublicScanOptions,
  broadcastConnectionEvents,
  publicConnectionTerminalError,
  filterScanObservations,
  findPeerInScan,
  isPeerAddressTarget,
  snapshotBlePeer
} from '../public/ble-manager'
import type {
  BleAdapter,
  BleAdapterState,
  AdapterReadinessOptions,
  AdapterWatchOptions,
  BleAdapterStateWatch
} from '../public/ble-adapter'
import { assertDirectConnectionCapability } from '../public/capabilities'
import type { BleCapabilities, CapabilityDescriptor } from '../public/capabilities'
import { BUILT_IN_FEATURE_IDS } from '../backend-contract/capabilities'
import { createPublicGattDatabase, type PublicGattDatabaseSource } from '../public/gatt'
import type { GattDatabase } from '../public/gatt'
import type { BleDiagnostics } from '../public/diagnostics'
import { diagnosticsUnavailable } from '../public/diagnostics'
import { normalizeOperationOptions } from '../public/operation-options'
import type { OperationOptions } from '../public/operation-options'
import { normalizeScanQuery, scanQueryTargetsAddresses } from '../public/scan-query'
import { createScanState } from '../public/scan-state'
import type { ScanStateController } from '../public/scan-state'
import { unsupportedPeerDirectory } from '../public/peer-directory'
import type { BlePeerDirectory } from '../public/peer-directory'
import { isPeerReference } from '../public/peer-reference'
import type { PeerReference } from '../public/peer-reference'
import { createPublicSecurity } from '../public/security'
import type { BleSecurity } from '../public/security'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from '../public/error-bridge'
import { toPublicCleanupRecord, type CleanupRecord as PublicCleanupRecord } from '../public/cleanup'
import { mapPublicBoundedAsyncStream, type PublicBoundedAsyncStream } from '../public/streams'
import { resolveStreamPolicy } from '../public/stream-presets'
import { CoreBoundedStream } from '../core/bounded-stream'
import { capacity } from '../backend-contract/primitives'
import {
  IpcBleManager,
  type IpcConnection,
  type IpcGattDatabase,
  type IpcNotificationValue,
  type IpcSubscription,
  type IpcWriteReceipt
} from './manager'

const IPC_ADAPTER_STATE_POLL_INTERVAL_MS = 25
const IPC_ADAPTER_STATE_STREAM_LIMITS = Object.freeze({
  itemCapacity: capacity(128),
  byteCapacity: capacity(64 * 1024),
  reservedControlCapacity: capacity(1)
})

export interface IpcPublicManagerOptions {
  readonly requireScanPlan?: boolean
  readonly discoveryKind?: BleManager['discovery']['kind']
  readonly capabilities?: BleCapabilities
  readonly adapter?: BleAdapter
  readonly diagnostics?: BleDiagnostics
  readonly peers?: BlePeerDirectory
  readonly gattDeliverySelection?: 'unknown' | 'controllable'
}

export class IpcPublicManagerAdapter implements BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly security: BleSecurity
  readonly discovery: BleManager['discovery']
  private readonly requireScanPlan: boolean
  private readonly gattDeliverySelection: 'unknown' | 'controllable'

  constructor(
    private readonly ipc: IpcBleManager,
    options: IpcPublicManagerOptions = {}
  ) {
    this.requireScanPlan = options.requireScanPlan ?? false
    this.gattDeliverySelection = options.gattDeliverySelection ?? 'unknown'
    this.capabilities = options.capabilities ?? ipc.capabilities
    this.adapter = options.adapter ?? createIpcAdapter(ipc)
    this.diagnostics = options.diagnostics ?? diagnosticsUnavailable()
    this.peers = options.peers ?? unsupportedPeerDirectory()
    this.security = createPublicSecurity(undefined, this.peers, this.capabilities, () => globalThis.performance.now())
    this.discovery = Object.freeze({
      kind: options.discoveryKind ?? ipc.bootstrap.discovery?.kind ?? discoveryKindFromCapabilities(this.capabilities)
    })
  }

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    try {
      assertPublicScanOptions(options)
      if (options.observation?.reportLostAfterMs !== undefined) {
        throw contractError('capability.unavailable', 'scan', 'ipc-public-manager.scan.report-lost-after')
      }
      if (options.platform !== undefined) {
        throw contractError('capability.unsupported', 'scan', 'ipc-public-manager.scan.platform-options')
      }
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const normalizedQuery = normalizeScanQuery(options.query)
      if (scanQueryTargetsAddresses(normalizedQuery)) {
        // The versioned IPC advertisement schema carries no radio address, so an addresses
        // clause can never match here; fail closed instead of silently observing nothing.
        throw contractError('capability.unsupported', 'scan', 'ipc-public-manager.scan.addresses')
      }
      const session = await this.ipc.scan(
        toIpcScanOptions(options, normalized.signal, normalizedQuery, normalized.deadline)
      )
      if (this.requireScanPlan && session.plan === null) {
        await session.stop().catch(() => undefined)
        throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.scan-plan')
      }
      const state = createScanState()
      state.emit({ state: 'active' })
      return new IpcPublicScanSession(
        session,
        mapPublicBoundedAsyncStream(
          filterScanObservations(session.observations, normalizedQuery, options.duplicates ?? 'coalesced'),
          observation => observation
        ),
        state,
        options
      )
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async find(options: FindOptions = {}): Promise<BlePeer> {
    try {
      const { select, ...scanOptions } = options
      const operation = normalizeOperationOptions(options, () => globalThis.performance.now())
      const scan = await this.scan({
        ...scanOptions,
        duplicates: 'coalesced',
        delivery: 'latest',
        timeoutMs: options.timeoutMs ?? 10_000
      })
      return await runWithCleanup(
        () => findPeerInScan(scan, select, { ...operation, now: () => globalThis.performance.now() }),
        () => scan.stop()
      )
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async choose(_options: import('../public/ble-manager').ChooseOptions = {}): Promise<BlePeer> {
    try {
      assertPublicChooseOptions(_options)
      normalizeOperationOptions(_options, () => globalThis.performance.now())
      throw contractError('capability.unsupported', 'chooser', 'ipc-public-manager.choose')
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async connect(
    peer: BlePeer | string | PeerReference | PeerAddress,
    options: ConnectOptions = {}
  ): Promise<import('../public/ble-manager').BleConnection> {
    try {
      assertPublicConnectOptions(options)
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      assertIpcConnectionOptions(options)
      assertDirectConnectionCapability(this.capabilities.get('connection:direct'), 'ipc-public-manager.connect.direct')
      if (isPeerAddressTarget(peer)) {
        assertDirectConnectionCapability(
          this.capabilities.get('peer:address-targeting'),
          'ipc-public-manager.connect.address'
        )
        throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.connect.address')
      }
      if (isReferenceLike(peer) && !isPeerReference(peer)) {
        throw contractError('peer.reference-invalid', 'connection', 'ipc-public-manager.connect-reference')
      }
      if (isPeerReference(peer)) {
        throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.peer-reference')
      }
      const peerId = typeof peer === 'string' ? peer : peer.id
      const base = await this.ipc.connect(peerId, {
        signal: normalized.signal ?? undefined,
        deadline: normalized.deadline
      })
      return new IpcPublicConnection(base, peer, this.capabilities, this.gattDeliverySelection)
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async withConnection<T>(
    peer: BlePeer | string,
    options: ConnectOptions,
    action: (connection: import('../public/ble-manager').BleConnection) => Promise<T>
  ): Promise<T> {
    const connection = await this.connect(peer, options)
    return runWithCleanup(
      () => action(connection),
      () => connection.release()
    )
  }

  async withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T> {
    const scan = await this.scan(options)
    return runWithCleanup(
      () => action(scan),
      () => scan.stop()
    )
  }

  async withDiscoveredConnection<T>(
    peer: BlePeer | string,
    options: ConnectOptions,
    action: (scope: {
      readonly connection: import('../public/ble-manager').BleConnection
      readonly gatt: GattDatabase
    }) => Promise<T>
  ): Promise<T> {
    return this.withConnection(peer, options, async connection => {
      const gatt = await connection.discover(options)
      return action(Object.freeze({ connection, gatt }))
    })
  }

  destroy(): Promise<PublicCleanupRecord> {
    return rehydratePublicPromise(this.ipc.destroy()).then(toPublicCleanupRecord)
  }

  /** Low-level host seam retained for Tauri's existing deterministic tests. */
  adapterState(): Promise<import('../backend-contract/primitives').SerializableRecord> {
    return rehydratePublicPromise(this.ipc.adapterState())
  }
}

class IpcPublicScanSession implements ScanSession {
  readonly plan: import('../backend-contract/scan-planning').ScanPlan | null
  private stopPromise: Promise<PublicCleanupRecord> | null = null
  private readonly timeoutHandle: ReturnType<typeof setTimeout> | null
  private readonly abortSignal: AbortSignal | null
  private readonly abortHandler: (() => void) | null

  constructor(
    private readonly inner: import('./manager').IpcScanSession,
    readonly observations: PublicBoundedAsyncStream<PublicScanObservation>,
    private readonly scanState: ScanStateController,
    options: ScanOptions
  ) {
    this.plan = inner.plan
    const stopAutomatically = () => {
      this.stop().catch(() => undefined)
    }
    this.abortSignal = options.signal ?? null
    this.abortHandler = this.abortSignal === null ? null : stopAutomatically
    this.abortSignal?.addEventListener('abort', stopAutomatically, { once: true })
    this.timeoutHandle =
      options.timeoutMs === undefined ? null : globalThis.setTimeout(stopAutomatically, options.timeoutMs)
  }

  stop(): Promise<PublicCleanupRecord> {
    if (this.stopPromise !== null) return this.stopPromise
    const result = this.stopInternal()
    this.stopPromise = result
    return result
  }

  private async stopInternal(): Promise<PublicCleanupRecord> {
    if (this.timeoutHandle !== null) globalThis.clearTimeout(this.timeoutHandle)
    if (this.abortSignal !== null && this.abortHandler !== null) {
      this.abortSignal.removeEventListener('abort', this.abortHandler)
    }
    this.scanState.emit({ state: 'stopping' })
    try {
      const cleanup = await rehydratePublicPromise(this.inner.stop()).then(toPublicCleanupRecord)
      this.scanState.emit(
        cleanup.state === 'released' ? { state: 'stopped' } : { state: 'failed', reason: 'scan-stop-failed' }
      )
      this.scanState.close()
      if (cleanup.state === 'release-failed') this.stopPromise = null
      return cleanup
    } catch (error) {
      this.scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
      this.scanState.close()
      this.stopPromise = null
      throw rehydratePublicError(error)
    }
  }

  get state(): AsyncIterable<import('../public/ble-manager').ScanStateEvent> {
    return this.scanState.stream
  }
}

class IpcPublicConnection implements BleConnection {
  readonly peer: BlePeer
  readonly handle: string
  readonly connectionId: string
  readonly ownerLeaseId: string
  readonly connectionGeneration: string
  readonly lifecycleEvents: AsyncIterable<BleConnectionEvent>
  readonly controls: BleConnectionControls

  constructor(
    private readonly base: IpcConnection,
    peer: BlePeer | string,
    capabilities: BleCapabilities,
    private readonly gattDeliverySelection: 'unknown' | 'controllable'
  ) {
    this.peer = typeof peer === 'string' ? snapshotBlePeer({ id: peer, name: null, rssi: null }) : snapshotBlePeer(peer)
    this.handle = base.handle
    this.connectionId = base.connectionId
    this.ownerLeaseId = base.ownerLeaseId
    this.connectionGeneration = base.connectionGeneration
    this.lifecycleEvents = broadcastConnectionEvents(
      mapIpcConnectionEvents(base.events, {
        attachmentId: base.attachmentId,
        peerId: base.peerId,
        connectionId: base.connectionId,
        ownerLeaseId: base.ownerLeaseId,
        connectionGeneration: base.connectionGeneration
      })
    )
    this.controls = createIpcConnectionControls(this.base, capabilities, this.connectionGeneration)
  }

  async discover(options: OperationOptions = {}): Promise<GattDatabase> {
    try {
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const database = await this.base.discover({
        signal: normalized.signal ?? undefined,
        deadline: normalized.deadline
      })
      return createPublicGattDatabase(createIpcGattSource(database, this.gattDeliverySelection))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async rediscoverGatt(options: RediscoverGattOptions): Promise<GattDatabase> {
    try {
      if (
        options === undefined ||
        options === null ||
        (options.reason !== 'service-changed' && options.reason !== 'manual')
      ) {
        throw contractError('argument.invalid', 'gatt', 'ipc-public-manager.rediscover-gatt.reason')
      }
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const database = await this.base.rediscoverGatt(
        {
          signal: normalized.signal ?? undefined,
          deadline: normalized.deadline
        },
        options.reason === 'manual' ? 'manual-rediscovery' : 'service-changed'
      )
      return createPublicGattDatabase(createIpcGattSource(database, this.gattDeliverySelection))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  disconnect(): Promise<PublicCleanupRecord> {
    return rehydratePublicPromise(this.base.disconnect()).then(toPublicCleanupRecord)
  }

  release(): Promise<PublicCleanupRecord> {
    return rehydratePublicPromise(this.base.release()).then(toPublicCleanupRecord)
  }
}

const IPC_RECEIPT_TIMESTAMP_LIMITATION = Object.freeze({
  code: 'ipc-receipt-timestamp',
  explanation:
    'The observation timestamp is the renderer receipt time because the IPC payload has no backend timestamp.',
  affectedGuarantee: 'backend measurement timestamp authority'
})

function requireIpcControlCapability(
  capabilities: BleCapabilities,
  id: `${string}:${string}`,
  operation: string
): CapabilityDescriptor {
  const descriptor = capabilities.get(id)
  if (descriptor === undefined || descriptor.state === 'unsupported') {
    throw contractError('capability.unsupported', 'connection', operation)
  }
  if (descriptor.state === 'unavailable') {
    throw contractError('capability.unavailable', 'connection', operation)
  }
  return descriptor
}

function ipcControlMetadata(
  generation: string,
  capabilities: CapabilityDescriptor,
  observedAtMonotonicMs: number
): BleControlObservationMetadata {
  return Object.freeze({
    connectionGeneration: generation,
    observedAtMonotonicMs,
    source: 'backend',
    authority: 'ipc-backend-operation',
    limitations: Object.freeze([IPC_RECEIPT_TIMESTAMP_LIMITATION, ...capabilities.limitations])
  })
}

async function runIpcControl<Value>(operation: () => Promise<Value>): Promise<Value> {
  try {
    return await operation()
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function unsupportedIpcControlStream<Value>(
  operation: string,
  code: 'capability.unsupported' | 'capability.unavailable' = 'capability.unsupported'
): AsyncIterable<Value> {
  return new UnsupportedIpcControlStream(operation, code)
}

class UnsupportedIpcControlStream<Value> implements AsyncIterable<Value> {
  constructor(
    private readonly operation: string,
    private readonly code: 'capability.unsupported' | 'capability.unavailable'
  ) {}

  [Symbol.asyncIterator](): AsyncIterator<Value> {
    return new UnsupportedIpcControlIterator(this.operation, this.code)
  }
}

class UnsupportedIpcControlIterator<Value> implements AsyncIterator<Value> {
  constructor(
    private readonly operation: string,
    private readonly code: 'capability.unsupported' | 'capability.unavailable'
  ) {}

  async next(): Promise<IteratorResult<Value, undefined>> {
    throw rehydratePublicError(contractError(this.code, 'connection', this.operation))
  }

  async return(): Promise<IteratorResult<Value, undefined>> {
    return { done: true, value: undefined }
  }
}

function createIpcConnectionControls(
  connection: Pick<IpcConnection, 'readRssi' | 'maximumWriteLength'>,
  capabilities: BleCapabilities,
  generation: string
): BleConnectionControls {
  const readRssi = (options: OperationOptions = {}): Promise<RssiObservation> =>
    runIpcControl(async () => {
      const descriptor = requireIpcControlCapability(
        capabilities,
        BUILT_IN_FEATURE_IDS.connectionRssi,
        'ipc-public-manager.controls.read-rssi'
      )
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const rssi = await connection.readRssi({
        signal: normalized.signal ?? undefined,
        deadline: normalized.deadline
      })
      const observation: RssiObservation = Object.freeze({
        ...ipcControlMetadata(generation, descriptor, globalThis.performance.now()),
        state: 'measured',
        rssi
      })
      return observation
    })

  const maximumWriteLength = (mode: WriteMode): Promise<MaximumWriteLengthObservation> =>
    runIpcControl(async () => {
      const descriptor = requireIpcControlCapability(
        capabilities,
        BUILT_IN_FEATURE_IDS.maximumWriteLength,
        'ipc-public-manager.controls.maximum-write-length'
      )
      if (mode !== 'with-response' && mode !== 'without-response') {
        throw contractError('argument.invalid', 'connection', 'ipc-public-manager.controls.maximum-write-length')
      }
      const maximumWriteLengthValue = await connection.maximumWriteLength(mode)
      const observation: MaximumWriteLengthObservation = Object.freeze({
        ...ipcControlMetadata(generation, descriptor, globalThis.performance.now()),
        state: 'measured',
        mode,
        maximumWriteLength: maximumWriteLengthValue
      })
      return observation
    })

  const unsupportedPromise = <Value>(operation: string): Promise<Value> =>
    runIpcControl(async () => {
      throw contractError('capability.unsupported', 'connection', operation)
    })

  return Object.freeze({
    readRssi,
    effectiveMtu: (): Promise<MtuObservation> => unsupportedPromise('ipc-public-manager.controls.effective-mtu'),
    requestMtu: (_mtu: number, _options: OperationOptions = {}): Promise<MtuNegotiation> =>
      unsupportedPromise('ipc-public-manager.controls.request-mtu'),
    maximumWriteLength,
    requestPriority: (
      _priority: ConnectionPriority,
      _options: OperationOptions = {}
    ): Promise<ConnectionPriorityResult> => unsupportedPromise('ipc-public-manager.controls.request-priority'),
    readPhy: (_options: OperationOptions = {}): Promise<PhyObservation> =>
      unsupportedPromise('ipc-public-manager.controls.read-phy'),
    requestPhy: (_preference: PhyPreference, _options: OperationOptions = {}): Promise<PhyUpdateResult> =>
      unsupportedPromise('ipc-public-manager.controls.request-phy'),
    parameters: (): Promise<ConnectionParametersObservation> =>
      unsupportedPromise('ipc-public-manager.controls.parameters'),
    parameterEvents: () =>
      unsupportedIpcControlStream<ConnectionParametersObservation>('ipc-public-manager.controls.parameter-events'),
    requestSubrate: (_mode: SubrateMode, _options: OperationOptions = {}): Promise<SubrateResult> =>
      unsupportedPromise('ipc-public-manager.controls.request-subrate'),
    writeReadiness: (_mode: 'without-response') => {
      const descriptor = capabilities.get(BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness)
      return descriptor?.state === 'unavailable'
        ? unsupportedIpcControlStream<WriteReadinessEvent>(
            'ipc-public-manager.controls.write-readiness',
            'capability.unavailable'
          )
        : unsupportedIpcControlStream<WriteReadinessEvent>('ipc-public-manager.controls.write-readiness')
    }
  })
}

function createIpcGattSource(
  database: IpcGattDatabase,
  deliverySelection: 'unknown' | 'controllable'
): PublicGattDatabaseSource {
  const changed = database.changed !== undefined && 'limits' in database.changed ? database.changed : undefined
  return {
    path: database.path,
    deliverySelection,
    ...(changed === undefined ? {} : { changed }),
    assertCurrent: () => database.assertCurrent(),
    monotonicNow: () => database.monotonicNow(),
    scheduleDeadline: (deadline, action) => database.scheduleDeadline(deadline, action),
    snapshot: () => database.snapshot(),
    read: (path: PortableCurrentCharacteristicPath, options: PortableOperationOptions) => database.read(path, options),
    write: async (path: PortableCurrentCharacteristicPath, value: Readonly<Uint8Array>, options: PortableWritePolicy) =>
      toPortableWriteReceipt(await database.write(path, value, options)),
    maximumWriteLength: async () => {
      throw contractError('capability.unsupported', 'gatt', 'ipc-public-manager.gatt.maximum-write-length')
    },
    writeLong: async () => {
      throw contractError('capability.unsupported', 'gatt', 'ipc-public-manager.gatt.write-long')
    },
    readDescriptor: (path: PortableCurrentDescriptorPath, options: PortableOperationOptions) =>
      database.readDescriptor(path, options),
    writeDescriptor: async (
      path: PortableCurrentDescriptorPath,
      value: Readonly<Uint8Array>,
      options: PortableWritePolicy
    ) => toPortableWriteReceipt(await database.writeDescriptor(path, value, options)),
    subscribe: async (path, options) => toPortableSubscription(await database.subscribe(path, options), path)
  }
}

function toPortableWriteReceipt(receipt: IpcWriteReceipt): import('../manager/consumer-handles').PortableWriteReceipt {
  return {
    terminal: {
      correlation: receipt.terminal.correlation,
      outcome: receipt.terminal.outcome === 'succeeded' ? 'succeeded' : 'failed',
      cause: isBleErrorCode(receipt.terminal.cause) ? receipt.terminal.cause : null
    },
    commitState: receipt.commitState === 'confirmed' ? 'confirmed' : 'unknown'
  }
}

function toPortableSubscription(
  subscription: IpcSubscription,
  path: PortableCurrentCharacteristicPath
): SubscriptionHandle {
  return {
    subscriptionId: subscription.subscriptionId,
    path,
    values: toPortableNotificationStream(subscription.values),
    remove: () => subscription.remove().then(toPublicCleanupRecord)
  }
}

function toPortableNotificationStream(
  source: BoundedAsyncStream<IpcNotificationValue>
): PortableBoundedAsyncStream<PortableNotificationValue> {
  return mapPublicBoundedAsyncStream(source, value => ({
    value: new Uint8Array(value.value),
    indication: value.delivery === 'indication',
    delivery: value.delivery,
    observedAtMonotonicMs: value.observedAtMonotonicMs,
    sequence: value.sequence
  }))
}

function toIpcScanOptions(
  options: ScanOptions,
  signal: AbortSignal | null,
  query: import('../backend-contract/scan-query').NormalizedScanQuery,
  deadline: number | null
) {
  const delivery = resolveStreamPolicy(options.delivery ?? 'balanced')
  return {
    query,
    signal: signal ?? undefined,
    deadline,
    stream: {
      itemCapacity: Number(delivery.itemCapacity),
      byteCapacity: Number(delivery.byteCapacity),
      reservedControlCapacity: Number(delivery.reservedControlCapacity),
      overflowPolicy: delivery.overflowPolicy
    }
  }
}

function isReferenceLike(value: unknown): boolean {
  return (
    typeof value === 'object' &&
    value !== null &&
    ('version' in value || 'backendId' in value || 'scope' in value || 'opaqueId' in value)
  )
}

function assertIpcConnectionOptions(options: ConnectOptions): void {
  if (options.intent === 'when-available') {
    throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.connect.when-available')
  }
  if (options.preferredPhy !== undefined) {
    throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.connect.preferred-phy')
  }
  if (options.transport !== undefined) {
    throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.connect.transport')
  }
}

/** @internal Shared IPC lifecycle projection used by the Tauri and Electron façade. */
export function mapIpcConnectionEvents(
  source: BoundedAsyncStream<import('../backend-contract/primitives').SerializableRecord>,
  expected: {
    readonly attachmentId: string
    readonly peerId: string
    readonly connectionId: string
    readonly ownerLeaseId: string
    readonly connectionGeneration: string
  }
): AsyncIterable<BleConnectionEvent> {
  let lastSequence = 0
  const lifecycle: AsyncIterable<BleConnectionEvent> = {
    [Symbol.asyncIterator]() {
      let iterator: IpcConnectionEventIterator
      try {
        if (typeof source[Symbol.asyncIterator] !== 'function') {
          throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-factory')
        }
        iterator = source[Symbol.asyncIterator]()
        if (typeof iterator !== 'object' || iterator === null || typeof iterator.next !== 'function') {
          throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-iterator')
        }
      } catch (error) {
        throw rehydratePublicError(
          error instanceof BackendContractError
            ? error
            : contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-construction')
        )
      }
      let returnPromise: IpcConnectionEventReturn | null = null
      let sourceClosed = false
      const returnSource = (): IpcConnectionEventReturn => {
        if (returnPromise !== null) return returnPromise
        sourceClosed = true
        try {
          const sourceReturn = iterator.return
          if (sourceReturn === undefined) {
            returnPromise = Promise.resolve({ done: true, value: undefined })
          } else if (typeof sourceReturn !== 'function') {
            returnPromise = Promise.reject(
              contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-return')
            )
          } else {
            returnPromise = Promise.resolve(sourceReturn.call(iterator))
          }
        } catch (error) {
          returnPromise = Promise.reject(error)
        }
        returnPromise = returnPromise.catch(error => {
          returnPromise = null
          throw error
        })
        return returnPromise
      }
      return {
        async next(): Promise<IteratorResult<BleConnectionEvent, undefined>> {
          if (sourceClosed) return { done: true, value: undefined }
          try {
            while (true) {
              const item = await iterator.next()
              if (typeof item !== 'object' || item === null) {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-result')
              }
              const done = readIpcConnectionField(item, 'done', 'ipc-public-manager.connection-event-result')
              if (typeof done !== 'boolean') {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-result')
              }
              if (done) {
                sourceClosed = true
                throw contractError('stream.closed', 'connection', 'ipc-public-manager.connection-events')
              }
              const itemValue = readIpcConnectionField(item, 'value', 'ipc-public-manager.connection-event-item')
              if (
                typeof itemValue !== 'object' ||
                itemValue === null ||
                Array.isArray(itemValue) ||
                !hasOwnIpcConnectionField(itemValue, 'kind', 'ipc-public-manager.connection-event-item')
              ) {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-item')
              }
              const itemKind = readIpcConnectionField(itemValue, 'kind', 'ipc-public-manager.connection-event-kind')
              if (itemKind === 'overflow') {
                throw contractError('stream.overflow', 'connection', 'ipc-public-manager.connection-events')
              }
              if (itemKind === 'terminal') {
                const reason = parseConnectionTerminalReason(
                  readIpcConnectionField(itemValue, 'reason', 'ipc-public-manager.connection-event-terminal')
                )
                if (reason === null) {
                  throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-terminal')
                }
                throw publicConnectionTerminalError(reason)
              }
              if (itemKind !== 'value') {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-kind')
              }
              const value = readIpcConnectionField(itemValue, 'value', 'ipc-public-manager.connection-event-value')
              if (typeof value !== 'object' || value === null || Array.isArray(value)) {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event-value')
              }
              const previous = parseConnectionState(
                readIpcConnectionField(value, 'previous', 'ipc-public-manager.connection-event')
              )
              const current = parseConnectionState(
                readIpcConnectionField(value, 'current', 'ipc-public-manager.connection-event')
              )
              const cause = parseConnectionCause(
                readIpcConnectionField(value, 'cause', 'ipc-public-manager.connection-event')
              )
              const sequence = readIpcConnectionField(value, 'sequence', 'ipc-public-manager.connection-event')
              if (
                readIpcConnectionField(value, 'attachmentId', 'ipc-public-manager.connection-event') !==
                  expected.attachmentId ||
                readIpcConnectionField(value, 'peerId', 'ipc-public-manager.connection-event') !== expected.peerId ||
                readIpcConnectionField(value, 'connectionId', 'ipc-public-manager.connection-event') !==
                  expected.connectionId ||
                readIpcConnectionField(value, 'ownerLeaseId', 'ipc-public-manager.connection-event') !==
                  expected.ownerLeaseId ||
                readIpcConnectionField(value, 'connectionGeneration', 'ipc-public-manager.connection-event') !==
                  expected.connectionGeneration ||
                previous === null ||
                current === null ||
                cause === null ||
                typeof sequence !== 'number' ||
                !Number.isSafeInteger(sequence) ||
                sequence <= lastSequence
              ) {
                throw contractError('protocol.malformed', 'ipc', 'ipc-public-manager.connection-event')
              }
              lastSequence = sequence
              return {
                done: false,
                value: Object.freeze({
                  kind: 'connection-lifecycle',
                  previous,
                  current,
                  cause,
                  connectionGeneration: expected.connectionGeneration,
                  sequence
                })
              }
            }
          } catch (error) {
            const primary = rehydratePublicError(error)
            try {
              await returnSource()
            } catch (cleanupError) {
              throw new AggregateError(
                [primary, rehydratePublicError(cleanupError)],
                'BLE IPC connection event and iterator cleanup both failed'
              )
            }
            throw primary
          }
        },
        return: async () => {
          try {
            await returnSource()
            return { done: true, value: undefined }
          } catch (error) {
            throw rehydratePublicError(error)
          }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
  return Object.freeze(lifecycle)
}

type IpcConnectionEventResult = IteratorResult<
  import('../backend-contract/streams').StreamItem<import('../backend-contract/primitives').SerializableRecord>,
  undefined
>
type IpcConnectionEventReturn = Promise<IpcConnectionEventResult>
type IpcConnectionEventIterator = {
  readonly next: () => IpcConnectionEventReturn
  readonly return?: () => IpcConnectionEventReturn
  readonly [Symbol.asyncIterator]: () => IpcConnectionEventIterator
}

function hasOwnIpcConnectionField(value: object, key: string, operation: string): boolean {
  try {
    return Object.prototype.hasOwnProperty.call(value, key)
  } catch {
    throw contractError('protocol.malformed', 'ipc', operation)
  }
}

function readIpcConnectionField(value: object, key: string, operation: string): unknown {
  try {
    return Reflect.get(value, key)
  } catch {
    throw contractError('protocol.malformed', 'ipc', operation)
  }
}

function parseConnectionTerminalReason(
  value: unknown
): import('../backend-contract/streams').StreamTerminalNotice['reason'] | null {
  return value === 'closed' ||
    value === 'overflow' ||
    value === 'source-failed' ||
    value === 'owner-released' ||
    value === 'connection-lost' ||
    value === 'service-changed' ||
    value === 'operation-aborted' ||
    value === 'operation-timed-out'
    ? value
    : null
}

function parseConnectionState(value: unknown): BleConnectionEvent['current'] | null {
  return value === 'connecting' ||
    value === 'connected' ||
    value === 'disconnecting' ||
    value === 'disconnected' ||
    value === 'lost'
    ? value
    : null
}

function parseConnectionCause(value: unknown): ConnectionLifecycleCause | null {
  return value === 'connected' ||
    value === 'backend-transition' ||
    value === 'requested-disconnect' ||
    value === 'peer-link-loss' ||
    value === 'adapter-loss' ||
    value === 'backend-restart' ||
    value === 'released' ||
    value === 'manager-destroyed' ||
    value === 'backend-failure'
    ? value
    : null
}

function createIpcAdapter(ipc: IpcBleManager): BleAdapter {
  const readState = async (options: import('./manager').IpcManagerOperationOptions = {}): Promise<BleAdapterState> => {
    try {
      const value = await ipc.adapterState(options)
      if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw contractError('protocol.malformed', 'adapter', 'ipc-public-manager.adapter-state')
      }
      const availability = Reflect.get(value, 'availability')
      const authorization = Reflect.get(value, 'authorization')
      const power = Reflect.get(value, 'power')
      if (!isAdapterAvailability(availability) || !isAdapterAuthorization(authorization) || !isAdapterPower(power)) {
        throw contractError('protocol.malformed', 'adapter', 'ipc-public-manager.adapter-state')
      }
      const backendGeneration = Reflect.get(value, 'backendGeneration')
      const updatedAt = Reflect.get(value, 'updatedAt')
      const safeReason = Reflect.get(value, 'safeReason')
      const expectedBackendGeneration = String(ipc.bootstrap.attachment.backendGeneration)
      if (backendGeneration !== expectedBackendGeneration) {
        throw contractError('protocol.violation', 'adapter', 'ipc-public-manager.adapter-generation')
      }
      if (typeof updatedAt !== 'number' || !Number.isFinite(updatedAt) || updatedAt < 0) {
        throw contractError('protocol.malformed', 'adapter', 'ipc-public-manager.adapter-updated-at')
      }
      if (safeReason !== null && typeof safeReason !== 'string') {
        throw contractError('protocol.malformed', 'adapter', 'ipc-public-manager.adapter-safe-reason')
      }
      return Object.freeze({ availability, authorization, power, backendGeneration, updatedAt, safeReason })
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
  return {
    id: String(ipc.bootstrap.attachment.adapter.adapterId),
    state: () => readState(),
    watchState: options => watchIpcAdapterState(readState, options),
    waitUntilReady: async (options: AdapterReadinessOptions = {}) => {
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const deadline = normalized.deadline === null ? globalThis.performance.now() + 10_000 : normalized.deadline
      while (true) {
        const current = await readState({ signal: normalized.signal ?? undefined, deadline })
        if (current.availability === 'unsupported' || current.power === 'unsupported')
          throw contractError('capability.unsupported', 'adapter', 'ipc-public-manager.adapter-ready')
        if (
          current.authorization === 'denied' ||
          current.authorization === 'restricted' ||
          current.authorization === 'unavailable'
        )
          throw contractError('permission.denied', 'adapter', 'ipc-public-manager.adapter-ready')
        if (current.availability === 'available' && current.power === 'on') return current
        if (normalized.signal?.aborted === true)
          throw contractError('operation.aborted', 'adapter', 'ipc-public-manager.adapter-ready')
        if (globalThis.performance.now() >= deadline)
          throw contractError('operation.timed-out', 'adapter', 'ipc-public-manager.adapter-ready')
        await new Promise(resolve => setTimeout(resolve, 25))
      }
    }
  }
}

async function watchIpcAdapterState(
  readState: (options?: import('./manager').IpcManagerOperationOptions) => Promise<BleAdapterState>,
  options: AdapterWatchOptions = {}
): Promise<BleAdapterStateWatch> {
  try {
    const signal = options.signal ?? null
    if (signal !== null && !(signal instanceof AbortSignal)) {
      throw contractError('argument.invalid', 'adapter', 'ipc-public-manager.watch-state.signal')
    }
    if (adapterWatchAborted(signal)) {
      throw contractError('operation.aborted', 'adapter', 'ipc-public-manager.watch-state')
    }
    const initial = await readState({ signal: signal ?? undefined })
    if (adapterWatchAborted(signal)) {
      throw contractError('operation.aborted', 'adapter', 'ipc-public-manager.watch-state')
    }
    const stream = new CoreBoundedStream<BleAdapterState>(IPC_ADAPTER_STATE_STREAM_LIMITS, 'drop-oldest')
    let current = initial
    let active = true
    let timer: ReturnType<typeof setTimeout> | null = null
    let stopPromise: Promise<CleanupRecord> | null = null

    const clearTimer = () => {
      if (timer !== null) {
        globalThis.clearTimeout(timer)
        timer = null
      }
    }
    const removeAbortHandler = () => signal?.removeEventListener('abort', abortHandler)
    const stop = (): Promise<CleanupRecord> => {
      if (stopPromise !== null) return stopPromise
      active = false
      clearTimer()
      removeAbortHandler()
      const result = stream.close().then(
        cleanup => {
          if (cleanup.state === 'release-failed') stopPromise = null
          return cleanup
        },
        error => {
          stopPromise = null
          throw error
        }
      )
      stopPromise = result
      return result
    }
    const abortHandler = () => {
      stop().catch(() => undefined)
    }
    const schedulePoll = () => {
      if (!active) return
      timer = globalThis.setTimeout(() => {
        timer = null
        poll().catch(() => undefined)
      }, IPC_ADAPTER_STATE_POLL_INTERVAL_MS)
    }
    const poll = async (): Promise<void> => {
      if (!active) return
      try {
        const next = await readState({ signal: signal ?? undefined })
        if (!active) return
        if (!sameAdapterState(current, next)) {
          current = next
          stream.emit(next, adapterStateByteLength(next))
        }
        schedulePoll()
      } catch {
        if (!active) return
        active = false
        clearTimer()
        removeAbortHandler()
        stream.finishWithReason('source-failed')
      }
    }
    signal?.addEventListener('abort', abortHandler, { once: true })
    schedulePoll()
    return Object.freeze({
      initial,
      values: mapPublicBoundedAsyncStream(stream, value => value),
      stop: () => stop().then(toPublicCleanupRecord)
    })
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function sameAdapterState(left: BleAdapterState, right: BleAdapterState): boolean {
  return (
    left.availability === right.availability &&
    left.authorization === right.authorization &&
    left.power === right.power &&
    left.backendGeneration === right.backendGeneration &&
    left.updatedAt === right.updatedAt &&
    left.safeReason === right.safeReason
  )
}

function adapterStateByteLength(state: BleAdapterState): number {
  const serialized = JSON.stringify(state)
  return serialized === undefined ? 0 : serialized.length
}

function adapterWatchAborted(signal: AbortSignal | null): boolean {
  return signal?.aborted === true
}

function isBleErrorCode(value: string | null): value is import('../backend-contract/errors').BleErrorCode {
  return value !== null && BLE_ERROR_CODES.some(code => code === value)
}

function isAdapterAvailability(value: unknown): value is BleAdapterState['availability'] {
  return value === 'available' || value === 'unavailable' || value === 'unsupported' || value === 'unknown'
}

function isAdapterAuthorization(value: unknown): value is BleAdapterState['authorization'] {
  return (
    value === 'granted' ||
    value === 'denied' ||
    value === 'restricted' ||
    value === 'not-determined' ||
    value === 'unavailable' ||
    value === 'unknown'
  )
}

function isAdapterPower(value: unknown): value is BleAdapterState['power'] {
  return value === 'on' || value === 'off' || value === 'resetting' || value === 'unsupported' || value === 'unknown'
}

function discoveryKindFromCapabilities(capabilities: BleCapabilities): BleManager['discovery']['kind'] {
  const continuous = capabilities.supports('discovery:continuous-scan')
  const chooser = capabilities.supports('discovery:system-chooser')
  if (continuous && chooser) return 'hybrid'
  if (chooser) return 'system-chooser'
  return 'continuous-scan'
}
