import { BLE_ERROR_CODES, contractError, type CleanupRecord } from '../backend-contract/errors'
import type { ConnectionLifecycleCause } from '../backend-contract/connection-lifecycle'
import type { BoundedAsyncStream, BoundedAsyncStreamIterator } from '../backend-contract/streams'
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
  BleConnectionEvent,
  BleConnection,
  BleManager,
  BlePeer,
  ConnectOptions,
  FindOptions,
  PublicScanObservation,
  ScanOptions,
  ScanSession
} from '../public/ble-manager'
import {
  assertPublicConnectOptions,
  assertPublicChooseOptions,
  assertPublicScanOptions,
  broadcastConnectionEvents,
  filterScanObservations,
  findPeerInScan,
  snapshotBlePeer
} from '../public/ble-manager'
import type { BleAdapter, BleAdapterState, AdapterReadinessOptions } from '../public/ble-adapter'
import { assertDirectConnectionCapability } from '../public/capabilities'
import type { BleCapabilities } from '../public/capabilities'
import { createPublicGattDatabase, type PublicGattDatabaseSource } from '../public/gatt'
import type { GattDatabase } from '../public/gatt'
import type { BleDiagnostics } from '../public/diagnostics'
import { diagnosticsUnavailable } from '../public/diagnostics'
import { normalizeOperationOptions } from '../public/operation-options'
import type { OperationOptions } from '../public/operation-options'
import { normalizeScanQuery } from '../public/scan-query'
import { createScanState } from '../public/scan-state'
import type { ScanStateController } from '../public/scan-state'
import { unsupportedPeerDirectory } from '../public/peer-directory'
import type { BlePeerDirectory } from '../public/peer-directory'
import { isPeerReference } from '../public/peer-reference'
import type { PeerReference } from '../public/peer-reference'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from '../public/error-bridge'
import { resolveStreamPolicy } from '../public/stream-presets'
import {
  IpcBleManager,
  type IpcConnection,
  type IpcGattDatabase,
  type IpcNotificationValue,
  type IpcSubscription,
  type IpcWriteReceipt
} from './manager'

export interface IpcPublicManagerOptions {
  readonly discoveryKind?: BleManager['discovery']['kind']
  readonly capabilities?: BleCapabilities
  readonly adapter?: BleAdapter
  readonly diagnostics?: BleDiagnostics
  readonly peers?: BlePeerDirectory
}

export class IpcPublicManagerAdapter implements BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly discovery: BleManager['discovery']

  constructor(
    private readonly ipc: IpcBleManager,
    options: IpcPublicManagerOptions = {}
  ) {
    this.capabilities = options.capabilities ?? ipc.capabilities
    this.adapter = options.adapter ?? createIpcAdapter(ipc)
    this.diagnostics = options.diagnostics ?? diagnosticsUnavailable()
    this.peers = options.peers ?? unsupportedPeerDirectory()
    this.discovery = Object.freeze({
      kind: options.discoveryKind ?? ipc.bootstrap.discovery?.kind ?? discoveryKindFromCapabilities(this.capabilities)
    })
  }

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    try {
      assertPublicScanOptions(options)
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const session = await this.ipc.scan(toIpcScanOptions(options, normalized.signal))
      const state = createScanState()
      state.emit({ state: 'active' })
      return new IpcPublicScanSession(
        session,
        filterScanObservations(
          session.observations,
          normalizeScanQuery(options.query),
          options.duplicates ?? 'coalesced'
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
    peer: BlePeer | string | PeerReference,
    options: ConnectOptions = {}
  ): Promise<import('../public/ble-manager').BleConnection> {
    try {
      assertPublicConnectOptions(options)
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      assertIpcConnectionOptions(options)
      assertDirectConnectionCapability(this.capabilities.get('connection:direct'), 'ipc-public-manager.connect.direct')
      if (isReferenceLike(peer) && !isPeerReference(peer)) {
        throw contractError('peer.reference-invalid', 'connection', 'ipc-public-manager.connect-reference')
      }
      if (isPeerReference(peer)) {
        throw contractError('capability.unsupported', 'connection', 'ipc-public-manager.peer-reference')
      }
      const peerId = typeof peer === 'string' ? peer : peer.id
      const base = await this.ipc.connect(peerId, {
        signal: normalized.signal ?? undefined,
        timeoutMs: options.timeoutMs
      })
      return new IpcPublicConnection(base, peer)
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

  destroy(): Promise<CleanupRecord> {
    return rehydratePublicPromise(this.ipc.destroy())
  }

  /** Low-level host seam retained for Tauri's existing deterministic tests. */
  adapterState(): Promise<import('../backend-contract/primitives').SerializableRecord> {
    return rehydratePublicPromise(this.ipc.adapterState())
  }
}

class IpcPublicScanSession implements ScanSession {
  private stopPromise: Promise<CleanupRecord> | null = null
  private readonly timeoutHandle: ReturnType<typeof setTimeout> | null
  private readonly abortSignal: AbortSignal | null
  private readonly abortHandler: (() => void) | null

  constructor(
    private readonly inner: import('./manager').IpcScanSession,
    readonly observations: BoundedAsyncStream<PublicScanObservation>,
    private readonly scanState: ScanStateController,
    options: ScanOptions
  ) {
    const stopAutomatically = () => {
      this.stop().catch(() => undefined)
    }
    this.abortSignal = options.signal ?? null
    this.abortHandler = this.abortSignal === null ? null : stopAutomatically
    this.abortSignal?.addEventListener('abort', stopAutomatically, { once: true })
    this.timeoutHandle =
      options.timeoutMs === undefined ? null : globalThis.setTimeout(stopAutomatically, options.timeoutMs)
  }

  stop(): Promise<CleanupRecord> {
    if (this.stopPromise !== null) return this.stopPromise
    const result = this.stopInternal()
    this.stopPromise = result
    return result
  }

  private async stopInternal(): Promise<CleanupRecord> {
    if (this.timeoutHandle !== null) globalThis.clearTimeout(this.timeoutHandle)
    if (this.abortSignal !== null && this.abortHandler !== null) {
      this.abortSignal.removeEventListener('abort', this.abortHandler)
    }
    this.scanState.emit({ state: 'stopping' })
    try {
      const cleanup = await rehydratePublicPromise(this.inner.stop())
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

  constructor(
    private readonly base: IpcConnection,
    peer: BlePeer | string
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
  }

  get events(): BoundedAsyncStream<import('../backend-contract/primitives').SerializableRecord> {
    return this.base.events
  }

  async discover(options: OperationOptions = {}): Promise<GattDatabase> {
    try {
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const database = await this.base.discover({
        signal: normalized.signal ?? undefined,
        deadline: normalized.deadline
      })
      return createPublicGattDatabase(createIpcGattSource(database))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  readRssi(options: OperationOptions = {}): Promise<number> {
    const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
    return rehydratePublicPromise(
      this.base.readRssi({ signal: normalized.signal ?? undefined, timeoutMs: options.timeoutMs })
    )
  }

  maximumWriteLength(mode: 'with-response' | 'without-response' = 'with-response'): Promise<number> {
    return rehydratePublicPromise(this.base.maximumWriteLength(mode))
  }

  requestMtu(_requestedMtu: number, _options: OperationOptions = {}): Promise<number> {
    return Promise.reject(
      rehydratePublicError(contractError('capability.unsupported', 'connection', 'ipc-public-manager.connection.mtu'))
    )
  }

  disconnect(): Promise<CleanupRecord> {
    return rehydratePublicPromise(this.base.disconnect())
  }

  release(): Promise<CleanupRecord> {
    return rehydratePublicPromise(this.base.release())
  }
}

function createIpcGattSource(database: IpcGattDatabase): PublicGattDatabaseSource {
  return {
    path: database.path,
    deliverySelection: 'unknown',
    changed: database.changed,
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
    remove: () => subscription.remove()
  }
}

function toPortableNotificationStream(
  source: BoundedAsyncStream<IpcNotificationValue>
): PortableBoundedAsyncStream<PortableNotificationValue> {
  return {
    limits: {
      itemCapacity: Number(source.limits.itemCapacity),
      byteCapacity: Number(source.limits.byteCapacity),
      reservedControlCapacity: Number(source.limits.reservedControlCapacity)
    },
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator](): BoundedAsyncStreamIterator<PortableNotificationValue> {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next() {
          const result = await iterator.next()
          if (result.done) return { done: true, value: undefined }
          if (result.value.kind === 'value') {
            return {
              done: false,
              value: {
                kind: 'value',
                value: {
                  value: new Uint8Array(result.value.value.value),
                  indication: result.value.value.delivery === 'indication',
                  delivery: result.value.value.delivery,
                  observedAtMonotonicMs: result.value.value.observedAtMonotonicMs,
                  sequence: result.value.value.sequence
                }
              }
            }
          }
          if (result.value.kind === 'overflow') {
            return { done: false, value: result.value }
          }
          return { done: false, value: result.value }
        },
        return: async () => {
          await iterator.return()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    },
    close: () => source.close()
  }
}

function toIpcScanOptions(options: ScanOptions, signal: AbortSignal | null) {
  const delivery = resolveStreamPolicy(options.delivery ?? 'balanced')
  return {
    signal: signal ?? undefined,
    timeoutMs: options.timeoutMs,
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

function mapIpcConnectionEvents(
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
  return {
    [Symbol.asyncIterator]() {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next(): Promise<IteratorResult<BleConnectionEvent, undefined>> {
          while (true) {
            const item = await iterator.next()
            if (item.done) return { done: true, value: undefined }
            if (item.value.kind !== 'value') {
              if (item.value.kind === 'terminal') return { done: true, value: undefined }
              throw contractError('stream.overflow', 'connection', 'ipc-public-manager.connection-events')
            }
            const value = item.value.value
            const previous = parseConnectionState(Reflect.get(value, 'previous'))
            const current = parseConnectionState(Reflect.get(value, 'current'))
            const cause = parseConnectionCause(Reflect.get(value, 'cause'))
            const sequence = Reflect.get(value, 'sequence')
            if (
              Reflect.get(value, 'attachmentId') !== expected.attachmentId ||
              Reflect.get(value, 'peerId') !== expected.peerId ||
              Reflect.get(value, 'connectionId') !== expected.connectionId ||
              Reflect.get(value, 'ownerLeaseId') !== expected.ownerLeaseId ||
              Reflect.get(value, 'connectionGeneration') !== expected.connectionGeneration ||
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
        },
        return: async () => {
          await iterator.return()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
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
