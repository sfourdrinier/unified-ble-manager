// src/tauri.ts — zero-plumbing Tauri application factory

import { contractError } from './backend-contract/errors'
import type { BleManagerCreateOptions } from './public/host-identity'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import { IpcBleManager } from './ipc/manager'
import { IpcPublicManagerAdapter } from './ipc/public-manager'
import { TauriBleIpcTransport } from './tauri/transport'
import type { TauriChannel, TauriInvoke } from './tauri/transport'
import type { BleManager } from './public/ble-manager'

// Tauri and Electron deliberately share the same public IPC projection. The
// host-specific module owns only transport loading and host-option admission;
// scan, connection, GATT, stream, lifecycle, and cleanup policy live in the
// common adapter.
const createTauriPublicManager = (ipc: IpcBleManager): BleManager => new IpcPublicManagerAdapter(ipc)

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
  await admitTauriCreateOptions(options, ipcManager)
  return createTauriPublicManager(ipcManager)
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
  await admitTauriCreateOptions(options, ipcManager)
  return createTauriPublicManager(ipcManager)
}

function assertTauriCreateOptions(options: BleManagerCreateOptions, ipc: IpcBleManager): void {
  if (options.adapterId !== undefined && options.adapterId !== String(ipc.bootstrap.attachment.adapter.adapterId)) {
    throw contractError('adapter.unavailable', 'adapter', 'tauri-manager.adapter')
  }
  if (options.restoration !== undefined) {
    throw contractError('capability.unsupported', 'restoration', 'tauri-manager.restoration')
  }
}

async function admitTauriCreateOptions(options: BleManagerCreateOptions, ipc: IpcBleManager): Promise<void> {
  try {
    assertTauriCreateOptions(options, ipc)
  } catch (error) {
    try {
      const cleanup = await ipc.destroy()
      if (cleanup.state === 'release-failed') {
        throw new AggregateError(
          [error, new Error('Tauri option rejection cleanup failed')],
          'BLE manager admission failed'
        )
      }
    } catch (cleanupError) {
      if (cleanupError instanceof AggregateError) throw cleanupError
      throw new AggregateError([error, cleanupError], 'BLE manager admission cleanup failed')
    }
    throw error
  }
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
