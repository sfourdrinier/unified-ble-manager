// src/node-corebluetooth.ts
//
// macOS Node entrypoint. The factories execute the shared Rust core
// (`DesktopCentral` over btleplug's CoreBluetooth radio) through the
// packaged N-API addon, identity-checked before any radio call. A missing or
// mismatched core fails loudly; nothing falls back to another backend.

import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import {
  createDesktopCoreBleManager,
  createDesktopCoreProvider,
  type DesktopCoreManagerOptions,
  type DesktopCoreProviderOptions
} from './node-desktop-manager'
import type { BleManager } from './public/ble-manager'

export {
  COREBLUETOOTH_BACKEND_ID,
  COREBLUETOOTH_IMPLEMENTATION_VERSION,
  COREBLUETOOTH_PLATFORM_ID,
  coreBluetoothCompatibility
} from './backends/desktop/platform-identity'
export * from './desktop-rust-core-exports'
export type { NodeBleManagerAppOptions } from './node-host-manager'
export type { DesktopCoreManagerOptions, DesktopCoreProviderOptions } from './node-desktop-manager'

export type CoreBluetoothBleManagerAppOptions = DesktopCoreManagerOptions
export type NativeCoreBluetoothProviderOptions = DesktopCoreProviderOptions

/** One-call Node CoreBluetooth manager over the shared Rust core. Does not fall back to another backend. */
export async function createCoreBluetoothBleManager(
  options: CoreBluetoothBleManagerAppOptions = {}
): Promise<BleManager> {
  return createDesktopCoreBleManager('corebluetooth', options)
}

/** Creates the production Node CoreBluetooth provider over the shared Rust core. */
export function createNativeCoreBluetoothBackendProvider(
  options: NativeCoreBluetoothProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopCoreProvider('corebluetooth', options, 'node')
}
