import { BackendContractError, BLE_ERROR_CODES, contractError, type CleanupRecord } from '../backend-contract/errors'
import type {
  BoundedAsyncStream,
  OverflowPolicy,
  StreamLimits,
  StreamTerminalNotice
} from '../backend-contract/streams'
import {
  byteLimit,
  capacity,
  ownBytes,
  resourceCount,
  type SerializableRecord,
  type SerializableValue
} from '../backend-contract/primitives'
import { CoreBoundedStream } from '../core/bounded-stream'
import { createGattCharacteristicProperties } from '../backend-contract/gatt'
import type { GattDatabaseChangedEvent } from '../backend-contract/gatt'
import { createPublicBleCapabilities, type BleCapabilities } from '../public/capabilities'
import type {
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableDatabasePath,
  PortableGattDatabaseSnapshot,
  PortableOperationOptions,
  PortableSubscriptionOptions,
  PortableWritePolicy
} from '../manager/consumer-handles'
import type { AttachmentRecord } from '../backend-contract/identity'
import type { PeerReference } from '../backend-contract/peer-reference'
import { IpcBleClient } from './client'
import { IPC_GATT_DATABASE_SCHEMA_VERSION } from './protocol'
import type { IpcClientTransport } from './protocol'

export {
  advertisementPassesViewFilter,
  type AdvertisementViewFilter,
  type AdvertisementViewRecord
} from './advertisement-view-filter'

const REMOTE_STREAM_LIMITS = Object.freeze({
  itemCapacity: capacity(128),
  byteCapacity: capacity(512 * 1024),
  reservedControlCapacity: capacity(1)
})

export interface IpcManagerOperationOptions {
  readonly signal?: AbortSignal
  readonly timeoutMs?: number
  readonly deliveryMode?: 'prefer-notification' | 'prefer-indication' | 'require-notification' | 'require-indication'
  readonly stream?: {
    readonly itemCapacity: number
    readonly byteCapacity: number
    readonly reservedControlCapacity: number
    readonly overflowPolicy: OverflowPolicy
  }
}

export interface IpcScanOptions extends IpcManagerOperationOptions {
  readonly serviceUuids?: readonly string[]
  readonly manufacturerData?: readonly { readonly companyId: number; readonly dataPrefix?: Readonly<Uint8Array> }[]
  readonly localNamePrefix?: string | null
}

export interface IpcWriteOptions extends IpcManagerOperationOptions {
  readonly mode?: 'with-response' | 'without-response'
}

export interface IpcManufacturerData {
  readonly companyId: number
  readonly data: Uint8Array
}

export interface IpcServiceData {
  readonly uuid: string
  readonly data: Uint8Array
}

export interface IpcAdvertisement {
  readonly peerId: string
  readonly peerReference?: PeerReference
  readonly localName: string | null
  readonly rssi: number | null
  readonly txPowerLevel: number | null
  readonly serviceUuids: readonly string[]
  readonly manufacturerData: readonly IpcManufacturerData[]
  readonly serviceData: readonly IpcServiceData[]
}

export interface IpcNotificationValue {
  readonly value: Uint8Array
  readonly delivery: 'notification' | 'indication' | 'unknown'
  readonly observedAtMonotonicMs: number
  readonly sequence: number
}

export interface IpcWriteReceipt {
  readonly terminal: {
    readonly correlation: string
    readonly outcome: 'succeeded' | 'failed'
    readonly cause: string | null
  }
  readonly mode: 'with-response' | 'without-response'
  readonly commitState: 'confirmed' | 'accepted' | 'unknown' | 'not-started'
  readonly bytesSubmitted: number
}

interface IpcConnectionEventSubscription {
  readonly events: BoundedAsyncStream<SerializableRecord>
  unsubscribe(): Promise<CleanupRecord>
}

export interface IpcCharacteristicRecord extends SerializableRecord {
  readonly handle: string
  readonly serviceUuid: string
  readonly serviceOccurrence: string
  readonly characteristicUuid: string
  readonly characteristicOccurrence: string
  readonly properties: readonly string[]
}

export interface IpcServiceRecord extends SerializableRecord {
  readonly uuid: string
  readonly occurrence: string
  readonly primary: boolean
  readonly includedServices: readonly { readonly uuid: string; readonly occurrence: string }[]
}

export interface IpcDescriptorRecord extends SerializableRecord {
  readonly handle: string
  readonly characteristicHandle: string
  readonly uuid: string
  readonly occurrence: string
}

interface StreamSink {
  readonly closeWithReason: (reason: 'owner-released' | 'source-failed') => void
  readonly deliver: (streamId: string, item: SerializableRecord) => void
}

/**
 * Framework-neutral manager façade for a trusted desktop webview transport.
 * The native host owns radio resources; this object only owns opaque handles,
 * cancellation, bounded stream projections, and deterministic release.
 */
export class IpcBleManager<Attachment extends string = string, Client extends string = string> {
  private readonly streams = new Map<string, StreamSink>()
  private readonly pendingStreamItems = new Map<string, SerializableRecord[]>()
  private readonly pendingStreamBytes = new Map<string, number>()
  private readonly pendingStreamOverflows = new Map<string, { droppedItems: number; droppedBytes: number }>()
  private readonly eventPump: Promise<void>
  private nextConnectionEventHandle = 1
  private lifecycle: 'active' | 'releasing' | 'released' = 'active'
  private releaseResult: Promise<CleanupRecord> | null = null

  private constructor(
    private readonly client: IpcBleClient<Attachment, Client>,
    readonly capabilities: BleCapabilities
  ) {
    this.eventPump = this.pumpEvents()
  }

  static async create<Attachment extends string, Client extends string>(
    transport: IpcClientTransport<Attachment, Client>
  ): Promise<IpcBleManager<Attachment, Client>> {
    const client = new IpcBleClient(transport)
    await client.initialize()
    return new IpcBleManager(
      client,
      createPublicBleCapabilities(
        client.bootstrap.capabilities,
        String(client.bootstrap.attachment.backendGeneration),
        true
      )
    )
  }

  get bootstrap() {
    return this.client.bootstrap
  }

  async adapterState(): Promise<SerializableRecord> {
    const payload = await this.route('adapter.state', Object.freeze({ deadline: null }))
    return requiredRecord(payload, 'state', 'ipc-manager.adapter-state')
  }

  async scan(options: IpcScanOptions = {}): Promise<IpcScanSession> {
    const manufacturerData = (options.manufacturerData ?? []).map(filter => ({
      companyId: filter.companyId,
      dataPrefix:
        filter.dataPrefix === undefined ? null : ownBytes(filter.dataPrefix, byteLimit(filter.dataPrefix.byteLength))
    }))
    const payload = await this.route(
      'scan.start',
      Object.freeze({
        serviceUuids: Object.freeze([...(options.serviceUuids ?? [])]),
        manufacturerData: Object.freeze(manufacturerData),
        localNamePrefix: options.localNamePrefix ?? null,
        deadline: operationDeadline(options),
        ...(options.stream === undefined
          ? {}
          : {
              streamItemCapacity: options.stream.itemCapacity,
              streamByteCapacity: options.stream.byteCapacity,
              streamReservedControlCapacity: options.stream.reservedControlCapacity,
              streamOverflowPolicy: options.stream.overflowPolicy
            })
      }),
      null,
      options.signal
    )
    const handle = requiredString(payload, 'handle', 'ipc-manager.scan')
    const observations = this.registerStream<IpcAdvertisement>(
      handle,
      isIpcAdvertisement,
      toRemoteStreamLimits(options.stream),
      options.stream?.overflowPolicy
    )
    return new IpcScanSession(this, handle, observations)
  }

  async connect(peerId: string, options: IpcManagerOperationOptions = {}): Promise<IpcConnection> {
    if (peerId.length === 0) throw new TypeError('peerId must not be empty')
    const payload = await this.route(
      'connection.connect',
      Object.freeze({ peerId, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    const returnedPeerId = requiredString(payload, 'peerId', 'ipc-manager.connect')
    const returnedOwnerLeaseId = requiredString(payload, 'ownerLeaseId', 'ipc-manager.connect')
    if (returnedPeerId !== peerId || returnedOwnerLeaseId !== String(this.bootstrap.rendererLease.leaseId)) {
      throw contractError('protocol.violation', 'ipc', 'ipc-manager.connect-identity')
    }
    return new IpcConnection(
      this,
      requiredString(payload, 'handle', 'ipc-manager.connect'),
      returnedPeerId,
      requiredString(payload, 'connectionId', 'ipc-manager.connect'),
      returnedOwnerLeaseId,
      requiredString(payload, 'connectionGeneration', 'ipc-manager.connect')
    )
  }

  destroy(): Promise<CleanupRecord> {
    if (this.lifecycle === 'released') return Promise.resolve({ state: 'released', failures: [] })
    if (this.releaseResult !== null) return this.releaseResult
    this.lifecycle = 'releasing'
    this.releaseResult = this.client
      .destroy()
      .then(async cleanup => {
        if (cleanup.state === 'released') {
          this.lifecycle = 'released'
          for (const sink of this.streams.values()) sink.closeWithReason('owner-released')
          this.streams.clear()
          this.pendingStreamItems.clear()
          this.pendingStreamBytes.clear()
          this.pendingStreamOverflows.clear()
          await this.eventPump
        } else {
          this.lifecycle = 'active'
          this.releaseResult = null
        }
        return cleanup
      })
      .catch(error => {
        this.lifecycle = 'active'
        this.releaseResult = null
        throw error
      })
    return this.releaseResult
  }

  async route(
    command: string,
    payload: SerializableRecord,
    binaryPayload: Uint8Array | null = null,
    signal: AbortSignal | null | undefined = null
  ): Promise<SerializableRecord> {
    this.assertActive()
    const deadline = payload.deadline
    if (deadline === null || deadline === undefined) {
      const receipt = await this.client.request({ command, payload, binaryPayload, signal: signal ?? null })
      return receipt.payload
    }
    if (typeof deadline !== 'number' || !Number.isFinite(deadline)) {
      throw new TypeError('Malformed IPC operation deadline')
    }
    if (globalThis.performance === undefined) throw new TypeError('A monotonic performance clock is required')
    const controller = new AbortController()
    const forwardAbort = () => controller.abort()
    signal?.addEventListener('abort', forwardAbort, { once: true })
    if (signal?.aborted === true) forwardAbort()
    let timedOut = false
    const timer = globalThis.setTimeout(
      () => {
        timedOut = true
        controller.abort()
      },
      Math.max(0, deadline - globalThis.performance.now())
    )
    try {
      const receipt = await this.client.request({ command, payload, binaryPayload, signal: controller.signal })
      return receipt.payload
    } catch (error) {
      if (timedOut && signal?.aborted !== true && error instanceof BackendContractError) {
        throw contractError('operation.timed-out', 'ipc', `ipc-manager.${command}`)
      }
      throw error
    } finally {
      globalThis.clearTimeout(timer)
      signal?.removeEventListener('abort', forwardAbort)
    }
  }

  registerStream<Value>(
    handle: string,
    isValue: (value: unknown) => value is Value,
    limits: StreamLimits = REMOTE_STREAM_LIMITS,
    overflowPolicy: OverflowPolicy = 'drop-oldest',
    onTerminal?: (reason: StreamTerminalNotice['reason']) => void
  ): BoundedAsyncStream<Value> {
    if (this.streams.has(handle)) throw new TypeError(`Duplicate remote stream handle: ${handle}`)
    const source = new CoreBoundedStream<Value>(limits, overflowPolicy)
    const deliver = (streamId: string, item: SerializableRecord): void => {
      if (item.kind === 'value') {
        const rawValue: unknown = item.value
        if (!isValue(rawValue)) throw contractError('protocol.malformed', 'ipc', 'ipc-manager.stream-value')
        source.emit(rawValue, estimateByteLength(rawValue))
        return
      }
      if (item.kind === 'overflow') {
        source.observeSourceOverflow({
          kind: 'overflow',
          policy: requiredOverflowPolicy(item.policy, 'ipc-manager.event'),
          droppedItems: resourceCount(Number(item.droppedItems)),
          droppedBytes: resourceCount(Number(item.droppedBytes)),
          replacedItems: resourceCount(Number(item.replacedItems))
        })
        return
      }
      if (item.kind === 'terminal') {
        const reason = requiredTerminalReason(item.reason, 'ipc-manager.event')
        source.finishWithReason(reason)
        onTerminal?.(reason)
        this.streams.delete(streamId)
        this.pendingStreamItems.delete(streamId)
        this.pendingStreamBytes.delete(streamId)
        this.pendingStreamOverflows.delete(streamId)
      }
    }
    const sink: StreamSink = {
      closeWithReason: reason => source.closeWithReason(reason),
      deliver
    }
    this.streams.set(handle, sink)
    const pending = this.pendingStreamItems.get(handle)
    const pendingOverflow = this.pendingStreamOverflows.get(handle)
    this.pendingStreamItems.delete(handle)
    this.pendingStreamBytes.delete(handle)
    this.pendingStreamOverflows.delete(handle)
    if (pendingOverflow !== undefined) {
      deliver(handle, {
        kind: 'overflow',
        policy: 'drop-oldest',
        droppedItems: pendingOverflow.droppedItems,
        droppedBytes: pendingOverflow.droppedBytes,
        replacedItems: 0
      })
    }
    if (pending !== undefined) {
      for (const item of pending) {
        deliver(handle, item)
      }
    }
    return source
  }

  subscribeConnectionEvents(
    connectionHandle: string,
    identity: SerializableRecord
  ): Promise<IpcConnectionEventSubscription> {
    return this.admitConnectionEvents(connectionHandle, identity)
  }

  private async admitConnectionEvents(
    connectionHandle: string,
    identity: SerializableRecord
  ): Promise<IpcConnectionEventSubscription> {
    const handle = `connection-events-ipc-${this.nextConnectionEventHandle++}`
    const payload = Object.freeze({
      ...identity,
      connectionHandle,
      connectionEventsHandle: handle,
      deadline: null
    })
    const response = await this.route('connection.events.subscribe', payload)
    const validation = validateConnectionEventResponse(response, handle, identity)
    if (validation !== null) {
      await this.route('connection.events.unsubscribe', Object.freeze({ connectionEventsHandle: handle })).catch(
        () => undefined
      )
      throw validation
    }
    const events = this.registerStream(handle, isIpcConnectionLifecycleEvent)
    try {
      const ready = await this.route('connection.events.ready', Object.freeze({ connectionEventsHandle: handle }))
      if (ready.state !== 'ready') {
        throw contractError('protocol.malformed', 'ipc', 'ipc-manager.connection-events-ready')
      }
    } catch (error) {
      this.closeStream(handle, 'source-failed')
      await this.route('connection.events.unsubscribe', Object.freeze({ connectionEventsHandle: handle })).catch(
        () => undefined
      )
      throw error
    }
    return {
      events,
      unsubscribe: async () => {
        const cleanup = cleanupRecord(
          await this.route('connection.events.unsubscribe', Object.freeze({ connectionEventsHandle: handle }))
        )
        if (cleanup.state === 'released') this.closeStream(handle)
        return cleanup
      }
    }
  }

  closeStream(handle: string, reason: 'owner-released' | 'source-failed' = 'owner-released'): void {
    this.pendingStreamItems.delete(handle)
    this.pendingStreamBytes.delete(handle)
    this.pendingStreamOverflows.delete(handle)
    const sink = this.streams.get(handle)
    if (sink === undefined) return
    this.streams.delete(handle)
    sink.closeWithReason(reason)
  }

  private async pumpEvents(): Promise<void> {
    for await (const event of this.client.events) {
      if (event.kind !== 'value') continue
      const eventValue = event.value
      // ElectronRendererBleClient validates the attachment lease before it
      // projects an event into this host-neutral stream; Tauri uses that same
      // client implementation over its Channel transport.
      const streamId = requiredString(eventValue, 'streamId', 'ipc-manager.event')
      const item = requiredRecord(eventValue, 'item', 'ipc-manager.event')
      const sink = this.streams.get(streamId)
      if (sink === undefined) {
        this.bufferPendingStreamItem(streamId, item)
        continue
      }
      try {
        sink.deliver(streamId, item)
      } catch {
        // A malformed stream item must not terminate the global pump. Close
        // only the affected stream so all other subscriptions keep running.
        const affected = this.streams.get(streamId)
        if (affected !== undefined) {
          affected.closeWithReason('source-failed')
          this.streams.delete(streamId)
          this.pendingStreamItems.delete(streamId)
          this.pendingStreamBytes.delete(streamId)
          this.pendingStreamOverflows.delete(streamId)
        }
      }
    }
  }

  private bufferPendingStreamItem(streamId: string, item: SerializableRecord): void {
    const pending = this.pendingStreamItems.get(streamId) ?? []
    let pendingBytes = this.pendingStreamBytes.get(streamId) ?? 0
    if (item.kind === 'terminal') {
      pending.length = 0
      pendingBytes = 0
    }
    const itemCapacity = Number(REMOTE_STREAM_LIMITS.itemCapacity)
    const byteCapacity = Number(REMOTE_STREAM_LIMITS.byteCapacity)
    const itemBytes = estimateByteLength(item)
    let droppedItems = 0
    let droppedBytes = 0
    if (itemBytes > byteCapacity && item.kind !== 'terminal') {
      pending.length = 0
      pendingBytes = 0
      droppedItems = 1
      droppedBytes = itemBytes
      pending.push({ kind: 'terminal', reason: 'overflow' })
      pendingBytes = estimateByteLength(pending[0])
      this.pendingStreamItems.set(streamId, pending)
      this.pendingStreamBytes.set(streamId, pendingBytes)
      const previous = this.pendingStreamOverflows.get(streamId)
      this.pendingStreamOverflows.set(streamId, {
        droppedItems: (previous?.droppedItems ?? 0) + droppedItems,
        droppedBytes: (previous?.droppedBytes ?? 0) + droppedBytes
      })
      return
    }
    while (pending.length >= itemCapacity || (pending.length > 0 && pendingBytes + itemBytes > byteCapacity)) {
      const removed = pending.shift()
      if (removed === undefined) break
      const removedBytes = estimateByteLength(removed)
      pendingBytes -= removedBytes
      droppedItems += 1
      droppedBytes += removedBytes
    }
    pending.push(item)
    pendingBytes += itemBytes
    this.pendingStreamItems.set(streamId, pending)
    this.pendingStreamBytes.set(streamId, pendingBytes)
    if (droppedItems > 0) {
      const previous = this.pendingStreamOverflows.get(streamId)
      this.pendingStreamOverflows.set(streamId, {
        droppedItems: (previous?.droppedItems ?? 0) + droppedItems,
        droppedBytes: (previous?.droppedBytes ?? 0) + droppedBytes
      })
    }
  }

  private assertActive(): void {
    if (this.lifecycle !== 'active') throw new TypeError('Tauri BLE manager has been released')
  }
}

export class IpcScanSession {
  constructor(
    private readonly manager: IpcBleManager,
    readonly handle: string,
    readonly observations: BoundedAsyncStream<IpcAdvertisement>
  ) {}

  async stop(): Promise<CleanupRecord> {
    const result = cleanupRecord(await this.manager.route('scan.stop', Object.freeze({ scanHandle: this.handle })))
    if (result.state === 'released') this.manager.closeStream(this.handle)
    return result
  }
}

export class IpcConnection {
  private readonly lifecycleEvents = new CoreBoundedStream<SerializableRecord>(REMOTE_STREAM_LIMITS, 'drop-oldest')
  private lifecycleAdmission: Promise<void> | null = null
  private lifecycleSubscription: IpcConnectionEventSubscription | null = null
  private readonly databases = new Set<IpcGattDatabase>()
  private readonly _connectionId: string
  private readonly _ownerLeaseId: string
  private readonly _connectionGeneration: string

  constructor(
    private readonly manager: IpcBleManager,
    readonly handle: string,
    readonly peerId: string,
    connectionId: string,
    ownerLeaseId: string,
    connectionGeneration: string
  ) {
    this._connectionId = connectionId
    this._ownerLeaseId = ownerLeaseId
    this._connectionGeneration = connectionGeneration
  }

  get connectionId(): string {
    return this._connectionId
  }

  get ownerLeaseId(): string {
    return this._ownerLeaseId
  }

  get connectionGeneration(): string {
    return this._connectionGeneration
  }

  get attachmentId(): string {
    return String(this.manager.bootstrap.attachment.attachmentId)
  }

  get events(): BoundedAsyncStream<SerializableRecord> {
    this.ensureLifecycleAdmission()
    return this.lifecycleEvents
  }

  private ensureLifecycleAdmission(): Promise<void> {
    if (this.lifecycleAdmission !== null) return this.lifecycleAdmission
    const admission = this.manager
      .subscribeConnectionEvents(this.handle, this.identityPayload())
      .then(subscription => {
        this.lifecycleSubscription = subscription
        this.pumpLifecycleEvents(subscription).catch(() => {
          this.lifecycleEvents.closeWithReason('source-failed')
        })
      })
      .catch(() => {
        this.lifecycleEvents.closeWithReason('source-failed')
        this.invalidateDatabases()
      })
    this.lifecycleAdmission = admission
    return admission
  }

  private async pumpLifecycleEvents(subscription: IpcConnectionEventSubscription): Promise<void> {
    for await (const event of subscription.events) {
      if (event.kind === 'terminal') {
        this.invalidateDatabases()
        this.lifecycleEvents.finishWithReason(requiredTerminalReason(event.reason, 'ipc-manager.connection-lifecycle'))
        return
      }
      if (event.kind === 'overflow') {
        this.lifecycleEvents.observeSourceOverflow({
          kind: 'overflow',
          policy: requiredOverflowPolicy(event.policy, 'ipc-manager.connection-lifecycle'),
          droppedItems: resourceCount(Number(event.droppedItems)),
          droppedBytes: resourceCount(Number(event.droppedBytes)),
          replacedItems: resourceCount(Number(event.replacedItems))
        })
        continue
      }
      const value = lifecycleEventValue(event)
      this.lifecycleEvents.emit(value, estimateByteLength(value))
    }
    this.lifecycleEvents.closeWithReason('source-failed')
    this.invalidateDatabases()
  }

  registerDatabase(database: IpcGattDatabase): void {
    this.databases.add(database)
  }

  private invalidateDatabases(): void {
    for (const database of this.databases) database.invalidate()
  }

  async discover(options: IpcManagerOperationOptions = {}): Promise<IpcGattDatabase> {
    await this.ensureLifecycleAdmission()
    const payload = await this.manager.route(
      'gatt.discover',
      Object.freeze({ ...this.identityPayload(), deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return IpcGattDatabase.fromPayload(this.manager, this, payload)
  }

  async readRssi(options: IpcManagerOperationOptions = {}): Promise<number> {
    const payload = await this.manager.route(
      'connection.rssi',
      Object.freeze({ ...this.identityPayload(), deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return requiredNumber(payload, 'rssi', 'ipc-manager.connection-rssi')
  }

  async maximumWriteLength(mode: 'with-response' | 'without-response' = 'with-response'): Promise<number> {
    const payload = await this.manager.route(
      'connection.maximum-write-length',
      Object.freeze({ ...this.identityPayload(), mode })
    )
    return requiredNumber(payload, 'bytes', 'ipc-manager.maximum-write-length')
  }

  async disconnect(): Promise<CleanupRecord> {
    this.invalidateDatabases()
    if (this.lifecycleAdmission !== null) {
      await this.lifecycleAdmission
    }
    if (this.lifecycleSubscription !== null) {
      await this.lifecycleSubscription.unsubscribe()
      this.lifecycleSubscription = null
    }
    return cleanupRecord(await this.manager.route('connection.disconnect', Object.freeze(this.identityPayload())))
  }

  release(): Promise<CleanupRecord> {
    return this.disconnect()
  }

  private identityPayload(): SerializableRecord {
    return Object.freeze({
      connectionHandle: this.handle,
      peerId: this.peerId,
      connectionId: this.connectionId,
      ownerLeaseId: this.ownerLeaseId,
      connectionGeneration: this.connectionGeneration
    })
  }
}

export class IpcGattDatabase {
  readonly characteristics: readonly IpcCharacteristic[]
  readonly descriptors: readonly IpcDescriptor[]
  private valid = true
  private readonly changedStream = new CoreBoundedStream<GattDatabaseChangedEvent>(REMOTE_STREAM_LIMITS, 'drop-oldest')

  private constructor(
    private readonly manager: IpcBleManager,
    readonly connection: IpcConnection,
    readonly handle: string,
    readonly databaseId: string,
    readonly databaseGeneration: string,
    readonly serviceRecords: readonly IpcServiceRecord[],
    characteristicRecords: readonly IpcCharacteristicRecord[],
    descriptorRecords: readonly IpcDescriptorRecord[]
  ) {
    this.characteristics = Object.freeze(characteristicRecords.map(record => new IpcCharacteristic(this, record)))
    this.descriptors = Object.freeze(descriptorRecords.map(record => new IpcDescriptor(this, record)))
  }

  static fromPayload(manager: IpcBleManager, connection: IpcConnection, payload: SerializableRecord): IpcGattDatabase {
    if (
      requiredNumber(payload, 'schemaVersion', 'ipc-manager.gatt-database-schema') !== IPC_GATT_DATABASE_SCHEMA_VERSION
    ) {
      throw contractError('protocol.incompatible', 'ipc', 'ipc-manager.gatt-database-schema')
    }
    const characteristics = requiredCharacteristicRecords(
      requiredRecordArray(payload, 'characteristics', 'ipc-manager.gatt-database')
    )
    const descriptors = requiredDescriptorRecords(
      requiredRecordArray(payload, 'descriptors', 'ipc-manager.gatt-database')
    )
    const services = requiredServiceRecords(requiredRecordArray(payload, 'services', 'ipc-manager.gatt-database'))
    const database = new IpcGattDatabase(
      manager,
      connection,
      requiredString(payload, 'handle', 'ipc-manager.gatt-database'),
      requiredString(payload, 'databaseId', 'ipc-manager.gatt-database'),
      requiredString(payload, 'databaseGeneration', 'ipc-manager.gatt-database'),
      services,
      characteristics,
      descriptors
    )
    connection.registerDatabase(database)
    return database
  }

  route(
    command: string,
    payload: SerializableRecord,
    binaryPayload: Uint8Array | null,
    signal?: AbortSignal
  ): Promise<SerializableRecord> {
    this.assertCurrent()
    return this.manager
      .route(
        command,
        Object.freeze({
          ...payload,
          databaseHandle: this.handle,
          databaseId: this.databaseId,
          databaseGeneration: this.databaseGeneration,
          ...this.connectionIdentityPayload()
        }),
        binaryPayload,
        signal
      )
      .catch(error => {
        if (error instanceof BackendContractError && error.normalized.code === 'gatt.stale-handle') {
          this.invalidate('service-changed')
        }
        throw error
      })
  }

  assertCurrent(): void {
    if (!this.valid) throw contractError('gatt.stale-handle', 'gatt', 'ipc-manager.gatt-database-current')
  }

  get changed(): BoundedAsyncStream<GattDatabaseChangedEvent> {
    return this.changedStream
  }

  invalidate(reason: 'service-changed' | null = null): void {
    if (!this.valid) return
    this.valid = false
    if (reason === 'service-changed') {
      this.changedStream.emit(
        Object.freeze({
          previousGeneration: this.databaseGeneration,
          reason,
          affectedHandleRange: null
        }),
        128
      )
      this.changedStream.finishWithReason('closed')
      return
    }
    this.changedStream.closeWithReason('connection-lost')
  }

  registerStream<Value>(
    handle: string,
    isValue: (value: unknown) => value is Value,
    limits?: StreamLimits,
    overflowPolicy?: OverflowPolicy,
    onTerminal?: (reason: StreamTerminalNotice['reason']) => void
  ): BoundedAsyncStream<Value> {
    return this.manager.registerStream<Value>(handle, isValue, limits, overflowPolicy, onTerminal)
  }

  closeStream(handle: string): void {
    this.manager.closeStream(handle)
  }

  private connectionIdentityPayload(): SerializableRecord {
    return Object.freeze({
      connectionHandle: this.connection.handle,
      peerId: this.connection.peerId,
      connectionId: this.connection.connectionId,
      ownerLeaseId: this.connection.ownerLeaseId,
      connectionGeneration: this.connection.connectionGeneration,
      attachmentId: this.path.attachmentId
    })
  }

  get path(): PortableDatabasePath {
    return ipcDatabasePath(this.manager.bootstrap.attachment, this.connection, this.databaseId, this.databaseGeneration)
  }

  monotonicNow(): number {
    if (globalThis.performance === undefined) {
      throw new TypeError('A monotonic performance clock is required')
    }
    return globalThis.performance.now()
  }

  scheduleDeadline(deadline: number, action: () => void): { cancel: () => void } {
    const delay = Math.max(0, deadline - this.monotonicNow())
    const timer = globalThis.setTimeout(action, delay)
    return {
      cancel: () => {
        globalThis.clearTimeout(timer)
      }
    }
  }

  async snapshot(): Promise<PortableGattDatabaseSnapshot> {
    const databasePath = this.path
    const services = this.serviceRecords.map(service => ({
      path: Object.freeze({
        ...databasePath,
        serviceUuid: service.uuid,
        serviceOccurrence: service.occurrence
      }),
      primary: service.primary,
      includedServices: Object.freeze(service.includedServices.map(reference => Object.freeze({ ...reference })))
    }))

    return Object.freeze({
      path: databasePath,
      services: Object.freeze(services),
      characteristics: Object.freeze(
        this.characteristics.map(characteristic =>
          Object.freeze({
            path: toCharacteristicPath(databasePath, characteristic.record),
            properties: createGattCharacteristicProperties({
              broadcast: characteristic.record.properties.includes('broadcast'),
              read: characteristic.record.properties.includes('read'),
              writeWithResponse: characteristic.record.properties.includes('write'),
              writeWithoutResponse: characteristic.record.properties.includes('write-without-response'),
              authenticatedSignedWrites: characteristic.record.properties.includes('authenticated-signed-writes'),
              notify: characteristic.record.properties.includes('notify'),
              indicate: characteristic.record.properties.includes('indicate'),
              extendedProperties: characteristic.record.properties.includes('extended-properties'),
              reliableWrite: characteristic.record.properties.includes('reliable-write'),
              writableAuxiliaries: characteristic.record.properties.includes('writable-auxiliaries')
            }),
            access: Object.freeze({ read: 'unknown', write: 'unknown' })
          })
        )
      ),
      descriptors: Object.freeze(
        this.descriptors.map(descriptor =>
          Object.freeze({
            path: Object.freeze({
              ...toCharacteristicPath(
                databasePath,
                characteristicRecordForDescriptor(this.characteristics, descriptor.record)
              ),
              descriptorUuid: descriptor.record.uuid,
              descriptorOccurrence: String(descriptor.record.occurrence)
            }),
            properties: Object.freeze({
              read: false,
              write: false,
              availability: Object.freeze({ read: 'unknown', write: 'unknown' }),
              access: Object.freeze({ read: 'unknown', write: 'unknown' })
            })
          })
        )
      )
    })
  }

  async read(
    path: PortableCurrentCharacteristicPath,
    options: PortableOperationOptions = EMPTY_OPERATION_OPTIONS
  ): Promise<Uint8Array> {
    return this.characteristicForPath(path).read(toIpcOptions(options))
  }

  async write(
    path: PortableCurrentCharacteristicPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<IpcWriteReceipt> {
    const mode = options.mode
    const payload = await this.characteristicForPath(path).write(bytes, {
      ...toIpcOptions(options),
      mode
    })
    return requiredWriteReceipt(payload, mode, bytes.byteLength)
  }

  async subscribe(
    path: PortableCurrentCharacteristicPath,
    options: PortableSubscriptionOptions = EMPTY_SUBSCRIPTION_OPTIONS
  ): Promise<IpcSubscription> {
    const subscription = await this.characteristicForPath(path).subscribe(toIpcOptions(options))
    subscription.path = path
    return subscription
  }

  async readDescriptor(
    path: PortableCurrentDescriptorPath,
    options: PortableOperationOptions = EMPTY_OPERATION_OPTIONS
  ): Promise<Uint8Array> {
    return this.descriptorForPath(path).read(toIpcOptions(options))
  }

  async writeDescriptor(
    path: PortableCurrentDescriptorPath,
    bytes: Readonly<Uint8Array>,
    options: PortableWritePolicy
  ): Promise<IpcWriteReceipt> {
    return this.descriptorForPath(path).write(bytes, {
      ...toIpcOptions(options),
      mode: options.mode
    })
  }

  private characteristicForPath(path: PortableCurrentCharacteristicPath): IpcCharacteristic {
    if (!databasePathMatches(path, this.path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'ipc-manager.gatt-path-identity')
    }
    const matches = this.characteristics.filter(
      characteristic =>
        characteristic.record.serviceUuid === path.serviceUuid &&
        characteristic.record.characteristicUuid === path.characteristicUuid &&
        String(characteristic.record.serviceOccurrence) === String(path.serviceOccurrence) &&
        String(characteristic.record.characteristicOccurrence) === String(path.characteristicOccurrence)
    )
    if (matches.length !== 1) {
      throw new TypeError(
        `Expected exactly one IPC characteristic ${path.characteristicUuid} in ${path.serviceUuid}; found ${matches.length}`
      )
    }
    const match = matches[0]
    if (match === undefined) {
      throw new TypeError(`IPC characteristic ${path.characteristicUuid} was missing after match`)
    }
    return match
  }

  private descriptorForPath(path: PortableCurrentDescriptorPath): IpcDescriptor {
    if (!databasePathMatches(path, this.path)) {
      throw contractError('gatt.stale-handle', 'gatt', 'ipc-manager.gatt-descriptor-path-identity')
    }
    const characteristic = this.characteristics.find(
      candidate =>
        candidate.record.serviceUuid === path.serviceUuid &&
        String(candidate.record.serviceOccurrence) === String(path.serviceOccurrence) &&
        candidate.record.characteristicUuid === path.characteristicUuid &&
        String(candidate.record.characteristicOccurrence) === String(path.characteristicOccurrence)
    )
    if (characteristic === undefined) {
      throw contractError('gatt.not-found', 'gatt', 'ipc-manager.gatt-descriptor-characteristic')
    }
    const descriptor = this.descriptors.find(
      candidate =>
        candidate.record.characteristicHandle === characteristic.handle &&
        candidate.record.uuid === path.descriptorUuid &&
        String(candidate.record.occurrence) === String(path.descriptorOccurrence)
    )
    if (descriptor === undefined) {
      throw contractError('gatt.not-found', 'gatt', 'ipc-manager.gatt-descriptor')
    }
    return descriptor
  }
}

function databasePathMatches(path: PortableCurrentCharacteristicPath, expected: PortableDatabasePath): boolean {
  return (
    path.validity === 'current' &&
    path.attachmentId === expected.attachmentId &&
    path.peerId === expected.peerId &&
    path.connectionId === expected.connectionId &&
    path.ownerLeaseId === expected.ownerLeaseId &&
    path.connectionGeneration === expected.connectionGeneration &&
    path.databaseId === expected.databaseId &&
    path.databaseGeneration === expected.databaseGeneration &&
    path.attachment.attachmentId === expected.attachment.attachmentId &&
    path.attachment.backendInstanceId === expected.attachment.backendInstanceId &&
    path.attachment.backendGeneration === expected.attachment.backendGeneration &&
    path.attachment.adapter.adapterId === expected.attachment.adapter.adapterId &&
    path.attachment.adapter.adapterGeneration === expected.attachment.adapter.adapterGeneration
  )
}

export class IpcCharacteristic {
  constructor(
    private readonly database: IpcGattDatabase,
    readonly record: IpcCharacteristicRecord
  ) {}

  get handle(): string {
    return this.record.handle
  }

  async read(options: IpcManagerOperationOptions = {}): Promise<Uint8Array> {
    const payload = await this.database.route(
      'gatt.read',
      Object.freeze({ characteristicHandle: this.handle, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return requiredBytes(payload, 'value', 'ipc-manager.gatt-read')
  }

  async write(bytes: Readonly<Uint8Array>, options: IpcWriteOptions = {}): Promise<SerializableRecord> {
    const owned = ownBytes(bytes, byteLimit(bytes.byteLength))
    return this.database.route(
      'gatt.write',
      Object.freeze({
        characteristicHandle: this.handle,
        mode: options.mode ?? 'with-response',
        deadline: operationDeadline(options)
      }),
      owned,
      options.signal
    )
  }

  async subscribe(options: IpcManagerOperationOptions = {}): Promise<IpcSubscription> {
    const payload = await this.database.route(
      'gatt.subscribe',
      Object.freeze({
        characteristicHandle: this.handle,
        deadline: operationDeadline(options),
        ...(options.deliveryMode === undefined ? {} : { deliveryMode: options.deliveryMode }),
        ...(options.stream === undefined
          ? {}
          : {
              streamItemCapacity: options.stream.itemCapacity,
              streamByteCapacity: options.stream.byteCapacity,
              streamReservedControlCapacity: options.stream.reservedControlCapacity,
              streamOverflowPolicy: options.stream.overflowPolicy
            })
      }),
      null,
      options.signal
    )
    const handle = requiredString(payload, 'handle', 'ipc-manager.gatt-subscribe')
    return new IpcSubscription(
      this.database,
      handle,
      this.database.registerStream<IpcNotificationValue>(
        handle,
        isIpcNotificationValue,
        toRemoteStreamLimits(options.stream),
        options.stream?.overflowPolicy,
        reason => {
          if (reason === 'service-changed') this.database.invalidate('service-changed')
        }
      )
    )
  }
}

export class IpcDescriptor {
  constructor(
    private readonly database: IpcGattDatabase,
    readonly record: IpcDescriptorRecord
  ) {}

  async read(options: IpcManagerOperationOptions = {}): Promise<Uint8Array> {
    const payload = await this.database.route(
      'gatt.descriptor.read',
      Object.freeze({ descriptorHandle: this.record.handle, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return requiredBytes(payload, 'value', 'ipc-manager.descriptor-read')
  }

  async write(bytes: Readonly<Uint8Array>, options: IpcWriteOptions = {}): Promise<IpcWriteReceipt> {
    const mode = options.mode ?? 'with-response'
    if (mode !== 'with-response') {
      throw contractError('argument.invalid', 'gatt', 'ipc-manager.gatt-descriptor-write-mode')
    }
    const owned = ownBytes(bytes, byteLimit(bytes.byteLength))
    const payload = await this.database.route(
      'gatt.descriptor.write',
      Object.freeze({ descriptorHandle: this.record.handle, mode, deadline: operationDeadline(options) }),
      owned,
      options.signal
    )
    return requiredWriteReceipt(payload, mode, owned.byteLength)
  }
}

export class IpcSubscription {
  path: PortableCurrentCharacteristicPath | null = null

  constructor(
    private readonly database: IpcGattDatabase,
    readonly handle: string,
    readonly values: BoundedAsyncStream<IpcNotificationValue>
  ) {}

  get subscriptionId(): string {
    return this.handle
  }

  async remove(): Promise<CleanupRecord> {
    const cleanup = cleanupRecord(
      await this.database.route('gatt.unsubscribe', Object.freeze({ subscriptionHandle: this.handle }), null)
    )
    if (cleanup.state === 'released') this.database.closeStream(this.handle)
    return cleanup
  }
}

const EMPTY_OPERATION_OPTIONS: PortableOperationOptions = Object.freeze({ signal: null, deadline: null })
const EMPTY_SUBSCRIPTION_OPTIONS: PortableSubscriptionOptions = Object.freeze({
  ...EMPTY_OPERATION_OPTIONS,
  delivery: {
    itemCapacity: capacity(128),
    byteCapacity: capacity(512 * 1024),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest' as const
  }
})

function ipcDatabasePath(
  attachment: AttachmentRecord<string>,
  connection: IpcConnection,
  databaseId: string,
  databaseGeneration: string
): PortableDatabasePath {
  return Object.freeze({
    attachment,
    attachmentId: attachment.attachmentId,
    peerId: connection.peerId,
    connectionId: connection.connectionId,
    ownerLeaseId: connection.ownerLeaseId,
    connectionGeneration: connection.connectionGeneration,
    databaseId,
    databaseGeneration
  })
}

function toIpcOptions(options: PortableOperationOptions | PortableSubscriptionOptions): IpcManagerOperationOptions {
  const timeoutMs =
    typeof options.deadline === 'number' && globalThis.performance !== undefined
      ? Math.max(1, options.deadline - globalThis.performance.now())
      : undefined
  const subscriptionOptions = isPortableSubscriptionOptions(options) ? options : null
  const delivery = subscriptionOptions?.delivery
  return {
    signal: options.signal ?? undefined,
    timeoutMs,
    deliveryMode: subscriptionOptions?.deliveryMode,
    stream:
      delivery === undefined
        ? undefined
        : {
            itemCapacity: Number(delivery.itemCapacity),
            byteCapacity: Number(delivery.byteCapacity),
            reservedControlCapacity: Number(delivery.reservedControlCapacity),
            overflowPolicy: delivery.overflowPolicy
          }
  }
}

function isPortableSubscriptionOptions(
  options: PortableOperationOptions | PortableSubscriptionOptions
): options is PortableSubscriptionOptions {
  return 'delivery' in options && options.delivery !== null && typeof options.delivery === 'object'
}

function toRemoteStreamLimits(stream: IpcManagerOperationOptions['stream']): StreamLimits {
  if (stream === undefined) return REMOTE_STREAM_LIMITS
  return {
    itemCapacity: capacity(stream.itemCapacity),
    byteCapacity: capacity(stream.byteCapacity),
    reservedControlCapacity: capacity(stream.reservedControlCapacity)
  }
}

function toCharacteristicPath(
  databasePath: PortableDatabasePath,
  record: IpcCharacteristicRecord
): PortableCurrentCharacteristicPath {
  return Object.freeze({
    ...databasePath,
    serviceUuid: record.serviceUuid,
    serviceOccurrence: String(record.serviceOccurrence),
    characteristicUuid: record.characteristicUuid,
    characteristicOccurrence: String(record.characteristicOccurrence),
    validity: 'current'
  })
}

function characteristicRecordForDescriptor(
  characteristics: readonly IpcCharacteristic[],
  descriptor: IpcDescriptorRecord
): IpcCharacteristicRecord {
  const match = characteristics.find(characteristic => characteristic.record.handle === descriptor.characteristicHandle)
  if (match === undefined) {
    throw new TypeError(`IPC descriptor ${descriptor.uuid} has no matching characteristic`)
  }
  return match.record
}

function operationDeadline(options: IpcManagerOperationOptions): number | null {
  if (options.timeoutMs === undefined) return null
  if (!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0) throw new TypeError('timeoutMs must be positive')
  if (globalThis.performance === undefined) throw new TypeError('A monotonic performance clock is required')
  return globalThis.performance.now() + options.timeoutMs
}

function isCleanupRecord(value: unknown): value is CleanupRecord {
  if (typeof value !== 'object' || value === null) return false
  if (!('state' in value) || !('failures' in value)) return false
  const state = Reflect.get(value, 'state')
  const failures = Reflect.get(value, 'failures')
  if (state !== 'released' && state !== 'release-failed') return false
  if (!Array.isArray(failures)) return false
  if (
    !failures.every(entry => {
      if (typeof entry !== 'object' || entry === null) return false
      if (!('resourceKind' in entry) || !('error' in entry)) return false
      const resourceKind = Reflect.get(entry, 'resourceKind')
      const error = Reflect.get(entry, 'error')
      return typeof resourceKind === 'string' && typeof error === 'object' && error !== null
    })
  )
    return false
  return (state === 'released') === (failures.length === 0)
}

function cleanupRecord(value: SerializableRecord): CleanupRecord {
  if (!isCleanupRecord(value)) {
    throw contractError('protocol.malformed', 'ipc', 'ipc-manager.cleanup-record')
  }
  return value
}

function requiredOverflowPolicy(value: SerializableValue | undefined, operation: string): OverflowPolicy {
  if (value === 'latest' || value === 'drop-oldest' || value === 'drop-newest' || value === 'error') {
    return value
  }
  throw contractError('protocol.malformed', 'ipc', operation)
}

function requiredTerminalReason(
  value: SerializableValue | undefined,
  operation: string
): StreamTerminalNotice['reason'] {
  if (
    value === 'closed' ||
    value === 'overflow' ||
    value === 'source-failed' ||
    value === 'owner-released' ||
    value === 'connection-lost' ||
    value === 'service-changed' ||
    value === 'operation-aborted' ||
    value === 'operation-timed-out'
  ) {
    return value
  }
  throw contractError('protocol.malformed', 'ipc', operation)
}

function isSerializableRecord(value: unknown): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function requiredRecord(record: SerializableRecord, key: string, operation: string): SerializableRecord {
  const value: unknown = record[key]
  if (!isSerializableRecord(value)) {
    throw contractError('protocol.malformed', 'ipc', operation)
  }
  return value
}

function isSerializableRecordArray(value: unknown): value is readonly SerializableRecord[] {
  return (
    Array.isArray(value) &&
    value.every(
      item => typeof item === 'object' && item !== null && !Array.isArray(item) && !(item instanceof Uint8Array)
    )
  )
}

function requiredRecordArray(
  record: SerializableRecord,
  key: string,
  operation: string
): readonly SerializableRecord[] {
  const value: unknown = record[key]
  if (!isSerializableRecordArray(value)) {
    throw contractError('protocol.malformed', 'ipc', operation)
  }
  return value
}

function requiredString(record: SerializableRecord, key: string, operation: string): string {
  const value = record[key]
  if (typeof value !== 'string' || value.length === 0) throw contractError('protocol.malformed', 'ipc', operation)
  return value
}

function requiredNumber(record: SerializableRecord, key: string, operation: string): number {
  const value = record[key]
  if (typeof value !== 'number' || !Number.isFinite(value)) throw contractError('protocol.malformed', 'ipc', operation)
  return value
}

function requiredBytes(record: SerializableRecord, key: string, operation: string): Uint8Array {
  const value = record[key]
  if (!(value instanceof Uint8Array)) throw contractError('protocol.malformed', 'ipc', operation)
  return new Uint8Array(value)
}

function requiredWriteReceipt(
  record: SerializableRecord,
  requestedMode: 'with-response' | 'without-response' | undefined,
  bytesSubmitted: number
): IpcWriteReceipt {
  const terminal = requiredRecord(record, 'terminal', 'ipc-manager.gatt-write')
  const mode = requiredString(record, 'mode', 'ipc-manager.gatt-write')
  const commitState = requiredString(record, 'commitState', 'ipc-manager.gatt-write')
  const submitted = requiredNumber(record, 'bytesSubmitted', 'ipc-manager.gatt-write')
  const correlation = requiredString(terminal, 'correlation', 'ipc-manager.gatt-write-terminal')
  const outcome = terminal.outcome
  const cause = terminal.cause
  const successful = outcome === 'succeeded'
  const expectedCommitState = mode === 'with-response' ? 'confirmed' : 'accepted'
  const knownCause = cause === null || (typeof cause === 'string' && BLE_ERROR_CODES.some(code => code === cause))
  if (
    (!successful && outcome !== 'failed') ||
    !knownCause ||
    (successful ? cause !== null || commitState !== expectedCommitState : cause === null) ||
    (!successful && commitState !== 'unknown' && commitState !== 'not-started') ||
    (mode !== 'with-response' && mode !== 'without-response') ||
    (commitState !== 'confirmed' &&
      commitState !== 'accepted' &&
      commitState !== 'unknown' &&
      commitState !== 'not-started') ||
    (requestedMode !== undefined && mode !== requestedMode) ||
    !Number.isSafeInteger(submitted) ||
    submitted < 0 ||
    submitted !== bytesSubmitted
  ) {
    throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-write-receipt')
  }
  return Object.freeze({
    terminal: Object.freeze({ correlation, outcome, cause }),
    mode,
    commitState,
    bytesSubmitted: submitted
  })
}

function estimateByteLength(value: unknown): number {
  if (value instanceof Uint8Array) return value.byteLength
  return new TextEncoder().encode(JSON.stringify(value)).byteLength
}

function lifecycleEventValue(value: unknown): SerializableRecord {
  if (isSerializableRecord(value) && value.kind === 'value') {
    return requiredRecord(value, 'value', 'ipc-manager.connection-lifecycle-value')
  }
  if (!isSerializableRecord(value)) {
    throw contractError('protocol.malformed', 'ipc', 'ipc-manager.connection-lifecycle')
  }
  return value
}

function validateConnectionEventResponse(
  response: SerializableRecord,
  handle: string,
  identity: SerializableRecord
): BackendContractError | null {
  try {
    if (
      requiredString(response, 'handle', 'ipc-manager.connection-events-subscribe') !== handle ||
      requiredString(response, 'connectionId', 'ipc-manager.connection-events-subscribe') !==
        requiredString(identity, 'connectionId', 'ipc-manager.connection-events-identity') ||
      requiredString(response, 'connectionGeneration', 'ipc-manager.connection-events-subscribe') !==
        requiredString(identity, 'connectionGeneration', 'ipc-manager.connection-events-identity') ||
      response.eventSchemaVersion !== 2
    ) {
      return contractError('protocol.incompatible', 'ipc', 'ipc-manager.connection-events-schema')
    }
    return null
  } catch (error) {
    if (error instanceof BackendContractError) return error
    return contractError('protocol.malformed', 'ipc', 'ipc-manager.connection-events-response')
  }
}

function isIpcNotificationValue(value: unknown): value is IpcNotificationValue {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (!('value' in value) || !('delivery' in value) || !('observedAtMonotonicMs' in value) || !('sequence' in value)) {
    return false
  }
  const rawValue: unknown = Reflect.get(value, 'value')
  const delivery: unknown = Reflect.get(value, 'delivery')
  const observedAtMonotonicMs: unknown = Reflect.get(value, 'observedAtMonotonicMs')
  const sequence: unknown = Reflect.get(value, 'sequence')
  return (
    rawValue instanceof Uint8Array &&
    (delivery === 'notification' || delivery === 'indication' || delivery === 'unknown') &&
    typeof observedAtMonotonicMs === 'number' &&
    Number.isFinite(observedAtMonotonicMs) &&
    observedAtMonotonicMs >= 0 &&
    typeof sequence === 'number' &&
    Number.isSafeInteger(sequence) &&
    sequence > 0
  )
}

function isIpcConnectionLifecycleEvent(value: unknown): value is SerializableRecord {
  if (!isSerializableRecord(value)) return false
  const record = value
  return (
    record.kind === 'connection-lifecycle' &&
    record.schemaVersion === 2 &&
    typeof record.attachmentId === 'string' &&
    record.attachmentId.length > 0 &&
    typeof record.peerId === 'string' &&
    record.peerId.length > 0 &&
    typeof record.connectionId === 'string' &&
    record.connectionId.length > 0 &&
    typeof record.connectionGeneration === 'string' &&
    record.connectionGeneration.length > 0 &&
    typeof record.ownerLeaseId === 'string' &&
    record.ownerLeaseId.length > 0 &&
    typeof record.sequence === 'number' &&
    Number.isSafeInteger(record.sequence) &&
    record.sequence > 0 &&
    isConnectionLifecycleState(record.previous) &&
    isConnectionLifecycleState(record.current) &&
    isConnectionLifecycleCause(record.cause)
  )
}

function isConnectionLifecycleState(value: unknown): boolean {
  return (
    value === 'connecting' ||
    value === 'connected' ||
    value === 'disconnecting' ||
    value === 'disconnected' ||
    value === 'lost'
  )
}

function isConnectionLifecycleCause(value: unknown): boolean {
  return (
    value === 'connected' ||
    value === 'backend-transition' ||
    value === 'requested-disconnect' ||
    value === 'peer-link-loss' ||
    value === 'adapter-loss' ||
    value === 'backend-restart' ||
    value === 'released' ||
    value === 'manager-destroyed' ||
    value === 'backend-failure'
  )
}

function isIpcManufacturerData(value: unknown): value is IpcManufacturerData {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (!('companyId' in value) || !('data' in value)) return false
  const companyId: unknown = Reflect.get(value, 'companyId')
  const data: unknown = Reflect.get(value, 'data')
  return typeof companyId === 'number' && Number.isInteger(companyId) && data instanceof Uint8Array
}

function isIpcServiceData(value: unknown): value is IpcServiceData {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (!('uuid' in value) || !('data' in value)) return false
  const uuid: unknown = Reflect.get(value, 'uuid')
  const data: unknown = Reflect.get(value, 'data')
  return typeof uuid === 'string' && uuid.length > 0 && data instanceof Uint8Array
}

function isIpcAdvertisement(value: unknown): value is IpcAdvertisement {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (
    !('peerId' in value) ||
    !('localName' in value) ||
    !('rssi' in value) ||
    !('txPowerLevel' in value) ||
    !('serviceUuids' in value) ||
    !('manufacturerData' in value) ||
    !('serviceData' in value)
  ) {
    return false
  }
  const peerId: unknown = Reflect.get(value, 'peerId')
  const localName: unknown = Reflect.get(value, 'localName')
  const rssi: unknown = Reflect.get(value, 'rssi')
  const txPowerLevel: unknown = Reflect.get(value, 'txPowerLevel')
  const serviceUuids: unknown = Reflect.get(value, 'serviceUuids')
  const manufacturerData: unknown = Reflect.get(value, 'manufacturerData')
  const serviceData: unknown = Reflect.get(value, 'serviceData')
  if (typeof peerId !== 'string' || peerId.length === 0) return false
  if (!(typeof localName === 'string' || localName === null)) return false
  if (!(typeof rssi === 'number' || rssi === null)) return false
  if (!(typeof txPowerLevel === 'number' || txPowerLevel === null)) return false
  if (!Array.isArray(serviceUuids) || !serviceUuids.every(entry => typeof entry === 'string')) return false
  if (!Array.isArray(manufacturerData) || !manufacturerData.every(entry => isIpcManufacturerData(entry))) {
    return false
  }
  if (!Array.isArray(serviceData) || !serviceData.every(entry => isIpcServiceData(entry))) return false
  return true
}

function isIpcCharacteristicRecord(value: unknown): value is IpcCharacteristicRecord {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (
    !('handle' in value) ||
    !('serviceUuid' in value) ||
    !('serviceOccurrence' in value) ||
    !('characteristicUuid' in value) ||
    !('characteristicOccurrence' in value) ||
    !('properties' in value)
  ) {
    return false
  }
  const handle: unknown = Reflect.get(value, 'handle')
  const serviceUuid: unknown = Reflect.get(value, 'serviceUuid')
  const serviceOccurrence: unknown = Reflect.get(value, 'serviceOccurrence')
  const characteristicUuid: unknown = Reflect.get(value, 'characteristicUuid')
  const characteristicOccurrence: unknown = Reflect.get(value, 'characteristicOccurrence')
  const properties: unknown = Reflect.get(value, 'properties')
  return (
    typeof handle === 'string' &&
    handle.length > 0 &&
    typeof serviceUuid === 'string' &&
    serviceUuid.length > 0 &&
    typeof serviceOccurrence === 'string' &&
    serviceOccurrence.length > 0 &&
    typeof characteristicUuid === 'string' &&
    characteristicUuid.length > 0 &&
    typeof characteristicOccurrence === 'string' &&
    characteristicOccurrence.length > 0 &&
    Array.isArray(properties) &&
    properties.every(entry => typeof entry === 'string')
  )
}

function isIpcServiceRecord(value: unknown): value is IpcServiceRecord {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const uuid: unknown = Reflect.get(value, 'uuid')
  const occurrence: unknown = Reflect.get(value, 'occurrence')
  const primary: unknown = Reflect.get(value, 'primary')
  const includedServices: unknown = Reflect.get(value, 'includedServices')
  return (
    typeof uuid === 'string' &&
    uuid.length > 0 &&
    typeof occurrence === 'string' &&
    occurrence.length > 0 &&
    typeof primary === 'boolean' &&
    Array.isArray(includedServices) &&
    includedServices.every(reference => {
      if (typeof reference !== 'object' || reference === null || Array.isArray(reference)) return false
      const referenceUuid: unknown = Reflect.get(reference, 'uuid')
      const referenceOccurrence: unknown = Reflect.get(reference, 'occurrence')
      return (
        typeof referenceUuid === 'string' &&
        referenceUuid.length > 0 &&
        typeof referenceOccurrence === 'string' &&
        referenceOccurrence.length > 0
      )
    })
  )
}

function requiredServiceRecords(records: readonly SerializableRecord[]): readonly IpcServiceRecord[] {
  const services: IpcServiceRecord[] = []
  for (const record of records) {
    if (!isIpcServiceRecord(record)) {
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-service-record')
    }
    const includedServicesValue = Reflect.get(record, 'includedServices')
    if (!Array.isArray(includedServicesValue)) {
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-service-included-services')
    }
    const includedServices = includedServicesValue.map(reference => {
      if (typeof reference !== 'object' || reference === null || Array.isArray(reference)) {
        throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-service-reference')
      }
      const uuid = Reflect.get(reference, 'uuid')
      const occurrence = Reflect.get(reference, 'occurrence')
      if (typeof uuid !== 'string' || typeof occurrence !== 'string') {
        throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-service-reference')
      }
      return Object.freeze({ uuid, occurrence })
    })
    services.push(
      Object.freeze({
        uuid: record.uuid,
        occurrence: record.occurrence,
        primary: record.primary,
        includedServices: Object.freeze(includedServices)
      })
    )
  }
  return Object.freeze(services)
}

function isIpcDescriptorRecord(value: unknown): value is IpcDescriptorRecord {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  if (!('handle' in value) || !('characteristicHandle' in value) || !('uuid' in value) || !('occurrence' in value)) {
    return false
  }
  const handle: unknown = Reflect.get(value, 'handle')
  const characteristicHandle: unknown = Reflect.get(value, 'characteristicHandle')
  const uuid: unknown = Reflect.get(value, 'uuid')
  const occurrence: unknown = Reflect.get(value, 'occurrence')
  return (
    typeof handle === 'string' &&
    handle.length > 0 &&
    typeof characteristicHandle === 'string' &&
    characteristicHandle.length > 0 &&
    typeof uuid === 'string' &&
    uuid.length > 0 &&
    typeof occurrence === 'string' &&
    occurrence.length > 0
  )
}

function requiredCharacteristicRecords(records: readonly SerializableRecord[]): readonly IpcCharacteristicRecord[] {
  const validated: IpcCharacteristicRecord[] = []
  for (const record of records) {
    if (!isIpcCharacteristicRecord(record))
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-database.characteristic-record')
    validated.push(record)
  }
  return Object.freeze(validated)
}

function requiredDescriptorRecords(records: readonly SerializableRecord[]): readonly IpcDescriptorRecord[] {
  const validated: IpcDescriptorRecord[] = []
  for (const record of records) {
    if (!isIpcDescriptorRecord(record)) {
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.gatt-database.descriptor-record')
    }
    validated.push(record)
  }
  return Object.freeze(validated)
}
