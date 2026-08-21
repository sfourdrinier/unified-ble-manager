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

import type { BleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'

import { IpcBleManager } from './ipc/manager'
import { TauriBleIpcTransport } from './tauri/transport'

// Normal Tauri factory — imports invoke/Channel from @tauri-apps/api/core internally.
// No transport plumbing from application code.
export async function createTauriBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  let tauriCore: { invoke: any; Channel: any }
  try {
    // @ts-expect-error — optional peer, may not be installed in CI
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
  return ipcManager as unknown as BleManager
}

// Test/custom-host injection — retains explicit transport for deterministic tests.
export interface TauriBleManagerEnvironment {
  readonly invoke: (cmd: string, args?: unknown) => Promise<unknown>
  readonly Channel: new () => { onmessage: (cb: (msg: unknown) => void) => void }
}

export async function createTauriBleManagerWithEnvironment(
  environment: TauriBleManagerEnvironment,
  options: BleManagerCreateOptions = {}
): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  const transport = new TauriBleIpcTransport(environment as any)
  const ipcManager = await IpcBleManager.create(transport)
  return ipcManager as unknown as BleManager
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
