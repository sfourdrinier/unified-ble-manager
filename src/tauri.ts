/* eslint-disable @typescript-eslint/no-explicit-any */
// src/tauri.ts — zero-plumbing Tauri factory (PR1)

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
// Overloaded to keep RC1 test compatibility: if called with { invoke, Channel } it delegates to WithEnvironment.
export async function createTauriBleManager(
  options: BleManagerCreateOptions & { invoke?: any; Channel?: any } = {}
): Promise<BleManager> {
  // Backward compat: RC1 tests call createTauriBleManager({ invoke, Channel })
  if ((options as any).invoke !== undefined && (options as any).Channel !== undefined) {
    return createTauriBleManagerWithEnvironment(options as any, {})
  }
  normalizeBleManagerCreateOptions(options as BleManagerCreateOptions)
  // Dynamically import Tauri API to keep host-neutral root free of Tauri peer.
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
  // Wrap IpcBleManager in public façade — for PR1 we return the IPC manager directly
  // cast to BleManager (same vocabulary, different capability document from trusted host).
  // Full PR2 wrapping will project IPC receipts/capabilities to public types.
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
