// src/tauri.ts — zero-plumbing Tauri application factory

import { assertPublicScanOptions, filterScanObservations, findPeerInScan, snapshotBlePeer } from './public/ble-manager'
import type { BleManager, BlePeer, FindOptions, ScanOptions } from './public/ble-manager'
import type { BleAdapter, BleAdapterState, AdapterReadinessOptions } from './public/ble-adapter'
import { diagnosticsUnavailable, type BleDiagnostics } from './public/diagnostics'
import type { BleConnection } from './public/ble-manager'
import type { ScanSession } from './public/ble-manager'
import type { OperationOptions } from './public/operation-options'
import { normalizeOperationOptions } from './public/operation-options'
import { resolveStreamPreset } from './public/stream-presets'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from './public/error-bridge'
import type { BleCapabilities } from './public/capabilities'
import { createPublicGattDatabase, type PublicGattDatabaseSource } from './public/gatt'
import { normalizeScanQuery } from './public/scan-query'
import { createScanState, type ScanStateController } from './public/scan-state'
import type {
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableNotificationValue,
  PortableOperationOptions,
  PortableWritePolicy,
  PortableWriteReceipt,
  PortableBoundedAsyncStream,
  SubscriptionHandle
} from './manager/consumer-handles'
import { contractError, BLE_ERROR_CODES } from './backend-contract/errors'

import { IpcBleManager } from './ipc/manager'
import type { IpcScanOptions } from './ipc/manager'
import type { IpcConnection } from './ipc/manager'
import type { IpcGattDatabase, IpcNotificationValue, IpcSubscription, IpcWriteReceipt } from './ipc/manager'
import type { CleanupRecord } from './backend-contract/errors'
import { TauriBleIpcTransport } from './tauri/transport'
import type { TauriChannel, TauriInvoke } from './tauri/transport'

function toIpcScanOptions(options: ScanOptions): IpcScanOptions {
  const delivery = resolveStreamPreset({ preset: options.delivery ?? 'balanced' })
  return {
    serviceUuids: undefined,
    manufacturerData: undefined,
    localNamePrefix: null,
    signal: options.signal,
    timeoutMs: options.timeoutMs,
    stream: {
      itemCapacity: Number(delivery.itemCapacity),
      byteCapacity: Number(delivery.byteCapacity),
      reservedControlCapacity: Number(delivery.reservedControlCapacity),
      overflowPolicy: delivery.overflowPolicy
    }
  }
}

function resolvePeerId(peer: BlePeer | string): string {
  return typeof peer === 'string' ? peer : peer.id
}

function resolveBlePeer(peer: BlePeer | string, peerId: string): BlePeer {
  return typeof peer === 'string' ? snapshotBlePeer({ id: peerId, name: null, rssi: null }) : snapshotBlePeer(peer)
}

class TauriScanSessionWrapper implements ScanSession {
  constructor(
    private readonly inner: import('./ipc/manager').IpcScanSession,
    private readonly filteredObservations: ScanSession['observations'],
    private readonly scanState: ScanStateController
  ) {
    scanState.emit({ state: 'active' })
  }

  async stop(): Promise<CleanupRecord> {
    this.scanState.emit({ state: 'stopping' })
    try {
      const cleanup = await rehydratePublicPromise(this.inner.stop())
      this.scanState.emit({ state: 'stopped' })
      this.scanState.close()
      return cleanup
    } catch (error) {
      this.scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
      this.scanState.close()
      throw error
    }
  }

  get observations(): ScanSession['observations'] {
    return this.filteredObservations
  }

  get state(): ScanSession['state'] {
    return this.scanState.stream
  }
}

function createTauriGattSource(database: IpcGattDatabase): PublicGattDatabaseSource {
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
      throw contractError('capability.unsupported', 'gatt', 'tauri.gatt.maximum-write-length')
    },
    writeLong: async () => {
      throw contractError('capability.unsupported', 'gatt', 'tauri.gatt.write-long')
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

function toPortableWriteReceipt(receipt: IpcWriteReceipt): PortableWriteReceipt {
  const cause = receipt.terminal.cause
  return {
    terminal: {
      correlation: receipt.terminal.correlation,
      outcome: receipt.terminal.outcome === 'succeeded' ? 'succeeded' : 'failed',
      cause: isBleErrorCode(cause) ? cause : null
    },
    commitState: receipt.commitState === 'confirmed' ? 'confirmed' : 'unknown'
  }
}

function isBleErrorCode(value: string | null): value is import('./backend-contract/errors').BleErrorCode {
  return value !== null && BLE_ERROR_CODES.some(code => code === value)
}

function toPortableSubscription(
  subscription: IpcSubscription,
  path: PortableCurrentCharacteristicPath
): SubscriptionHandle {
  return {
    subscriptionId: subscription.subscriptionId,
    path,
    values: toPortableNotificationStream(subscription.values),
    remove: () => toPortableCleanup(subscription.remove())
  }
}

function toPortableNotificationStream(
  source: import('./backend-contract/streams').BoundedAsyncStream<IpcNotificationValue>
): PortableBoundedAsyncStream<PortableNotificationValue> {
  return {
    limits: {
      itemCapacity: Number(source.limits.itemCapacity),
      byteCapacity: Number(source.limits.byteCapacity),
      reservedControlCapacity: Number(source.limits.reservedControlCapacity)
    },
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator]() {
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
            return {
              done: false,
              value: {
                kind: 'overflow',
                policy: result.value.policy,
                droppedItems: Number(result.value.droppedItems),
                droppedBytes: Number(result.value.droppedBytes),
                replacedItems: Number(result.value.replacedItems)
              }
            }
          }
          return {
            done: false,
            value: {
              kind: 'terminal',
              reason: result.value.reason,
              droppedItems: Number(result.value.droppedItems),
              droppedBytes: Number(result.value.droppedBytes),
              replacedItems: Number(result.value.replacedItems)
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
    },
    close: () => toPortableCleanup(source.close())
  }
}

async function toPortableCleanup(
  operation: Promise<CleanupRecord>
): Promise<import('./manager/consumer-handles').PortableCleanupRecord> {
  const cleanup = await operation
  return {
    state: cleanup.state,
    failures: cleanup.failures.map(failure => ({
      resourceKind: failure.resourceKind,
      error: {
        code: failure.error.code,
        domain: failure.error.domain,
        operation: failure.error.operation,
        retryability: failure.error.retryability,
        platform:
          failure.error.platform === null
            ? null
            : {
                domain: failure.error.platform.domain,
                code: failure.error.platform.code,
                safeMessage: failure.error.platform.safeMessage,
                metadata: toPortableSerializableRecord(failure.error.platform.metadata)
              }
      }
    }))
  }
}

function toPortableSerializableRecord(
  value: import('./backend-contract/primitives').SerializableRecord
): import('./manager/consumer-handles').PortableSerializableRecord {
  const record: Record<string, import('./manager/consumer-handles').PortableSerializableValue> = {}
  for (const [key, entry] of Object.entries(value)) {
    record[key] = toPortableSerializableValue(entry)
  }
  return Object.freeze(record)
}

function toPortableSerializableValue(
  value: import('./backend-contract/primitives').SerializableValue
): import('./manager/consumer-handles').PortableSerializableValue {
  if (value instanceof Uint8Array) return new Uint8Array(value)
  if (Array.isArray(value)) return Object.freeze(value.map(entry => toPortableSerializableValue(entry)))
  if (isSerializableRecord(value)) return toPortableSerializableRecord(value)
  return value
}

function isSerializableRecord(
  value: import('./backend-contract/primitives').SerializableValue
): value is import('./backend-contract/primitives').SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

class TauriBleConnectionWrapper implements BleConnection {
  readonly peer: BlePeer
  readonly peerId: string
  readonly handle: string
  readonly connectionId: string

  constructor(
    private readonly base: IpcConnection,
    peer: BlePeer | string,
    peerId: string
  ) {
    this.peer = resolveBlePeer(peer, peerId)
    this.peerId = peerId
    this.handle = base.handle
    this.connectionId = base.connectionId
  }

  get events(): IpcConnection['events'] {
    return this.base.events
  }

  async discover(options: OperationOptions = {}): Promise<import('./public/gatt').GattDatabase> {
    try {
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const database = await this.base.discover({
        signal: normalized.signal ?? undefined,
        timeoutMs:
          normalized.deadline === null ? undefined : Math.max(1, normalized.deadline - globalThis.performance.now())
      })
      return createPublicGattDatabase(createTauriGattSource(database))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  readRssi(options?: import('./ipc/manager').IpcManagerOperationOptions): Promise<number> {
    return rehydratePublicPromise(this.base.readRssi(options))
  }

  maximumWriteLength(mode?: 'with-response' | 'without-response'): Promise<number> {
    return rehydratePublicPromise(this.base.maximumWriteLength(mode))
  }

  disconnect(): Promise<CleanupRecord> {
    return rehydratePublicPromise(this.base.disconnect())
  }

  release(): Promise<CleanupRecord> {
    return rehydratePublicPromise(this.base.release())
  }
}

// Public Tauri manager adapter — wraps the IPC manager so the declared
// `BleManager` contract is backed by a real translation, not a type cast.
// It supports both the new `filter`-based ScanOptions and the legacy top-level
// `serviceUuids` shape used by the existing deterministic TCK, and it
// augments the IPC connection with a `peer` field for the public façade while
// preserving Ipc-specific members (adapterState, GATT) for test compatibility.
class TauriBleManagerAdapter implements BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly discovery: BleManager['discovery'] = Object.freeze({ kind: 'continuous-scan' })

  constructor(private readonly ipc: IpcBleManager) {
    this.capabilities = ipc.capabilities
    this.adapter = createTauriAdapter(ipc)
    this.diagnostics = diagnosticsUnavailable()
  }

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    try {
      assertPublicScanOptions(options)
      const ipcOptions = toIpcScanOptions(options)
      const session = await this.ipc.scan(ipcOptions)
      return new TauriScanSessionWrapper(
        session,
        filterScanObservations(session.observations, normalizeScanQuery(options.query)),
        createScanState()
      )
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  find(options: FindOptions = {}): Promise<BlePeer> {
    return this.scan({
      ...options,
      duplicates: 'coalesced',
      delivery: 'latest',
      timeoutMs: options.timeoutMs ?? 10_000
    }).then(scan =>
      runWithCleanup(
        () => findPeerInScan(scan, options.select),
        () => scan.stop()
      )
    )
  }

  choose(): Promise<BlePeer> {
    return Promise.reject(rehydratePublicError(contractError('capability.unsupported', 'chooser', 'tauri.choose')))
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<BleConnection> {
    try {
      const peerId = resolvePeerId(peer)
      const base = await this.ipc.connect(peerId, options)
      return new TauriBleConnectionWrapper(base, peer, peerId)
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    useConnection: (connection: BleConnection) => Promise<T>
  ): Promise<T> {
    const connection = await this.connect(peer, options)
    return runWithCleanup(
      () => useConnection(connection),
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
    options: OperationOptions,
    action: (scope: {
      readonly connection: BleConnection
      readonly gatt: import('./public/gatt').GattDatabase
    }) => Promise<T>
  ): Promise<T> {
    return this.withConnection(peer, options, async connection => {
      const gatt = await connection.discover(options)
      return action(Object.freeze({ connection, gatt }))
    })
  }

  destroy(): Promise<CleanupRecord> {
    return this.ipc.destroy().catch(error => {
      throw rehydratePublicError(error)
    })
  }

  // Ipc-specific surface retained for existing TCK — not part of the public
  // `BleManager` interface but present on the runtime object for compatibility.
  adapterState(): Promise<unknown> {
    return rehydratePublicPromise(this.ipc.adapterState())
  }
}

function createTauriAdapter(ipc: IpcBleManager): BleAdapter {
  const state = async (): Promise<BleAdapterState> => {
    const value = await ipc.adapterState()
    if (typeof value !== 'object' || value === null) {
      throw contractError('protocol.malformed', 'adapter', 'tauri.adapter.state')
    }
    const availability = Reflect.get(value, 'availability')
    const authorization = Reflect.get(value, 'authorization')
    const power = Reflect.get(value, 'power')
    const updatedAt = Reflect.get(value, 'updatedAt')
    const safeReason = Reflect.get(value, 'safeReason')
    if (typeof availability !== 'string' || typeof authorization !== 'string' || typeof power !== 'string') {
      throw contractError('protocol.malformed', 'adapter', 'tauri.adapter.state')
    }
    if (!isAdapterAvailability(availability) || !isAdapterAuthorization(authorization) || !isAdapterPower(power)) {
      throw contractError('protocol.malformed', 'adapter', 'tauri.adapter.state')
    }
    return Object.freeze({
      availability,
      authorization,
      power,
      backendGeneration: String(ipc.bootstrap.attachment.backendGeneration),
      updatedAt: typeof updatedAt === 'number' ? updatedAt : 0,
      safeReason: typeof safeReason === 'string' ? safeReason : null
    })
  }
  return {
    id: String(ipc.bootstrap.attachment.adapter.adapterId),
    state,
    waitUntilReady: async (options: AdapterReadinessOptions = {}) => {
      const normalized = normalizeOperationOptions(options, () => globalThis.performance.now())
      const deadline = normalized.deadline ?? globalThis.performance.now() + 10_000
      while (true) {
        const current = await state()
        if (current.availability === 'unsupported' || current.power === 'unsupported') {
          throw rehydratePublicError(contractError('capability.unsupported', 'adapter', 'tauri.adapter.ready'))
        }
        if (current.authorization === 'denied' || current.authorization === 'restricted') {
          throw rehydratePublicError(contractError('permission.denied', 'adapter', 'tauri.adapter.ready'))
        }
        if (current.availability === 'available' && current.power === 'on') return current
        if (normalized.signal?.aborted === true) {
          throw rehydratePublicError(contractError('operation.aborted', 'adapter', 'tauri.adapter.ready'))
        }
        if (globalThis.performance.now() >= deadline) {
          throw rehydratePublicError(contractError('operation.timed-out', 'adapter', 'tauri.adapter.ready'))
        }
        await new Promise(resolve =>
          setTimeout(resolve, Math.min(50, Math.max(1, deadline - globalThis.performance.now())))
        )
      }
    }
  }
}

function isAdapterAvailability(value: string): value is BleAdapterState['availability'] {
  return value === 'available' || value === 'unavailable' || value === 'unsupported' || value === 'unknown'
}

function isAdapterAuthorization(value: string): value is BleAdapterState['authorization'] {
  return (
    value === 'granted' ||
    value === 'denied' ||
    value === 'restricted' ||
    value === 'not-determined' ||
    value === 'unavailable' ||
    value === 'unknown'
  )
}

function isAdapterPower(value: string): value is BleAdapterState['power'] {
  return value === 'on' || value === 'off' || value === 'resetting' || value === 'unsupported' || value === 'unknown'
}

// Normal Tauri factory — imports invoke/Channel from @tauri-apps/api/core internally.
// No transport plumbing from application code.
export async function createTauriBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  let tauriCore: { invoke: TauriInvoke; Channel: new <T>() => TauriChannel<T> }
  try {
    const imported: { invoke: TauriInvoke; Channel: new <T>() => TauriChannel<T> } = await import(
      '@tauri-apps/api/core'
    )
    tauriCore = imported
  } catch {
    throw new Error(
      '[unified-ble-manager/tauri] @tauri-apps/api is required for createTauriBleManager(). ' +
        'Add it as a dependency or use createTauriBleManagerWithEnvironment for tests.'
    )
  }
  const transport = new TauriBleIpcTransport({
    invoke: tauriCore.invoke,
    Channel: tauriCore.Channel
  })
  const ipcManager = await IpcBleManager.create(transport)
  return new TauriBleManagerAdapter(ipcManager)
}

// Test/custom-host injection — retains explicit transport for deterministic tests.
export interface TauriBleManagerEnvironment {
  readonly invoke: TauriInvoke
  readonly Channel: new <T>() => TauriChannel<T>
}

export async function createTauriBleManagerWithEnvironment(
  environment: TauriBleManagerEnvironment,
  options: BleManagerCreateOptions = {}
): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  const transport = new TauriBleIpcTransport(environment)
  const ipcManager = await IpcBleManager.create(transport)
  return new TauriBleManagerAdapter(ipcManager)
}

export interface TauriBleProvider {
  /** Creates one independently leased manager for the authenticated Tauri webview. */
  createManager(): Promise<BleManager>
}

export function createTauriBleProvider(options: BleManagerCreateOptions = {}): TauriBleProvider {
  return Object.freeze({
    createManager: () => createTauriBleManager(options)
  })
}
