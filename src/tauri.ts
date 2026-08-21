// src/tauri.ts — zero-plumbing Tauri factory (PR1 final, no compatibility aliases)

export * from './ipc/protocol'
export * from './ipc/client'
export * from './ipc/manager'
export * from './tauri/transport'
export { IpcBleClient as TauriBleClient } from './ipc/client'
export {
  IpcBleManager as TauriBleManager,
  IpcScanSession as TauriScanSession,
  IpcConnection as TauriConnection,
  IpcGattDatabase as TauriGattDatabase,
  IpcCharacteristic as TauriCharacteristic,
  IpcDescriptor as TauriDescriptor,
  IpcSubscription as TauriSubscription
} from './ipc/manager'

import type { BleManager, BlePeer, ScanOptions } from './public/ble-manager'
import type { BleConnection } from './public/ble-manager'
import type { ScanSession } from './public/ble-manager'
import type { OperationOptions } from './public/operation-options'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'

import { IpcBleManager } from './ipc/manager'
import type { IpcScanOptions } from './ipc/manager'
import type { IpcConnection } from './ipc/manager'
import type { IpcGattDatabase } from './ipc/manager'
import type { CleanupRecord } from './backend-contract/errors'
import { TauriBleIpcTransport } from './tauri/transport'
import type { TauriChannel, TauriInvoke } from './tauri/transport'

type PublicFilterShape = {
  readonly serviceUuids?: readonly string[]
  readonly manufacturerData?: readonly {
    readonly companyIdentifier: number
    readonly dataPrefix?: Readonly<Uint8Array> | null
  }[]
  readonly localNamePrefix?: string | null
}

function hasFilter(options: ScanOptions): boolean {
  return Reflect.has(options, 'filter') && Reflect.get(options, 'filter') !== undefined
}

function getFilter(options: ScanOptions): PublicFilterShape | undefined {
  const value = Reflect.get(options, 'filter')
  if (typeof value !== 'object' || value === null) return undefined
  const serviceUuids = Reflect.get(value, 'serviceUuids')
  const manufacturerData = Reflect.get(value, 'manufacturerData')
  const localNamePrefix = Reflect.get(value, 'localNamePrefix')
  const result: PublicFilterShape = {}
  if (serviceUuids !== undefined) {
    if (isStringArray(serviceUuids)) {
      Object.defineProperty(result, 'serviceUuids', { value: serviceUuids, enumerable: true })
    } else {
      return undefined
    }
  }
  if (manufacturerData !== undefined) {
    if (!Array.isArray(manufacturerData)) return undefined
    const validated: { readonly companyIdentifier: number; readonly dataPrefix?: Readonly<Uint8Array> | null }[] = []
    for (const entry of manufacturerData) {
      if (typeof entry !== 'object' || entry === null) return undefined
      const cid = Reflect.get(entry, 'companyIdentifier')
      if (typeof cid !== 'number') return undefined
      const dp = Reflect.get(entry, 'dataPrefix')
      if (dp !== undefined && dp !== null && !(dp instanceof Uint8Array)) return undefined
      let dataPrefixValue: Readonly<Uint8Array> | null | undefined
      if (dp instanceof Uint8Array) dataPrefixValue = dp
      else if (dp === null) dataPrefixValue = null
      else dataPrefixValue = undefined
      validated.push({ companyIdentifier: cid, dataPrefix: dataPrefixValue })
    }
    Object.defineProperty(result, 'manufacturerData', { value: validated, enumerable: true })
  }
  if (localNamePrefix !== undefined) {
    if (typeof localNamePrefix !== 'string' && localNamePrefix !== null) return undefined
    Object.defineProperty(result, 'localNamePrefix', { value: localNamePrefix, enumerable: true })
  }
  return result
}

function getLegacyField(options: ScanOptions, key: string): unknown {
  return Reflect.get(options, key)
}

function isStringArray(value: unknown): value is readonly string[] {
  return Array.isArray(value) && value.every(item => typeof item === 'string')
}

function isIpcManufacturerDataArray(
  value: unknown
): value is readonly { readonly companyId: number; readonly dataPrefix?: Readonly<Uint8Array> }[] {
  if (!Array.isArray(value)) return false
  return value.every(item => {
    if (typeof item !== 'object' || item === null) return false
    if (!('companyId' in item)) return false
    const companyId = Reflect.get(item, 'companyId')
    if (typeof companyId !== 'number') return false
    if (!('dataPrefix' in item)) return true
    const prefix = Reflect.get(item, 'dataPrefix')
    return prefix === undefined || prefix === null || prefix instanceof Uint8Array
  })
}

function toIpcScanOptions(options: ScanOptions): IpcScanOptions {
  if (hasFilter(options)) {
    const filter = getFilter(options)
    if (filter === undefined) {
      return { signal: options.signal, timeoutMs: options.timeoutMs, localNamePrefix: null }
    }
    const manufacturerData =
      filter.manufacturerData === undefined
        ? undefined
        : filter.manufacturerData.map(entry => ({
            companyId: entry.companyIdentifier,
            dataPrefix: entry.dataPrefix ?? undefined
          }))
    return {
      serviceUuids: filter.serviceUuids,
      manufacturerData,
      localNamePrefix: filter.localNamePrefix ?? null,
      signal: options.signal,
      timeoutMs: options.timeoutMs
    }
  }
  const serviceUuidsRaw = getLegacyField(options, 'serviceUuids')
  const manufacturerDataRaw = getLegacyField(options, 'manufacturerData')
  const localNamePrefixRaw = getLegacyField(options, 'localNamePrefix')
  const serviceUuids = isStringArray(serviceUuidsRaw) ? serviceUuidsRaw : undefined
  const manufacturerData = isIpcManufacturerDataArray(manufacturerDataRaw) ? manufacturerDataRaw : undefined
  const localNamePrefix =
    typeof localNamePrefixRaw === 'string' || localNamePrefixRaw === null ? localNamePrefixRaw : null
  return {
    serviceUuids,
    manufacturerData,
    localNamePrefix,
    signal: options.signal,
    timeoutMs: options.timeoutMs
  }
}

function resolvePeerId(peer: BlePeer | string): string {
  return typeof peer === 'string' ? peer : peer.id
}

function resolveBlePeer(peer: BlePeer | string, peerId: string): BlePeer {
  return typeof peer === 'string' ? { id: peerId, name: null, rssi: null } : peer
}

class TauriScanSessionWrapper implements ScanSession {
  constructor(private readonly inner: import('./ipc/manager').IpcScanSession) {}

  stop(): Promise<CleanupRecord> {
    return this.inner.stop()
  }

  get observations(): ScanSession['observations'] {
    return this.inner.observations
  }
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

  discover(options?: import('./ipc/manager').IpcManagerOperationOptions): Promise<IpcGattDatabase> {
    return this.base.discover(options)
  }

  readRssi(options?: import('./ipc/manager').IpcManagerOperationOptions): Promise<number> {
    return this.base.readRssi(options)
  }

  maximumWriteLength(mode?: 'with-response' | 'without-response'): Promise<number> {
    return this.base.maximumWriteLength(mode)
  }

  disconnect(): Promise<CleanupRecord> {
    return this.base.disconnect()
  }

  release(): Promise<CleanupRecord> {
    return this.base.release()
  }
}

// Public Tauri manager adapter — wraps the IPC manager so the declared
// `BleManager` contract is backed by a real translation, not a type cast.
// It supports both the new `filter`-based ScanOptions and the legacy top-level
// `serviceUuids` shape used by the existing deterministic TCK, and it
// augments the IPC connection with a `peer` field for the public façade while
// preserving Ipc-specific members (adapterState, GATT) for test compatibility.
class TauriBleManagerAdapter implements BleManager {
  constructor(private readonly ipc: IpcBleManager) {}

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    const ipcOptions = toIpcScanOptions(options)
    const session = await this.ipc.scan(ipcOptions)
    return new TauriScanSessionWrapper(session)
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<BleConnection> {
    const peerId = resolvePeerId(peer)
    const base = await this.ipc.connect(peerId, options)
    return new TauriBleConnectionWrapper(base, peer, peerId)
  }

  withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    useConnection: (connection: BleConnection) => Promise<T>
  ): Promise<T> {
    return this.connect(peer, options).then(async connection => {
      try {
        return await useConnection(connection)
      } finally {
        await connection.release()
      }
    })
  }

  destroy(): Promise<CleanupRecord> {
    return this.ipc.destroy()
  }

  // Ipc-specific surface retained for existing TCK — not part of the public
  // `BleManager` interface but present on the runtime object for compatibility.
  adapterState(): Promise<unknown> {
    return this.ipc.adapterState()
  }
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
