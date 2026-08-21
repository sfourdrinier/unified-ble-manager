/* eslint-disable @typescript-eslint/no-explicit-any */
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
import type { OperationOptions } from './public/operation-options'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'

import { IpcBleManager } from './ipc/manager'
import type { IpcScanOptions } from './ipc/manager'
import { TauriBleIpcTransport } from './tauri/transport'
import type { TauriChannel } from './tauri/transport'

// Public Tauri manager adapter — wraps the IPC manager so the declared
// `BleManager` contract is backed by a real translation, not a type cast.
// It supports both the new `filter`-based ScanOptions and the legacy top-level
// `serviceUuids` shape used by the existing deterministic TCK, and it
// augments the IPC connection with a `peer` field for the public façade while
// preserving Ipc-specific members (adapterState, GATT) for test compatibility.
class TauriBleManagerAdapter implements BleManager {
  constructor(private readonly ipc: IpcBleManager) {}

  async scan(options: ScanOptions = {} as ScanOptions): Promise<any> {
    const anyOptions: any = options as any
    let ipcOptions: IpcScanOptions
    if (anyOptions.filter !== undefined) {
      const filter = anyOptions.filter as {
        serviceUuids?: readonly string[]
        manufacturerData?: readonly { companyId: number; dataPrefix?: Readonly<Uint8Array> }[]
        localNamePrefix?: string | null
      }
      ipcOptions = {
        serviceUuids: filter.serviceUuids,
        manufacturerData: filter.manufacturerData as any,
        localNamePrefix: filter.localNamePrefix ?? null,
        signal: anyOptions.signal,
        timeoutMs: anyOptions.timeoutMs
      } as IpcScanOptions
    } else {
      ipcOptions = {
        serviceUuids: anyOptions.serviceUuids,
        manufacturerData: anyOptions.manufacturerData,
        localNamePrefix: anyOptions.localNamePrefix,
        signal: anyOptions.signal,
        timeoutMs: anyOptions.timeoutMs
      } as IpcScanOptions
    }
    const session = await this.ipc.scan(ipcOptions)
    return session as any
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<any> {
    const peerId = typeof peer === 'string' ? peer : peer.id
    const ipcConn: any = await this.ipc.connect(peerId, options as any)
    if (ipcConn.peer === undefined) {
      ipcConn.peer = typeof peer === 'string' ? { id: peerId, name: null, rssi: null } : peer
    }
    if (ipcConn.peerId === undefined) ipcConn.peerId = peerId
    return ipcConn
  }

  withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    useConnection: (connection: any) => Promise<T>
  ): Promise<T> {
    return this.connect(peer, options).then(async connection => {
      try {
        return await useConnection(connection)
      } finally {
        await (connection as any).release()
      }
    })
  }

  destroy(): Promise<any> {
    return this.ipc.destroy()
  }

  // Ipc-specific surface retained for existing TCK — not part of the public
  // `BleManager` interface but present on the runtime object for compatibility.
  adapterState(): Promise<any> {
    return (this.ipc as any).adapterState()
  }
}

// Normal Tauri factory — imports invoke/Channel from @tauri-apps/api/core internally.
// No transport plumbing from application code.
export async function createTauriBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  let tauriCore: { invoke: any; Channel: any }
  try {
    tauriCore = await import('@tauri-apps/api/core')
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
  readonly invoke: (cmd: string, args?: unknown) => Promise<unknown>
  readonly Channel: new <T>() => TauriChannel<T>
}

export async function createTauriBleManagerWithEnvironment(
  environment: TauriBleManagerEnvironment,
  options: BleManagerCreateOptions = {}
): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  const transport = new TauriBleIpcTransport(environment as unknown as any)
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
