import type { CleanupRecord } from '../backend-contract/errors'
import type { BoundedAsyncStream, StreamItem } from '../backend-contract/streams'
import { byteLimit, capacity, ownBytes, resourceCount, type SerializableRecord } from '../backend-contract/primitives'
import { CoreBoundedStream } from '../core/bounded-stream'
import { IpcBleClient } from './client'
import type { IpcClientTransport } from './protocol'

const REMOTE_STREAM_LIMITS = Object.freeze({
  itemCapacity: capacity(128),
  byteCapacity: capacity(512 * 1024),
  reservedControlCapacity: capacity(1)
})

export interface IpcManagerOperationOptions {
  readonly signal?: AbortSignal
  readonly timeoutMs?: number
}

export interface IpcScanOptions extends IpcManagerOperationOptions {
  readonly serviceUuids?: readonly string[]
  readonly manufacturerData?: readonly { readonly companyId: number; readonly dataPrefix?: Readonly<Uint8Array> }[]
  readonly localNamePrefix?: string | null
}

export interface IpcWriteOptions extends IpcManagerOperationOptions {
  readonly mode?: 'with-response' | 'without-response'
}

export interface IpcAdvertisement extends SerializableRecord {
  readonly peerId: string
  readonly localName: string | null
  readonly rssi: number | null
}

export interface IpcCharacteristicRecord extends SerializableRecord {
  readonly handle: string
  readonly serviceUuid: string
  readonly serviceOccurrence: string
  readonly characteristicUuid: string
  readonly characteristicOccurrence: string
  readonly properties: readonly string[]
}

export interface IpcDescriptorRecord extends SerializableRecord {
  readonly handle: string
  readonly characteristicHandle: string
  readonly uuid: string
  readonly occurrence: string
}

interface RemoteStream<Value> {
  readonly source: CoreBoundedStream<Value>
  readonly publicStream: BoundedAsyncStream<Value>
}

/**
 * Framework-neutral manager façade for a trusted desktop webview transport.
 * The native host owns radio resources; this object only owns opaque handles,
 * cancellation, bounded stream projections, and deterministic release.
 */
export class IpcBleManager<Attachment extends string = string, Client extends string = string> {
  private readonly streams = new Map<string, RemoteStream<unknown>>()
  private readonly eventPump: Promise<void>
  private lifecycle: 'active' | 'releasing' | 'released' = 'active'
  private releaseResult: Promise<CleanupRecord> | null = null

  private constructor(private readonly client: IpcBleClient<Attachment, Client>) {
    this.eventPump = this.pumpEvents()
  }

  static async create<Attachment extends string, Client extends string>(
    transport: IpcClientTransport<Attachment, Client>
  ): Promise<IpcBleManager<Attachment, Client>> {
    const client = new IpcBleClient(transport)
    await client.initialize()
    return new IpcBleManager(client)
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
        deadline: operationDeadline(options)
      }),
      null,
      options.signal
    )
    const handle = requiredString(payload, 'handle', 'ipc-manager.scan')
    const observations = this.registerStream<IpcAdvertisement>(handle)
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
    return new IpcConnection(
      this,
      requiredString(payload, 'handle', 'ipc-manager.connect'),
      requiredString(payload, 'peerId', 'ipc-manager.connect')
    )
  }

  destroy(): Promise<CleanupRecord> {
    if (this.lifecycle === 'released') return Promise.resolve({ state: 'released', failures: [] })
    if (this.releaseResult !== null) return this.releaseResult
    this.lifecycle = 'releasing'
    this.releaseResult = this.client.destroy().then(async cleanup => {
      if (cleanup.state === 'released') {
        this.lifecycle = 'released'
        for (const stream of this.streams.values()) stream.source.closeWithReason('owner-released')
        this.streams.clear()
        await this.eventPump
      } else {
        this.lifecycle = 'active'
        this.releaseResult = null
      }
      return cleanup
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
    const timer = globalThis.setTimeout(() => controller.abort(), Math.max(0, deadline - globalThis.performance.now()))
    try {
      const receipt = await this.client.request({ command, payload, binaryPayload, signal: controller.signal })
      return receipt.payload
    } finally {
      globalThis.clearTimeout(timer)
      signal?.removeEventListener('abort', forwardAbort)
    }
  }

  registerStream<Value>(handle: string): BoundedAsyncStream<Value> {
    if (this.streams.has(handle)) throw new TypeError(`Duplicate remote stream handle: ${handle}`)
    const source = new CoreBoundedStream<Value>(REMOTE_STREAM_LIMITS, 'drop-oldest')
    this.streams.set(handle, { source: source as CoreBoundedStream<unknown>, publicStream: source })
    return source
  }

  closeStream(handle: string, reason: 'owner-released' | 'source-failed' = 'owner-released'): void {
    const stream = this.streams.get(handle)
    if (stream === undefined) return
    this.streams.delete(handle)
    stream.source.closeWithReason(reason)
  }

  private async pumpEvents(): Promise<void> {
    for await (const event of this.client.events) {
      if (event.kind !== 'value') continue
      const streamId = requiredString(event.value, 'streamId', 'ipc-manager.event')
      const item = requiredRecord(event.value, 'item', 'ipc-manager.event') as unknown as StreamItem<unknown>
      const remote = this.streams.get(streamId)
      if (remote === undefined) continue
      if (item.kind === 'value') {
        remote.source.emit(item.value, estimateByteLength(item.value))
      } else if (item.kind === 'overflow') {
        remote.source.observeSourceOverflow({
          kind: 'overflow',
          policy: item.policy,
          droppedItems: resourceCount(Number(item.droppedItems)),
          droppedBytes: resourceCount(Number(item.droppedBytes)),
          replacedItems: resourceCount(Number(item.replacedItems))
        })
      } else if (item.kind === 'terminal') {
        remote.source.finishWithReason(item.reason)
        this.streams.delete(streamId)
      }
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
  constructor(
    private readonly manager: IpcBleManager,
    readonly handle: string,
    readonly peerId: string
  ) {}

  async discover(options: IpcManagerOperationOptions = {}): Promise<IpcGattDatabase> {
    const payload = await this.manager.route(
      'gatt.discover',
      Object.freeze({ connectionHandle: this.handle, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return IpcGattDatabase.fromPayload(this.manager, this, payload)
  }

  async readRssi(options: IpcManagerOperationOptions = {}): Promise<number> {
    const payload = await this.manager.route(
      'connection.rssi',
      Object.freeze({ connectionHandle: this.handle, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return requiredNumber(payload, 'rssi', 'ipc-manager.connection-rssi')
  }

  async maximumWriteLength(mode: 'with-response' | 'without-response' = 'with-response'): Promise<number> {
    const payload = await this.manager.route(
      'connection.maximum-write-length',
      Object.freeze({ connectionHandle: this.handle, mode })
    )
    return requiredNumber(payload, 'bytes', 'ipc-manager.maximum-write-length')
  }

  async disconnect(): Promise<CleanupRecord> {
    return cleanupRecord(
      await this.manager.route('connection.disconnect', Object.freeze({ connectionHandle: this.handle }))
    )
  }

  release(): Promise<CleanupRecord> {
    return this.disconnect()
  }
}

export class IpcGattDatabase {
  readonly characteristics: readonly IpcCharacteristic[]
  readonly descriptors: readonly IpcDescriptor[]

  private constructor(
    private readonly manager: IpcBleManager,
    readonly connection: IpcConnection,
    readonly handle: string,
    characteristicRecords: readonly IpcCharacteristicRecord[],
    descriptorRecords: readonly IpcDescriptorRecord[]
  ) {
    this.characteristics = Object.freeze(characteristicRecords.map(record => new IpcCharacteristic(this, record)))
    this.descriptors = Object.freeze(descriptorRecords.map(record => new IpcDescriptor(this, record)))
  }

  static fromPayload(manager: IpcBleManager, connection: IpcConnection, payload: SerializableRecord): IpcGattDatabase {
    return new IpcGattDatabase(
      manager,
      connection,
      requiredString(payload, 'handle', 'ipc-manager.gatt-database'),
      requiredRecordArray(payload, 'characteristics', 'ipc-manager.gatt-database') as IpcCharacteristicRecord[],
      requiredRecordArray(payload, 'descriptors', 'ipc-manager.gatt-database') as IpcDescriptorRecord[]
    )
  }

  route(
    command: string,
    payload: SerializableRecord,
    binaryPayload: Uint8Array | null,
    signal?: AbortSignal
  ): Promise<SerializableRecord> {
    return this.manager.route(
      command,
      Object.freeze({ databaseHandle: this.handle, ...payload }),
      binaryPayload,
      signal
    )
  }

  registerStream<Value>(handle: string): BoundedAsyncStream<Value> {
    return this.manager.registerStream<Value>(handle)
  }

  closeStream(handle: string): void {
    this.manager.closeStream(handle)
  }
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
      Object.freeze({ characteristicHandle: this.handle, deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    const handle = requiredString(payload, 'handle', 'ipc-manager.gatt-subscribe')
    return new IpcSubscription(this.database, handle, this.database.registerStream<Uint8Array>(handle))
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

  write(bytes: Readonly<Uint8Array>, options: IpcWriteOptions = {}): Promise<SerializableRecord> {
    return this.database.route(
      'gatt.descriptor.write',
      Object.freeze({ descriptorHandle: this.record.handle, deadline: operationDeadline(options) }),
      ownBytes(bytes, byteLimit(bytes.byteLength)),
      options.signal
    )
  }
}

export class IpcSubscription {
  constructor(
    private readonly database: IpcGattDatabase,
    readonly handle: string,
    readonly values: BoundedAsyncStream<Uint8Array>
  ) {}

  async remove(): Promise<CleanupRecord> {
    const cleanup = cleanupRecord(
      await this.database.route('gatt.unsubscribe', Object.freeze({ subscriptionHandle: this.handle }), null)
    )
    if (cleanup.state === 'released') this.database.closeStream(this.handle)
    return cleanup
  }
}

function operationDeadline(options: IpcManagerOperationOptions): number | null {
  if (options.timeoutMs === undefined) return null
  if (!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0) throw new TypeError('timeoutMs must be positive')
  if (globalThis.performance === undefined) throw new TypeError('A monotonic performance clock is required')
  return globalThis.performance.now() + options.timeoutMs
}

function cleanupRecord(value: SerializableRecord): CleanupRecord {
  if ((value.state !== 'released' && value.state !== 'release-failed') || !Array.isArray(value.failures)) {
    throw new TypeError('Malformed cleanup receipt')
  }
  return value as unknown as CleanupRecord
}

function requiredRecord(record: SerializableRecord, key: string, operation: string): SerializableRecord {
  const value = record[key]
  if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array) {
    throw new TypeError(`Malformed ${operation} record`)
  }
  return value as SerializableRecord
}

function requiredRecordArray(record: SerializableRecord, key: string, operation: string): SerializableRecord[] {
  const value = record[key]
  if (!Array.isArray(value) || value.some(item => typeof item !== 'object' || item === null || Array.isArray(item))) {
    throw new TypeError(`Malformed ${operation} array`)
  }
  return value as SerializableRecord[]
}

function requiredString(record: SerializableRecord, key: string, operation: string): string {
  const value = record[key]
  if (typeof value !== 'string' || value.length === 0) throw new TypeError(`Malformed ${operation} string`)
  return value
}

function requiredNumber(record: SerializableRecord, key: string, operation: string): number {
  const value = record[key]
  if (typeof value !== 'number' || !Number.isFinite(value)) throw new TypeError(`Malformed ${operation} number`)
  return value
}

function requiredBytes(record: SerializableRecord, key: string, operation: string): Uint8Array {
  const value = record[key]
  if (!(value instanceof Uint8Array)) throw new TypeError(`Malformed ${operation} bytes`)
  return new Uint8Array(value)
}

function estimateByteLength(value: unknown): number {
  if (value instanceof Uint8Array) return value.byteLength
  return new TextEncoder().encode(JSON.stringify(value)).byteLength
}
