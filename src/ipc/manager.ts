import {
  BackendContractError,
  BLE_ERROR_CODES,
  contractError,
  type CleanupFailure,
  type CleanupRecord
} from '../backend-contract/errors'
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
import {
  createGattCharacteristicProperties,
  type CharacteristicProperties,
  type GattAccessRequirements,
  type GattDescriptorProperties
} from '../backend-contract/gatt'
import type { GattDatabaseChangedEvent } from '../backend-contract/gatt'
import { createPublicBleCapabilities, type BleCapabilities } from '../public/capabilities'
import { BUILT_IN_FEATURE_IDS, type CapabilityDescriptor } from '../backend-contract/capabilities'
import type {
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableDatabasePath,
  PortableGattDatabaseSnapshot,
  PortableOperationOptions,
  PortableSubscriptionOptions,
  PortableWritePolicy
} from '../manager/consumer-handles'
import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { AttachmentRecord } from '../backend-contract/identity'
import type { PeerReference } from '../backend-contract/peer-reference'
import { snapshotScanPlan } from '../backend-contract/scan-planning'
import type { ScanPlan } from '../backend-contract/scan-planning'
import { normalizeScanQuery } from '../public/scan-query'
import { BleCleanupError, collectCleanupPhases } from '../public/error-bridge'
import { IpcBleClient } from './client'
import { IPC_GATT_DATABASE_SCHEMA_VERSION } from './protocol'
import type { IpcCapabilitySnapshotV2, IpcClientTransport } from './protocol'
import { decodeIpcScanQuery, encodeIpcScanQuery } from './scan-planning'
import type { NormalizedScanQuery } from '../backend-contract/scan-query'

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

const MAX_PENDING_STREAM_IDS = 256
const MAX_PENDING_STREAM_ITEMS = 512
const MAX_PENDING_STREAM_BYTES = 2 * 1024 * 1024
const MAX_PENDING_STREAM_AGE_MS = 5_000
const MAX_PENDING_TOMBSTONES = 256
const MAX_PENDING_TOMBSTONE_AGE_MS = 5_000

export interface IpcBleManagerCreateOptions {
  readonly now?: () => number
}

export interface IpcPendingStreamAccounting {
  readonly pendingIdCount: number
  readonly pendingItemCount: number
  readonly pendingByteCount: number
  readonly tombstoneCount: number
  readonly activeStreamHandles: readonly string[]
}

interface PendingStreamRecord {
  readonly items: SerializableRecord[]
  bytes: number
  readonly createdAt: number
}

interface PendingStreamTombstone {
  readonly reason: 'overflow' | 'source-failed'
  readonly createdAt: number
}

const ipcPendingInspectors = new WeakMap<IpcBleManager, () => IpcPendingStreamAccounting>()
const ipcProvisionalInspectors = new WeakMap<IpcBleManager, () => IpcProvisionalAdmissionAccounting>()

export function inspectIpcPendingStreamAccountingForTests(manager: IpcBleManager): IpcPendingStreamAccounting {
  const inspect = ipcPendingInspectors.get(manager)
  if (inspect === undefined) {
    throw contractError('argument.invalid', 'ipc', 'ipc-manager.pending-inspect')
  }
  return inspect()
}

export interface IpcProvisionalAdmissionAccounting {
  readonly unresolvedConnectionCount: number
  readonly unresolvedEventSubscriptionCount: number
}

export function inspectIpcProvisionalAdmissionForTests(manager: IpcBleManager): IpcProvisionalAdmissionAccounting {
  const inspect = ipcProvisionalInspectors.get(manager)
  if (inspect === undefined) {
    throw contractError('argument.invalid', 'ipc', 'ipc-manager.provisional-inspect')
  }
  return inspect()
}

interface ProvisionalConnectIdentity {
  readonly handle: string | null
  readonly peerId: string | null
  readonly ownerLeaseId: string | null
  readonly connectionId: string | null
  readonly connectionGeneration: string | null
}

interface UnresolvedProvisional {
  readonly kind: 'connection' | 'connection-events'
  retry: () => Promise<CleanupRecord>
  error: unknown | null
}

export interface IpcManagerOperationOptions {
  readonly signal?: AbortSignal
  readonly timeoutMs?: number
  readonly deadline?: number | null
  readonly reason?: Extract<GattDatabaseChangedEvent['reason'], 'service-changed' | 'manual-rediscovery'>
  readonly deliveryMode?: 'prefer-notification' | 'prefer-indication' | 'require-notification' | 'require-indication'
  readonly stream?: {
    readonly itemCapacity: number
    readonly byteCapacity: number
    readonly reservedControlCapacity: number
    readonly overflowPolicy: OverflowPolicy
  }
}

export interface IpcScanOptions extends IpcManagerOperationOptions {
  readonly query?: NormalizedScanQuery
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
export type IpcScanObservation = IpcAdvertisement | AdvertisementObservation<string>

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
  readonly closeWithReason: (reason: StreamTerminalNotice['reason']) => void
  readonly deliver: (streamId: string, item: SerializableRecord) => void
  readonly notifyOwnerTerminal: (reason: StreamTerminalNotice['reason']) => void
}

/**
 * Framework-neutral manager façade for a trusted desktop webview transport.
 * The native host owns radio resources; this object only owns opaque handles,
 * cancellation, bounded stream projections, and deterministic release.
 */
export class IpcBleManager<Attachment extends string = string, Client extends string = string> {
  private readonly streams = new Map<string, StreamSink>()
  private readonly pendingStreams = new Map<string, PendingStreamRecord>()
  private readonly pendingStreamOverflows = new Map<string, { droppedItems: number; droppedBytes: number }>()
  private readonly pendingTombstones = new Map<string, PendingStreamTombstone>()
  private aggregatePendingItems = 0
  private aggregatePendingBytes = 0
  private readonly eventPump: Promise<void>
  private nextConnectionEventHandle = 1
  private lifecycle: 'active' | 'releasing' | 'released' = 'active'
  private releaseResult: Promise<CleanupRecord> | null = null
  private readonly ownerCleanupLedger: { run: () => void; error: unknown | null }[] = []
  private pumpDead = false
  private pumpFailure: unknown | null = null
  private readonly unresolvedProvisionals: UnresolvedProvisional[] = []

  private constructor(
    private readonly client: IpcBleClient<Attachment, Client>,
    readonly capabilities: BleCapabilities,
    private readonly now: () => number
  ) {
    this.eventPump = this.pumpEvents()
    void this.eventPump.then(
      () => undefined,
      error => {
        this.pumpFailure = error
        this.terminalizeEventPump('source-failed')
      }
    )
    ipcPendingInspectors.set(this, () => this.pendingAccounting())
    ipcProvisionalInspectors.set(this, () => this.provisionalAdmissionAccounting())
  }

  static async create<Attachment extends string, Client extends string>(
    transport: IpcClientTransport<Attachment, Client>,
    options: IpcBleManagerCreateOptions = {}
  ): Promise<IpcBleManager<Attachment, Client>> {
    const client = new IpcBleClient(transport)
    await client.initialize()
    return new IpcBleManager(
      client,
      createPublicBleCapabilities(
        projectRemoteCapabilities(client.bootstrap.capabilities),
        String(client.bootstrap.attachment.backendGeneration),
        true
      ),
      options.now ?? (() => Date.now())
    )
  }

  get bootstrap() {
    return this.client.bootstrap
  }

  async adapterState(options: IpcManagerOperationOptions = {}): Promise<SerializableRecord> {
    const payload = await this.route(
      'adapter.state',
      Object.freeze({ deadline: operationDeadline(options) }),
      null,
      options.signal
    )
    return requiredRecord(payload, 'state', 'ipc-manager.adapter-state')
  }

  async scan(options: IpcScanOptions = {}): Promise<IpcScanSession> {
    const query = encodeIpcScanQuery(options.query ?? normalizeScanQuery())
    const manufacturerData = (options.manufacturerData ?? []).map(filter => ({
      companyId: filter.companyId,
      dataPrefix:
        filter.dataPrefix === undefined ? null : ownBytes(filter.dataPrefix, byteLimit(filter.dataPrefix.byteLength))
    }))
    const payload = await this.route(
      'scan.start',
      Object.freeze({
        query,
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
    let plan: ScanPlan | null = null
    try {
      plan =
        payload.plan === undefined
          ? null
          : decodeIpcScanPlan(
              payload.plan,
              payload.backendGeneration,
              String(this.bootstrap.attachment.backendGeneration),
              options.query?.digest
            )
    } catch (error) {
      try {
        await this.route('scan.stop', Object.freeze({ scanHandle: handle }))
      } catch (cleanupError) {
        throw new AggregateError([error, cleanupError], 'IPC scan validation cleanup failed')
      } finally {
        this.closeStream(handle)
      }
      throw error
    }
    const observations = this.registerStream<IpcScanObservation>(
      handle,
      isIpcScanObservation,
      toRemoteStreamLimits(options.stream),
      options.stream?.overflowPolicy,
      reason => {
        if (reason === 'overflow' || reason === 'source-failed') {
          this.route('scan.stop', Object.freeze({ scanHandle: handle })).catch(() => undefined)
        }
      }
    )
    return new IpcScanSession(this, handle, observations, plan)
  }

  async connect(peerId: string, options: IpcManagerOperationOptions = {}): Promise<IpcConnection> {
    if (typeof peerId !== 'string' || peerId.length === 0) {
      throw contractError('argument.invalid', 'connection', 'ipc-manager.connect.peer-id')
    }
    const deadline = operationDeadline(options)
    const payload = await this.route(
      'connection.connect',
      Object.freeze({ peerId, deadline }),
      null,
      options.signal
    )
    if (deadline !== null && deadline <= globalThis.performance.now()) {
      const expired = decodeProvisionalConnectIdentity(payload)
      await this.compensateFailedConnect(
        expired,
        contractError('operation.timed-out', 'ipc', 'ipc-manager.connection.connect')
      )
    }
    const provisional = decodeProvisionalConnectIdentity(payload)
    const admissionError = validateProvisionalConnectIdentity(
      provisional,
      peerId,
      String(this.bootstrap.rendererLease.leaseId)
    )
    if (admissionError !== null) {
      await this.compensateFailedConnect(provisional, admissionError)
    }
    if (
      provisional.handle === null ||
      provisional.peerId === null ||
      provisional.ownerLeaseId === null ||
      provisional.connectionId === null ||
      provisional.connectionGeneration === null
    ) {
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.connect')
    }
    return new IpcConnection(
      this,
      provisional.handle,
      provisional.peerId,
      provisional.connectionId,
      provisional.ownerLeaseId,
      provisional.connectionGeneration
    )
  }

  destroy(): Promise<CleanupRecord> {
    if (this.lifecycle === 'released') return Promise.resolve({ state: 'released', failures: [] })
    if (this.releaseResult !== null) return this.releaseResult
    this.releaseResult = this.runDestroy()
    return this.releaseResult
  }

  private async runDestroy(): Promise<CleanupRecord> {
    const provisionalPhases = await this.flushUnresolvedProvisionals()
    this.lifecycle = 'releasing'
    try {
      const cleanup = await this.client.destroy()
      if (cleanup.state === 'released') {
        for (const sink of this.streams.values()) sink.closeWithReason('owner-released')
        this.streams.clear()
        this.clearPendingAccounting()
        await this.eventPump.catch(() => undefined)
      }
      const ownerPhases = this.flushOwnerCleanupLedger()
      const combined = collectCleanupPhases([
        { cleanup },
        ...(this.pumpFailure === null ? [] : [{ error: this.pumpFailure }]),
        ...ownerPhases,
        ...provisionalPhases
      ])
      if (combined.state === 'released' && ownerPhases.length === 0 && this.pumpFailure === null) {
        this.lifecycle = 'released'
        this.pumpFailure = null
      } else {
        this.lifecycle = 'active'
        this.releaseResult = null
      }
      return combined
    } catch (error) {
      this.lifecycle = 'active'
      this.releaseResult = null
      throw error
    }
  }

  async route(
    command: string,
    payload: SerializableRecord,
    binaryPayload: Uint8Array | null = null,
    signal: AbortSignal | null | undefined = null
  ): Promise<SerializableRecord> {
    this.assertActive()
    if (signal?.aborted === true) {
      throw contractError('operation.aborted', 'ipc', `ipc-manager.${command}`)
    }
    const deadline = payload.deadline
    if (deadline === null || deadline === undefined) {
      const receipt = await this.client.request({ command, payload, binaryPayload, signal: signal ?? null })
      return receipt.payload
    }
    if (typeof deadline !== 'number' || !Number.isFinite(deadline)) {
      throw new TypeError('Malformed IPC operation deadline')
    }
    if (globalThis.performance === undefined) throw new TypeError('A monotonic performance clock is required')
    if (deadline <= globalThis.performance.now()) {
      throw contractError('operation.timed-out', 'ipc', `ipc-manager.${command}`)
    }
    const controller = new AbortController()
    let callerAborted = false
    const forwardAbort = () => {
      callerAborted = true
      controller.abort()
    }
    signal?.addEventListener('abort', forwardAbort, { once: true })
    let timedOut = false
    const timer = globalThis.setTimeout(
      () => {
        timedOut = true
        controller.abort()
      },
      deadline - globalThis.performance.now()
    )
    try {
      const receipt = await this.client.request({ command, payload, binaryPayload, signal: controller.signal })
      return receipt.payload
    } catch (error) {
      if (timedOut && !callerAborted && error instanceof BackendContractError) {
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
    this.expirePendingState()
    const tombstone = this.pendingTombstones.get(handle)
    const source = new CoreBoundedStream<Value>(limits, overflowPolicy)
    const deliver = (streamId: string, item: SerializableRecord): void => {
      if (item.kind === 'value') {
        const rawValue: unknown = item.value
        if (!isValue(rawValue)) {
          this.streams.delete(handle)
          source.closeWithReason('source-failed')
          onTerminal?.('source-failed')
          throw contractError('protocol.malformed', 'ipc', 'ipc-manager.stream-value')
        }
        const push = source.emit(rawValue, estimateByteLength(rawValue))
        if (push.terminated) {
          this.streams.delete(handle)
          onTerminal?.('overflow')
        }
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
        this.discardPendingStream(streamId)
      }
    }
    const sink: StreamSink = {
      closeWithReason: reason => source.closeWithReason(reason),
      deliver,
      notifyOwnerTerminal: reason => {
        onTerminal?.(reason)
      }
    }
    if (tombstone !== undefined) {
      this.pendingTombstones.delete(handle)
      source.closeWithReason(tombstone.reason)
      this.captureOwnerCleanup(() => {
        onTerminal?.(tombstone.reason)
      })
      return source
    }
    this.streams.set(handle, sink)
    const pending = this.takePendingStream(handle)
    const pendingOverflow = this.pendingStreamOverflows.get(handle)
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
      for (const item of pending.items) {
        deliver(handle, item)
      }
    }
    return source
  }

  subscribeConnectionEvents(
    connectionHandle: string,
    identity: SerializableRecord,
    signal?: AbortSignal
  ): Promise<IpcConnectionEventSubscription> {
    return this.admitConnectionEvents(connectionHandle, identity, signal)
  }

  private async admitConnectionEvents(
    connectionHandle: string,
    identity: SerializableRecord,
    signal?: AbortSignal
  ): Promise<IpcConnectionEventSubscription> {
    const handle = `connection-events-ipc-${this.nextConnectionEventHandle++}`
    const payload = Object.freeze({
      ...identity,
      connectionHandle,
      connectionEventsHandle: handle,
      deadline: null
    })
    const response = await this.route('connection.events.subscribe', payload, null, signal)
    if (signal?.aborted === true) {
      await this.compensateFailedEventAdmission(
        handle,
        contractError('operation.aborted', 'ipc', 'ipc-manager.connection-events-subscribe')
      )
    }
    const validation = validateConnectionEventResponse(response, handle, identity)
    if (validation !== null) {
      await this.compensateFailedEventAdmission(handle, validation)
    }
    const events = this.registerStream(handle, isIpcConnectionLifecycleEvent, undefined, undefined, reason => {
      if (reason === 'overflow' || reason === 'source-failed') {
        this.route('connection.events.unsubscribe', Object.freeze({ connectionEventsHandle: handle })).catch(
          () => undefined
        )
      }
    })
    try {
      const ready = await this.route('connection.events.ready', Object.freeze({ connectionEventsHandle: handle }))
      if (ready.state !== 'ready') {
        throw contractError('protocol.malformed', 'ipc', 'ipc-manager.connection-events-ready')
      }
    } catch (error) {
      this.closeStream(handle, 'source-failed')
      await this.compensateFailedEventAdmission(handle, error)
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

  closeStream(
    handle: string,
    reason: 'owner-released' | 'source-failed' | 'connection-lost' | 'service-changed' = 'owner-released'
  ): void {
    this.discardPendingStream(handle)
    this.pendingTombstones.delete(handle)
    const sink = this.streams.get(handle)
    if (sink === undefined) return
    this.streams.delete(handle)
    sink.closeWithReason(reason)
  }

  private async pumpEvents(): Promise<void> {
    let cause: StreamTerminalNotice['reason'] = 'source-failed'
    try {
      for await (const event of this.client.events) {
        if (event.kind === 'terminal') {
          cause = event.reason === 'overflow' ? 'overflow' : 'source-failed'
          break
        }
        if (event.kind !== 'value') continue
        try {
          const eventValue = event.value
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
            const affected = this.streams.get(streamId)
            if (affected !== undefined) {
              affected.closeWithReason('source-failed')
              this.captureOwnerCleanup(() => affected.notifyOwnerTerminal('source-failed'))
              this.streams.delete(streamId)
              this.discardPendingStream(streamId)
            }
          }
        } catch (error) {
          this.pumpFailure = error
          cause = 'source-failed'
          break
        }
      }
    } catch (error) {
      this.pumpFailure = error
      cause = 'source-failed'
    } finally {
      this.terminalizeEventPump(cause)
    }
  }

  private terminalizeEventPump(cause: StreamTerminalNotice['reason']): void {
    if (this.pumpDead) return
    this.pumpDead = true
    const sinks = [...this.streams.values()]
    this.streams.clear()
    this.clearPendingAccounting()
    for (const sink of sinks) {
      sink.closeWithReason(cause)
      this.captureOwnerCleanup(() => sink.notifyOwnerTerminal(cause))
    }
  }

  private captureOwnerCleanup(run: () => void): void {
    try {
      run()
    } catch (error) {
      this.ownerCleanupLedger.push({ run, error })
    }
  }

  private flushOwnerCleanupLedger(): { readonly error: unknown }[] {
    const failures: { readonly error: unknown }[] = []
    for (const entry of this.ownerCleanupLedger) {
      if (entry.error === null) continue
      try {
        entry.run()
        entry.error = null
      } catch (error) {
        entry.error = error
        failures.push({ error })
      }
    }
    return failures
  }

  private bufferPendingStreamItem(streamId: string, item: SerializableRecord): void {
    this.expirePendingState()
    if (this.pendingTombstones.has(streamId)) return
    const existing = this.pendingStreams.get(streamId)
    if (existing === undefined) {
      while (this.pendingStreams.size >= MAX_PENDING_STREAM_IDS) {
        const oldestId = this.oldestPendingStreamId()
        if (oldestId === null) break
        this.evictPendingStream(oldestId, 'overflow')
      }
    }
    const pending = existing ?? { items: [], bytes: 0, createdAt: this.now() }
    if (existing === undefined) this.pendingStreams.set(streamId, pending)
    if (item.kind === 'terminal') {
      this.aggregatePendingItems -= pending.items.length
      this.aggregatePendingBytes -= pending.bytes
      pending.items.length = 0
      pending.bytes = 0
    }
    const itemCapacity = Number(REMOTE_STREAM_LIMITS.itemCapacity)
    const byteCapacity = Number(REMOTE_STREAM_LIMITS.byteCapacity)
    const itemBytes = estimateByteLength(item)
    let droppedItems = 0
    let droppedBytes = 0
    if (itemBytes > byteCapacity && item.kind !== 'terminal') {
      this.aggregatePendingItems -= pending.items.length
      this.aggregatePendingBytes -= pending.bytes
      pending.items.length = 0
      pending.bytes = 0
      droppedItems = 1
      droppedBytes = itemBytes
      const overflowTerminal = { kind: 'terminal', reason: 'overflow' }
      const overflowBytes = estimateByteLength(overflowTerminal)
      pending.items.push(overflowTerminal)
      pending.bytes = overflowBytes
      this.aggregatePendingItems += 1
      this.aggregatePendingBytes += overflowBytes
      this.recordPendingOverflow(streamId, droppedItems, droppedBytes)
      this.enforceAggregatePendingBudget()
      this.assertPendingAccounting()
      return
    }
    while (
      pending.items.length >= itemCapacity ||
      (pending.items.length > 0 && pending.bytes + itemBytes > byteCapacity)
    ) {
      const removed = pending.items.shift()
      if (removed === undefined) break
      const removedBytes = estimateByteLength(removed)
      pending.bytes -= removedBytes
      this.aggregatePendingItems -= 1
      this.aggregatePendingBytes -= removedBytes
      droppedItems += 1
      droppedBytes += removedBytes
    }
    pending.items.push(item)
    pending.bytes += itemBytes
    this.aggregatePendingItems += 1
    this.aggregatePendingBytes += itemBytes
    if (droppedItems > 0) this.recordPendingOverflow(streamId, droppedItems, droppedBytes)
    this.enforceAggregatePendingBudget()
    this.assertPendingAccounting()
  }

  private pendingAccounting(): IpcPendingStreamAccounting {
    let pendingItemCount = 0
    let pendingByteCount = 0
    for (const record of this.pendingStreams.values()) {
      pendingItemCount += record.items.length
      pendingByteCount += record.bytes
    }
    return {
      pendingIdCount: this.pendingStreams.size,
      pendingItemCount,
      pendingByteCount,
      tombstoneCount: this.pendingTombstones.size,
      activeStreamHandles: Object.freeze([...this.streams.keys()])
    }
  }

  private expirePendingState(): void {
    const now = this.now()
    for (const streamId of [...this.pendingStreams.keys()]) {
      const record = this.pendingStreams.get(streamId)
      if (record === undefined) continue
      if (now - record.createdAt >= MAX_PENDING_STREAM_AGE_MS) {
        this.evictPendingStream(streamId, 'source-failed')
      }
    }
    for (const streamId of [...this.pendingTombstones.keys()]) {
      const tombstone = this.pendingTombstones.get(streamId)
      if (tombstone === undefined) continue
      if (now - tombstone.createdAt >= MAX_PENDING_TOMBSTONE_AGE_MS) {
        this.pendingTombstones.delete(streamId)
      }
    }
  }

  private enforceAggregatePendingBudget(): void {
    while (
      this.aggregatePendingItems > MAX_PENDING_STREAM_ITEMS ||
      this.aggregatePendingBytes > MAX_PENDING_STREAM_BYTES
    ) {
      const oldestId = this.oldestPendingStreamId()
      if (oldestId === null) return
      this.evictPendingStream(oldestId, 'overflow')
    }
  }

  private oldestPendingStreamId(): string | null {
    const oldest = this.pendingStreams.keys().next()
    return oldest.done === true ? null : oldest.value
  }

  private evictPendingStream(streamId: string, reason: 'overflow' | 'source-failed'): void {
    this.discardPendingStream(streamId)
    this.rememberTombstone(streamId, reason)
  }

  private takePendingStream(streamId: string): PendingStreamRecord | undefined {
    const pending = this.pendingStreams.get(streamId)
    if (pending === undefined) return undefined
    this.pendingStreams.delete(streamId)
    this.aggregatePendingItems -= pending.items.length
    this.aggregatePendingBytes -= pending.bytes
    this.assertPendingAccounting()
    return pending
  }

  private discardPendingStream(streamId: string): void {
    const pending = this.pendingStreams.get(streamId)
    this.pendingStreamOverflows.delete(streamId)
    if (pending === undefined) return
    this.pendingStreams.delete(streamId)
    this.aggregatePendingItems -= pending.items.length
    this.aggregatePendingBytes -= pending.bytes
    this.assertPendingAccounting()
  }

  private rememberTombstone(streamId: string, reason: 'overflow' | 'source-failed'): void {
    while (this.pendingTombstones.size >= MAX_PENDING_TOMBSTONES) {
      const oldest = this.pendingTombstones.keys().next()
      if (oldest.done === true) break
      this.pendingTombstones.delete(oldest.value)
    }
    this.pendingTombstones.set(streamId, { reason, createdAt: this.now() })
  }

  private recordPendingOverflow(streamId: string, droppedItems: number, droppedBytes: number): void {
    const previous = this.pendingStreamOverflows.get(streamId)
    this.pendingStreamOverflows.set(streamId, {
      droppedItems: (previous?.droppedItems ?? 0) + droppedItems,
      droppedBytes: (previous?.droppedBytes ?? 0) + droppedBytes
    })
  }

  private clearPendingAccounting(): void {
    this.pendingStreams.clear()
    this.pendingStreamOverflows.clear()
    this.pendingTombstones.clear()
    this.aggregatePendingItems = 0
    this.aggregatePendingBytes = 0
  }

  private assertPendingAccounting(): void {
    const accounting = this.pendingAccounting()
    if (
      accounting.pendingItemCount !== this.aggregatePendingItems ||
      accounting.pendingByteCount !== this.aggregatePendingBytes ||
      this.aggregatePendingItems < 0 ||
      this.aggregatePendingBytes < 0
    ) {
      throw contractError('protocol.violation', 'ipc', 'ipc-manager.pending-accounting')
    }
  }

  private assertActive(): void {
    if (this.lifecycle !== 'active' || this.pumpDead) {
      throw new TypeError('Tauri BLE manager has been released')
    }
  }

  async retryUnresolvedAdmissionCleanup(): Promise<{ readonly error?: unknown; readonly cleanup?: CleanupRecord }[]> {
    return this.flushUnresolvedProvisionals()
  }

  private provisionalAdmissionAccounting(): IpcProvisionalAdmissionAccounting {
    let unresolvedConnectionCount = 0
    let unresolvedEventSubscriptionCount = 0
    for (const entry of this.unresolvedProvisionals) {
      if (entry.error === null) continue
      if (entry.kind === 'connection') unresolvedConnectionCount += 1
      else unresolvedEventSubscriptionCount += 1
    }
    return { unresolvedConnectionCount, unresolvedEventSubscriptionCount }
  }

  private async compensateFailedConnect(
    provisional: ProvisionalConnectIdentity,
    admissionError: unknown
  ): Promise<never> {
    if (provisional.handle === null) {
      this.unresolvedProvisionals.push({
        kind: 'connection',
        retry: async () => ({ state: 'released', failures: [] }),
        error: admissionError
      })
      throw admissionError
    }
    const disconnectPayload = Object.freeze({
      connectionHandle: provisional.handle,
      ...(provisional.peerId === null ? {} : { peerId: provisional.peerId }),
      ...(provisional.ownerLeaseId === null ? {} : { ownerLeaseId: provisional.ownerLeaseId }),
      ...(provisional.connectionId === null ? {} : { connectionId: provisional.connectionId }),
      ...(provisional.connectionGeneration === null ? {} : { connectionGeneration: provisional.connectionGeneration })
    })
    const retry = async (): Promise<CleanupRecord> =>
      cleanupRecord(await this.route('connection.disconnect', disconnectPayload))
    try {
      const cleanup = await retry()
      if (cleanup.state === 'released') throw admissionError
      this.unresolvedProvisionals.push({ kind: 'connection', retry, error: new BleCleanupError(cleanup) })
      throw new AggregateError([admissionError, new BleCleanupError(cleanup)], 'BLE cleanup failed')
    } catch (error) {
      if (error === admissionError) throw admissionError
      if (error instanceof AggregateError) throw error
      this.unresolvedProvisionals.push({ kind: 'connection', retry, error })
      throw new AggregateError([admissionError, error], 'BLE cleanup failed')
    }
  }

  private async compensateFailedEventAdmission(handle: string, admissionError: unknown): Promise<never> {
    const retry = async (): Promise<CleanupRecord> =>
      cleanupRecord(
        await this.route('connection.events.unsubscribe', Object.freeze({ connectionEventsHandle: handle }))
      )
    try {
      const cleanup = await retry()
      if (cleanup.state === 'released') throw admissionError
      this.unresolvedProvisionals.push({
        kind: 'connection-events',
        retry,
        error: new BleCleanupError(cleanup)
      })
      throw new AggregateError([admissionError, new BleCleanupError(cleanup)], 'BLE cleanup failed')
    } catch (error) {
      if (error === admissionError) throw admissionError
      if (error instanceof AggregateError) throw error
      this.unresolvedProvisionals.push({ kind: 'connection-events', retry, error })
      throw new AggregateError([admissionError, error], 'BLE cleanup failed')
    }
  }

  private async flushUnresolvedProvisionals(): Promise<
    { readonly error?: unknown; readonly cleanup?: CleanupRecord }[]
  > {
    const phases: { readonly error?: unknown; readonly cleanup?: CleanupRecord }[] = []
    for (const entry of this.unresolvedProvisionals) {
      if (entry.error === null) continue
      try {
        const cleanup = await entry.retry()
        if (cleanup.state === 'released') {
          entry.error = null
          phases.push({ cleanup })
        } else {
          entry.error = new BleCleanupError(cleanup)
          phases.push({ cleanup })
        }
      } catch (error) {
        entry.error = error
        phases.push({ error })
      }
    }
    return phases
  }
}

const REMOTE_SECURITY_CAPABILITY_IDS = new Set<string>([
  BUILT_IN_FEATURE_IDS.securityState,
  BUILT_IN_FEATURE_IDS.securityPair,
  BUILT_IN_FEATURE_IDS.securityCancelPairing,
  BUILT_IN_FEATURE_IDS.securityUnpair,
  BUILT_IN_FEATURE_IDS.securityCustomCeremony
])

const REMOTE_RENDERER_UNSUPPORTED_CAPABILITY_IDS = new Set<string>([
  BUILT_IN_FEATURE_IDS.connectionEffectiveMtu,
  BUILT_IN_FEATURE_IDS.connectionRequestMtu,
  BUILT_IN_FEATURE_IDS.connectionPriority,
  BUILT_IN_FEATURE_IDS.connectionPhy,
  BUILT_IN_FEATURE_IDS.connectionParameters,
  BUILT_IN_FEATURE_IDS.connectionSubrate,
  BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness
])

export function projectRemoteCapabilities(snapshot: IpcCapabilitySnapshotV2): IpcCapabilitySnapshotV2 {
  return Object.freeze({
    ...snapshot,
    descriptors: Object.freeze(
      snapshot.descriptors.map(descriptor =>
        REMOTE_SECURITY_CAPABILITY_IDS.has(descriptor.id)
          ? unsupportedRemoteSecurityDescriptor(descriptor)
          : REMOTE_RENDERER_UNSUPPORTED_CAPABILITY_IDS.has(descriptor.id)
            ? unsupportedRemoteRendererDescriptor(descriptor)
            : descriptor
      )
    )
  })
}

function unsupportedRemoteRendererDescriptor(descriptor: CapabilityDescriptor): CapabilityDescriptor {
  const limitation = Object.freeze({
    code: 'ipc-renderer-control-unavailable',
    explanation: 'This renderer IPC projection does not currently route this native control.',
    affectedGuarantee: 'native control support over renderer IPC'
  })
  return Object.freeze({
    ...descriptor,
    state: 'unsupported' as const,
    evidence: Object.freeze({
      ...descriptor.evidence,
      receiptId: `ipc-renderer-control-unavailable-${descriptor.id}`,
      evidenceLevel: 'blocked' as const,
      sourceDigest: 'ipc-renderer-control-projection-v1',
      limitations: Object.freeze([limitation])
    }),
    limitations: Object.freeze([limitation])
  })
}

function unsupportedRemoteSecurityDescriptor(descriptor: CapabilityDescriptor): CapabilityDescriptor {
  const limitation = Object.freeze({
    code: 'ipc-security-backend-unavailable',
    explanation: 'This desktop IPC projection does not currently route a native security backend.',
    affectedGuarantee: 'security operation support over trusted-host IPC'
  })
  return Object.freeze({
    ...descriptor,
    state: 'unsupported' as const,
    evidence: Object.freeze({
      ...descriptor.evidence,
      receiptId: `ipc-security-unavailable-${descriptor.id}`,
      evidenceLevel: 'blocked' as const,
      sourceDigest: 'ipc-security-projection-v1',
      limitations: Object.freeze([limitation])
    }),
    limitations: Object.freeze([limitation])
  })
}

export class IpcScanSession {
  private stopResult: Promise<CleanupRecord> | null = null

  constructor(
    private readonly manager: IpcBleManager,
    readonly handle: string,
    readonly observations: BoundedAsyncStream<IpcScanObservation>,
    readonly plan: ScanPlan | null
  ) {}

  stop(): Promise<CleanupRecord> {
    if (this.stopResult !== null) return this.stopResult
    const result = this.manager
      .route('scan.stop', Object.freeze({ scanHandle: this.handle }))
      .then(payload => cleanupRecord(payload))
      .then(cleanup => {
        if (cleanup.state === 'released') {
          this.manager.closeStream(this.handle)
        } else {
          this.stopResult = null
        }
        return cleanup
      })
      .catch(error => {
        this.stopResult = null
        throw error
      })
    this.stopResult = result
    return result
  }
}

export class IpcConnection {
  private readonly lifecycleEvents = new CoreBoundedStream<SerializableRecord>(REMOTE_STREAM_LIMITS, 'drop-oldest')
  private lifecycleAdmission: Promise<void> | null = null
  private readonly admissionAbort = new AbortController()
  private lifecycleSubscription: IpcConnectionEventSubscription | null = null
  private readonly databases = new Set<IpcGattDatabase>()
  private readonly _connectionId: string
  private readonly _ownerLeaseId: string
  private readonly _connectionGeneration: string
  private disconnectResult: Promise<CleanupRecord> | null = null
  private lifecycleReleased = false
  private connectionReleased = false

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
    this.ensureLifecycleAdmission().catch(() => undefined)
    return this.lifecycleEvents
  }

  private ensureLifecycleAdmission(): Promise<void> {
    if (this.lifecycleAdmission !== null) return this.lifecycleAdmission
    const admission = this.manager
      .subscribeConnectionEvents(this.handle, this.identityPayload(), this.admissionAbort.signal)
      .then(async subscription => {
        if (this.admissionAbort.signal.aborted || this.connectionReleased) {
          const cleanup = await subscription.unsubscribe()
          if (cleanup.state !== 'released') {
            throw new BleCleanupError(cleanup)
          }
          return
        }
        this.lifecycleSubscription = subscription
        this.pumpLifecycleEvents(subscription).catch(() => {
          this.lifecycleEvents.closeWithReason('source-failed')
        })
      })
      .catch(error => {
        this.lifecycleEvents.closeWithReason('source-failed')
        this.invalidateDatabases()
        throw error
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

  private invalidateDatabases(reason: GattDatabaseChangedEvent['reason'] | null = null): void {
    for (const database of this.databases) database.invalidate(reason)
    this.databases.clear()
  }

  async discover(options: IpcManagerOperationOptions = {}): Promise<IpcGattDatabase> {
    this.invalidateDatabases(options.reason ?? null)
    await this.awaitLifecycleAdmission(options)
    const payload = await this.manager.route(
      'gatt.discover',
      Object.freeze({
        ...this.identityPayload(),
        deadline: operationDeadline(options),
        ...(options.reason === undefined ? {} : { rediscoveryReason: options.reason })
      }),
      null,
      options.signal
    )
    if (options.reason !== undefined && payload.rediscoveryReason !== options.reason) {
      throw contractError('protocol.incompatible', 'ipc', 'ipc-manager.rediscovery-reason')
    }
    return IpcGattDatabase.fromPayload(this.manager, this, payload)
  }

  rediscoverGatt(
    options: IpcManagerOperationOptions,
    reason: Extract<GattDatabaseChangedEvent['reason'], 'service-changed' | 'manual-rediscovery'>
  ): Promise<IpcGattDatabase> {
    if (reason !== 'service-changed' && reason !== 'manual-rediscovery') {
      throw contractError('argument.invalid', 'gatt', 'ipc-manager.rediscover-gatt.reason')
    }
    return this.discover({ ...options, reason })
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

  disconnect(): Promise<CleanupRecord> {
    if (this.disconnectResult !== null) return this.disconnectResult
    const result = this.disconnectInternal().catch(error => {
      this.disconnectResult = null
      throw error
    })
    this.disconnectResult = result
    return result
  }

  private async disconnectInternal(): Promise<CleanupRecord> {
    this.invalidateDatabases()
    this.admissionAbort.abort()
    if (this.lifecycleSubscription === null) {
      this.lifecycleReleased = true
    }
    const failures: CleanupFailure[] = []
    let disconnectError: unknown = null
    const provisionalPhases = await this.manager.retryUnresolvedAdmissionCleanup()
    for (const phase of provisionalPhases) {
      if (phase.error !== undefined) failures.push(cleanupFailureFromUnknown('connection-events', phase.error))
      else if (phase.cleanup?.state === 'release-failed') failures.push(...phase.cleanup.failures)
    }
    if (!this.lifecycleReleased && this.lifecycleSubscription !== null) {
      try {
        const cleanup = await this.lifecycleSubscription.unsubscribe()
        if (cleanup.state === 'released') {
          this.lifecycleReleased = true
          this.lifecycleSubscription = null
        } else {
          failures.push(...cleanup.failures)
        }
      } catch (error) {
        failures.push(cleanupFailureFromUnknown('connection-events', error))
      }
    }
    if (!this.connectionReleased) {
      try {
        const cleanup = cleanupRecord(
          await this.manager.route('connection.disconnect', Object.freeze(this.identityPayload()))
        )
        if (cleanup.state === 'released') {
          this.connectionReleased = true
        } else {
          failures.push(...cleanup.failures)
        }
      } catch (error) {
        disconnectError = error
        failures.push(cleanupFailureFromUnknown('connection', error))
      }
    }
    if (this.lifecycleReleased && this.connectionReleased) {
      return { state: 'released', failures: [] }
    }
    this.disconnectResult = null
    if (disconnectError !== undefined && disconnectError !== null && this.lifecycleReleased && failures.length === 1) {
      throw disconnectError
    }
    if (disconnectError !== undefined && disconnectError !== null && failures.length > 1) {
      throw new AggregateError(
        failures.map(failure => new BackendContractError(failure.error)),
        'IPC connection cleanup failed'
      )
    }
    return { state: 'release-failed', failures: Object.freeze(failures) }
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

  private async awaitLifecycleAdmission(options: IpcManagerOperationOptions): Promise<void> {
    const admission = this.ensureLifecycleAdmission()
    const deadlineAt = operationDeadline(options)
    if (options.signal === undefined && deadlineAt === null) {
      return admission
    }
    return new Promise<void>((resolve, reject) => {
      let settled = false
      const timer =
        deadlineAt === null
          ? null
          : globalThis.setTimeout(
              () => {
                finish(reject, contractError('operation.timed-out', 'ipc', 'ipc-manager.connection-events-admission'))
              },
              Math.max(0, deadlineAt - globalThis.performance.now())
            )
      const abort = () => {
        finish(reject, contractError('operation.aborted', 'ipc', 'ipc-manager.connection-events-admission'))
      }
      const finish = (settle: (value: void | PromiseLike<void>) => void, value: void | BackendContractError): void => {
        if (settled) return
        settled = true
        if (timer !== null) globalThis.clearTimeout(timer)
        options.signal?.removeEventListener('abort', abort)
        if (value instanceof BackendContractError) {
          reject(value)
          return
        }
        settle(value)
      }
      options.signal?.addEventListener('abort', abort, { once: true })
      if (options.signal?.aborted === true) {
        abort()
        return
      }
      admission.then(
        () => finish(resolve, undefined),
        error => {
          if (!settled) {
            settled = true
            if (timer !== null) globalThis.clearTimeout(timer)
            options.signal?.removeEventListener('abort', abort)
            reject(error)
          }
        }
      )
    })
  }
}

export class IpcGattDatabase {
  readonly characteristics: readonly IpcCharacteristic[]
  readonly descriptors: readonly IpcDescriptor[]
  private valid = true
  private readonly changedStream = new CoreBoundedStream<GattDatabaseChangedEvent>(REMOTE_STREAM_LIMITS, 'drop-oldest')
  private readonly subscriptions = new Set<IpcSubscription>()

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

  invalidate(reason: GattDatabaseChangedEvent['reason'] | null = null): void {
    if (!this.valid) return
    this.valid = false
    for (const subscription of this.subscriptions) {
      subscription.closeFromDatabase(reason === 'service-changed' ? 'service-changed' : 'connection-lost')
    }
    this.subscriptions.clear()
    if (reason !== null) {
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

  closeStream(
    handle: string,
    reason: 'owner-released' | 'source-failed' | 'connection-lost' | 'service-changed' = 'owner-released'
  ): void {
    this.manager.closeStream(handle, reason)
  }

  registerSubscription(subscription: IpcSubscription): void {
    this.subscriptions.add(subscription)
  }

  forgetSubscription(subscription: IpcSubscription): void {
    this.subscriptions.delete(subscription)
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
            properties: characteristicPropertiesFromRecord(characteristic.record),
            access: characteristicAccessFromRecord(characteristic.record)
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
            properties: descriptorPropertiesFromRecord(descriptor.record)
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
    const subscription = new IpcSubscription(
      this.database,
      handle,
      this.database.registerStream<IpcNotificationValue>(
        handle,
        isIpcNotificationValue,
        toRemoteStreamLimits(options.stream),
        options.stream?.overflowPolicy,
        reason => {
          if (reason === 'service-changed') this.database.invalidate('service-changed')
          if (reason === 'overflow' || reason === 'source-failed') {
            this.database
              .route('gatt.unsubscribe', Object.freeze({ subscriptionHandle: handle }), null)
              .catch(() => undefined)
          }
        }
      )
    )
    this.database.registerSubscription(subscription)
    return subscription
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
  private removeResult: Promise<CleanupRecord> | null = null

  constructor(
    private readonly database: IpcGattDatabase,
    readonly handle: string,
    readonly values: BoundedAsyncStream<IpcNotificationValue>
  ) {}

  get subscriptionId(): string {
    return this.handle
  }

  remove(): Promise<CleanupRecord> {
    if (this.removeResult !== null) return this.removeResult
    const result = this.database
      .route('gatt.unsubscribe', Object.freeze({ subscriptionHandle: this.handle }), null)
      .then(payload => cleanupRecord(payload))
      .then(cleanup => {
        if (cleanup.state === 'released') {
          this.database.closeStream(this.handle)
          this.database.forgetSubscription(this)
        } else {
          this.removeResult = null
        }
        return cleanup
      })
      .catch(error => {
        this.removeResult = null
        throw error
      })
    this.removeResult = result
    return result
  }

  closeFromDatabase(reason: 'connection-lost' | 'service-changed'): void {
    this.database.closeStream(this.handle, reason)
    this.removeResult = Promise.resolve({ state: 'released', failures: [] })
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
  if (typeof options.deadline === 'number') {
    if (globalThis.performance !== undefined && options.deadline <= globalThis.performance.now()) {
      throw contractError('operation.timed-out', 'ipc', 'ipc-manager.deadline-expired')
    }
  }
  const subscriptionOptions = isPortableSubscriptionOptions(options) ? options : null
  const delivery = subscriptionOptions?.delivery
  return {
    signal: options.signal ?? undefined,
    deadline: typeof options.deadline === 'number' ? options.deadline : undefined,
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
  if (options.deadline !== undefined) {
    if (options.deadline === null) return null
    if (!Number.isFinite(options.deadline)) throw new TypeError('deadline must be finite or null')
    return options.deadline
  }
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

function cleanupFailureFromUnknown(resourceKind: string, error: unknown): CleanupFailure {
  if (error instanceof BackendContractError) {
    return { resourceKind, error: error.normalized }
  }
  return {
    resourceKind,
    error: {
      code: 'platform.failure',
      domain: 'connection',
      operation: `ipc-manager.${resourceKind}.cleanup`,
      platform: null,
      retryability: 'caller-decides'
    }
  }
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

function decodeIpcScanPlan(
  value: unknown,
  backendGeneration: unknown,
  expectedGeneration: string,
  expectedQueryDigest: string | undefined
): ScanPlan {
  if (
    typeof backendGeneration !== 'string' ||
    backendGeneration.length === 0 ||
    backendGeneration !== expectedGeneration
  ) {
    throw contractError('protocol.violation', 'ipc', 'ipc-manager.scan-plan-generation')
  }
  if (!isScanPlanWireRecord(value)) {
    throw contractError('protocol.malformed', 'ipc', 'ipc-manager.scan-plan')
  }
  try {
    const sourceQuery = decodeIpcScanQuery(value.sourceQuery, 'ipc-manager.scan-plan.source-query')
    const residual = requiredRecord(value, 'residual', 'ipc-manager.scan-plan.residual')
    const residualQuery = decodeIpcScanQuery(residual.query, 'ipc-manager.scan-plan.residual-query')
    const candidate = {
      ...value,
      sourceQuery,
      residual: { ...residual, query: residualQuery }
    }
    if (!isScanPlanRecord(candidate)) {
      throw contractError('protocol.malformed', 'ipc', 'ipc-manager.scan-plan')
    }
    const plan = snapshotScanPlan(candidate)
    if (expectedQueryDigest === undefined || plan.queryDigest !== expectedQueryDigest) {
      throw contractError('protocol.violation', 'ipc', 'ipc-manager.scan-plan-digest')
    }
    return plan
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    throw contractError('protocol.malformed', 'ipc', 'ipc-manager.scan-plan')
  }
}

function isScanPlanWireRecord(value: unknown): value is SerializableRecord {
  if (!isSerializableRecord(value)) return false
  return (
    'sourceQuery' in value &&
    'queryDigest' in value &&
    'residualQueryDigest' in value &&
    'nativeGuarantee' in value &&
    'native' in value &&
    'residual' in value &&
    'unavailable' in value &&
    'limitations' in value &&
    'estimatedCost' in value
  )
}

function isScanPlanRecord(value: unknown): value is ScanPlan {
  if (!isScanPlanWireRecord(value)) return false
  const sourceQuery = value.sourceQuery
  const native = value.native
  const residual = value.residual
  return (
    isNormalizedScanQueryRecord(sourceQuery) &&
    isScanProjectionRecord(native) &&
    isSerializableRecord(residual) &&
    isNormalizedScanQueryRecord(residual.query) &&
    isScanProjectionRecord(residual) &&
    Array.isArray(value.unavailable) &&
    Array.isArray(value.limitations) &&
    typeof value.queryDigest === 'string' &&
    typeof value.residualQueryDigest === 'string' &&
    (value.nativeGuarantee === 'exact' || value.nativeGuarantee === 'safe-superset') &&
    (value.estimatedCost === 'native-only' ||
      value.estimatedCost === 'low' ||
      value.estimatedCost === 'moderate' ||
      value.estimatedCost === 'high')
  )
}

function isNormalizedScanQueryRecord(value: unknown): boolean {
  return (
    isSerializableRecord(value) &&
    (value.anyOf === null || Array.isArray(value.anyOf)) &&
    (value.exclude === null || Array.isArray(value.exclude)) &&
    typeof value.digest === 'string'
  )
}

function isScanProjectionRecord(value: unknown): boolean {
  return isSerializableRecord(value) && Array.isArray(value.predicates) && typeof value.complete === 'boolean'
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

function optionalNonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null
}

function decodeProvisionalConnectIdentity(payload: SerializableRecord): ProvisionalConnectIdentity {
  return {
    handle: optionalNonEmptyString(payload.handle),
    peerId: optionalNonEmptyString(payload.peerId),
    ownerLeaseId: optionalNonEmptyString(payload.ownerLeaseId),
    connectionId: optionalNonEmptyString(payload.connectionId),
    connectionGeneration: optionalNonEmptyString(payload.connectionGeneration)
  }
}

function validateProvisionalConnectIdentity(
  provisional: ProvisionalConnectIdentity,
  requestedPeerId: string,
  ownerLeaseId: string
): BackendContractError | null {
  if (provisional.handle === null) {
    return contractError('protocol.malformed', 'ipc', 'ipc-manager.connect-handle')
  }
  if (
    provisional.peerId === null ||
    provisional.ownerLeaseId === null ||
    provisional.connectionId === null ||
    provisional.connectionGeneration === null
  ) {
    return contractError('protocol.malformed', 'ipc', 'ipc-manager.connect-identity')
  }
  if (provisional.peerId !== requestedPeerId || provisional.ownerLeaseId !== ownerLeaseId) {
    return contractError('protocol.violation', 'ipc', 'ipc-manager.connect-identity')
  }
  return null
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

function isIpcScanObservation(value: unknown): value is IpcScanObservation {
  return isIpcAdvertisement(value) || isNativeScanObservation(value)
}

function isNativeScanObservation(value: unknown): value is AdvertisementObservation<string> {
  const requiredKeys = [
    'device',
    'provenance',
    'sourceTimestamp',
    'receivedAtMonotonicMs',
    'ingressOrdinal',
    'scanSessionId',
    'localName',
    'rssi',
    'txPower',
    'connectable',
    'appearance',
    'serviceUuids',
    'solicitedServiceUuids',
    'overflowServiceUuids',
    'serviceData',
    'manufacturerData',
    'rawRecord',
    'scanResponseRecord'
  ]
  return typeof value === 'object' && value !== null && !Array.isArray(value) && requiredKeys.every(key => key in value)
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
  const propertiesMetadata: unknown = Reflect.get(value, 'propertiesMetadata')
  const access: unknown = Reflect.get(value, 'access')
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
    properties.every(entry => typeof entry === 'string') &&
    (propertiesMetadata === undefined || isCharacteristicProperties(propertiesMetadata)) &&
    (access === undefined || isGattAccessRequirements(access))
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
  const properties: unknown = Reflect.get(value, 'properties')
  return (
    typeof handle === 'string' &&
    handle.length > 0 &&
    typeof characteristicHandle === 'string' &&
    characteristicHandle.length > 0 &&
    typeof uuid === 'string' &&
    uuid.length > 0 &&
    typeof occurrence === 'string' &&
    occurrence.length > 0 &&
    (properties === undefined || isGattDescriptorProperties(properties))
  )
}

function isGattAccessRequirements(value: unknown): value is GattAccessRequirements {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  return isGattAccessValue(Reflect.get(value, 'read')) && isGattAccessValue(Reflect.get(value, 'write'))
}

function characteristicPropertiesFromRecord(record: IpcCharacteristicRecord): CharacteristicProperties {
  const metadata = Reflect.get(record, 'propertiesMetadata')
  if (isCharacteristicProperties(metadata)) return metadata
  return createGattCharacteristicProperties({
    broadcast: record.properties.includes('broadcast'),
    read: record.properties.includes('read'),
    writeWithResponse: record.properties.includes('write') || record.properties.includes('write-with-response'),
    writeWithoutResponse: record.properties.includes('write-without-response'),
    authenticatedSignedWrites: record.properties.includes('authenticated-signed-writes'),
    notify: record.properties.includes('notify'),
    indicate: record.properties.includes('indicate'),
    extendedProperties: record.properties.includes('extended-properties'),
    reliableWrite: record.properties.includes('reliable-write'),
    writableAuxiliaries: record.properties.includes('writable-auxiliaries')
  })
}

function characteristicAccessFromRecord(record: IpcCharacteristicRecord): GattAccessRequirements {
  const access = Reflect.get(record, 'access')
  if (isGattAccessRequirements(access)) return Object.freeze({ read: access.read, write: access.write })
  return Object.freeze({ read: 'unknown', write: 'unknown' })
}

function descriptorPropertiesFromRecord(record: IpcDescriptorRecord): GattDescriptorProperties {
  const properties = Reflect.get(record, 'properties')
  if (isGattDescriptorProperties(properties)) return properties
  return Object.freeze({
    read: false,
    write: false,
    availability: Object.freeze({ read: 'unknown', write: 'unknown' }),
    access: Object.freeze({ read: 'unknown', write: 'unknown' })
  })
}

function isGattAccessValue(value: unknown): value is GattAccessRequirements['read'] {
  return (
    value === 'none' ||
    value === 'encrypted' ||
    value === 'authenticated' ||
    value === 'authorized' ||
    value === 'unknown'
  )
}

function isCharacteristicProperties(value: unknown): value is CharacteristicProperties {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const booleanKeys = [
    'broadcast',
    'read',
    'writeWithResponse',
    'writeWithoutResponse',
    'authenticatedSignedWrites',
    'notify',
    'indicate',
    'extendedProperties',
    'reliableWrite',
    'writableAuxiliaries'
  ]
  if (!booleanKeys.every(key => typeof Reflect.get(value, key) === 'boolean')) return false
  const availability = Reflect.get(value, 'availability')
  if (typeof availability !== 'object' || availability === null || Array.isArray(availability)) return false
  const availabilityKeys = [
    'broadcast',
    'read',
    'writeWithResponse',
    'writeWithoutResponse',
    'authenticatedSignedWrites',
    'notify',
    'indicate',
    'extendedProperties',
    'reliableWrite',
    'writableAuxiliaries'
  ]
  return availabilityKeys.every(key => {
    const state = Reflect.get(availability, key)
    return state === 'known' || state === 'unknown'
  })
}

function isGattDescriptorProperties(value: unknown): value is GattDescriptorProperties {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const availability = Reflect.get(value, 'availability')
  return (
    typeof Reflect.get(value, 'read') === 'boolean' &&
    typeof Reflect.get(value, 'write') === 'boolean' &&
    typeof availability === 'object' &&
    availability !== null &&
    !Array.isArray(availability) &&
    (Reflect.get(availability, 'read') === 'known' || Reflect.get(availability, 'read') === 'unknown') &&
    (Reflect.get(availability, 'write') === 'known' || Reflect.get(availability, 'write') === 'unknown') &&
    isGattAccessRequirements(Reflect.get(value, 'access'))
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
