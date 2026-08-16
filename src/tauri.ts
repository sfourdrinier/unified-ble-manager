// src/tauri.ts

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

import { IpcBleManager } from './ipc/manager'
import { TauriBleIpcTransport, type TauriBleIpcTransportOptions } from './tauri/transport'

export interface TauriBleProvider {
  /** Creates one independently leased manager for the authenticated Tauri webview. */
  createManager(): Promise<IpcBleManager>
}

/**
 * Creates a reusable Tauri host provider. Adapter selection is configured on
 * the trusted Rust plugin, never accepted as renderer authority.
 */
export function createTauriBleProvider(options: TauriBleIpcTransportOptions): TauriBleProvider {
  return Object.freeze({
    createManager: () => createTauriBleManager(options)
  })
}

/** Creates the complete public Tauri manager over the authenticated v2 plugin transport. */
export function createTauriBleManager(options: TauriBleIpcTransportOptions): Promise<IpcBleManager> {
  return IpcBleManager.create(new TauriBleIpcTransport(options))
}
